use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::time::{sleep, Duration};

use Copy_ManGa_downloader::types::ChapterDetails;
use Copy_ManGa_downloader::{
    BASE_WEBSITE, CopyMangaSource, MangaSource, check_manga_update, check_manga_updates,
    display_chapter_list, download, input_line, input_number, pause_on_error,
    read_manga_downloaded, save_chapter_details, set_manga_completed, update_selected_mangas,
};

#[tokio::main]
async fn main(){
    let source = match CopyMangaSource::new(BASE_WEBSITE) {
        Ok(c) => Arc::new(c) as Arc<dyn MangaSource>,
        Err(e) => {
            eprintln!("\n==============================");
            eprintln!("程序发生错误，已停止运行：");
            eprintln!("{}", e);
            eprintln!("==============================");
            pause_on_error();
            return;
        }
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_count:Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));

    let flag = cancelled.clone();
    let count = cancelled_count.clone();

    tokio::spawn(async move {
        loop {
            tokio::signal::ctrl_c().await.ok();
            flag.store(true, Ordering::SeqCst);
            count.fetch_add(1, Ordering::SeqCst);
            println!("\n⚠ 收到 Ctrl+C，正在返回主菜单...");
            println!("⚠ 下一次按 Ctrl+C 将直接退出程序...\n\n");
       }
   });
   'outer: loop{
       cancelled.store(false, Ordering::SeqCst);
        if cancelled_count.load(Ordering::SeqCst) >=2{
            break 'outer;
        }
       println!("1:搜索  2:指定漫画检查更新  3:检查全部更新  4:标记完结  5:退出");
       let choice =input_number("你想要干什么：", &cancelled).await;


        match choice{
            Some(choice) => {
                match choice {
                            1 => {
                                if let Err(e) = run(source.clone(), cancelled.clone()).await{
                                    eprintln!("\n==============================");
                                    eprintln!("程序发生错误，已停止运行：");
                                    eprintln!("{}", e);
                                    eprintln!("==============================");
                                    pause_on_error();
                                }
                            }
                            2 => {
                                if let Err(e) = check_selected_manga_update(
                                    source.clone(),
                                    cancelled.clone(),
                                )
                                .await
                                {
                                    eprintln!("\n==============================");
                                    eprintln!("检查更新失败：");
                                    eprintln!("{}", e);
                                    eprintln!("==============================");
                                    pause_on_error();
                                }
                            }
                            3 => {
                                if let Err(e) = async {
                                    let updates =
                                        check_manga_updates(source.as_ref(), &cancelled).await?;
                                    update_selected_mangas(updates, source.clone(), cancelled.clone()).await
                                }
                                .await
                                {
                                    eprintln!("\n==============================");
                                    eprintln!("检查更新失败：");
                                    eprintln!("{}", e);
                                    eprintln!("==============================");
                                    pause_on_error();
                                }
                            }
                            4 => {
                                if let Err(e) = mark_manga_completed(cancelled.clone()).await {
                                    eprintln!("\n==============================");
                                    eprintln!("标记完结失败：");
                                    eprintln!("{}", e);
                                    eprintln!("==============================");
                                    pause_on_error();
                                }
                            }
                            5 =>{
                                println!("正在退出程序.....");
                                break 'outer;
                            }
                            _ => {
                                println!("⚠ 输入无效，请重新输入");
                                continue 'outer;
                            }
                        }
                    }

            None=> {
                println!("⚠ 输入无效，请重新输入");
                continue 'outer;
            }

        }
        }
    }


async fn mark_manga_completed(cancelled: Arc<AtomicBool>) -> Result<(), Box<dyn std::error::Error>> {
    let local_mangas = read_manga_downloaded().await?;
    if local_mangas.is_empty() {
        println!("本地还没有已下载的漫画");
        return Ok(());
    }

    println!("\n本地漫画:");
    for (index, manga) in local_mangas.iter().enumerate() {
        let status = if manga.completed { "已完结" } else { "未完结" };
        println!("{}.{} [{}]", index, manga.name, status);
    }
    println!(
        "输入 0-{0} 选择一部漫画，输入 {0} 以外的数字返回",
        local_mangas.len() - 1
    );

    let Some(choice) = input_number("请输入要标记的漫画序号：", &cancelled).await else {
        return Ok(());
    };
    let Some(manga) = local_mangas.get(choice) else {
        println!("序号超出范围，返回主菜单...");
        return Ok(());
    };

    let new_status = !manga.completed;
    set_manga_completed(&manga.name, new_status).await?;
    if new_status {
        println!("已将 {} 标记为已完结，检查更新时会跳过", manga.name);
    } else {
        println!("已将 {} 标记为未完结，检查更新时会正常检查", manga.name);
    }
    Ok(())
}

async fn check_selected_manga_update(
    source: Arc<dyn MangaSource>,
    cancelled: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let local_mangas = read_manga_downloaded().await?;
    if local_mangas.is_empty() {
        println!("本地还没有已下载的漫画");
        return Ok(());
    }

    println!("\n本地漫画:");
    for (index, manga) in local_mangas.iter().enumerate() {
        let status = if manga.completed { "已完结" } else { "未完结" };
        println!(
            "{}.{} (已下载 {} 话) [{}]",
            index,
            manga.name,
            manga.chapter_names.len(),
            status
        );
    }
    println!(
        "输入 0-{0} 选择一部漫画，输入 {0} 以外的数字返回",
        local_mangas.len() - 1
    );

    let Some(choice) = input_number("请输入要检查更新的漫画序号：", &cancelled).await else {
        return Ok(());
    };
    let Some(manga) = local_mangas.get(choice) else {
        println!("序号超出范围，返回主菜单...");
        return Ok(());
    };

    let update = check_manga_update(source.as_ref(), manga, &cancelled).await?;
    if cancelled.load(Ordering::SeqCst) {
        return Ok(());
    }

    match update {
        Some(update) => {
            update_selected_mangas(vec![update], source, cancelled).await
        }
        None => {
            println!("{} 没有可下载的新章节", manga.name);
            Ok(())
        }
    }
}

async fn run(source: Arc<dyn MangaSource>, cancelled: Arc<AtomicBool>) -> Result<(), Box<dyn std::error::Error>> {

    // 输入关键词期间按了 Ctrl+C（返回 None）或 stdin 关闭都放弃本次搜索
    let Some(key_word) = input_line("输入关键词：\n", &cancelled).await else {
        println!("搜索已取消，未返回结果");
        return Ok(());
    };

    let list = source.search(&key_word, &cancelled).await?;
    if cancelled.load(Ordering::SeqCst) {
        println!("搜索已取消，未返回结果");
        return Ok(());
    }

    let Some(choice) = input_number("请输入要下载的漫画序号", &cancelled).await else {
        return Ok(());
    };
    let Some(selected_manga) = list.get(choice) else {
        println!("序号超出范围，返回搜索...");
        return Ok(());
    };

    let chapters = source
        .fetch_chapters(&selected_manga.path_word, &cancelled)
        .await?;
    if cancelled.load(Ordering::SeqCst) {
        eprintln!("获取章节详情失败，退出....");
        return Ok(());
    }

    let mut chapter_details = ChapterDetails {
        name: selected_manga.name.clone(),
        path_word: selected_manga.path_word.clone(),
        completed: false,
        source: source.id().to_string(),
        chapters,
    };

    for chapter in &mut chapter_details.chapters {
        if let Ok(pages_url) = source
            .fetch_pages(&chapter_details.path_word, &chapter.chapter_uuid, &cancelled)
            .await
        {
            chapter.pages_url = pages_url;
            chapter.len = chapter.pages_url.len();
        }
        // 每话之间歇一下，避免请求过快触发站点限流
        sleep(Duration::from_millis(1000)).await;
    }
    dbg!(&chapter_details);
    let json_path = save_chapter_details(&chapter_details).await?;
    println!("章节详情已保存到: {}", json_path.display());

    display_chapter_list(&chapter_details);

   let (begin,end) = loop {
        let mut begin =
            input_number("请输入起始话数(包含该话)：", &cancelled)
                .await
                .expect("获取起始话数失败");

        if begin < 1 {
            println!("起始范围错误，请重新输入");
            continue;
        }

        begin -= 1;

        let mut end =
            input_number("请输入结束话数(包含该话)：", &cancelled)
                .await
                .expect("获取结束话数失败");

        if end < begin + 1 || end > chapter_details.chapters.len() {
            println!("结束范围错误，请重新输入");
            continue;
        }

        end -= 1;

        break (begin,end);
    };

    download(chapter_details, begin, end, source.http().clone(), cancelled.clone()).await?;

    Ok(())
}
