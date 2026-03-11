#![allow(unused_variables)]
use anyhow::Result;
use chromiumoxide::cdp::browser_protocol::network::StreamResourceContentParamsBuilder;
use chromiumoxide::cdp::browser_protocol::target::CreateTargetParams;
use chromiumoxide::handler;
use futures::stream::Once;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use reqwest::header::{HeaderMap, REFERER};
use serde::Deserialize;
use serde_json::Value;
use std::str::Bytes;
use std::{env ,error::Error};
use std::io::{self, Write};
use std::path::PathBuf;

use text_io::read;
use winreg::RegKey;
use winreg::enums::*;
use std::fmt;

use std::fs;

//浏览器
use chromiumoxide::browser::{self, Browser, BrowserConfig, BrowserConfigBuilder};
use futures::StreamExt;
use chromiumoxide::Handler;
use tokio::time::{sleep,Duration,timeout};


//下载器相关
use tokio::io::copy;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Semaphore, SemaphorePermit};
use std::sync::Arc;
use tokio::task::JoinHandle;

//注册表相关
use std::os::windows::process::CommandExt; // 为了隐藏 PowerShell 窗口
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};


//

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct Chapter {
    //下载图片时依据的结构，len是图片数量，pages_url是每张图片的链接，number是第几章，url是该话的链接
    number: usize,
    url: String,
    title: String,
    pages_url: Vec<String>,
    len: usize,
}

#[derive(Deserialize, Debug)]
struct Response {
    //搜索时用到的结构，用于储存搜索结果
    code: i32,
    message: String,
    results: Results,
}

#[derive(Deserialize, Debug)]
struct Results {
    list: Vec<ManGa_item>,
}

#[derive(Deserialize, Debug, Clone)]
struct ManGa_item {
    name: String,
    path_word: String,
    cover:String,
    author:Vec<Author>,
}

#[derive(Deserialize, Debug, Clone)]
struct Author{
    name:String,
    alias:Option<String>,
    path_word:String
}

#[derive(Debug, Deserialize, Clone)]
struct Js_chapters {
    //从控制台获取的章节的名称与相应地址
    names: Vec<String>,
    path_words: Vec<String>,
    len: usize,
}

#[derive(Debug, Deserialize, Clone)]
struct ErrorLog {
    chapter_title: String,
    error_message: String,
}

impl fmt::Display for ErrorLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "章节: {}, \n 错误信息: {}\n",
            self.chapter_title, self.error_message
        )
    }
}

async fn kill_self_processes() {
    // 关键修改：只匹配命名的“前缀”，这样无论后面随机数是多少，都能抓出来
    // 注意：这里要跟你在 main 里面生成的文件夹前缀保持一致
    let target_prefix = "manga_downloader_profile_";

    println!("正在扫描并清理后台僵尸进程...");

    let ps_script = format!(
        r#"
        $target = '*{}*'
        $procs = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | 
                 Where-Object {{ 
                    ($_.Name -eq 'msedge.exe' -or $_.Name -eq 'chrome.exe') -and 
                    $_.CommandLine -like $target 
                 }}
        
        if ($procs) {{
            $count = $procs.Count
            $procs | ForEach-Object {{ 
                Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
            }}
        }} else {{
            Write-Output "未发现相关的僵尸进程。"
        }}
    "#,
        target_prefix
    );

    let output = Command::new("powershell")
        .args(&["-NoProfile", "-Command", &ps_script])
        .creation_flags(0x08000000)
        .output();

    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
        }
        Err(e) => println!("无法执行清理脚本: {}", e),
    }
}

async fn clean_old_profiles() {
    let temp_dir = env::temp_dir();

    // 读取 temp 目录下的所有内容
    if let Ok(entries) = fs::read_dir(temp_dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            // 检查是不是我们的文件夹
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("manga_downloader_profile_") {
                        // 尝试删除，失败了就忽略，绝不卡死程序
                        if let Err(_) = fs::remove_dir_all(&path) {
                            // 默默忽略，或者打印个 debug 信息
                        } else {
                            println!("已清理过期缓存: {}", name);
                        }
                    }
                }
            }
        }
    }
}

fn get_browser_path_from_registry() -> Option<PathBuf> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);

    // 1. 查找 Chrome (App Paths)
    if let Ok(key) =
        hklm.open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\chrome.exe")
    {
        if let Ok(path_str) = key.get_value::<String, _>("") {
            // 获取默认值
            return Some(PathBuf::from(path_str));
        }
    }

    // 2. 查找 Edge (App Paths)
    if let Ok(key) =
        hklm.open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\msedge.exe")
    {
        if let Ok(path_str) = key.get_value::<String, _>("") {
            return Some(PathBuf::from(path_str));
        }
    }

    // 3. 备用方案：查找卸载注册表 (有时候 App Paths 不准)
    let uninstall_keys = [
        "SOFTWARE\\Clients\\StartMenuInternet\\Google Chrome\\shell\\open\\command",
        "SOFTWARE\\Clients\\StartMenuInternet\\Microsoft Edge\\shell\\open\\command",
    ];

    for key_path in uninstall_keys {
        if let Ok(key) = hklm.open_subkey(key_path) {
            if let Ok(raw_cmd) = key.get_value::<String, _>("") {
                // 1. 去除引号
                let mut cmd = raw_cmd.replace("\"", "");

                // 2. 截取 .exe 结尾的路径 (关键修复)
                if let Some(idx) = cmd.to_lowercase().find(".exe") {
                    cmd = cmd[..idx + 4].to_string();
                }

                let path = PathBuf::from(&cmd);
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }
    None
}

async fn search(client: Client, base_website: &str) -> Result<Response, Box<dyn Error>> {
    print!("输入关键词：\n");
    let _ = io::stdout().flush();
    let key_word: String = read!();
    let base_url = format!("{}/api/kb/web/searchcd/comics", &base_website);
    let params = [
        ("offset", "0"),
        ("platform", "2"),
        ("limit", "12"), 
        ("q", &key_word),
        ("q_type", ""),
    ];


    let response = client.get(base_url).query(&params).send().await.expect("搜索失败1");

    let resp_text = response.text().await.expect("搜索失败2");
    //dbg!(&resp_text);
    let resp_json: Response = serde_json::from_str(&resp_text)?;
    //dbg!(format!("\n\n\n resp_json= {}\n\n\n",&resp_json));

    //println!("reponse：{:#?}", resp_json);

    println!("以下为搜索结果(仅列举至多12项)：");
    let lists = &resp_json.results.list;
    for (index, item) in lists.iter().enumerate() {
        println!("{}.{}", index, item.name);
    }
    print!("请输入要下载的漫画序号：");
    let _ = io::stdout().flush();
    Ok(resp_json)
}


async fn get_browser(client: Client) -> Result<(Browser,Handler), Box<dyn Error>> {
    
    let timestamp = 20260311u128;
    let unique_profile_name = format!("manga_downloader_profile_{}", timestamp);
    let user_data_path = env::temp_dir().join(&unique_profile_name);


    let builder:BrowserConfigBuilder = BrowserConfig::builder();
    let path = get_browser_path_from_registry().unwrap();
    println!("成功找到浏览器路径:{:?}",path);
    println!("正在打开浏览器.......");


    let options = builder
        .user_data_dir(user_data_path)
        .launch_timeout(Duration::from_secs(5))
        .request_timeout(Duration::from_secs(30))
        .chrome_executable(path)
        .args([
            "--no-sandbox",
            "--disable-setuid-sandbox",
            "--disable-gpu",
            "--disable-software-rasterizer",
            "--disable-extensions",       // 禁用扩展
            "--disable-infobars",         // 禁用顶部提示条
            "--no-first-run",             // 禁止首次运行向导
            "--no-default-browser-check", // 禁止询问是否设为默认浏览器
            "--disable-infobars",         // 禁止顶部提示条
            "--disable-extensions",       // 禁用扩展，提高速度
            "--password-store=basic",     // 禁用系统密码弹窗 
            "--disable-dev-shm-usage",
            "about:blank",
            ])
        .build()?;


        
       let (browser,mut handler) = Browser::launch(options).await?;
       println!("成功打开浏览器！");
       println!("\n\n\n"); 

    Ok((browser,handler))   
}


// fn write_chapter(path: &String, chapter: &Chapter) -> Result<(), Box<dyn Error>> {
    
//     let file = fs::File::create(path)?;
//     let writer = io::BufWriter::new(file);
//     serde_json::to_writer_pretty(writer, &chapter)?;

//     return Ok(());
// }

// fn read_chapter(path:&String)-> Result<Chapter, Box<dyn Error>> {
    
//     let file = fs::File::open(path)?;
//     let reader = io::BufReader::new(file);
//     let chapter = serde_json::from_reader(reader)?;
//     return Ok(chapter);
// }

#[tokio::main]
async fn main() {
    // 真正的逻辑放在 run() 里，main 只负责捕获错误
    if let Err(e) = run().await {
        eprintln!("\n==============================");
        eprintln!("程序发生严重错误，已停止运行：");
        eprintln!("{}", e);
        eprintln!("==============================");
    }

    sleep(Duration::from_secs(180)).await;
    println!("\n按回车键退出...");
    let _ = std::io::stdin().read_line(&mut String::new());
}

async fn run() -> Result<(), Box<dyn Error>> {
    kill_self_processes().await;
    clean_old_profiles().await;
    println!("======这是一个拷贝漫画的漫画下载器======");
    println!("默认保存路径在当前文件夹的download文件夹下\n\n");
    sleep(Duration::from_secs(2)).await;

    //初始化数据
    let mut download_chapters: Vec<Chapter> = Vec::new();
    let base_website = "https://ios.copymanga.club";
    let mut error_logs:Vec<ErrorLog> = Vec::new(); 

    //初始化client
    let mut headers = HeaderMap::new();
    headers.insert(REFERER, base_website.parse().unwrap());
    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
        .danger_accept_invalid_certs(true)
        .default_headers(headers)
        .build()?;



    let (mut browser,mut handler) = get_browser(client.clone()).await?;
   
    tokio::spawn(async move{
        while let Some(event) =handler.next().await {}
    });
    //初始化结束


    let resp_json:Response = search(client.clone(), &base_website).await.expect("搜索函数运行失败");
    //dbg!(&resp_json);
    let choice: i32 = read!();
    println!("请稍后...");
    let lists = &resp_json.results.list;
    let selected_item = lists[choice as usize].clone();
    let title = selected_item.name.clone();
    let path_word = selected_item.path_word.clone();

    let url: String = format!("{}/comic/{}", &base_website, &path_word);

    //启动浏览器

    let page = browser.new_page(url).await.expect("打开漫画详情页失败");

    // 等待外层容器出现，确保页面已加载
    page.wait_for_navigation().await?;

    let script = r#"
        (function() {
            window.Mydiv = document.getElementById('default全部');
            const container = window.Mydiv;
            if (!container) return [];
            
            const links = container.querySelectorAll('ul a');
            const data = {
                names: [],
                path_words:[],
                len:0
            };
            
            for (const link of links) {
                // 模拟你的逻辑：确保 a 标签里有 li 标签
                if (link.querySelector('li')) {
                    data.names.push(link.innerText.trim());
                    data.path_words.push(link.href);
                    data.len++;
                }
            }
            return JSON.stringify(data);
        })()
    "#;

    let Ok(remote_object) = page.evaluate(script).await else{
        panic!("获取漫画话数失败!");    
    };

    //dbg!(&remote_object);;
    let object = remote_object.value().unwrap();
    //dbg!("js获取的数据是",&object);
    let json_str = object.as_str().expect("JS返回的不是字符串");
    let js_chapters: Js_chapters = serde_json::from_str(json_str).unwrap();
    //dbg!(&js_chapters);
    let counts = js_chapters.len;
    let names = &js_chapters.names;

    println!("目录：");
    for (index, name) in names.iter().enumerate() {
        println!("{}:{}", index + 1, name);
    }
    println!("\n该漫画共有{}话\n", counts);
    

    print!("请输入起始话数(包含该话)：");
    let _ = io::stdout().flush();

    let start: usize = read!();
    let start = start - 1;
    if start >= counts {
        println!("输入的话数有误");
        sleep(Duration::from_secs(3)).await;
        return Ok(());
    }

    print!("请输入结束话数(包含该话)：");
    let _ = io::stdout().flush();

    let end: usize = read!();
    if end > counts || end <= start {
        println!("输入的话数有误");
        sleep(Duration::from_secs(3)).await;
        return Ok(());
    }



    //搜集章节信息
    for i in start..end {
        let link = js_chapters.path_words[i].clone();
        let Chapter_title = js_chapters.names[i].clone();

        //获取每一话的url与title
        download_chapters.push(Chapter {
            number: i,
            url: link,
            title: Chapter_title,
            ..Default::default()
        });
    }
    page.close();

    //解析章节页面的初始化
    let mut one_tab_count :usize  = 0;
    let mut chapter_tab = browser.new_page(&download_chapters[0].url).await.expect("解析第一话时，页面打开失败");

//解析章节页面，获取一话里的图片链接
    for chapter in &mut download_chapters {

        //限制单个tab解析章节数，防止内存泄漏
        //超出20个章节就重开一个tab
        one_tab_count +=1;
        if one_tab_count >= 20 {
            chapter_tab.close();
            one_tab_count =0;
            chapter_tab = browser.new_page(&chapter.url).await.expect("解析页面打开失败");
        }

        chapter_tab.goto(&chapter.url).await?;
        chapter_tab.wait_for_navigation().await?;

        println!("正在解析：{}", chapter.title);


         
        let script = r#"(async () => {
            return await new Promise((resolve) => {
                // --- 配置区 (可根据网速调整) ---
                const scrollStep = 500;   
                const frequency = 16;    
                const waitTime = 1500;   
                // ---------------------------

                let totalHeight = 0;
                let noChangeTicks = 0;
                
                const maxTicks = waitTime / frequency; 

                const timer = setInterval(() => {
                    const scrollHeight = document.body.scrollHeight;
                    const currentPos = window.scrollY + window.innerHeight;

                    window.scrollBy(0, scrollStep);

                    // 2. 检测是否触底 (留 50px 容差)
                    if (currentPos >= scrollHeight - 50) {
                        noChangeTicks++;

                        // 如果高度变了（加载出新图了），重置计数器
                        if (scrollHeight > totalHeight) {
                            totalHeight = scrollHeight;
                            noChangeTicks = 0;
                        }

                        // 如果连续 N 次循环高度都没变，说明真的到底了
                        if (noChangeTicks >= maxTicks) {
                            clearInterval(timer);
                            
                            // 3. 抓取结果
                            let images = document.querySelectorAll('img'); 
                            let urls = [];
                            images.forEach((img) => {
                                // 优先 data-src，其次 src
                                let url = img.getAttribute('data-src');
                                if (url) urls.push(url);
                            });
                            
                            resolve(JSON.stringify(urls));
                        }
                    } else {
                        // 还没到底，重置计数器
                        if (scrollHeight > totalHeight) {
                            totalHeight = scrollHeight;
                        }
                        noChangeTicks = 0;
                    }
                }, frequency);
            });
        })();
        "#;

        //上诉js代码返回的是该话的每一张图的url的数组
        
        let Js_pages_url_response = chapter_tab.evaluate(script).await.expect("解析失败1");
    
        //dbg!(&Js_pages_url_response);
    
        let Js_pages_url_response = Js_pages_url_response
        .value()
        .unwrap()
        .as_str()
        .expect("不是String")
        .to_string();

       // dbg!(&Js_pages_url_response);
        let Js_pages_url_response:Vec<String> = serde_json::from_str(&Js_pages_url_response).unwrap(); 
        
        
        chapter.pages_url = Js_pages_url_response;
        chapter.len = chapter.pages_url.len(); 
        println!("{}.{}共{}页",chapter.number,chapter.title,chapter.len);

// match Js_pages_url_response {
//     Some(val) => {
//         if let Some(json_str) = val.as_str() {
//             match serde_json::from_str::<Vec<String>>(json_str) {
//                 Ok(urls) => {
//                     chapter.pages_url = urls;
//                     chapter.len = chapter.pages_url.len();
//                     println!("{} 共 {} 页", chapter.title, chapter.len);
//                 }
//                 Err(e) => {
//                     println!("解析JSON失败: {}，跳过本章", e);
//                     // 记录错误日志...
//                     continue; 
//                 }
//             }
//         } else {
//              println!("JS返回的数据不是字符串，跳过本章");
//              continue;
//         }
//     },
//     None => {
//         println!("页面加载超时或JS执行未返回数据，跳过本章: {}", chapter.title);
//         // 这里可以选择重试逻辑，而不是直接让程序崩溃
//         continue;
//     }
// }

    }

    chapter_tab.close();
    browser.close().await?;

    clean_old_profiles();

    new_download(download_chapters, title, client.clone()).await;

    
   
    //打印错误日志
    for log in error_logs {
        println!("错误章节记录：{}", log);
    }

    Ok(())
}

async fn download(
    chapters: Vec<Chapter>,
    title: String,
    client: Client,
) -> Result<(), Box<dyn Error>> {
       //为多线程下载做准备，限制线程数量
        let once_max_dowload = Arc::new(Semaphore::new(64));

    for chapter in chapters {
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        //创建漫画文件夹
        let path = format!("./download/{}/{}", title, chapter.title);
        fs::create_dir_all(&path)?;

        //创建进度条
        let pb = ProgressBar::new(chapter.len as u64);
        pb.set_style(ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>7}/{len:7} {msg}")
            .unwrap()
            .progress_chars("█=>"));
        pb.set_message(format!("下载中: {}", chapter.title));
        //创建进度条

 

        for (index, page_url) in chapter.pages_url.iter().enumerate() {
            let client_clone = client.clone();
            let chapter_clone = chapter.clone();
            let page_len_clone = chapter.pages_url.len().clone();
            let title_clone = title.clone();
            let page_url_clone = page_url.clone();
            let pb_clone = pb.clone();                                

            let once_max_download_clone = once_max_dowload.clone();

        //创建子进程
        let handle = tokio::spawn(async move{
                let aquire = once_max_download_clone.acquire_owned().await.unwrap();

                let page_path = format!(
                    "./download/{}/{}/{}.webp",
                    title_clone,
                    chapter_clone.title,
                    index + 1
                );
                let max_retries = 3;

                //在开始下载前创建相应图片文件
                let mut page = tokio::fs::File::create(&page_path).await.unwrap();
                for i in 1..=max_retries {


                    //发送网络请求
                    let response = client_clone.
                    get(&page_url_clone)
                    .send()
                    .await;

                    match response {
                        Ok(mut res) => {
                            let mut bytes = res.bytes_stream();

                        //流下载    
                        while let Some(chunk) = bytes.next().await {
                                match chunk {
                                    Ok(chunk) => {
                                        page.write_all(&chunk).await.unwrap();
                                        
                                    }
                                    Err(e) => {
                                        println!("下载出错: {}", e);
                                        println!("正在重试第{}次",i);
                                        break;
                                    }
                                }
                            } 
                        pb_clone.inc(1);
                        break;
                            }


                        Err(e) => {
                            println!(
                                "发起请求失败：{}第{}页",
                                chapter_clone.title,
                                index + 1,
                                
                            );
                            if i < max_retries {
                                println!("正在重试第{}次...", i);
                            } else {
                                println!("达到最大重试次数，跳过该页");
                            }
                            sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
                
            });

            handles.push(handle);
        }

        for handle in handles {
             let _ = handle.await;
            }

        pb.finish_with_message(format!("{} 下载完毕", chapter.title));
    
        }
    
       
    println!("\n所有章节下载完成！");
    println!("温馨提醒：");
    println!("会有极小概率一话页数没有完整加载出来，导致尾部缺页情况发生，");
    println!("可以根据每话之间的页数对比 or 是否有汉化组尾页来确定是否缺页");
    println!("重新下载该话能补全页数\n\n");
    Ok(())
    }

async fn new_download(
    chapters: Vec<Chapter>,
    title: String,
    client: Client,
) -> Result<(), Box<dyn Error>> {
       //为多线程下载做准备，限制线程数量
        let once_max_dowload = Arc::new(Semaphore::new(64));

    for chapter in chapters {
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        //创建漫画文件夹
        let path = format!("./download/{}/{}", title, chapter.title);
        fs::create_dir_all(&path)?;

        //创建进度条
        let pb = ProgressBar::new(chapter.len as u64);
        pb.set_style(ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>7}/{len:7} {msg}")
            .unwrap()
            .progress_chars("█=>"));
        pb.set_message(format!("下载中: {}", chapter.title));
        //创建进度条

 

        for (index, page_url) in chapter.pages_url.iter().enumerate() {
            let client_clone = client.clone();
            let chapter_clone = chapter.clone();
            let page_len_clone = chapter.pages_url.len().clone();
            let title_clone = title.clone();
            let page_url_clone = page_url.clone();
            let pb_clone = pb.clone();                                

            let once_max_download_clone = once_max_dowload.clone();

        //创建子进程
        let handle = tokio::spawn(async move{
                let aquire = once_max_download_clone.acquire_owned().await.unwrap();

                let page_path = format!(
                    "./download/{}/{}/{}.webp",
                    title_clone,
                    chapter_clone.title,
                    index + 1
                );
                let limit = Duration::from_secs(60);


                //在开始下载前创建相应图片文件
                let timeout_result = timeout(limit, async {             
                        
                    let mut isErr :bool = false;
                    loop{

                        let mut page = tokio::fs::File::create(&page_path).await.unwrap();

                        //发送网络请求
                        let response = client_clone.
                        get(&page_url_clone)
                        .send()
                        .await;

                        match response {
                            Ok(res) =>{

                                let mut steam = res.bytes_stream();

                                    while let Some(chunk) = steam.next().await{
                                        if let Ok(chunk) = chunk{
                                            page.write_all(&chunk).await.unwrap();
                                        }
                                    }
                                    
                                    if isErr {
                                        println!("{}:第{}页下载重试完成，下载成功！",chapter_clone.title,index+1);
                                    }
                                    pb_clone.inc(1);
                                    break;
                            }
                            Err(_) =>{
                                if !isErr {
                                    println!("{}:第{}页下载失败，正在重试...",chapter_clone.title,index+1);
                                    isErr = true;
                                }
                            }
                        }
                        
                    } 
               })
                .await
                .expect("超过重试时长(60s)，跳过下载该页");
                    
    
                    }   
            );

            handles.push(handle);
        }

        for handle in handles {
             let _ = handle.await;
            }

        pb.finish_with_message(format!("{} 下载完毕", chapter.title));
    
        }
    
       
    println!("\n所有章节下载完成！");
    println!("温馨提醒：");
    println!("会有极小概率一话页数没有完整加载出来，导致尾部缺页情况发生，");
    println!("可以根据每话之间的页数对比 or 是否有汉化组尾页来确定是否缺页");
    println!("重新下载该话能补全页数\n\n");
    Ok(())
    }

