#![allow(unused_variables)]
use anyhow::Result;
use chromiumoxide::cdp::browser_protocol::network::StreamResourceContentParamsBuilder;
use chromiumoxide::cdp::browser_protocol::target::CreateTargetParams;
use chromiumoxide::handler;
use futures::stream::Once;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use reqwest::header::{HeaderMap, REFERER};
use serde::Deserialize;
use serde_json::Value;
use std::str::Bytes;
use std::{env ,error::Error};
use std::io::{self, Write};
use std::path::PathBuf;

use winreg::RegKey;
use winreg::enums::*;
use std::{fmt, result};

use std::fs;
use std::collections::HashSet;

//浏览器
use chromiumoxide::browser::{self, Browser, BrowserConfig, BrowserConfigBuilder};
use futures::StreamExt;
use chromiumoxide::Handler;
use tokio::time::{sleep,Duration,timeout};


//下载器相关
use tokio::io::copy;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Semaphore, SemaphorePermit};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::task::JoinHandle;

//注册表相关
use std::os::windows::process::CommandExt; // 为了隐藏 PowerShell 窗口
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// 声明 src/home.rs 为 crate 模块，否则 use crate::home::home 会报 E0432
mod home;

use crate::home::home;

//

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct Chapter {
    //下载图片时依据的结构，len是图片数量，pages_url是每张图片的链接，number是第几章，url是该话的链接
    number: usize,
    url: String,
    title: String,
    pages_url: Vec<String>,
    len: usize,
}

#[derive(Deserialize, Debug)]
struct Response {
    //搜索时用到的结构，用于储存搜索结果
    code: i32,
    message: String,
    results: Results,
}

#[derive(Deserialize, Debug)]
struct Results {
    list: Vec<ManGa_item>,
}

#[derive(Deserialize, Debug, Clone)]
struct ManGa_item {
    name: String,
    path_word: String,
    cover:String,
    author:Vec<Author>,
}

#[derive(Deserialize, Debug, Clone)]
struct Author{
    name:String,
    alias:Option<String>,
    path_word:String
}

#[derive(Debug, Deserialize, Clone)]
struct Js_chapters {
    //从控制台获取的章节的名称与相应地址
    names: Vec<String>,
    path_words: Vec<String>,
    len: usize,
}

#[derive(Debug, Deserialize, Clone)]
struct ErrorLog {
    chapter_title: String,
    error_message: String,
}

impl fmt::Display for ErrorLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "章节: {}, \n 错误信息: {}\n",
            self.chapter_title, self.error_message
        )
    }
}

/// run() 的退出原因：正常完成一部漫画，还是被 Ctrl+C 取消
enum RunOutcome {
    Completed,
    Cancelled,
}

const INSTANCE_REGISTRY_DIR: &str = "manga_downloader_instances";
const PROFILE_PREFIX: &str = "manga_downloader_profile_";

fn register_instance(profile_name: &str) -> std::io::Result<()> {
    let registry_dir = std::env::temp_dir().join(INSTANCE_REGISTRY_DIR);
    fs::create_dir_all(&registry_dir)?;
    let pid_file = registry_dir.join(format!("{}.pid", profile_name));
    let my_pid = std::process::id();
    fs::write(&pid_file, my_pid.to_string())?;
    println!("实例已注册: {} (PID: {})", profile_name, my_pid);
    Ok(())
}

fn unregister_instance(profile_name: &str) {
    let pid_file = std::env::temp_dir()
        .join(INSTANCE_REGISTRY_DIR)
        .join(format!("{}.pid", profile_name));
    let _ = fs::remove_file(&pid_file);
}

fn get_living_profiles() -> HashSet<String> {
    let registry_dir = std::env::temp_dir().join(INSTANCE_REGISTRY_DIR);
    let mut living = HashSet::new();
    let mut stale_files: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = fs::read_dir(&registry_dir) {
        let mut pid_checks: Vec<(String, u32)> = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map_or(false, |ext| ext == "pid") {
                if let Some(profile_name) = path.file_stem().and_then(|n| n.to_str()) {
                    match fs::read_to_string(&path) {
                        Ok(pid_str) => {
                            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                                pid_checks.push((profile_name.to_string(), pid));
                            } else {
                                stale_files.push(path);
                            }
                        }
                        Err(_) => stale_files.push(path),
                    }
                }
            }
        }

        if !pid_checks.is_empty() {
            let pid_list: Vec<String> = pid_checks.iter().map(|(_, pid)| pid.to_string()).collect();
            let ps_script = format!(
                r#"$pids = @({}); foreach ($p in $pids) {{ $alive = Get-Process -Id $p -ErrorAction SilentlyContinue; if ($alive) {{ Write-Output "ALIVE:$p" }} else {{ Write-Output "DEAD:$p" }} }}"#,
                pid_list.join(",")
            );

            let alive_pids: HashSet<u32> = match Command::new("powershell")
                .args(&["-NoProfile", "-Command", &ps_script])
                .creation_flags(0x08000000)
                .output()
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    stdout
                        .lines()
                        .filter(|line| line.starts_with("ALIVE:"))
                        .filter_map(|line| line.trim_start_matches("ALIVE:").parse::<u32>().ok())
                        .collect()
                }
                Err(_) => HashSet::new(),
            };

            for (profile_name, pid) in pid_checks {
                if alive_pids.contains(&pid) {
                    living.insert(profile_name);
                } else {
                    stale_files.push(registry_dir.join(format!("{}.pid", profile_name)));
                }
            }
        }

        for f in stale_files {
            let _ = fs::remove_file(&f);
        }
    }

    living
}

async fn kill_self_processes() {
    let living = get_living_profiles();

    println!("正在扫描并清理后台僵尸进程...");
    if !living.is_empty() {
        println!("当前活跃实例: {:?}", living);
    }

    let living_patterns: Vec<String> = living.iter().map(|p| format!("'*{}*'", p)).collect();
    let living_array = living_patterns.join(",");

    let ps_script = format!(
        r#"
        $targetPrefix = '*{prefix}*'
        $livingPatterns = @({living})

        $procs = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
                 Where-Object {{
                    ($_.Name -eq 'msedge.exe' -or $_.Name -eq 'chrome.exe') -and
                    $_.CommandLine -like $targetPrefix
                 }}

        if ($procs) {{
            $procs | ForEach-Object {{
                $cmdLine = $_.CommandLine
                $isLiving = $false
                foreach ($pattern in $livingPatterns) {{
                    if ($cmdLine -like $pattern) {{
                        $isLiving = $true
                        break
                    }}
                }}
                if (-not $isLiving) {{
                    Write-Output "清理僵尸进程 PID: $($_.ProcessId)"
                    Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
                }} else {{
                    Write-Output "保留活跃进程 PID: $($_.ProcessId)"
                }}
            }}
        }} else {{
            Write-Output "未发现相关的浏览器进程。"
        }}
    "#,
        prefix = PROFILE_PREFIX,
        living = living_array
    );

    let output = Command::new("powershell")
        .args(&["-NoProfile", "-Command", &ps_script])
        .creation_flags(0x08000000)
        .output();

    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if !stdout.trim().is_empty() {
                println!("{}", stdout);
            }
        }
        Err(e) => println!("无法执行清理脚本: {}", e),
    }
}

async fn clean_old_profiles() {
    let living = get_living_profiles();
    let temp_dir = env::temp_dir();

    if let Ok(entries) = fs::read_dir(temp_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with(PROFILE_PREFIX) {
                        if living.contains(name) {
                            println!("跳过活跃实例的文件夹: {}", name);
                        } else {
                            match fs::remove_dir_all(&path) {
                                Ok(_) => println!("已清理过期缓存: {}", name),
                                Err(e) => println!("无法删除 {}: {}", name, e),
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn clean_own_profile(profile_name: &str) {
    let profile_dir = env::temp_dir().join(profile_name);
    match fs::remove_dir_all(&profile_dir) {
        Ok(_) => println!("已清理自身缓存文件夹: {}", profile_name),
        Err(e) => println!("无法清理自身缓存文件夹 {}: {}", profile_name, e),
    }
}

fn get_browser_path_from_registry() -> Option<PathBuf> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);

    // 1. 查找 Chrome (App Paths)
    if let Ok(key) =
        hklm.open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\chrome.exe")
    {
        if let Ok(path_str) = key.get_value::<String, _>("") {
            // 获取默认值
            return Some(PathBuf::from(path_str));
        }
    }

    // 2. 查找 Edge (App Paths)
    if let Ok(key) =
        hklm.open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\msedge.exe")
    {
        if let Ok(path_str) = key.get_value::<String, _>("") {
            return Some(PathBuf::from(path_str));
        }
    }

    // 3. 备用方案：查找卸载注册表 (有时候 App Paths 不准)
    let uninstall_keys = [
        "SOFTWARE\\Clients\\StartMenuInternet\\Google Chrome\\shell\\open\\command",
        "SOFTWARE\\Clients\\StartMenuInternet\\Microsoft Edge\\shell\\open\\command",
    ];

    for key_path in uninstall_keys {
        if let Ok(key) = hklm.open_subkey(key_path) {
            if let Ok(raw_cmd) = key.get_value::<String, _>("") {
                // 1. 去除引号
                let mut cmd = raw_cmd.replace("\"", "");

                // 2. 截取 .exe 结尾的路径 (关键修复)
                if let Some(idx) = cmd.to_lowercase().find(".exe") {
                    cmd = cmd[..idx + 4].to_string();
                }

                let path = PathBuf::from(&cmd);
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// input_cancellable() 的结果：读到一行输入，或等待期间被 Ctrl+C 取消
enum InputOutcome {
    Line(String),
    Cancelled,
}

/// 全局唯一的 stdin 行接收器：由后台读取线程写入，所有输入点从这里取行，
/// 这样等待输入期间按 Ctrl+C 也能立即被取消标志打断
/// 注意：static 要求 Sync，而 mpsc::Receiver 只 Send 不 Sync，所以要套 Mutex
static STDIN_LINES: OnceLock<Mutex<mpsc::Receiver<String>>> = OnceLock::new();

/// 启动后台 stdin 读取线程：此后 stdin 只由该线程读取，输入点通过 channel 取行
fn spawn_stdin_reader() {
    let (tx, rx) = mpsc::channel::<String>();
    let _ = STDIN_LINES.set(Mutex::new(rx));
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match io::stdin().read_line(&mut line) {
                // EOF 或读取失败：退出线程（此时接收端会得到 Disconnected）
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    // 主程序已退出：停止发送
                    if tx.send(line.trim().to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// 显示提示并等待一行输入；等待期间按下 Ctrl+C 会立即返回 Cancelled，无需再按回车
fn input_cancellable(
    prompt: &str,
    cancelled: &AtomicBool,
) -> Result<InputOutcome, Box<dyn Error>> {
    print!("{}", prompt);
    let _ = io::stdout().flush();
    // 全程序同一时刻只有一个输入点在等行，锁竞争可以忽略
    let rx = STDIN_LINES
        .get()
        .expect("stdin 读取线程未初始化")
        .lock()
        .unwrap();
    loop {
        if cancelled.load(Ordering::SeqCst) {
            // 丢弃已排队但未消费的行，避免误喂给下一个提示
            while rx.try_recv().is_ok() {}
            return Ok(InputOutcome::Cancelled);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => return Ok(InputOutcome::Line(line)),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("标准输入已关闭".into());
            }
        }
    }
}

/// 提示并等待用户输入一个数字；输入无效则重新提示，按 Ctrl+C 返回 Ok(None)
fn input_number(prompt: &str, cancelled: &AtomicBool) -> Result<Option<usize>, Box<dyn Error>> {
    loop {
        match input_cancellable(prompt, cancelled)? {
            InputOutcome::Cancelled => return Ok(None),
            InputOutcome::Line(line) => match line.parse::<usize>() {
                Ok(n) => return Ok(Some(n)),
                Err(_) => println!("输入无效，请输入数字。"),
            },
        }
    }
}

async fn search(
    client: Client,
    base_website: &str,
    cancelled: &AtomicBool,
) -> Result<Option<Response>, Box<dyn Error>> {
    // 输入关键词期间按了 Ctrl+C：直接放弃本次搜索
    let key_word = match input_cancellable("输入关键词：\n", cancelled)? {
        InputOutcome::Cancelled => return Ok(None),
        InputOutcome::Line(line) => line,
    };
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


async fn get_browser(profile_name: &str) -> Result<(Browser, Handler), Box<dyn Error>> {

    let user_data_path = env::temp_dir().join(profile_name);


    let builder:BrowserConfigBuilder = BrowserConfig::builder();
    let path = get_browser_path_from_registry().unwrap();
    println!("成功找到浏览器路径:{:?}",path);
    println!("正在打开浏览器 (profile: {}).......", profile_name);


    let options = builder
        .user_data_dir(user_data_path)
        .launch_timeout(Duration::from_secs(5))
        .request_timeout(Duration::from_secs(30))
        .chrome_executable(path)
        .args([
            "--no-sandbox",
            "--disable-setuid-sandbox",
            "--disable-gpu",
            "--disable-software-rasterizer",
            "--disable-extensions",
            "--disable-infobars",
            "--no-first-run",
            "--no-default-browser-check",
            "--password-store=basic",
            "--disable-dev-shm-usage",
            "about:blank",
            ])
        .build()?;



       let (browser, handler) = Browser::launch(options).await?;
       println!("成功打开浏览器！");
       println!("\n\n\n");

    Ok((browser,handler))
}

fn get_client(base_website: &str)-> Result<Client, Box<dyn Error>> {
     //初始化client
    let mut headers = HeaderMap::new();
    headers.insert(REFERER, base_website.parse().unwrap());
    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/58.0.3029.110 Safari/537.3")
        .danger_accept_invalid_certs(true)
        .default_headers(headers)
        .build()?;
    Ok(client)
}

#[tokio::main]
async fn main() {
    // 安装自定义 panic 钩子：出错时先打印错误信息，再等待用户按键，
    // 防止程序一报错窗口立即关闭、来不及看错误内容
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        default_hook(panic_info);
        pause_on_error();
    }));

    // 真正的逻辑放在 real_main() 中：出错时打印错误信息并暂停，而不是直接关闭窗口
    if let Err(e) = manager().await {
        eprintln!("\n==============================");
        eprintln!("程序发生错误，已停止运行：");
        eprintln!("{}", e);
        eprintln!("==============================");
        pause_on_error();
    }
}

/// 报错后等待用户按回车再退出，避免窗口立即关闭看不到错误信息
fn pause_on_error() {
    eprintln!("按回车键退出...");
    // 优先从统一的后台读取线程取行（如果已启动），否则退回直接读 stdin
    match STDIN_LINES.get() {
        Some(m) => {
            let _ = m.lock().unwrap().recv();
        }
        None => {
            let mut _s = String::new();
            let _ = std::io::stdin().read_line(&mut _s);
        }
    }
}

async fn manager() -> Result<(), Box<dyn Error>> {
    // 启动时一次性清理僵尸进程和过期缓存（保护其他活跃实例）
    kill_self_processes().await;
    clean_old_profiles().await;
    let base_website = "https://ios.copymanga.club";

    // 生成唯一 profile 名并注册，防止其他实例误杀本进程的浏览器
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis();

    let profile_name = format!("{}{}", PROFILE_PREFIX, timestamp);
    register_instance(&profile_name)?;

    // 启动后台 stdin 读取线程：之后所有输入都从它的 channel 取行，
    // 保证等待输入期间 Ctrl+C 也能立即生效
    spawn_stdin_reader();


    let client = get_client(base_website)?;

    let (mut browser, mut handler) = get_browser(&profile_name).await?;

    tokio::spawn(async move{
        while let Some(event) =handler.next().await {}
    });
    //初始化结束

    // 全局 Ctrl+C 监听：整个程序只注册一次，任何时候按下都会置位取消标志
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    tokio::spawn(async move {
        loop {
            tokio::signal::ctrl_c().await.ok();
            flag.store(true, Ordering::SeqCst);
            println!("\n⚠ 收到 Ctrl+C，正在返回搜索...");
        }
    });

    'outer: loop {
        // 每次重新进入 run（搜索）前复位取消标志，避免上一次的 Ctrl+C 影响本次
        cancelled.store(false, Ordering::SeqCst);

        // 真正的逻辑放在 run() 里，main 只负责捕获错误；浏览器在 main 中只启动一次，多部漫画复用同一个浏览器
        match run(client.clone(), &browser, base_website, cancelled.clone()).await {
            Err(e) => {
                eprintln!("\n==============================");
                eprintln!("程序发生严重错误，已停止运行：");
                eprintln!("{}", e);

                eprintln!("==============================");
            }
            // Ctrl+C 取消：跳过 y/n 询问，直接回到搜索
            Ok(RunOutcome::Cancelled) => continue 'outer,
            Ok(RunOutcome::Completed) => {}
        }

        // 一部漫画下载完成后，询问是否继续（y 继续下载下一部，n 退出程序）
        loop {
            match input_cancellable("是否继续下载? (y/n)", &cancelled)? {
                // 等待输入期间按了 Ctrl+C：回到搜索
                InputOutcome::Cancelled => continue 'outer,
                InputOutcome::Line(line) => match line.to_lowercase().as_str() {
                    "y" => break,
                    "n" => break 'outer,
                    _ => println!("输入无效，请输入 'y' 或 'n'。"),
                },
            }
        }
    }

    // 程序退出：关闭浏览器并清理自身实例
    if let Err(e) = browser.close().await {
        eprintln!("关闭浏览器时出错: {}", e);
    }
    unregister_instance(&profile_name);
    clean_own_profile(&profile_name).await;

    Ok(())
}

async fn run(
    client: Client,
    browser: &Browser,
    base_website: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<RunOutcome, Box<dyn Error>> {
    println!("======这是一个拷贝漫画的漫画下载器======");
    println!("默认保存路径在当前文件夹的download文件夹下\n\n");

    //初始化数据
    let mut download_chapters: Vec<Chapter> = Vec::new();
    let error_logs: Vec<ErrorLog> = Vec::new();

    // Ctrl+C 监听在 real_main() 中全局只注册一次，这里通过检查 cancelled 标志响应取消


    // 搜索（含输入关键词）期间按了 Ctrl+C：直接回到搜索
    let Some(resp_json) = search(client.clone(), base_website, &cancelled).await? else {
        println!("⚠ 已取消，返回搜索...");
        return Ok(RunOutcome::Cancelled);
    };
    //dbg!(&resp_json);
    let Some(choice) = input_number("请输入要下载的漫画序号：", &cancelled)? else {
        println!("⚠ 已取消，返回搜索...");
        return Ok(RunOutcome::Cancelled);
    };
    println!("请稍后...");
    let lists = &resp_json.results.list;
    if choice >= lists.len() {
        println!("序号超出范围，返回搜索...");
        return Ok(RunOutcome::Cancelled);
    }
    let selected_item = lists[choice].clone();
    let title = selected_item.name.clone();
    let path_word = selected_item.path_word.clone();

    let url: String = format!("{}/comic/{}", &base_website, &path_word);


    let page = browser.new_page(url).await.expect("打开漫画详情页失败");

    // 等待外层容器出现，确保页面已加载
   let mut wait_count = 0;
let max_retries = 20;

while wait_count < max_retries {
    if cancelled.load(Ordering::SeqCst) {
        println!("⚠ 已取消，返回搜索...");
        page.close().await.ok();
        return Ok(RunOutcome::Cancelled);
    }
    // 尝试寻找该元素
    if page.find_element("#default全部").await.is_ok() {
        println!("目标容器 #default全部 已挂载到 DOM！");
        break;
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("{}",format!("加载失败，正在重试 ({} / {})", wait_count + 1, max_retries));
    wait_count += 1;
}

    if wait_count >= max_retries {
        panic!("超时未找到漫画列表容器，可能是网页结构改变或网络延迟过高");
    }

    println!("成功进入网页");

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

    let Ok(remote_object) = page.evaluate(script).await else{
        panic!("获取漫画话数失败!");
    };

    let Ok(remote_object) = timeout(Duration::from_secs(30), async {
        let result = timeout(Duration::from_secs(5), async {
            loop {
                match page.evaluate(script).await {
                    Ok(res) => break res,
                    Err(_) => {
                        println!("获取漫画话数失败，正在重试...");
                    }
                }
            }
        })
        .await
        .expect("多次重试后仍无法获取漫画话数，可能是网页结构改变或网络问题，跳过该漫画");

        result
        })
    .await
    else {
        panic!("获取漫画话数超时，可能是网页结构改变或网络问题，跳过该漫画");
    };

    //dbg!(&remote_object);;
    let object = remote_object.value().unwrap();
    //dbg!("js获取的数据是",&object);
    let json_str = object.as_str().expect("JS返回的不是字符串");
    let js_chapters: Js_chapters = serde_json::from_str(json_str).unwrap();
    if cancelled.load(Ordering::SeqCst) {
        println!("⚠ 已取消，返回搜索...");
        page.close().await.ok();
        return Ok(RunOutcome::Cancelled);
    }
    // dbg!(&js_chapters);
    let counts = js_chapters.len;
    let names = &js_chapters.names;

    println!("目录：");
    for (index, name) in names.iter().enumerate() {
        println!("{}:{}", index + 1, name);
    }
    println!("\n该漫画共有{}话\n", counts);


    let Some(start) = input_number("请输入起始话数(包含该话)：", &cancelled)? else {
        println!("⚠ 已取消，返回搜索...");
        page.close().await.ok();
        return Ok(RunOutcome::Cancelled);
    };
    if start == 0 || start > counts {
        println!("输入的话数有误");
        sleep(Duration::from_secs(3)).await;
        page.close().await.ok();
        return Ok(RunOutcome::Completed);
    }
    let start = start - 1;

    let Some(end) = input_number("请输入结束话数(包含该话)：", &cancelled)? else {
        println!("⚠ 已取消，返回搜索...");
        page.close().await.ok();
        return Ok(RunOutcome::Cancelled);
    };
    if end > counts || end <= start {
        println!("输入的话数有误");
        sleep(Duration::from_secs(3)).await;
        page.close().await.ok();
        return Ok(RunOutcome::Completed);
    }

    // 先收集需要下载的章节基本信息（url 和 title）
    for i in start..end {
        let link = js_chapters.path_words[i].clone();
        let chapter_title = js_chapters.names[i].clone();
        download_chapters.push(Chapter {
            number: i,
            url: link,
            title: chapter_title,
            ..Default::default()
        });
    }

    page.close().await?;

    // 解析章节页面的初始化
    let mut one_tab_count: usize = 0;
    let mut chapter_tab = browser
        .new_page(&download_chapters[0].url)
        .await
        .expect("解析第一话时，页面打开失败");

    // ===== 核心改动：解析一章，立即下载一章 =====
    let mut was_cancelled = false;
    for chapter in &mut download_chapters {

        // 检查是否收到 Ctrl+C 中断信号
        if cancelled.load(Ordering::SeqCst) {
            println!("⚠ 已取消，返回搜索...");
            was_cancelled = true;
            break;
        }

        // 限制单个 tab 解析章节数，防止内存泄漏
        one_tab_count += 1;
        if one_tab_count >= 20 {
            chapter_tab.close().await?;
            one_tab_count = 0;
            chapter_tab = browser
                .new_page(&chapter.url)
                .await
                .expect("解析页面打开失败");
        }

        chapter_tab.goto(&chapter.url).await?;
        chapter_tab.wait_for_navigation().await?;

        println!("正在解析：{}", chapter.title);

        let script = r#"(async () => {
            return await new Promise((resolve) => {
                // --- 配置区 (可根据网速调整) ---
                const scrollStep = 500;
                const frequency = 16;
                const waitTime = 1500;
                // ---------------------------

                let totalHeight = 0;
                let noChangeTicks = 0;

                const maxTicks = waitTime / frequency;

                const timer = setInterval(() => {
                    const scrollHeight = document.body.scrollHeight;
                    const currentPos = window.scrollY + window.innerHeight;

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
                                let url = img.getAttribute('data-src');
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

        let js_pages_url_response = chapter_tab.evaluate(script).await.expect("解析失败1");

        let js_pages_url_response = js_pages_url_response
            .value()
            .unwrap()
            .as_str()
            .expect("不是String")
            .to_string();

        let js_pages_url_response: Vec<String> =
            serde_json::from_str(&js_pages_url_response).unwrap();

        chapter.pages_url = js_pages_url_response;
        chapter.len = chapter.pages_url.len();
        println!(
            "{}.{} 共 {} 页，解析完毕，立即开始下载...",
            chapter.number, chapter.title, chapter.len
        );

        // 解析期间按了 Ctrl+C：放弃下载该章，直接返回搜索
        if cancelled.load(Ordering::SeqCst) {
            println!("⚠ 已取消，返回搜索...");
            was_cancelled = true;
            break;
        }

        // ===== 解析完一章后，立即下载该章 =====
        new_download(vec![chapter.clone()], title.clone(), client.clone(), cancelled.clone()).await?;
    }

    chapter_tab.close().await?;

    // 因 Ctrl+C 中断：跳过完成提示，直接返回搜索
    if was_cancelled {
        return Ok(RunOutcome::Cancelled);
    }

    // 打印错误日志
    for log in error_logs {
        println!("错误章节记录：{}", log);
    }

    println!("\n全部章节解析并下载完成！");

    Ok(RunOutcome::Completed)
}

async fn new_download(
    chapters: Vec<Chapter>,
    title: String,
    client: Client,
    cancelled: Arc<AtomicBool>,
) -> Result<(), Box<dyn Error>> {
       //为多线程下载做准备，限制线程数量
        let once_max_dowload = Arc::new(Semaphore::new(64));

    for chapter in chapters {
        // 已取消：不再派发本章剩余页面的下载任务
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        //创建漫画文件夹
        let path = format!("./download/{}/{}", title, chapter.title);
        fs::create_dir_all(&path)?;

        //创建进度条
        let pb = ProgressBar::new(chapter.len as u64);
        pb.set_style(ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos:>7}/{len:7} {msg}")
            .unwrap()
            .progress_chars("█=>"));
        pb.set_message(format!("下载中: {}", chapter.title));
        //创建进度条



        for (index, page_url) in chapter.pages_url.iter().enumerate() {
            // 已取消：停止派发剩余页面
            if cancelled.load(Ordering::SeqCst) {
                break;
            }
            let client_clone = client.clone();
            let chapter_clone = chapter.clone();
            let page_len_clone = chapter.pages_url.len().clone();
            let title_clone = title.clone();
            let page_url_clone = page_url.clone();
            let pb_clone = pb.clone();

            let once_max_download_clone = once_max_dowload.clone();
            let cancelled_clone = cancelled.clone();

        //创建子进程
        let handle = tokio::spawn(async move{
                // 已取消：直接结束，不参与下载
                if cancelled_clone.load(Ordering::SeqCst) {
                    return;
                }
                let aquire = once_max_download_clone.acquire_owned().await.unwrap();

                let page_path = format!(
                    "./download/{}/{}/{}.webp",
                    title_clone,
                    chapter_clone.title,
                    index + 1
                );
                let limit = Duration::from_secs(60);


                //在开始下载前创建相应图片文件
                let timed_out = timeout(limit, async {

                    let mut isErr :bool = false;
                    loop{
                        // 已取消：放弃当前页的下载与重试
                        if cancelled_clone.load(Ordering::SeqCst) {
                            break;
                        }

                        let mut page = tokio::fs::File::create(&page_path).await.unwrap();

                        //发送网络请求
                        let response = client_clone.
                        get(&page_url_clone)
                        .send()
                        .await;

                        match response {
                            Ok(res) =>{

                                let mut steam = res.bytes_stream();
                                let mut aborted = false;

                                    while let Some(chunk) = steam.next().await{
                                        // 接收数据期间按了 Ctrl+C：停止接收
                                        if cancelled_clone.load(Ordering::SeqCst) {
                                            aborted = true;
                                            break;
                                        }
                                        if let Ok(chunk) = chunk{
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
                                        println!("{}:第{}页下载重试完成，下载成功！",chapter_clone.title,index+1);
                                    }
                                    pb_clone.inc(1);
                                    break;
                            }
                            Err(_) =>{
                                if !isErr {
                                    println!("{}:第{}页下载失败，正在重试...",chapter_clone.title,index+1);
                                    isErr = true;
                                }
                            }
                        }

                    }
               })
                .await
                .is_err();

                // 超时：跳过该页并删掉不完整的文件，而不是 panic 退出整个程序
                if timed_out {
                    println!("{}:第{}页下载超时(60s)，跳过该页", chapter_clone.title, index + 1);
                    let _ = tokio::fs::remove_file(&page_path).await;
                }


                    }
            );

            handles.push(handle);
        }

        for handle in handles {
            if let Err(e) = handle.await {
                eprintln!("下载任务失败: {}", e);
            }
        }

        if cancelled.load(Ordering::SeqCst) {
            pb.finish_with_message(format!("{} 下载已取消", chapter.title));
            return Ok(());
        }

        pb.finish_with_message(format!("{} 下载完毕", chapter.title));

        }


    // println!("\n所有章节下载完成！");
    // println!("温馨提醒：");
    // println!("会有极小概率一话页数没有完整加载出来，导致尾部缺页情况发生，");
    // println!("可以根据每话之间的页数对比 or 是否有汉化组尾页来确定是否缺页");
    // println!("重新下载该话能补全页数\n\n");
    Ok(())
    }
