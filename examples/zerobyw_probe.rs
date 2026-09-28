use Copy_ManGa_downloader::{MangaSource, ZerobywSource};
use std::sync::atomic::AtomicBool;

/// 探测 zerobyw 的搜索、章节列表与图片直链解析是否正常
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = ZerobywSource::new()?;
    let cancelled = AtomicBool::new(false);

    let list = source.search("海盗战记", &cancelled).await?;
    let Some(manga) = list.first() else {
        return Err("搜索无结果".into());
    };
    println!("== 选中: {} (kuid={})", manga.name, manga.path_word);

    let chapters = source.fetch_chapters(&manga.path_word, &cancelled).await?;
    println!("== 共解析出 {} 个未锁章节", chapters.len());
    for chapter in chapters.iter().take(10) {
        println!("  {}  zjid={}", chapter.chapter_name, chapter.chapter_uuid);
    }

    if let Some(first) = chapters.first() {
        let pages = source
            .fetch_pages(&manga.path_word, &first.chapter_uuid, &cancelled)
            .await?;

        println!(
            "\n== {} 的图片直链（共 {} 页）",
            first.chapter_name,
            pages.len()
        );
        for page in pages.iter().take(3) {
            println!("  {}", page);
        }
    }
    Ok(())
}
