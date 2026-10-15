use Copy_ManGa_downloader::{MangaSource, ZerobywSource};
use std::sync::atomic::AtomicBool;

/// 联网探测 zerobyw 的搜索、章节大纲与图片直链解析是否正常。
///
/// 用法：`cargo run --example zerobyw_probe [kuid]`
/// 不传 kuid 时按关键词搜索并取第一条。
///
/// 会分别解析**首话**（站点页面上未上锁）与**最后一话**（站点页面上通常标记为
/// 需登录 / VIP）的图片直链，用来确认付费墙不影响实际下载。
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = ZerobywSource::new()?;
    let cancelled = AtomicBool::new(false);

    let kuid = match std::env::args().nth(1) {
        Some(kuid) => kuid,
        None => {
            let list = source.search("停止时间的勇者", &cancelled).await?;
            let manga = list.first().ok_or("搜索无结果")?;
            println!("== 选中: {} (kuid={})", manga.name, manga.path_word);
            manga.path_word.clone()
        }
    };

    let chapters = source.fetch_chapters(&kuid, &cancelled).await?;
    println!("== kuid={kuid} 详情页共解析出 {} 话", chapters.len());
    for chapter in chapters.iter().take(5) {
        println!("  {}  zjid={}", chapter.chapter_name, chapter.chapter_uuid);
    }
    if chapters.len() > 8 {
        println!("  ...");
        for chapter in chapters.iter().skip(chapters.len() - 3) {
            println!("  {}  zjid={}", chapter.chapter_name, chapter.chapter_uuid);
        }
    }

    // 首话在站点页面上是开放的；最后一话通常被标成需登录 / VIP，
    // 两者都能解析出图片直链，才说明 CDN 兜底生效
    let mut targets = Vec::new();
    if let Some(first) = chapters.first() {
        targets.push(("首话", first));
    }
    if chapters.len() > 1
        && let Some(last) = chapters.last()
    {
        targets.push(("末话（页面上锁）", last));
    }

    for (label, chapter) in targets {
        let pages = source
            .fetch_pages(&kuid, &chapter.chapter_uuid, &cancelled)
            .await?;
        println!(
            "\n== {label} 第{}话 解析到 {} 页",
            chapter.chapter_name,
            pages.len()
        );
        for page in pages.iter().take(3) {
            println!("  {}", page);
        }
        if pages.len() > 3 {
            println!("  ...");
            println!("  {}", pages.last().unwrap());
        }
    }

    Ok(())
}
