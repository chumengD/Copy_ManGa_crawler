#![allow(unused_variables)]
use anyhow::Result;
use headless_chrome::{Browser, LaunchOptionsBuilder};
use reqwest::blocking::Client;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self,copy,Write};
use std::thread::sleep;
use std::time::Duration;
use text_io::read;
use serde_json::Value;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
struct Chapter {      //下载图片时依据的结构，len是图片数量，pages_url是每张图片的链接，number是第几章，url是该话的链接
    number: usize,
    url: String,
    title: String,
    pages_url: Vec<String>,
    len: usize,
}

#[derive(Deserialize, Debug)]
 struct Response{                      //搜索时用到的结构，用于储存搜索结果
    code: i32,
    message: String,
    results: Results,
}

#[derive(Deserialize, Debug)] 
struct Results{          
    list: Vec<ManGa_item>,
}

#[derive(Deserialize, Debug, Clone)]
struct ManGa_item{
    name: String,
    path_word: String,
}

#[derive(Debug, Deserialize, Clone)] 
struct Js_chapters {                 //从控制台获取的章节的名称与相应地址
    names: Vec<String>,
    path_words: Vec<String>,
    len: usize,
}

fn get_browser_path() -> Option<PathBuf> {
    // 1. 常见的 Chrome 路径
    let chrome_paths = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ];
    for path in &chrome_paths {
        if PathBuf::from(path).exists() {
            return Some(PathBuf::from(path));
        }
    }

    // 2. 如果没找到 Chrome，尝试 Edge 路径
    let edge_path = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe";
    if PathBuf::from(edge_path).exists() {
        println!("未检测到 Chrome，将使用 Microsoft Edge...");
        return Some(PathBuf::from(edge_path));
    }

    None
}

fn search(client: Client) ->Result<Response, Box<dyn Error>> {
     print!("输入关键词：\n");
     io::stdout().flush()?;
    let key_word: String = read!();
    let base_url = "https://mangacopy.com/api/kb/web/searchcd/comics";
    let params = [
        ("offset", "0"),
        ("platform", "2"),
        ("limit", "10"), // 我改成 10 了，你可以改回 2
        ("q", &key_word), // 这里 reqwest 会自动把中文转成 %E5%...
        ("q_type", ""),
    ];


    let response = client.get(base_url)
        .query(&params)
        .send()?;

    let resp_text = response.text()?;
    let resp_json: Response = serde_json::from_str(&resp_text)?;

    //println!("reponse：{:#?}", resp_json);

    println!("以下为搜索结果：");
    let lists = &resp_json.results.list;
    for (index, item) in lists.iter().enumerate() {
        println!("{}.{}", index, item.name);
    }
    print!("请输入要下载的漫画序号：");
    
    io::stdout().flush()?;
    Ok(resp_json)

}



fn main() -> Result<(), Box<dyn Error>> {
    println!("====这是一个拷贝漫画的漫画下载器====");
    println!("启动chrome浏览器内核中...");
    

    let mut download_chapters: Vec<Chapter> = Vec::new();

    let options = LaunchOptionsBuilder::default()
        .headless(true)
        .window_size(Some((1920, 1080)))
        .args(vec![
            OsStr::new("--disable-remote-fonts"),
            OsStr::new("--disable-gpu"),
            OsStr::new("--no-sandbox"),
            OsStr::new("--disable-dev-shm-usage"),
        ])
        .build()?;
    let browser = Browser::new(options)?;

    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
        .danger_accept_invalid_certs(true)
        .build()?;

    let tab = browser.new_tab()?;
    //初始化结束


    

    let resp_json = search(client.clone())?;
    let choice: i32 = read!();
    let lists = &resp_json.results.list;
    let selected_item = lists[choice as usize].clone();
    let title = selected_item.name.clone();
    let path_word = selected_item.path_word.clone();
  

    let url: String = format!("https://mangacopy.com/comic/{}", &path_word);
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

    println!("该漫画共有{}话", counts);

    print!("请输入起始话数(包含该话)：");
    io::stdout().flush()?;

    let start: usize = read!();
    let start:usize =start -1;
    if start >= counts  {
        println!("输入的话数有误");
        return Ok(());
    }

    print!("请输入结束话数(包含该话)：");
    io::stdout().flush()?;

    let end: usize = read!();
    if end > counts || end <= start {
        println!("输入的话数有误");
        return Ok(());
    }

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

    //解析章节页面，获取图片链接
    for chapter in &mut download_chapters {
        println!("正在解析：{}",chapter.title);
        

        let chapter_tab = browser.new_tab()?;
        chapter_tab.set_default_timeout(std::time::Duration::from_secs(60));
        
        if let Err(e) = chapter_tab.navigate_to(&chapter.url) {
            println!("无法加载页面 {}: {}, 跳过", chapter.title, e);
            chapter_tab.close(true)?;
            continue;
        }

        let script = r#"(async () => {
            return await new Promise((resolve) => {
                // --- 配置区 (可根据网速调整) ---
                const scrollStep = 500;   // 每次下移 80px (越大越快，但太大会漏图，建议 50-100)
                const frequency = 16;    // 每 16ms 动一次 (模拟 60fps 顺滑动画)
                const waitTime = 1000;   // 触底后等待多久(ms)确认真的没图了
                // ---------------------------

                let totalHeight = 0;
                let noChangeTicks = 0;
                // 计算需要等待多少个 tick (频率是 16ms，所以 2000ms / 16ms ≈ 125 次)
                const maxTicks = waitTime / frequency; 

                const timer = setInterval(() => {
                    const scrollHeight = document.body.scrollHeight;
                    const currentPos = window.scrollY + window.innerHeight;

                    // 1. 执行滚动
                    // 这里不加 behavior: smooth，因为我们通过 setInterval 自己实现了物理上的 smooth
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
        chapter_tab.close(true)?;
    }

    download(download_chapters,title,client.clone())?;

    Ok(())
}

fn download(chapters: Vec<Chapter>, title: String, client: Client) -> Result<(), Box<dyn Error>> {

   
    
    for chapter in chapters {

        let path = format!("./download/{}/{}",title, chapter.title);
        fs::create_dir_all(&path)?;
        let page_len = chapter.pages_url.len();

        for (index ,page_url) in chapter.pages_url.iter().enumerate(){
            let page_path = format!("./download/{}/{}/{}.webp",title, chapter.title, index + 1);
            let mut page = fs::File::create(&page_path)?;

            let mut response = client.get(page_url)
            .send()?;

            match copy(&mut response,&mut page){
                Ok(_) => println!("下载中：{}:{}/{}", chapter.title, index + 1,page_len),
                Err(e) => println!("下载失败：{}，错误信息：{}", chapter.title, e),
            }   
        }
        
    }
    println!("\n 所有章节下载完成！");
    Ok(())
}

