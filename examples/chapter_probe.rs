use Copy_ManGa_downloader::{fetch_chapter_contents, fetch_chapter_outline, get_client, types::ManGa_item};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = "https://ios.copymanga.club";
    let path_word = "miaobukeyan";
    let client = get_client(base)?;
    let cancelled = Arc::new(AtomicBool::new(false));

    let manga = ManGa_item {
        name: path_word.to_string(),
        path_word: path_word.to_string(),
        cover: String::new(),
        author: Vec::new(),
    };
    let payload = fetch_chapter_outline(client.clone(), base, &manga, &cancelled)
        .await?
        .ok_or("被取消")?;

    println!(
        "== {} ({}) 共 {} 章",
        payload.name,
        payload.path_word,
        payload.chapters.len()
    );
    for chapter in payload.chapters.iter().take(10) {
        println!(
            "  {}  {}/comic/{}/chapter/{}",
            chapter.chapter_name, base, payload.path_word, chapter.chapter_uuid
        );
    }

    if let Some(first) = payload.chapters.first() {
        let contents = fetch_chapter_contents(
            client.clone(),
            base,
            &payload.path_word,
            &first.chapter_uuid,
            &first.chapter_name,
            &cancelled,
        )
        .await?
        .ok_or("被取消")?;

        println!(
            "\n== {} 的图片直链（共 {} 页）",
            contents.chapter_name,
            contents.pages_url.len()
        );
        for page in contents.pages_url.iter().take(5) {
            println!("  {}", page);
        }
    }
    Ok(())
}
