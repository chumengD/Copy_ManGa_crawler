//! 漫画下载器主流程：菜单交互、图片下载、本地状态管理与更新检查。
//!
//! 各站点的搜索/章节/图片解析逻辑由 [`source::MangaSource`] 适配器实现
//! （见 [`copymanga`]），主流程只面向该接口，新增站点无需改动这里。

pub mod copymanga;
pub mod source;
pub mod types;
pub mod zerobyw;

pub use copymanga::{BASE_WEBSITE, CopyMangaSource};
pub use source::MangaSource;
pub use zerobyw::ZerobywSource;

use std::collections::HashSet;
use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use tokio::fs::{create_dir_all, read_dir, read_to_string, write};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt}; // read_line 供 input_line()，write_all 供 download()
use tokio::io::{BufReader, Stdin};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{Duration, sleep, timeout};

use types::{ChapterContents, ChapterDetails, LocalManga, MangaOutline, MangaUpdate};

/// 检查全部更新时，相邻两部漫画之间的间隔，避免连续请求触发站点限流
const BETWEEN_MANGA_DELAY: Duration = Duration::from_millis(1000);

/// 进程内共享的 stdin 读取器。必须在多次 input_line 之间复用同一个 BufReader：
/// 每次新建会把管道里待读的多行一口气读进自己的缓冲区，用完即弃，导致后续输入丢失
/// （交互时"预打"的输入、重定向/管道输入都会中招）。
fn global_stdin() -> &'static Mutex<BufReader<Stdin>> {
    static STDIN: OnceLock<Mutex<BufReader<Stdin>>> = OnceLock::new();
    STDIN.get_or_init(|| Mutex::new(BufReader::new(tokio::io::stdin())))
}

/// stdin 是否已读到 EOF（管道关闭 / 输入流结束）。
/// EOF 时 input_line 返回 None 与 Ctrl+C 取消无法从返回值区分，
/// 调用方（主菜单）据此决定退出而不是当成"输入无效"死循环。
pub fn stdin_at_eof() -> &'static AtomicBool {
    static EOF: AtomicBool = AtomicBool::new(false);
    &EOF
}

/// 等待一行输入；等待期间 cancelled 被置位（Ctrl+C）立即返回 None。
/// 全程持有全局 stdin 锁，读行 future 随 select 一起被丢弃时锁会自动释放，
/// 不会出现"后台读线程泄漏 / 多个读者抢 stdin"的问题。
/// 返回 None 表示被取消或 stdin 已关闭（EOF，见 stdin_at_eof）。
pub async fn input_line(prompt: &str, cancelled: &AtomicBool) -> Option<String> {
    print!("{}", prompt);
    let _ = std::io::stdout().flush();

    let mut reader = global_stdin().lock().await;
    loop {
        let mut line = String::new();
        tokio::select! {
            // Ctrl+C：取消标志由调用方（bin 里的全局监听任务）置位
            _ = wait_cancelled(cancelled) => return None,
            res = reader.read_line(&mut line) => match res {
                Ok(0) => {
                    stdin_at_eof().store(true, Ordering::SeqCst);
                    return None; // EOF：stdin 已关闭
                }
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

    // 落盘时保留已有的完结标记与来源标识，避免检查/更新章节时
    // 把用户手动标记的完结状态或漫画所属源冲掉。
    let mut details = details.clone();
    if let Ok(text) = read_to_string(&output_path).await {
        if let Ok(existing) = serde_json::from_str::<ChapterDetails>(&text) {
            details.completed = existing.completed;
            if !existing.source.is_empty() {
                details.source = existing.source;
            }
        }
    }

    let json = serde_json::to_string_pretty(&details).context("章节详情序列化为 JSON 失败")?;
    write(&output_path, format!("{json}\n"))
        .await
        .with_context(|| format!("写入章节详情失败: {}", output_path.display()))?;

    Ok(output_path)
}

/// 读写 `<漫画名>.json` 里的完结标记；文件不存在时会先建一个最小 JSON。
pub async fn set_manga_completed(manga_name: &str, completed: bool) -> Result<PathBuf> {
    let file_stem = sanitize_file_name(manga_name);
    let dir_name = if manga_name.trim().is_empty() {
        file_stem.clone()
    } else {
        manga_name.to_string()
    };
    let output_dir = Path::new("download").join(&dir_name);
    let output_path = output_dir.join(format!("{file_stem}.json"));

    create_dir_all(&output_dir)
        .await
        .with_context(|| format!("创建目录失败: {}", output_dir.display()))?;

    let mut details = match read_to_string(&output_path).await {
        Ok(text) => serde_json::from_str::<ChapterDetails>(&text).with_context(|| {
            format!("解析章节详情失败: {}", output_path.display())
        })?,
        Err(_) => ChapterDetails {
            name: manga_name.to_string(),
            path_word: String::new(),
            completed: false,
            source: String::new(),
            chapters: Vec::new(),
        },
    };
    details.completed = completed;

    let json = serde_json::to_string_pretty(&details).context("章节详情序列化为 JSON 失败")?;
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

        // 没有图片直链的章节直接跳过：正常流程不会出现，出现说明上游拉取直链失败，
        // 跳过可避免建出空章节目录还提示"下载完毕"
        if chapter.pages_url.is_empty() {
            println!("{} 没有图片直链，跳过", chapter.chapter_name);
            continue;
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
/// 章节详情 JSON 只用来补充 path_word / source / completed，不作为“是否下载过”的依据。
/// 空的章节目录不算已下载：更新中断会留下空目录，需要重新检测补下。
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
                    // 空目录不算已下载：更新中断会留下空章节目录，
                    // 计入的话下次检查更新会误判为"没有更新"，永远补不上
                    if is_empty_dir(&child_path).await {
                        continue;
                    }
                    chapter_names.push(chapter_name.to_string());
                }
            }
        }

        let (path_word, completed, source) = read_local_meta(&path, name).await;
        mangas.push(LocalManga {
            name: name.to_string(),
            path_word,
            completed,
            source,
            chapter_names,
        });
    }

    Ok(mangas)
}

/// 目录里一个条目都没有才算空；读取失败按非空处理（保持原有判定，不多打扰）
async fn is_empty_dir(dir: &Path) -> bool {
    match read_dir(dir).await {
        Ok(mut entries) => entries
            .next_entry()
            .await
            .map(|next| next.is_none())
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// 从本地 `<漫画名>.json` 里读出 path_word、完结标记和来源源标识。
/// 文件不存在或格式不兼容时返回 `(None, false, None)`，由调用方决定是否兜底。
async fn read_local_meta(manga_dir: &Path, name: &str) -> (Option<String>, bool, Option<String>) {
    let metadata_path = manga_dir.join(format!("{}.json", sanitize_file_name(name)));
    let Ok(text) = read_to_string(metadata_path).await else {
        return (None, false, None);
    };
    let Ok(details) = serde_json::from_str::<ChapterDetails>(&text) else {
        return (None, false, None);
    };

    let path_word = {
        let path_word = details.path_word.trim();
        if path_word.is_empty() {
            None
        } else {
            Some(path_word.to_string())
        }
    };
    let source = {
        let source = details.source.trim();
        if source.is_empty() {
            None
        } else {
            Some(source.to_string())
        }
    };
    (path_word, details.completed, source)
}

/// 解析漫画 ID 并拉取线上章节大纲，顺带把快照落盘（缓存漫画 ID / source，保留完结标记）。
/// 不判断完结与否——是否跳过由调用方决定。
/// 返回 `Ok(None)` 表示被取消、漫画不属于当前源或没找到精确匹配（原因已打印）。
pub async fn fetch_manga_outline(
    source: &dyn MangaSource,
    manga: &LocalManga,
    cancelled: &AtomicBool,
) -> Result<Option<MangaOutline>, Box<dyn Error>> {
    if cancelled.load(Ordering::SeqCst) {
        return Ok(None);
    }

    // 旧版下载的 JSON 里没有 source 字段，按当前源处理（向后兼容）；
    // 有 source 但与当前源不符的漫画跳过，避免拿错站点的 ID 去请求。
    if let Some(manga_source) = &manga.source {
        if manga_source != source.id() {
            println!(
                "{} 属于源 {}，当前源为 {}，跳过",
                manga.name, manga_source, source.id()
            );
            return Ok(None);
        }
    }

    println!("正在检查: {}", manga.name);

    // 新版下载会在漫画文件夹里留下 <漫画名>.json，里面已缓存漫画 ID，直接用即可；
    // 只有旧版下载没有该 JSON 时，才联网用名称反查漫画 ID。
    let path_word = match manga.path_word.clone() {
        Some(path_word) => {
            println!("使用本地记录的漫画 ID: {}", path_word);
            path_word
        }
        None => {
            println!("本地缺少漫画 ID，正在联网查找 {} ...", manga.name);
            let Some(found) = source.find_by_name(&manga.name, cancelled).await? else {
                if cancelled.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                println!("[!] {} 没有找到精确匹配的线上漫画，已跳过", manga.name);
                return Ok(None);
            };
            found
        }
    };

    let online_chapters = source.fetch_chapters(&path_word, cancelled).await?;
    if cancelled.load(Ordering::SeqCst) {
        println!("⚠ 已取消，停止检查更新");
        return Ok(None);
    }

    // 无论有没有新章节，都把线上章节详情落盘，保证每个漫画文件夹里都有 <漫画名>.json
    let snapshot = ChapterDetails {
        name: manga.name.clone(),
        path_word: path_word.clone(),
        completed: false,
        source: source.id().to_string(),
        chapters: online_chapters.clone(),
    };
    if let Err(e) = save_chapter_details(&snapshot).await {
        eprintln!("[!] 保存 {} 的章节详情失败: {}", manga.name, e);
    }

    Ok(Some(MangaOutline {
        name: manga.name.clone(),
        path_word,
        source: source.id().to_string(),
        online_chapters,
    }))
}

/// 检查单部漫画的更新。
/// 返回 `Ok(Some(update))` 表示有新章节；`Ok(None)` 表示无更新、已完结、属于其他源、被跳过或已取消。
async fn check_single_manga_update(
    source: &dyn MangaSource,
    manga: &LocalManga,
    cancelled: &AtomicBool,
) -> Result<Option<MangaUpdate>, Box<dyn Error>> {
    if cancelled.load(Ordering::SeqCst) {
        return Ok(None);
    }

    if manga.completed {
        println!("{} 已完结，跳过检查更新", manga.name);
        return Ok(None);
    }

    let Some(online) = fetch_manga_outline(source, manga, cancelled).await? else {
        if cancelled.load(Ordering::SeqCst) {
            println!("⚠ 已取消，停止检查更新");
        }
        return Ok(None);
    };

    let local_chapter_names = manga
        .chapter_names
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();

    let new_chapters = online
        .online_chapters
        .iter()
        .filter(|chapter| !local_chapter_names.contains(chapter.chapter_name.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if !new_chapters.is_empty() {
        println!("{} 发现 {} 个新章节:", online.name, new_chapters.len());
        for (index, chapter) in new_chapters.iter().enumerate() {
            println!("  {}.{}", index + 1, chapter.chapter_name);
        }
        Ok(Some(MangaUpdate {
            name: online.name,
            path_word: online.path_word,
            source: online.source,
            online_chapters: online.online_chapters,
            new_chapters,
        }))
    } else {
        println!("{} 没有更新", online.name);
        Ok(None)
    }
}

/// 对比本地章节目录和线上章节目录，返回每部漫画新增的章节。
pub async fn check_manga_updates(
    source: &dyn MangaSource,
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

        match check_single_manga_update(source, manga, cancelled).await? {
            Some(update) => updates.push(update),
            None => {
                if cancelled.load(Ordering::SeqCst) {
                    return Ok(Vec::new());
                }
            }
        }

        // 每部漫画之间歇一下，避免连续请求触发站点限流
        sleep(BETWEEN_MANGA_DELAY).await;
    }

    Ok(updates)
}

/// 让用户选择要更新的漫画，并直接下载选中漫画的全部新章节。
pub async fn update_selected_mangas(
    updates: Vec<MangaUpdate>,
    source: Arc<dyn MangaSource>,
    cancelled: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error>> {
    if updates.is_empty() {
        println!("所有漫画都没有更新");
        return Ok(());
    }

    let choice = if updates.len() == 1 {
        0
    } else {
        println!("\n可更新的漫画:");
        for (index, update) in updates.iter().enumerate() {
            println!(
                "{}:{} (新增 {} 话)",
                index,
                update.name,
                update.new_chapters.len()
            );
        }
        println!(
            "输入 0-{0} 选择一部漫画，输入 {0} 以外的数字返回",
            updates.len() - 1
        );

        let Some(choice) = input_number("请输入要更新的漫画序号：", &cancelled).await else {
            return Ok(());
        };
        if choice >= updates.len() {
            println!("序号超出范围，返回主菜单...");
            return Ok(());
        }
        choice
    };

    let update = &updates[choice];
    if update.new_chapters.is_empty() {
        println!("{} 没有新章节可下载", update.name);
        return Ok(());
    }

    println!("即将下载 {} 的全部新章节:", update.name);
    for (index, chapter) in update.new_chapters.iter().enumerate() {
        println!("{}:{}", index + 1, chapter.chapter_name);
    }

    // 章节大纲接口只返回章节列表，不含图片直链；下载前必须逐话补齐，
    // 否则 download() 拿到空 pages_url 会一个文件都不下。
    // 拉取失败的话跳过并在结尾提示，下次检查更新会再次将其列为新章节。
    let mut new_chapters = Vec::new();
    for chapter in &update.new_chapters {
        if cancelled.load(Ordering::SeqCst) {
            println!("⚠ 已取消，停止更新");
            return Ok(());
        }

        match source
            .fetch_pages(&update.path_word, &chapter.chapter_uuid, &cancelled)
            .await
        {
            Ok(pages_url) if !pages_url.is_empty() => {
                new_chapters.push(ChapterContents {
                    chapter_name: chapter.chapter_name.clone(),
                    chapter_uuid: chapter.chapter_uuid.clone(),
                    len: pages_url.len(),
                    pages_url,
                });
            }
            Ok(_) => eprintln!("[!] {} 没有获取到图片直链，已跳过", chapter.chapter_name),
            Err(e) => eprintln!("[!] {} 获取图片直链失败，已跳过: {}", chapter.chapter_name, e),
        }

        // 每话之间歇一下，避免请求过快触发站点限流
        sleep(BETWEEN_MANGA_DELAY).await;
    }

    if new_chapters.is_empty() {
        println!("{} 的新章节全部拉取直链失败，本次不下载", update.name);
        return Ok(());
    }

    let manga_name = update.name.clone();
    // 新增章节从头下到尾，不再让用户选范围。
    let begin = 0;
    let end = new_chapters.len() - 1;
    let download_details = ChapterDetails {
        name: manga_name.clone(),
        path_word: update.path_word.clone(),
        completed: false,
        source: update.source.clone(),
        chapters: new_chapters,
    };

    // 更新也要落盘 <漫画名>.json，把漫画 ID 缓存进漫画文件夹，
    // 这样下次检查更新就能直接读本地缓存，不必再联网反查。
    // 写入的是线上完整章节列表，避免用只含本次新增话的局部数据覆盖旧记录。
    // completed / source 字段由 save_chapter_details 从已有文件保留。
    let snapshot = ChapterDetails {
        name: manga_name.clone(),
        path_word: update.path_word.clone(),
        completed: false,
        source: update.source.clone(),
        chapters: update.online_chapters.clone(),
    };
    let json_path = save_chapter_details(&snapshot).await?;
    println!("章节详情已保存到: {}", json_path.display());

    download(download_details, begin, end, source.http().clone(), cancelled).await?;

    println!("{} 更新完成", manga_name);
    Ok(())
}
