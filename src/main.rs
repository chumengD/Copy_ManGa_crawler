use Copy_ManGa_downloader::types::Chapter;
use Copy_ManGa_downloader::{
    BASE_WEBSITE, check_manga_updates, display_chapter_list, download, fetch_chapter_contents,
    fetch_chapter_outline, get_client, input_number, pause_on_error,
    save_chapter_details, search, update_selected_mangas
};
use anyhow::Context;
use reqwest::Client;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main(){
    let base_website = BASE_WEBSITE;
    let client = match get_client(base_website) {
        Ok(c) => c,
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
       println!("1:搜索  2:检查漫画更新  3：退出");
        let choice =input_number("你想要干什么：", &cancelled).await;

        
        match choice{
            Some(choice) => {
                match choice {
                            1 => {
                                if let Err(e) = run(client.clone(), base_website, cancelled.clone()).await{
                                    eprintln!("\n==============================");
                                    eprintln!("程序发生错误，已停止运行：");
                                    eprintln!("{}", e);
                                    eprintln!("==============================");
                                    pause_on_error();
                                }
                            }
                            2 => {
                                if let Err(e) = async {
                                    let updates =
                                        check_manga_updates(client.clone(), base_website, &cancelled).await?;
                                    update_selected_mangas(updates, client.clone(), cancelled.clone()).await
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
                            3 =>{
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


async fn run(client:Client,base_website:&str,cancelled: Arc<AtomicBool>) -> Result<(), Box<dyn std::error::Error>> {
    
    let result = search(client.clone(), base_website, &cancelled).await?;
    match result{
        Some(res) =>{
            let Some(choice) = input_number("请输入要下载的漫画序号", &cancelled).await else {
                return Ok(());
            };
            let Some(selected_manga) = res.results.list.get(choice) else {
                println!("序号超出范围，返回搜索...");
                return Ok(());
            };
            let chapter_details = fetch_chapter_outline(client.clone(), base_website, selected_manga, &cancelled).await?;
            dbg!(&chapter_details);
            let Some(mut chapter_details) = chapter_details else {
                eprintln!("获取章节详情失败，退出....");
                return Ok(());
            };
            let path_word = chapter_details.path_word.clone();
            for chapter in &mut chapter_details.chapters{
                if let Ok(Some(content)) = fetch_chapter_contents(client.clone(), base_website, &path_word, &chapter.chapter_uuid, &chapter.chapter_name, &cancelled).await {
                    *chapter = content;
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

            download(chapter_details, begin, end, client.clone(), cancelled.clone()).await?;


            // println!("搜索结果: {:?}", res);
            // if let Some(chapters) = chapters{
            //     println!("获取到章节: {:?}", chapters);
            // }
            

        }
        _ => println!("搜索已取消，未返回结果"),
    }
    Ok(())
}
