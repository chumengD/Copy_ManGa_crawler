use crossterm::{
    cursor::{Hide, Show},
    execute,
};



fn read_input_num() ->Option<i32>{
    loop{
        let mut input = String::new();
        match std::io::stdin().read_line(&mut input){
            Ok(0) | Err(_) => continue,
            Ok(_) =>{
                match input.trim().parse::<i32>(){
                    Ok(num) => return Some(num),
                    Err(_) => {
                        println!("输入无效，请输入一个整数。");
                        continue;
                    }
                }
            }
        }
    }
}



pub async fn home() -> Option<i32> {
    execute!(std::io::stdout(), Hide).unwrap();
    println!("按下相应按键选择功能："); 
    println!("1.搜索"); 
    println!("2.查看下载");
    println!("3.检查漫画更新");
    println!("4.退出");
    let input = read_input_num().unwrap();
    execute!(std::io::stdout(), Show).unwrap();
    match input {
        1 => {
            println!("搜索功能尚未实现。");
            None
        }
        2 => {
            println!("查看下载功能尚未实现。");
            None
        }
        3 => {
            println!("检查漫画更新功能尚未实现。");
            None
        }
        4 => {
            println!("退出程序。");
            std::process::exit(0);
        }
        _ => {
            println!("无效的选择，请输入1-4之间的数字。");
            None
        }
    }
}