use Copy_ManGa_downloader::{BASE_WEBSITE, CopyMangaSource, MangaSource};
use std::sync::atomic::AtomicBool;

/// 探测拷贝漫画的章节大纲与图片直链解析是否正常
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path_word = "miaobukeyan";
    let source = CopyMangaSource::new(BASE_WEBSITE)?;
    let cancelled = AtomicBool::new(false);

    let chapters = source.fetch_chapters(path_word, &cancelled).await?;
    if chapters.is_empty() {
        return Err("未获取到章节（可能被取消或被站点软限流）".into());
    }

    println!("== {} 共 {} 章", path_word, chapters.len());
    for chapter in chapters.iter().take(10) {
        println!(
            "  {}  {}/comic/{}/chapter/{}",
            chapter.chapter_name, BASE_WEBSITE, path_word, chapter.chapter_uuid
        );
    }

    if let Some(first) = chapters.first() {
        let pages = source
            .fetch_pages(path_word, &first.chapter_uuid, &cancelled)
            .await?;

        println!(
            "\n== {} 的图片直链（共 {} 页）",
            first.chapter_name,
            pages.len()
        );
        for page in pages.iter().take(5) {
            println!("  {}", page);
        }
    }
    Ok(())
}
