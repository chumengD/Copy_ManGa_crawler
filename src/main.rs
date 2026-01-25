#![allow(unused_variables)]
use anyhow::Result;
use headless_chrome::{Browser, LaunchOptionsBuilder};
use reqwest::blocking::Client;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::copy;
use std::thread::sleep;
use std::time::Duration;
use text_io::read;
use serde_json::Value;
use serde::Deserialize;

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



fn search(client: Client) ->Result<Response, Box<dyn Error>> {
     print!("输入关键词：");
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
    Ok(resp_json)

}






impl fmt::Display for Chapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Chapter {} \n{} \n({}) 共{}页",
            self.number, self.title, self.url, self.len
        )
    }
}

fn main() -> Result<(), Box<dyn Error>> {
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


    /*print!("请输入关键词：");
    let key_word: String = read!();

    

    tab.navigate_to(&format!("https://mangacopy.com/search?q={key_word}"))?;

    sleep(Duration::from_secs(1));
    let elements = tab.wait_for_elements(".exemptComic_Item")?;

    for (index, element) in elements.iter().enumerate() {
        let title = element.find_element(".twoLines")?.get_inner_text()?;
        println!("{}: {}", index, title);
    }

    print!("请输入要下载的漫画序号：");
    let choice: usize = read!();
*/

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

/*    // 修正点 1: 直接使用全局 XPath，不依赖中间变量，避免 "Invalid search result range"
    // 修正点 2: 你的 HTML 里 a 包裹的是 li，不是 p。所以是 a[li]
    // 逻辑解释: 找到 id='default全部' 下面的 ul，再找下面所有“包含 li 子元素的 a 标签”
    let xpath = r#"//div[@id='default全部']//ul//a[li]"#;
    
    // 执行查找
    let chapters = tab.find_elements_by_xpath(xpath)?;
    let counts = chapters.len();

    println!("该漫画共有{}话", counts);*/

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
    let start: usize = read!();
    let start:usize =start -1;
    if start >= counts  {
        println!("输入的话数有误");
        return Ok(());
    }

    print!("请输入结束话数（包含该话）：");
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
                                let url = img.getAttribute('data-src') || img.getAttribute('src');
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


        // ==========================================
        //  方案二实现：Rust 端维护状态机进行滚动
        // ==========================================
        
        let mut last_height: u64 = 0;
        let mut retries = 0;
        let max_retries = 1; 

        loop {
            // 1. 执行平滑滚动 (每次滚一屏)
            // behavior: 'smooth' 配合下面的 sleep 实现拟人化
            let scroll_cmd = "window.scrollBy({ top: window.innerHeight*14, behavior: 'smooth' });";
            chapter_tab.evaluate(scroll_cmd, false)?;

            // 2. Rust 等待滚动动画完成 (短等待)
            // 比如 800 毫秒，给浏览器平滑滚动的时间
            sleep(Duration::from_millis(50));

            // 3. 检查是否触底 (利用 JS 计算)
            // 允许 10px 的误差
            let check_bottom_js = "window.scrollY + window.innerHeight >= document.body.scrollHeight - 10";
            let is_at_bottom = chapter_tab
                .evaluate(check_bottom_js, false)?
                .value.unwrap()
                .as_bool()
                .unwrap_or(false);

            // 获取当前高度用于比较
            let height_js = "document.body.scrollHeight";
            let current_height = chapter_tab
                .evaluate(height_js, false)?
                .value.unwrap()
                .as_u64() // headless_chrome 返回的是 serde_json::Value
                .unwrap_or(0);

            if is_at_bottom {
                println!("    -> 触底检测 ({}/{})...", retries + 1, max_retries);
                
                // 4. 触底后的长等待 (给懒加载留时间)
                sleep(Duration::from_secs(1));

                // 再次获取高度
                let new_height = chapter_tab
                    .evaluate(height_js, false)?
                    .value.unwrap()
                    .as_u64()
                    .unwrap_or(0);

                if new_height == last_height {
                    retries += 1;
                    if retries >= max_retries {
                        println!("    ✅ 判定加载完毕");
                        break; // 真正退出循环
                    }
                } else {
                    println!("    ⬇️ 发现新内容，继续滚动");
                    retries = 0;
                    last_height = new_height;
                }
            } else {
                // 还没到底，重置状态，继续滚
                retries = 0;
                last_height = current_height;
            }
        }
        // ==========================================
        //  滚动结束
        // ==========================================

        let pages = chapter_tab.wait_for_elements("img")?;
        for page in pages {
            let page_url = page.get_attribute_value("data-src")?;
            if let Some(url) = page_url {
                chapter.pages_url.push(url);
            }
        }
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
                Ok(_) => println!("下载完成：{}:{}/{}", chapter.title, index + 1,page_len),
                Err(e) => println!("下载失败：{}，错误信息：{}", chapter.title, e),
            }   
        }
        
    }
    println!("\n 所有章节下载完成！");
    Ok(())
}

