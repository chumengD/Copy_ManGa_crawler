#![allow(unused_variables)]
use anyhow::Result;
use headless_chrome::{Browser, LaunchOptionsBuilder};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, REFERER};
use serde::Deserialize;
use serde_json::Value;
use std::{env ,error::Error ,ffi::OsStr};
use std::io::{self, Write, copy};
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::thread::sleep;
use std::time::Duration;
use std::{fs, thread};
use text_io::read;
use winreg::RegKey;
use winreg::enums::*;
use std::fmt;

use std::os::windows::process::CommandExt; // 为了隐藏 PowerShell 窗口
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use glob::glob;
use image::ImageReader;

use std::mem;
use std::net::TcpStream; // 用于检测端口 // 用于防止主进程杀浏览器
use std::sync::Arc;
use std::sync::mpsc;

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

fn kill_zombie_processes() {
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

fn clean_old_profiles() {
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

fn search(client: Client, base_website: &str) -> Result<Response, Box<dyn Error>> {
    print!("输入关键词：\n");
    let _ = io::stdout().flush();
    let key_word: String = read!();
    let base_url = format!("{}/api/kb/web/searchcd/comics", &base_website);
    let params = [
        ("offset", "0"),
        ("platform", "2"),
        ("limit", "12"), // 我改成 10 了
        ("q", &key_word),
        ("q_type", ""),
    ];

    let response = client.get(base_url).query(&params).send()?;

    let resp_text = response.text()?;
    let resp_json: Response = serde_json::from_str(&resp_text)?;

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

fn get_browser(client: Client) -> Result<(Browser, bool), Box<dyn Error>> {
    let port = 9222;
    let addr = format!("127.0.0.1:{}", port);

    //检查端口是否开启
    if TcpStream::connect(&addr).is_ok() {
        println!(r"检查到运行中的浏览器内核（端口{}），正在接入", port);
        let CDP = client.get(format!("http://{}/json/version", addr)).send()?;
        let cdp_json = serde_json::from_str::<Value>(&CDP.text()?)?;
        let web_socket_debugger_url = cdp_json["webSocketDebuggerUrl"]
            .as_str()
            .ok_or("无法获取 webSocketDebuggerUrl")?;
        dbg!(&web_socket_debugger_url);
        let browser = Browser::connect(web_socket_debugger_url.to_string())?;
        return Ok((browser, false)); // false 表示不是“主”，只是接入者
    }

    kill_zombie_processes();
    clean_old_profiles();

    println!("未检测到运行中的浏览器内核，正在启动新的浏览器内核...");
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let unique_profile_name = format!("manga_downloader_profile_{}", timestamp);
    let user_data_path = env::temp_dir().join(&unique_profile_name);
    clean_old_profiles();

    println!("配置独立环境: {:?}", user_data_path);

    let mut builder = LaunchOptionsBuilder::default();

    match get_browser_path_from_registry() {
        Some(path) => {
            println!("已检测到浏览器路径: {:?}", path); // <--- 加上这一句
            builder.path(Some(path));
        }
        None => {
            println!("警告：未在注册表中找到 Chrome/Edge。");
            // 如果你不想让它下载，可以在这里直接 return Err(...) 退出
        }
    }

    let options = builder
        .headless(true)
        .window_size(Some((1920, 1080)))
        .user_data_dir(Some(user_data_path))
        .args(vec![
            OsStr::new("--no-sandbox"),
            OsStr::new("--disable-setuid-sandbox"),
            OsStr::new("--disable-gpu"),
            OsStr::new("--disable-software-rasterizer"),
            OsStr::new("--disable-extensions"),       // 禁用扩展
            OsStr::new("--disable-infobars"),         // 禁用顶部提示条
            OsStr::new("--no-first-run"),             // 禁止首次运行向导
            OsStr::new("--no-default-browser-check"), // 禁止询问是否设为默认浏览器
            OsStr::new("--disable-infobars"),         // 禁止顶部提示条
            OsStr::new("--disable-extensions"),       // 禁用扩展，提高速度
            OsStr::new("--password-store=basic"),     // 禁用系统密码弹窗
            OsStr::new("--remote-debugging-port=9222"), 
            OsStr::new("about:blank"),
            
        ])
        .build()?;

    // 带超时的启动检测（简化版）
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let res = Browser::new(options);
        tx.send(res).unwrap();
    });

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(res) => match res {
            Ok(b) => Ok((b, true)), // true 表示我是“主”，拥有者
            Err(e) => {
                clean_old_profiles(); // 启动失败，核弹清理
                Err(format!("启动失败: {}", e).into())
            }
        },
        Err(_) => {
            clean_old_profiles(); // 超时，核弹清理
            Err("启动超时，已清理进程".into())
        }
    }
}

fn smart_exit(
    browser: Browser,
    is_master: bool,
) -> Result<(), Box<dyn Error>> {
    

    let tabs = || -> usize {
        match browser.get_tabs().lock() {
            Ok(tabs) => tabs.len(),
            Err(_) => 0,
        }
    };

    let count_now = tabs();

    if count_now > 0 {
        // 通常 Browser 还会保留一个空白的 Target 页，所以这里可能是 > 1 或 > 0
        println!("检测到还有其他任务在运行，进入5秒观察期...");
        sleep(Duration::from_secs(5));

        let count_after = tabs();
        if count_after > 0 {
            println!(
                "5秒后仍有 {} 个 Tab 活动，判定为多窗口运行中。",
                count_after
            );
            if is_master {
                println!("我是主窗口，但我不能杀进程。正在执行【权限剥离】...");
                // 【核心黑魔法】
                // mem::forget 会消耗掉 browser 变量的所有权，但不调用它的 drop()
                // 这样浏览器进程就会被“遗弃”在后台继续运行，供其他窗口使用
                mem::forget(browser);
                println!(">> 内核已保留，本窗口安全退出。");
            } else {
                println!(">> 我是从窗口，断开连接退出。");
            }
            return Ok(());
        }
    }

    // 如果运行到这里，说明没有其他 Tab 了，或者我是最后一个
    if is_master {
        println!("没有其他活动窗口，正在彻底关闭浏览器内核...");
        // 这里不需要做任何事，让 browser 正常离开作用域，就会触发 Drop 自动关闭进程
    } else {
        println!("我是从窗口，任务结束。");
        kill_zombie_processes(); // 主动清理，防止僵尸进程残留
        // 从窗口无法关闭主进程，只能自己断开。
        // 如果想要从窗口也能关闭主进程，可以通过 ws 发送 Browser.close 命令，
        // 但为了安全，建议让主窗口（如果有）或超时机制来处理，或者手动清理。
    }

    Ok(())
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

fn main() {
    // 真正的逻辑放在 run() 里，main 只负责捕获错误
    if let Err(e) = run() {
        eprintln!("\n==============================");
        eprintln!("程序发生严重错误，已停止运行：");
        eprintln!("{}", e);
        eprintln!("==============================");
    }

    println!("\n按回车键退出...");
    let _ = std::io::stdin().read_line(&mut String::new());
}

fn run() -> Result<(), Box<dyn Error>> {
    println!("======这是一个拷贝漫画的漫画下载器======");
    println!("默认保存路径在当前文件夹的download文件夹下\n\n");
    sleep(Duration::from_secs(2));

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

        
    //初始化浏览器内核
    let (browser, is_master) = get_browser(client.clone())?;


    sleep(Duration::from_secs(1));
    let tab = {
        let tabs = browser.get_tabs().lock().unwrap();
        tabs.first()
            .ok_or("未找到初始标签页，请检查浏览器是否正常启动")?
            .clone()
    };

    //初始化结束
    println!("启动内核成功，进入搜索\n");

    let resp_json = search(client.clone(), &base_website)?;
    let choice: i32 = read!();
    println!("请稍后...");
    let lists = &resp_json.results.list;
    let selected_item = lists[choice as usize].clone();
    let title = selected_item.name.clone();
    let path_word = selected_item.path_word.clone();

    let url: String = format!("{}/comic/{}", &base_website, &path_word);
    tab.navigate_to(&url)?;

    // 等待外层容器出现，确保页面已加载
    tab.wait_for_element("div#default全部")?;

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

    let remote_object = tab.evaluate(script, true)?;
    //dbg!(&remote_object);
    let object = remote_object.value.unwrap();
    //dbg!(&object);
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
        sleep(Duration::from_secs(3));
        return Ok(());
    }

    print!("请输入结束话数(包含该话)：");
    let _ = io::stdout().flush();

    let end: usize = read!();
    if end > counts || end <= start {
        println!("输入的话数有误");
        sleep(Duration::from_secs(3));
        return Ok(());
    }

    //是否转化图片格式

    let is_jpg: char = 'n';
    // println!("\n默认图片后缀为.webp");
    // println!("是否需要将图片转换为jpg?(转换需要花费更长的时间)");
    // print!("(y/n):");
    // let is_jpg :char = read!();
    // let _ = io::stdout().flush();
    // if is_jpg!='y' && is_jpg!= 'n'{
    //     println!("输入的字符不是y或n");
    //     sleep(Duration::from_secs(3));
    //     return Ok(());
    // }

    //搜集章节信息
    for i in start..end {
        let link = js_chapters.path_words[i].clone();
        let Chapter_title = js_chapters.names[i].clone();

        download_chapters.push(Chapter {
            number: i,
            url: link,
            title: Chapter_title,
            ..Default::default()
        });
    }
    tab.close(true)?;

    //解析章节页面的初始化
    let mut one_tab_count :usize  = 0;
    let mut chapter_tab = browser.new_tab()?;

//解析章节页面，获取图片链接
    for chapter in &mut download_chapters {

        //限制单个tab解析章节数，防止内存泄漏
        //超出20个章节就重开一个tab
        one_tab_count +=1;
        if one_tab_count >= 20 {
            chapter_tab.close(true)?;
            one_tab_count =0;
            chapter_tab = browser.new_tab()?;
            chapter_tab.set_default_timeout(std::time::Duration::from_secs(60));
        }

        println!("正在解析：{}", chapter.title);



        if let Err(e) = chapter_tab.navigate_to(&chapter.url) {
            println!("无法进入页面 {}: {}, 跳过", chapter.title, e);
            error_logs.push(ErrorLog {
                chapter_title: chapter.title.clone(),
                error_message: format!("无法进入页面: {}", e),
            });
            continue;
        }

         
        let script = r#"(async () => {
            return await new Promise((resolve) => {
                // --- 配置区 (可根据网速调整) ---
                const scrollStep = 500;   
                const frequency = 16;    
                const waitTime = 1000;   
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

        let Js_pages_url_response = chapter_tab.evaluate(script, true)?;
        //dbg!(&Js_pages_url_response);
        let Js_pages_url = Js_pages_url_response.value.unwrap();
        //dbg!(&Js_pages_url);
        let urls: Vec<String> = serde_json::from_str(Js_pages_url.as_str().unwrap()).unwrap();
        //dbg!(&urls);

        chapter.pages_url = urls;
        //dbg!(&chapter.pages_url);
        chapter.len = chapter.pages_url.len();

        // //创建相应日志文件保存获取的图片链接，而不是一直保存在内存里
        // let path = format!("./download/log/{}/{}.json", title, chapter.title);
        // if let Err(e) = write_chapter(&path,&chapter){
        //     println!("无法写入章节日志 {}: {}, 跳过", chapter.title, e);
        //     error_logs.push(ErrorLog {
        //         chapter_title: chapter.title.clone(),
        //         error_message: format!("无法写入章节日志: {}", e),
        //     });
        //     continue;
        // }

        println!("{}共{}页", chapter.title, chapter.len);
    }

    chapter_tab.close(true)?;

    download(download_chapters, title, client.clone(), is_jpg)?;

    smart_exit(browser, is_master)?;

    //打印错误日志
    for log in error_logs {
        println!("错误章节记录：{}", log);
    }

    sleep(Duration::from_secs(180));

    Ok(())
}

fn download(
    chapters: Vec<Chapter>,
    title: String,
    client: Client,
    is_jpg: char,
) -> Result<(), Box<dyn Error>> {
    for chapter in chapters {
        let mut handles: Vec<JoinHandle<()>> = Vec::new();
        let path = format!("./download/{}/{}", title, chapter.title);
        fs::create_dir_all(&path)?;

        //创建进度条
        let pb = ProgressBar::new(chapter.len as u64);
        pb.set_style(ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>7}/{len:7} {msg}")
            .unwrap()
            .progress_chars("█=>"));
        pb.set_message(format!("下载中: {}", chapter.title));

        for (index, page_url) in chapter.pages_url.iter().enumerate() {
            let client_clone = client.clone();
            let chapter_clone = chapter.clone();
            let page_len_clone = chapter.pages_url.len().clone();
            let title_clone = title.clone();
            let page_url_clone = page_url.clone();
            let pb_clone = pb.clone();

            //创建子进程
            let handle = thread::spawn(move || {
                let page_path = format!(
                    "./download/{}/{}/{}.webp",
                    title_clone,
                    chapter_clone.title,
                    index + 1
                );
                let max_retries = 3;

                for i in 1..=max_retries {
                    let mut page = fs::File::create(&page_path).unwrap();
                    let response = client_clone.get(&page_url_clone).send();

                    match response {
                        Ok(mut res) => {
                            match copy(&mut res, &mut page) {
                                Ok(_) => {
                                    pb_clone.inc(1);
                                    break; // 成功后跳出重试循环
                                }
                                Err(e) => {
                                    println!(
                                        "下载失败：{}第{}页，错误信息：{}",
                                        chapter_clone.title,
                                        index + 1,
                                        e
                                    );
                                    if i < max_retries {
                                        println!("正在重试第{}次...", i);
                                    } else {
                                        println!("达到最大重试次数，跳过该页");
                                    }
                                    sleep(Duration::from_secs(1));
                                }
                            }
                        }
                        Err(e) => {
                            println!(
                                "发起请求失败：{}第{}页，错误信息：{}",
                                chapter_clone.title,
                                index + 1,
                                e
                            );
                            if i < max_retries {
                                println!("正在重试第{}次...", i);
                            } else {
                                println!("达到最大重试次数，跳过该页");
                            }
                            sleep(Duration::from_secs(1));
                        }
                    }
                }
            });

            handles.push(handle);
        }

        for handle in handles {
            if let Err(e) = handle.join() {
                println!("线程执行失败: {:?}", e);
            }
        }

        pb.finish_with_message(format!("{} 下载完毕", chapter.title));

        if is_jpg == 'y' {
            println!("  [转换中] 正在将本章图片转为 JPG...");

            // 注意：变量 `path` 是你在上面定义的文件夹路径
            let pattern = format!("{}/{}", path, "*.webp");

            // 使用 glob 遍历文件夹下的 webp 文件
            for entry in glob(&pattern)? {
                match entry {
                    Ok(file_path) => {
                        // 1. 打开并解码 WebP 图片
                        // 参考文档: https://docs.rs/image/0.25.9/image/io/struct.Reader.html#method.open
                        match ImageReader::open(&file_path) {
                            Ok(reader) => match reader.decode() {
                                Ok(img) => {
                                    // 2. 修改后缀名为 jpg
                                    let jpg_path = file_path.with_extension("jpg");

                                    if let Err(e) = img.save(&jpg_path) {
                                        eprintln!("    保存 JPG 失败: {:?} -> {}", file_path, e);
                                    } else {
                                        fs::remove_file(&file_path)?;

                                        // 打印进度点，避免刷屏
                                        print!(".");
                                        let _ = io::stdout().flush();
                                    }
                                }
                                Err(e) => eprintln!("    图片解码失败: {:?} -> {}", file_path, e),
                            },
                            Err(e) => eprintln!("    无法打开文件: {:?} -> {}", file_path, e),
                        }
                    }
                    Err(e) => eprintln!("    Glob 路径错误: {:?}", e),
                }
            }
            println!("\n  [完成] 本章转换结束");
        }
    }

    println!("\n所有章节下载完成！");
    println!("温馨提醒：");
    println!("会有极小概率一话页数没有完整加载出来，导致尾部缺页情况发生，");
    println!("可以根据每话之间的页数对比 or 是否有汉化组尾页来确定是否缺页");
    println!("重新下载该话能补全页数\n\n");
    Ok(())
}
