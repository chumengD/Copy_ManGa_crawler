#![allow(unused_variables)]
use anyhow::Result;
use reqwest::Client;
use reqwest::header::{HeaderMap, REFERER};

use serde_json::{Value, json};
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

use std::error::Error;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::io::Write;
use std::print;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::time::{sleep, Duration, timeout};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt}; // read_line 供 input_line()，write_all 供 download()
use tokio::fs::{create_dir_all, read_dir, read_to_string, write};
use tokio::sync::Semaphore;

use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};


pub mod types;
use types::{ChapterContents, ChapterDetails, Config, LocalManga, ManGa_item, MangaUpdate, Response};

use regex::Regex;
use anyhow::{anyhow, bail, Context};

/// 相邻网络请求之间的最小间隔，避免请求过快触发站点限流（Too Many Requests）
const REQUEST_DELAY: Duration = Duration::from_millis(1000);



// pub async fn search_manga_chapters(client: Client,base_website: &str,cancelled: &AtomicBool)->{

// }

pub async fn search(
    client: Client,
    base_website: &str,
    cancelled: &AtomicBool,
) -> Result<Option<Response>, Box<dyn Error>> {
    // 输入关键词期间按了 Ctrl+C（返回 Ok(None)）或 stdin 关闭（返回 Err）都放弃本次搜索
    let Some(key_word) = input_line("输入关键词：\n", cancelled).await else {
        return Ok(None);
    };
    // let key_word = String::from("19");
    let base_url = format!("{}/api/kb/web/searchci/comics", &base_website);
    let params = [
        ("offset", "0"),
        ("platform", "2"),
        ("limit", "12"),
        ("q", &key_word),
        ("q_type", ""),
    ];

let mut response: Option<reqwest::Response> = None;

for _ in 0..5 {
    // 重试期间按了 Ctrl+C：放弃本次搜索
    if cancelled.load(Ordering::SeqCst) {
        return Ok(None);
    }
    match client.get(&base_url).query(&params).send().await {
        Ok(res) => {
            if res.status().is_success() {
                response = Some(res);
                break;
            } else {
                println!("搜索请求失败，状态码: {}，正在重试...", res.status());
            }
        }
        Err(e) => {
            println!("搜索请求发生错误: {}, 正在重试...", e);
        }
    }
    // 请求之间歇一下，避免过快触发站点限流
    sleep(REQUEST_DELAY).await;
}
    // let response = client.get(base_url).query(&params).send().await.expect("搜索失败1");
    // let response1 = client.get("https://ios.copymanga.club/search?q=1&q_type=").send().await.expect("搜索失败1");
    //dbg!(&response);

    let Some(response) = response else {
        return Err("搜索失败：重试 5 次后仍然没有成功响应".into());
    };
    let resp_text = response.text().await?;
    //dbg!(&resp_text);
    let resp_json: Response = serde_json::from_str(&resp_text)?;
    //dbg!(format!("\n\n\n resp_json= {}\n\n\n",&resp_json));

    println!("reponse：{:#?}", resp_json);

    println!("以下为搜索结果(仅列举至多12项)：");
    let lists = &resp_json.results.list;
    for (index, item) in lists.iter().enumerate() {
        println!("{}.{}", index, item.name);
    }
    Ok(Some(resp_json))
}

/// 根据 path_word 或完整漫画页 URL 获取解密后的章节列表详情。
/// 返回 None 表示等待请求期间被取消。
pub async fn fetch_chapter_outline(
    client: Client,
    base_website: &str,
    manga: &ManGa_item,
    cancelled: &AtomicBool,
) -> Result<Option<ChapterDetails>, Box<dyn Error>> {
    let base_website = base_website.trim_end_matches('/');
    let path_word = manga.path_word.trim().trim_start_matches('/');
    let page_url = format!("{base_website}/comic/{path_word}");

    let mut html: Option<String> = None;
    for _ in 0..5 {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(None);
        }

        match client.get(&page_url).send().await {
            Ok(response) if response.status().is_success() => {
                html = Some(response.text().await?);
                break;
            }
            Ok(response) => {
                println!("漫画详情页请求失败，状态码: {}，正在重试...", response.status());
            }
            Err(e) => {
                println!("漫画详情页请求发生错误: {e}，正在重试...");
            }
        }

        sleep(Duration::from_secs(2)).await;
    }
    let Some(html) = html else {
        return Err("漫画详情页请求失败：重试 5 次后仍然没有成功响应".into());
    };
    let (key, dnts) = extract_page_secrets(&html)?;
    dbg!(format!("\n\n\n key= {}\n dnts= {}\n\n\n",&key,&dnts));

    // 连续请求详情页与章节接口之间歇一下
    sleep(REQUEST_DELAY).await;

    let api_url = format!("{base_website}/comicdetail/{path_word}/chapters");
    let headers = [("dnts", dnts.as_str()), ("Referer", page_url.as_str())];
    let mut body: Option<String> = None;
    for _ in 0..5 {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(None);
        }

        let mut request = client.get(&api_url);
        for (name, value) in headers {
            request = request.header(name, value);
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => {
                body = Some(response.text().await?);
                break;
            }
            Ok(response) => {
                println!("章节接口请求失败，状态码: {}，正在重试...", response.status());
            }
            Err(e) => {
                println!("章节接口请求发生错误: {e}，正在重试...");
            }
        }

        sleep(Duration::from_secs(2)).await;
    }
    let Some(body) = body else {
        return Err("章节接口请求失败：重试 5 次后仍然没有成功响应".into());
    };

    let body_json: Value = serde_json::from_str(&body)
        .with_context(|| format!("章节接口返回不是合法 JSON: {body}"))?;
    if body_json.get("code").and_then(Value::as_i64) != Some(200) {
        return Err(anyhow!("章节接口返回异常: {body_json}").into());
    }

    let results = body_json
        .get("results")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("章节接口返回缺少 results 字段"))?;
    let payload = decrypt_results(results, &key)?;

    let total = payload["groups"]
        .as_object()
        .map(|groups| {
            groups
                .values()
                .filter_map(|group| group["chapters"].as_array())
                .map(|chapters| chapters.len())
                .sum()
        })
        .unwrap_or(0);
    if total == 0 {
        eprintln!(
            "[!] 接口正常但返回空章节列表 —— 当前出口 IP 大概率被站点软限流，可稍后重试或更换出口 IP。"
        );
    }

    Ok(Some(ChapterDetails {
        name: manga.name.clone(),
        path_word: path_word.to_string(),
        chapters: extract_chapter_contents(&payload),
    }))
}

/// 获取某一话的图片直链：章节页 HTML 里带中 AES 密钥 `cct` 和加密内容 `contentKey`，
/// 解密后是一个 `[{ "url": "..." }, ...]` 数组。
pub async fn fetch_chapter_contents(
    client: Client,
    base_website: &str,
    path_word: &str,
    uuid: &str,
    chapter_name: &str,
    cancelled: &AtomicBool,
) -> Result<Option<ChapterContents>, Box<dyn Error>> {
    let base_website = base_website.trim_end_matches('/');
    let path_word = path_word.trim().trim_start_matches('/');
    let uuid = uuid.trim();
    let page_url = format!("{base_website}/comic/{path_word}/chapter/{uuid}");

    let mut html: Option<String> = None;
    for _ in 0..5 {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(None);
        }

        match client.get(&page_url).send().await {
            Ok(response) if response.status().is_success() => {
                html = Some(response.text().await?);
                break;
            }
            Ok(response) => {
                println!("章节目录页请求失败，状态码: {}，正在重试...", response.status());
            }
            Err(e) => {
                println!("章节目录页请求发生错误: {e}，正在重试...");
            }
        }

        sleep(Duration::from_secs(2)).await;
    }

    let Some(html) = html else {
        return Err("章节目录页请求失败：重试 5 次后仍然没有成功响应".into());
    };

    let (cct, content_key) = extract_chapter_secrets(&html)?;
    let pages_url: Vec<String> =
        serde_json::from_value::<Vec<Value>>(decrypt_results(&content_key, &cct)?)?
            .into_iter()
            .filter_map(|page| {
                page.get("url")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();

    Ok(Some(ChapterContents {
        chapter_name: chapter_name.to_string(),
        chapter_uuid: uuid.to_string(),
        len: pages_url.len(),
        pages_url,
    }))
}



fn decrypt_results(results: &str, key: &str) -> Result<Value> {
    let iv = results
        .get(..16)
        .ok_or_else(|| anyhow!("results 长度不足 16 字符，无法取出 IV"))?
        .as_bytes();
    let hex_ct = results
        .get(16..)
        .ok_or_else(|| anyhow!("results 缺少密文部分"))?;
    let mut buf = hex::decode(hex_ct).context("章节密文 hex 解码失败")?;

    if buf.is_empty() || buf.len() % 16 != 0 {
        bail!("章节密文长度（{} 字节）不是 16 的倍数", buf.len());
    }
    let plaintext = Aes128CbcDec::new_from_slices(key.as_bytes(), iv)
        .map_err(|_| anyhow!("AES-128 密钥/IV 长度错误"))?
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow!("PKCS7 padding 校验失败（密钥可能不对）"))?;

    serde_json::from_slice(plaintext).context("解密结果不是合法 JSON")
}



pub fn get_client(base_website: &str)-> Result<Client, Box<dyn Error>> {
     //初始化client
    let mut headers = HeaderMap::new();
    headers.insert(REFERER, base_website.parse().unwrap());
    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
        .danger_accept_invalid_certs(true)
        .default_headers(headers)
        .build()?;
    Ok(client)
}




/// 等待一行输入；等待期间 cancelled 被置位（Ctrl+C）立即返回 None。
/// 不依赖任何全局状态：每次提示现场读取一行，读行 future 随 select 一起被丢弃，
/// 不存在"后台读线程泄漏 / 多个读者抢 stdin"的问题。
/// 返回 None 表示被取消或 stdin 已关闭（EOF）。
pub async fn input_line(prompt: &str, cancelled: &AtomicBool) -> Option<String> {
    print!("{}", prompt);
    let _ = std::io::stdout().flush();

    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    loop {
        let mut line = String::new();
        tokio::select! {
            // Ctrl+C：取消标志由调用方（bin 里的全局监听任务）置位
            _ = wait_cancelled(cancelled) => return None,
            res = reader.read_line(&mut line) => match res {
                Ok(0) => return None, // EOF：stdin 已关闭
                Ok(_) => {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        println!("输入为空，请重新输入：");
                        continue;
                    }
                    return Some(line);
                }
                Err(e) => {
                    eprintln!("读取控制台输入时出错: {}", e);
                    return None;
                }
            }
        }
    }
}

/// 每隔 50ms 检查一次取消标志，置位后立即返回
async fn wait_cancelled(cancelled: &AtomicBool) {
    while !cancelled.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 提示并等待用户输入一个数字；输入无效则重新提示；取消/EOF 返回 None
pub async fn input_number(prompt: &str, cancelled: &AtomicBool) -> Option<usize> {
    loop {
        match input_line(prompt, cancelled).await {
            None => return None,
            Some(line) => match line.parse() {
                Ok(n) => return Some(n),
                Err(_) => println!("输入无效，请输入数字。"),
            },
        }
    }
}


fn extract_page_secrets(html: &str) -> Result<(String, String)> {
    let ccz_re = Regex::new(r"var\s+ccz\s*=\s*'([^']+)'").unwrap();
    let dnt_re = Regex::new(r#"id="dnt"[^>]*value="([^"]*)""#).unwrap();
    let key = ccz_re
        .captures(html)
        .map(|c| c[1].to_string())
        .ok_or_else(|| anyhow!("页面中未找到 AES 密钥 ccz（站点可能已更新加密方案）"))?;
    let dnts = dnt_re
        .captures(html)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "3".to_string());
    Ok((key, dnts))
}

fn extract_chapter_secrets(html: &str) -> Result<(String, String)> {
    let cct_re = Regex::new(r"var\s+cct\s*=\s*'([^']+)'").unwrap();
    let content_key_re = Regex::new(r"var\s+contentKey\s*=\s*'([^']+)'").unwrap();

    let cct = cct_re
        .captures(html)
        .map(|c| c[1].to_string())
        .ok_or_else(|| anyhow!("阅读页中未找到 AES 密钥 cct（站点可能已更新加密方案）"))?;
    let content_key = content_key_re
        .captures(html)
        .map(|c| c[1].to_string())
        .ok_or_else(|| anyhow!("阅读页中未找到 contentKey"))?;

    Ok((cct, content_key))
}

pub async fn load_config() -> Result<Config> {
    let contents = read_to_string("config.toml")
        .await
        .context("读取 config.toml 失败")?;
    let config = toml::from_str::<Config>(&contents).context("解析 config.toml 失败")?;
    Ok(config)
}

pub async fn save_chapter_details(
    details: &ChapterDetails,
) -> Result<PathBuf> {
    let file_stem = sanitize_file_name(&details.name);
    // 目录名与 download() 建漫画文件夹时保持一致（都用原始漫画名），
    // 否则名字含特殊字符时 JSON 会落到另一个文件夹，read_local_path_word 就读不到。
    let dir_name = if details.name.trim().is_empty() {
        file_stem.clone()
    } else {
        details.name.clone()
    };
    let output_dir = Path::new("download").join(&dir_name);
    let output_path = output_dir.join(format!("{file_stem}.json"));

    create_dir_all(&output_dir)
        .await
        .with_context(|| format!("创建目录失败: {}", output_dir.display()))?;
    let json = serde_json::to_string_pretty(details).context("章节详情序列化为 JSON 失败")?;
    write(&output_path, format!("{json}\n"))
        .await
        .with_context(|| format!("写入章节详情失败: {}", output_path.display()))?;

    Ok(output_path)
}

pub async fn download(
    chapter_details: ChapterDetails,
    begin:usize,
    end:usize,
    client: Client,
    cancelled: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error>> {
    //为多线程下载做准备，限制并发数量，避免请求过快触发站点限流
    let once_max_dowload = Arc::new(Semaphore::new(4));

    let manga_title = &chapter_details.name;

    let chapters = &chapter_details.chapters[begin..=end];

    for chapter in chapters {
        // 已取消：不再派发本章剩余页面的下载任务
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        //创建漫画文件夹
        let path = format!("./download/{}/{}", manga_title, chapter.chapter_name);
        create_dir_all(&path).await?;

        //创建进度条
        let pb = ProgressBar::new(chapter.len as u64);
        pb.set_style(ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>7}/{len:7} {msg}")
            .unwrap()
            .progress_chars("█=>"));
        pb.set_message(format!("下载中: {}", chapter.chapter_name));
        //创建进度条

        for (index, page_url) in chapter.pages_url.iter().enumerate() {
            // 已取消：停止派发剩余页面
            if cancelled.load(Ordering::SeqCst) {
                break;
            }
            let client_clone = client.clone();
            let chapter_clone = chapter.clone();
            let title_clone = manga_title.clone();
            let page_url_clone = page_url.clone();
            let pb_clone = pb.clone();

            let once_max_download_clone = once_max_dowload.clone();
            let cancelled_clone = cancelled.clone();

            //创建子进程
            let handle = tokio::spawn(async move {
                // 已取消：直接结束，不参与下载
                if cancelled_clone.load(Ordering::SeqCst) {
                    return;
                }
                let _permit = once_max_download_clone.acquire_owned().await.unwrap();

                // 每个页面请求前稍微等待，控制整体下载速率
                sleep(Duration::from_millis(300)).await;

                let page_path = format!(
                    "./download/{}/{}/{}.webp",
                    title_clone,
                    chapter_clone.chapter_name,
                    index + 1
                );
                let limit = Duration::from_secs(60);

                //在开始下载前创建相应图片文件
                let timed_out = timeout(limit, async {
                    let mut isErr: bool = false;
                    loop {
                        // 已取消：放弃当前页的下载与重试
                        if cancelled_clone.load(Ordering::SeqCst) {
                            break;
                        }

                        let mut page = tokio::fs::File::create(&page_path).await.unwrap();

                        //发送网络请求
                        let response = client_clone.get(&page_url_clone).send().await;

                        match response {
                            Ok(res) if res.status().is_success() => {
                                let mut steam = res.bytes_stream();
                                let mut aborted = false;

                                while let Some(chunk) = steam.next().await {
                                    // 接收数据期间按了 Ctrl+C：停止接收
                                    if cancelled_clone.load(Ordering::SeqCst) {
                                        aborted = true;
                                        break;
                                    }
                                    if let Ok(chunk) = chunk {
                                        page.write_all(&chunk).await.unwrap();
                                    }
                                }

                                if aborted {
                                    // 先关闭文件句柄（Windows 上打开中的文件无法删除），
                                    // 再删掉写了一半的图片，避免留下损坏文件
                                    drop(page);
                                    let _ = tokio::fs::remove_file(&page_path).await;
                                    break;
                                }

                                if isErr {
                                    println!(
                                        "{}:第{}页下载重试完成，下载成功！",
                                        chapter_clone.chapter_name,
                                        index + 1
                                    );
                                }
                                pb_clone.inc(1);
                                break;
                            }
                            Ok(res) => {
                                // 429/503 等限流或错误状态：不写入文件，等待后重试
                                if !isErr {
                                    println!(
                                        "{}:第{}页下载失败（状态码 {}），正在重试...",
                                        chapter_clone.chapter_name,
                                        index + 1,
                                        res.status()
                                    );
                                    isErr = true;
                                }
                                sleep(Duration::from_secs(2)).await;
                            }
                            Err(_) => {
                                if !isErr {
                                    println!(
                                        "{}:第{}页下载失败，正在重试...",
                                        chapter_clone.chapter_name,
                                        index + 1
                                    );
                                    isErr = true;
                                }
                                // 失败后等待再重试，避免请求过快
                                sleep(Duration::from_secs(2)).await;
                            }
                        }
                    }
                })
                .await
                .is_err();

                // 超时：跳过该页并删掉不完整的文件，而不是 panic 退出整个程序
                if timed_out {
                    println!(
                        "{}:第{}页下载超时(60s)，跳过该页",
                        chapter_clone.chapter_name,
                        index + 1
                    );
                    let _ = tokio::fs::remove_file(&page_path).await;
                }
            });

            handles.push(handle);
        }

        for handle in handles {
            if let Err(e) = handle.await {
                eprintln!("下载任务失败: {}", e);
            }
        }

        if cancelled.load(Ordering::SeqCst) {
            pb.finish_with_message(format!("{} 下载已取消", chapter.chapter_name));
            return Ok(());
        }

        pb.finish_with_message(format!("{} 下载完毕", chapter.chapter_name));
    }

    Ok(())
}

fn extract_chapter_contents(details: &Value) -> Vec<ChapterContents> {
    let mut chapters = Vec::new();

    let Some(groups) = details.get("groups").and_then(Value::as_object) else {
        return chapters;
    };

    for group in groups.values() {
        let Some(group_chapters) = group.get("chapters").and_then(Value::as_array) else {
            continue;
        };

        for chapter in group_chapters {
            let Some(name) = chapter.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(uuid) = chapter.get("id").and_then(Value::as_str) else {
                continue;
            };

            chapters.push(ChapterContents {
                chapter_name: name.to_string(),
                chapter_uuid: uuid.to_string(),
                len: 0,
                pages_url: Vec::new(),
            });
        }
    }

    chapters
}

fn sanitize_file_name(value: &str) -> String {
    let stem = value
        .trim()
        .trim_start_matches('/')
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>();

    if stem.is_empty() {
        "chapter_details".to_string()
    } else {
        stem
    }
}


pub fn display_chapter_list(chapters:&ChapterDetails){
     for (index,content)in chapters.chapters.iter().enumerate() {
                println!("{}:{}",index+1,content.chapter_name);
            }
    println!("该漫画共{}话",chapters.chapters.len());
}


/// 报错后等待用户按回车再退出，避免窗口立即关闭看不到错误信息
pub fn pause_on_error() {
    eprintln!("按回车键退出...");
    let mut _s = String::new();
    let _ = std::io::stdin().read_line(&mut _s);
}

/// 扫描 download 目录。一级文件夹是漫画名，其下的子文件夹代表已下载章节。
/// 章节详情 JSON 只用来补充 path_word，不作为“是否下载过”的依据。
pub async fn read_manga_downloaded() -> Result<Vec<LocalManga>, Box<dyn Error>> {
    let download_dir = Path::new("download");
    if !download_dir.exists() {
        return Ok(Vec::new());
    }

    let mut mangas = Vec::new();
    let mut entries = read_dir(download_dir)
        .await
        .with_context(|| format!("读取下载目录失败: {}", download_dir.display()))?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        let mut chapter_names = Vec::new();
        let mut dir_entries = read_dir(&path).await?;
        while let Some(child) = dir_entries.next_entry().await? {
            let child_path = child.path();
            if child_path.is_dir() {
                if let Some(chapter_name) = child_path.file_name().and_then(|name| name.to_str()) {
                    chapter_names.push(chapter_name.to_string());
                }
            }
        }

        mangas.push(LocalManga {
            name: name.to_string(),
            path_word: read_local_path_word(&path, name).await,
            chapter_names,
        });
    }

    Ok(mangas)
}

/// 从本地 `<漫画名>.json` 里读出 path_word。
/// 这是缓存：文件不存在或格式不兼容时都返回 None，由调用方决定是否兜底。
async fn read_local_path_word(manga_dir: &Path, name: &str) -> Option<String> {
    let metadata_path = manga_dir.join(format!("{}.json", sanitize_file_name(name)));
    let text = read_to_string(metadata_path).await.ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;

    value
        .get("path_word")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path_word| !path_word.is_empty())
        .map(str::to_string)
}

async fn find_path_word_by_name(
    client: Client,
    base_website: &str,
    name: &str,
    cancelled: &AtomicBool,
) -> Result<Option<String>, Box<dyn Error>> {
    let url = format!("{}/api/kb/web/searchci/comics", base_website.trim_end_matches('/'));
    let params = [
        ("offset", "0"),
        ("platform", "2"),
        ("limit", "20"),
        ("q", name),
        ("q_type", ""),
    ];

    for _ in 0..3 {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(None);
        }

        match client.get(&url).query(&params).send().await {
            Ok(response) if response.status().is_success() => {
                let text = response.text().await?;
                let result: Response = serde_json::from_str(&text)?;
                return Ok(result
                    .results
                    .list
                    .into_iter()
                    .find(|item| item.name == name)
                    .map(|item| item.path_word));
            }
            Ok(response) => {
                println!("搜索漫画 ID 失败，状态码: {}，正在重试...", response.status());
            }
            Err(e) => {
                println!("搜索漫画 ID 发生错误: {e}，正在重试...");
            }
        }

        sleep(Duration::from_secs(2)).await;
    }

    Err("搜索漫画 ID 失败：重试 3 次后仍然没有成功响应".into())
}

/// 对比本地章节目录和线上章节目录，返回每部漫画新增的章节。
pub async fn check_manga_updates(
    client: Client,
    base_website: &str,
    cancelled: &AtomicBool,
) -> Result<Vec<MangaUpdate>, Box<dyn Error>> {
    let local_mangas = read_manga_downloaded().await?;
    if local_mangas.is_empty() {
        return Ok(Vec::new());
    }

    let mut updates = Vec::new();
    for manga in &local_mangas {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }

        println!("正在检查: {}", manga.name);

        // 新版下载会在漫画文件夹里留下 <漫画名>.json，里面已缓存 path_word，直接用即可；
        // 只有旧版下载没有该 JSON 时，才联网用名称反查 path_word。
        let path_word = match manga.path_word.clone() {
            Some(path_word) => {
                println!("使用本地记录的漫画 ID: {}", path_word);
                path_word
            }
            None => {
                println!("本地缺少漫画 ID，正在联网查找 {} ...", manga.name);
                let Some(found) =
                    find_path_word_by_name(client.clone(), base_website, &manga.name, cancelled)
                        .await?
                else {
                    println!("[!] {} 没有找到精确匹配的线上漫画，已跳过", manga.name);
                    continue;
                };
                found
            }
        };

        let online = fetch_chapter_outline(
            client.clone(),
            base_website,
            &ManGa_item {
                name: manga.name.clone(),
                path_word: path_word.to_string(),
                cover: String::new(),
                author: Vec::new(),
            },
            cancelled,
        )
        .await?;

        let Some(online) = online else {
            println!("⚠ 已取消，停止检查更新");
            return Ok(Vec::new());
        };

        // 无论有没有新章节，都把线上章节详情落盘，保证每个漫画文件夹里都有 <漫画名>.json
        // （顺带把 path_word 缓存进去，下次检查就不必再联网反查）。
        if let Err(e) = save_chapter_details(&online).await {
            eprintln!("[!] 保存 {} 的章节详情失败: {}", manga.name, e);
        }

        let local_chapter_names = manga.chapter_names.iter().map(String::as_str).collect::<HashSet<_>>();

        let new_chapters = online
            .chapters
            .iter()
            .filter(|chapter| !local_chapter_names.contains(chapter.chapter_name.as_str()))
            .cloned()
            .collect::<Vec<_>>();

        if !new_chapters.is_empty() {
            println!(
                "{} 发现 {} 个新章节:",
                manga.name,
                new_chapters.len()
            );
            for (index, chapter) in new_chapters.iter().enumerate() {
                println!("  {}.{}", index + 1, chapter.chapter_name);
            }
            updates.push(MangaUpdate {
                name: manga.name.clone(),
                path_word: path_word.to_string(),
                online_chapters: online.chapters.clone(),
                new_chapters,
            });
        } else {
            println!("{} 没有更新", manga.name);
        }

        // 每部漫画之间歇一下，避免连续请求触发站点限流
        sleep(REQUEST_DELAY).await;
    }

    Ok(updates)
}

/// 让用户选择要更新的漫画，并下载这些漫画的新章节。
pub async fn update_selected_mangas(
    mut updates: Vec<MangaUpdate>,
    client: Client,
    cancelled: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error>> {
    if updates.is_empty() {
        println!("所有漫画都没有更新");
        return Ok(());
    }

    println!("\n可更新的漫画:");
    for (index, update) in updates.iter().enumerate() {
        println!(
            "{}:{} (新增 {} 话)",
            index,
            update.name,
            update.new_chapters.len()
        );
    }
    println!("输入 0-{0} 选择一部漫画，输入 {0} 以外的数字返回", updates.len() - 1);

    let Some(choice) = input_number("请输入要更新的漫画序号：", &cancelled).await else {
        return Ok(());
    };

    let Some(update) = updates.get_mut(choice) else {
        println!("序号超出范围，返回主菜单...");
        return Ok(());
    };

    let new_count = update.new_chapters.len();
    println!("即将下载 {} 的新章节：", update.name);
    for (index, chapter) in update.new_chapters.iter().enumerate() {
        println!("{}:{}", index + 1, chapter.chapter_name);
    }

    let (begin, end) = loop {
        let Some(begin) = input_number("请输入起始新章节序号(包含该话)：", &cancelled).await else {
            return Ok(());
        };
        if begin < 1 || begin > new_count {
            println!("起始范围错误，请重新输入");
            continue;
        }

        let Some(end) = input_number("请输入结束新章节序号(包含该话)：", &cancelled).await else {
            return Ok(());
        };
        if end < begin || end > new_count {
            println!("结束范围错误，请重新输入");
            continue;
        }

        break (begin - 1, end - 1);
    };

    let manga_name = update.name.clone();
    // 把整份新增章节列表交给 download，由它按 begin..=end 切片，
    // 两个调用方共用同一套下标约定（0 基，含头含尾）。
    let download_details = ChapterDetails {
        name: manga_name.clone(),
        path_word: update.path_word.clone(),
        chapters: update.new_chapters.clone(),
    };

    // 更新也要落盘 <漫画名>.json，把 path_word 缓存进漫画文件夹，
    // 这样下次检查更新就能直接读本地缓存，不必再联网反查。
    // 写入的是线上完整章节列表，避免用只含本次新增话的局部数据覆盖旧记录。
    let snapshot = ChapterDetails {
        name: manga_name.clone(),
        path_word: update.path_word.clone(),
        chapters: update.online_chapters.clone(),
    };
    let json_path = save_chapter_details(&snapshot).await?;
    println!("章节详情已保存到: {}", json_path.display());

    download(
        download_details,
        begin,
        end,
        client,
        cancelled,
    )
    .await?;

    println!("{} 更新完成", manga_name);
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_chapter_name_and_uuid() {
        let details = json!({
            "groups": {
                "default": {
                    "chapters": [
                        {"id": "uuid-01", "name": "第01话"},
                        {"id": "uuid-02", "name": "第02话"}
                    ]
                }
            }
        });

        let chapters = extract_chapter_contents(&details);

        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].chapter_name, "第01话");
        assert_eq!(chapters[0].chapter_uuid, "uuid-01");
        assert_eq!(chapters[1].chapter_uuid, "uuid-02");
    }

    #[test]
    fn search_works() {
        let client = get_client("https://ios.copymanga.club").unwrap();
        let cancelled = AtomicBool::new(false);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(search(client, "https://ios.copymanga.club", &cancelled));
        assert!(result.is_ok());
    }
}


