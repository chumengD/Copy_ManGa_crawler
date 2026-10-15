//! zerobyw（zero搬运网）站点适配器。
//!
//! 章节大纲与图片直链都走桌面端 `/pc/` 路径，而不是 `/Android/` 页面：
//! - 搜索：`GET /Android/souo/?keyword=...`（结果里的 kuid 两个前端通用）
//! - 详情页：`/pc/details/?kuid=N`
//!   * `const mangaDownloadChapters = [{"zjid":..,"zjname":..}, ...]`
//!     含**全部**章节，站点在 Android 页面上抹掉链接的需登录 / VIP 章节也在这里
//!   * 封面图 `tupa.zerobyw33.com/manhua/{manga_code}/...` 提供 CDN 上的漫画代码
//! - 阅读页：`/pc/view/index.php?zjid=N`，图片直链写在 `img.manga-image` 的 src 里
//! - 上锁章节：阅读页不嵌图片（付费墙只做在页面上），但图片本体在 CDN 上并不鉴权，
//!   按 `{CDN}/manhua/{manga_code}/{zjname}/{页码:03d}.{ext}` 直接探测即可拿全
//!
//! 因此 **下载不依赖登录**：站点标记为「需登录」或「VIP」的章节同样能下载完整。
//! Discuz 登录表单仍然保留，仅用于站点策略变化时手动带上会话。
//! 登录会话 cookie 持久化在 `download/.zerobyw_cookies.json`，下次启动自动恢复。

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::future::join_all;
use regex::Regex;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderMap, REFERER};
use reqwest::{Client, Proxy, Url};
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;
use tokio::fs::{create_dir_all, remove_file, write};
use tokio::time::{sleep, Duration};

use crate::input_line;
use crate::input_number;
use crate::source::MangaSource;
use crate::types::{ChapterContents, ManGa_item};
use crate::wait_cancelled;

/// 站点地址直接硬编码。该站域名尾部的数字会不定期变化，失效时改这里
pub const BASE_WEBSITE: &str = "https://www.zerobyw33.com";

/// 图片 CDN。站点页面上锁（需登录 / VIP）的章节，图片本体依然由这个域名直接提供，
/// 这是绕开页面付费墙的关键
pub const CDN_WEBSITE: &str = "https://tupa.zerobyw33.com";

/// CDN 按页码顺序探测时的页码上限（正常章节远小于此值）
const MAX_CHAPTER_PAGES: u32 = 1000;

/// 一轮并发探测的页码数量
const PROBE_BATCH: u32 = 12;

/// 连续探测不到多少个页码就判定章节结束（留出余量以容忍个别缺页）
const MISS_TOLERANCE: usize = 6;

/// CDN 图片候选扩展名，按站点实际使用频率排序
const IMAGE_EXTS: [&str; 3] = ["jpg", "png", "webp"];

/// 本机常见代理端口（Clash / Mihomo 混合端口）
const LOCAL_PROXY_PORT: u16 = 7897;

/// 登录会话 cookie 的保存位置。检查更新扫描 download 目录时只认子目录，
/// 这个文件不会被误当成漫画。
fn cookie_file() -> PathBuf {
    Path::new("download").join(".zerobyw_cookies.json")
}

/// 解析访问站点与图片 CDN 用的代理。
///
/// 该站与其 CDN 在不少网络下直连会被限速到超时（实测约一半请求拿不到完整响应，
/// 剩余请求也慢到 20s 以上），走本机代理则稳定。
/// 优先级：`ZEROBYW_PROXY` 环境变量 > 本机 127.0.0.1:7897 有监听 > 直连；
/// 环境变量取值为 `off` / `none` / `direct` / `0` 时强制直连。
/// 直连时 reqwest 仍会自行读取 `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`。
fn resolve_proxy() -> Option<String> {
    if let Ok(raw) = std::env::var("ZEROBYW_PROXY") {
        let value = raw.trim();
        if matches!(
            value.to_ascii_lowercase().as_str(),
            "off" | "none" | "direct" | "0"
        ) {
            return None;
        }
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }

    let addr = format!("127.0.0.1:{LOCAL_PROXY_PORT}");
    let socket = addr.parse().ok()?;
    TcpStream::connect_timeout(&socket, StdDuration::from_millis(200)).ok()?;
    Some(format!("http://{addr}"))
}

/// 详情页里一次拿到的、后续拉图片要用的信息
#[derive(Clone, Debug, Default)]
struct MangaMeta {
    /// CDN 路径中的漫画代码，如 `t1a1ud8n1al`
    manga_code: String,
    /// 全部章节（含站点在页面上标为需登录 / VIP 的），按详情页顺序：zjid, zjname
    chapters: Vec<(String, String)>,
}

impl MangaMeta {
    /// 由 zjid 反查 CDN 目录名（zjname）
    fn chapter_dir(&self, zjid: &str) -> Option<&str> {
        self.chapters
            .iter()
            .find(|(id, _)| id == zjid)
            .map(|(_, dir)| dir.as_str())
    }

    fn is_empty(&self) -> bool {
        self.manga_code.is_empty() && self.chapters.is_empty()
    }
}

/// 详情页 `mangaDownloadChapters` 数组里的一项。zjid / zjname 正常是字符串，
/// 但站点偶尔会给出数字，用 Value 接住更稳
#[derive(Deserialize)]
struct RawChapter {
    zjid: Value,
    zjname: Value,
}

fn value_as_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 拼接 CDN 上某一页的地址。页码补零到 3 位（001.jpg），与站点页面里的写法一致
fn cdn_page_url(manga_code: &str, zjname: &str, page: u32, extension: &str) -> String {
    format!("{CDN_WEBSITE}/manhua/{manga_code}/{zjname}/{page:03}.{extension}")
}

/// 从图片直链推导落盘扩展名。
/// zerobyw 的 CDN 直链就是真实格式（.jpg/.png/.webp），认不出时按站点默认的 jpg。
fn image_extension_from_url(sample_page_url: Option<&str>) -> &'static str {
    const KNOWN: [&str; 6] = ["jpg", "jpeg", "png", "webp", "gif", "avif"];
    sample_page_url
        .and_then(url_extension)
        .and_then(|extension| KNOWN.into_iter().find(|known| *known == extension.as_str()))
        .unwrap_or("jpg")
}

/// 从链接里取扩展名（小写）。取不到或不像扩展名时返回 None
fn url_extension(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let extension = without_query.rsplit('.').next()?;
    if !extension.is_empty()
        && extension.len() <= 5
        && extension.chars().all(|c| c.is_ascii_alphabetic())
    {
        Some(extension.to_ascii_lowercase())
    } else {
        None
    }
}

pub struct ZerobywSource {
    base_website: String,
    client: Client,
    cookie_jar: Arc<Jar>,
    /// kuid -> 详情页解析结果，避免同一部漫画每一话都重拉一次详情页
    meta_cache: Mutex<HashMap<String, Arc<MangaMeta>>>,
}

impl ZerobywSource {
    pub fn new() -> Result<Self> {
        Self::with_base_website(BASE_WEBSITE)
    }

    pub fn with_base_website(base_website: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(REFERER, base_website.parse()?);
        let url = Url::parse(base_website)?;
        let cookie_jar = Arc::new(Jar::default());
        // 恢复上次保存的登录会话；站点域名变了旧 cookie 自然对不上
        if let Ok(text) = std::fs::read_to_string(cookie_file()) {
            if let Ok(pairs) = serde_json::from_str::<Vec<String>>(&text) {
                for pair in pairs {
                    cookie_jar.add_cookie_str(&pair, &url);
                }
            }
        }
        let mut builder = Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
            .danger_accept_invalid_certs(true)
            // 登录成功后站点下发的会话 cookie 要在后续章节请求里自动带上，
            // 没有 cookie store 登录就只对当次响应生效
            .cookie_provider(cookie_jar.clone())
            .default_headers(headers);
        if let Some(proxy) = resolve_proxy() {
            println!(
                "zerobyw：通过代理 {proxy} 访问站点与图片 CDN（可用环境变量 ZEROBYW_PROXY 覆盖，设为 off 表示直连）"
            );
            builder = builder.proxy(Proxy::all(proxy.as_str())?);
        }
        let client = builder.build()?;
        Ok(Self {
            base_website: base_website.trim_end_matches('/').to_string(),
            client,
            cookie_jar,
            meta_cache: Mutex::new(HashMap::new()),
        })
    }

    /// 带重试的 GET：网络错误或非 2xx 时按 delay 间隔重试。
    /// request 闭包每次重试时重建请求；返回 `Ok(None)` 表示等待期间被 Ctrl+C 取消。
    async fn get_with_retry(
        &self,
        request: impl Fn() -> reqwest::RequestBuilder,
        what: &str,
        retries: usize,
        delay: Duration,
        cancelled: &AtomicBool,
    ) -> Result<Option<String>> {
        for _ in 0..retries {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(None);
            }

            // select 包住在途请求：Ctrl+C 时立即丢弃请求并中止连接，
            // 而不是等当前请求自然超时才轮到标志检查
            let response = tokio::select! {
                _ = wait_cancelled(cancelled) => return Ok(None),
                res = request().send() => res,
            };

            match response {
                Ok(res) if res.status().is_success() => {
                    let text = tokio::select! {
                        _ = wait_cancelled(cancelled) => return Ok(None),
                        text = res.text() => text?,
                    };
                    return Ok(Some(text));
                }
                Ok(res) => {
                    println!("{what}请求失败，状态码: {}，正在重试...", res.status());
                }
                Err(e) => {
                    println!("{what}请求发生错误: {e}，正在重试...");
                }
            }

            sleep(delay).await;
        }
        Err(anyhow!("{what}失败：重试 {retries} 次后仍然没有成功响应"))
    }

    /// 缓存里取详情页元信息
    fn cached_meta(&self, kuid: &str) -> Option<Arc<MangaMeta>> {
        // 锁中毒只说明别的线程持锁时 panic 过，缓存内容本身仍然可用
        self.meta_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(kuid)
            .cloned()
    }

    fn store_meta(&self, kuid: &str, meta: Arc<MangaMeta>) {
        self.meta_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(kuid.to_string(), meta);
    }

    /// 拉取（并缓存）详情页元信息：CDN 漫画代码 + 全部章节。
    /// 解析不出东西时返回空 meta，由调用方回退到 Android 页面。
    async fn manga_meta(&self, kuid: &str, cancelled: &AtomicBool) -> Result<Arc<MangaMeta>> {
        if let Some(meta) = self.cached_meta(kuid) {
            return Ok(meta);
        }

        let url = format!("{}/pc/details/?kuid={kuid}", self.base_website);
        let Some(html) = self
            .get_with_retry(
                || self.client.get(&url),
                "漫画详情页",
                3,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Err(anyhow!("已取消"));
        };

        let meta = Arc::new(parse_manga_meta(&html).unwrap_or_default());
        // 空 meta 不缓存：站点偶发抖动时下次还能重新解析
        if !meta.is_empty() {
            self.store_meta(kuid, meta.clone());
        }
        Ok(meta)
    }

    /// HEAD 探测 CDN 上某个地址是否存在。
    /// `Some(true)` 存在，`Some(false)` 明确不存在（404 等），
    /// `None` 表示请求本身失败（网络抖动 / 超时），调用方据此区分对待。
    async fn page_exists(&self, url: &str, referer: &str, cancelled: &AtomicBool) -> Option<bool> {
        for _ in 0..2 {
            if cancelled.load(Ordering::SeqCst) {
                return None;
            }
            let response = tokio::select! {
                _ = wait_cancelled(cancelled) => return None,
                res = self.client.head(url).header(REFERER, referer).send() => res,
            };
            match response {
                Ok(res) => return Some(res.status().is_success()),
                Err(_) => continue,
            }
        }
        None
    }

    /// 阅读页不嵌图片（站点把锁做在页面上）时，按
    /// `{CDN}/manhua/{漫画代码}/{章节目录}/{页码}.{扩展名}` 直接探测 CDN。
    ///
    /// 图片本体不鉴权，所以站点标记为需登录 / VIP 的章节同样能拿全。
    /// 探测按页码顺序分批并发进行，连续 MISS_TOLERANCE 个页码不存在即认为章节结束。
    async fn probe_cdn_pages(
        &self,
        manga_code: &str,
        zjname: &str,
        referer: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<String>> {
        // 同一章节所有页格式一致，探第 1 页即可确定扩展名
        let mut extension = None;
        for candidate in IMAGE_EXTS {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(Vec::new());
            }
            let url = cdn_page_url(manga_code, zjname, 1, candidate);
            if self.page_exists(&url, referer, cancelled).await == Some(true) {
                extension = Some(candidate);
                break;
            }
        }
        let Some(extension) = extension else {
            // 站点偶尔会先放出章节、稍后才把图片同步到 CDN（常见于刚更新的 VIP 话），
            // 这种情况只能跳过，等站点补图后再下
            return Err(anyhow!(
                "CDN 上没有这一话的图片（第 1 页在 {IMAGE_EXTS:?} 下都不存在），站点可能还没上传"
            ));
        };

        let mut pages = Vec::new();
        let mut consecutive_miss = 0usize;
        let mut next_page = 1u32;

        while next_page <= MAX_CHAPTER_PAGES && consecutive_miss < MISS_TOLERANCE {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(Vec::new());
            }

            let batch: Vec<u32> =
                (next_page..(next_page + PROBE_BATCH).min(MAX_CHAPTER_PAGES + 1)).collect();
            let urls: Vec<String> = batch
                .iter()
                .map(|page| cdn_page_url(manga_code, zjname, *page, extension))
                .collect();
            let results = join_all(
                urls.iter()
                    .map(|url| self.page_exists(url, referer, cancelled)),
            )
            .await;

            for (offset, found) in results.into_iter().enumerate() {
                if found == Some(true) {
                    consecutive_miss = 0;
                    pages.push(urls[offset].clone());
                } else {
                    consecutive_miss += 1;
                }
            }

            next_page = batch.last().copied().unwrap_or(next_page) + 1;
        }

        if pages.is_empty() {
            return Err(anyhow!("CDN 没有返回该章节的任何图片"));
        }
        Ok(pages)
    }

    /// 用账号密码登录 zerobyw。成功后登录 cookie 已存进自带 client 的 cookie store，
    /// 之后搜索/章节/图片请求会自动带上。
    ///
    /// 下载章节并不需要登录（图片走 CDN 直链），这里只在站点策略变化时作为兜底。
    /// 站点登录是 Discuz 论坛表单：先 GET 登录页拿到服务端生成的
    /// formhash/loginhash，再原样 POST 回表单 action，缺了会被拒。
    pub async fn login(&self, username: &str, password: &str, cancelled: &AtomicBool) -> Result<()> {
        let login_url = format!(
            "{}/member.php?mod=logging&action=login",
            self.base_website
        );
        let Some(html) = self
            .get_with_retry(
                || self.client.get(&login_url),
                "登录页",
                3,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Err(anyhow!("登录已取消"));
        };

        let (action, formhash) = parse_login_form(&html)?;
        // action 是相对地址（member.php?...），页面 <base href> 指向站点根，直接拼根即可
        let post_url = format!("{}/{}", self.base_website, action);
        let referer = format!("{}/", self.base_website);
        let Some(text) = self
            .get_with_retry(
                || {
                    self.client.post(&post_url).header(REFERER, &referer).form(&[
                        ("formhash", formhash.as_str()),
                        ("referer", referer.as_str()),
                        ("loginfield", "username"),
                        ("username", username),
                        ("password", password),
                        ("questionid", "0"),
                        ("answer", ""),
                        // cookie 有效期 30 天（站点 cookietime 单位为秒）
                        ("cookietime", "2592000"),
                    ])
                },
                "登录",
                3,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Err(anyhow!("登录已取消"));
        };

        // 站点响应页内嵌 JS 变量 discuz_uid：登录成功为非 0 用户 ID，失败为 '0'
        let uid = Regex::new(r"discuz_uid\s*=\s*'(\d+)'")
            .unwrap()
            .captures(&text)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        match uid {
            Some(uid) if uid != "0" => {
                // 保存失败只影响下次启动要重新登录，不算登录失败
                if let Err(e) = self.save_cookies().await {
                    eprintln!("[!] 登录成功，但保存会话失败（下次启动需重新登录）：{e}");
                }
                Ok(())
            }
            _ => {
                let reason = login_message(&text)
                    .unwrap_or_else(|| "站点没有给出原因，请核对账号密码后重试".to_string());
                Err(anyhow!(reason))
            }
        }
    }

    /// 当前是否已有有效登录会话：带 cookie 请求登录页，
    /// 还渲染登录表单说明未登录（已登录时站点返回"您已登录"提示页，没有表单）。
    async fn session_valid(&self, cancelled: &AtomicBool) -> Result<bool> {
        let login_url = format!(
            "{}/member.php?mod=logging&action=login",
            self.base_website
        );
        let Some(html) = self
            .get_with_retry(
                || self.client.get(&login_url),
                "登录状态检查",
                3,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Err(anyhow!("已取消"));
        };
        let doc = Html::parse_document(&html);
        let form_sel = Selector::parse("form[action*='loginsubmit=yes']").unwrap();
        Ok(doc.select(&form_sel).next().is_none())
    }

    /// 把当前会话 cookie 落盘，下次启动自动恢复，免去重复登录。
    async fn save_cookies(&self) -> Result<()> {
        let url = Url::parse(&self.base_website)?;
        let Some(header) = self.cookie_jar.cookies(&url) else {
            return Err(anyhow!("cookie store 里没有会话"));
        };
        // CookieStore 给出的是 "name=value; name=value" 请求头形态，拆成单条保存
        let pairs: Vec<String> = header
            .to_str()
            .context("会话 cookie 头不是有效的 ASCII")?
            .split(';')
            .map(str::trim)
            .filter(|pair| !pair.is_empty())
            .map(str::to_string)
            .collect();
        if pairs.is_empty() {
            return Err(anyhow!("cookie store 里没有会话"));
        }
        create_dir_all("download")
            .await
            .context("创建 download 目录失败")?;
        write(cookie_file(), serde_json::to_string(&pairs)?)
            .await
            .with_context(|| format!("写入登录会话失败: {}", cookie_file().display()))?;
        Ok(())
    }

    /// 启动时的登录引导。
    ///
    /// 章节大纲与图片直链都不需要登录（含站点标记为需登录 / VIP 的章节），
    /// 所以这里只说明情况、默认直接开始；已保存的会话仍有效时静默恢复。
    /// Ctrl+C / stdin 关闭（EOF）时静默退出。
    pub async fn login_guide(&self, cancelled: &AtomicBool) {
        println!();
        match self.session_valid(cancelled).await {
            Ok(true) => {
                println!("已恢复上次保存的 zerobyw 登录会话");
                return;
            }
            Ok(false) => {
                // cookie 过期或无效：清掉旧文件，问用户要不要重新登录
                let _ = remove_file(cookie_file()).await;
            }
            Err(e) => {
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                eprintln!("[!] zerobyw 登录状态检查失败：{e}，按未登录继续");
            }
        }

        println!("zerobyw 的章节大纲与图片直链都不需要登录：站点在页面上标记为「需登录」或");
        println!("「VIP」的章节，图片同样由 CDN 直接提供，可直接开始下载。");
        println!("只有站点策略变化导致章节读不出来时，才需要用账号登录带上会话。");
        let Some(1) = input_number("1: 现在登录  0: 直接开始\n", cancelled).await else {
            return; // 跳过 / Ctrl+C / 输入结束
        };

        for _ in 0..3 {
            let Some(username) = input_line("请输入 zerobyw 账号：", cancelled).await else {
                return;
            };
            let Some(password) = input_line("请输入密码：", cancelled).await else {
                return;
            };

            println!("正在登录...");
            match self.login(&username, &password, cancelled).await {
                Ok(()) => {
                    println!("登录成功，后续请求会带上登录会话");
                    return;
                }
                Err(e) => {
                    if cancelled.load(Ordering::SeqCst) {
                        return;
                    }
                    println!("登录失败：{e}");
                    if input_number("1: 重试登录  0: 跳过\n", cancelled).await != Some(1) {
                        return;
                    }
                }
            }
        }
        println!("连续 3 次登录失败，已跳过登录");
    }

    /// 请求并解析搜索/列表页的漫画条目（供 search 与 find_by_name 复用）
    async fn fetch_search_results(
        &self,
        keyword: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ManGa_item>> {
        let url = format!("{}/Android/souo/", self.base_website);
        let Some(html) = self
            .get_with_retry(
                || self.client.get(&url).query(&[("keyword", keyword)]),
                "搜索",
                3,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Ok(Vec::new());
        };

        let doc = Html::parse_document(&html);
        let item_sel = Selector::parse(".manga-item a[href*='kuid=']").unwrap();
        let name_sel = Selector::parse(".manga-name").unwrap();
        let cover_sel = Selector::parse("img.cover-img").unwrap();

        Ok(doc
            .select(&item_sel)
            .filter_map(|a| {
                let kuid = extract_kuid(a.value().attr("href")?)?;
                let name = a
                    .select(&name_sel)
                    .next()?
                    .text()
                    .collect::<String>()
                    .trim()
                    .to_string();
                let cover = a
                    .select(&cover_sel)
                    .next()
                    .and_then(|img| img.value().attr("src"))
                    .unwrap_or_default()
                    .to_string();

                Some(ManGa_item {
                    name,
                    path_word: kuid,
                    cover,
                    author: Vec::new(),
                })
            })
            .collect())
    }
}

/// 从 `/Android/details/?kuid=22912` 这类链接里抠出 kuid
fn extract_kuid(href: &str) -> Option<String> {
    let idx = href.find("kuid=")?;
    let rest = &href[idx + "kuid=".len()..];
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num.is_empty() {
        None
    } else {
        Some(num)
    }
}

/// 解析详情页里的 `const mangaDownloadChapters = [...]` 与 CDN 漫画代码。
///
/// 这段内嵌数据是章节大纲最完整的来源：站点在 `/Android/` 页面上把需登录 /
/// VIP 章节的链接整个抹掉，而这里始终是全部章节。
fn parse_manga_meta(html: &str) -> Option<MangaMeta> {
    let chapters = extract_download_chapters(html);
    // CDN 路径形如 tupa.zerobyw33.com/manhua/{manga_code}/{zjname}/{页码}.{ext}
    let manga_code = Regex::new(r#"tupa\.zerobyw33\.com/manhua/([^/'\"\\\s]+)/"#)
        .unwrap()
        .captures(html)
        .map(|captures| captures[1].to_string())
        .unwrap_or_default();

    if chapters.is_empty() && manga_code.is_empty() {
        return None;
    }
    Some(MangaMeta {
        manga_code,
        chapters,
    })
}

/// 抠出 `mangaDownloadChapters` 数组并解析为 (zjid, zjname) 列表
fn extract_download_chapters(html: &str) -> Vec<(String, String)> {
    // 非贪婪匹配遇到数组内的 `];` 会截断，解析失败时再用贪婪匹配兜一次
    let patterns = [
        r"(?s)const\s+mangaDownloadChapters\s*=\s*(\[.*?\])\s*;",
        r"(?s)const\s+mangaDownloadChapters\s*=\s*(\[.*\])\s*;",
    ];

    for pattern in patterns {
        let Some(captures) = Regex::new(pattern).unwrap().captures(html) else {
            continue;
        };
        let Ok(raw) = serde_json::from_str::<Vec<RawChapter>>(&captures[1]) else {
            continue;
        };
        return raw
            .into_iter()
            .map(|chapter| {
                (
                    value_as_string(&chapter.zjid),
                    value_as_string(&chapter.zjname),
                )
            })
            .filter(|(zjid, zjname)| !zjid.is_empty() && !zjname.is_empty())
            .collect();
    }

    Vec::new()
}

/// 从阅读页 HTML 里抠出所有 CDN 图片直链（保持页面顺序，去重）。
/// 页面给的是协议相对地址（`//tupa...`），统一补成 https。
fn extract_reader_images(html: &str) -> Vec<String> {
    let pattern = r#"(?:https?:)?//tupa\.zerobyw33\.com/manhua/[^"'\s\\<>]+?\.(?:jpg|jpeg|png|webp|gif|avif)"#;
    let re = Regex::new(pattern).unwrap();

    let mut seen = HashSet::new();
    let mut pages = Vec::new();
    for found in re.find_iter(html) {
        let raw = found.as_str();
        let url = match raw.strip_prefix("//") {
            Some(rest) => format!("https://{rest}"),
            None => raw.to_string(),
        };
        if seen.insert(url.clone()) {
            pages.push(url);
        }
    }
    pages
}

/// 解析 Android 详情页的章节列表，返回 (可读章节, 需登录数, VIP 数)。
/// 只在 `/pc/details/` 改版、解析不出 `mangaDownloadChapters` 时作为兜底：
/// 这个页面上锁章节没有 zjid，因此兜底拿不到它们的章节号。
fn parse_chapter_list_android(html: &str) -> (Vec<ChapterContents>, usize, usize) {
    let doc = Html::parse_document(html);
    let item_sel = Selector::parse(".chapter-grid a.chapter-item").unwrap();
    let zjid_re = Regex::new(r"zjid=(\d+)").unwrap();

    let mut chapters = Vec::new();
    let mut login_locked = 0usize;
    let mut vip_locked = 0usize;
    for a in doc.select(&item_sel) {
        let zjid = a
            .value()
            .attr("href")
            .and_then(|href| zjid_re.captures(href))
            .map(|c| c[1].to_string());
        match zjid {
            Some(zjid) => {
                let name = a.text().collect::<String>().trim().to_string();
                chapters.push(ChapterContents {
                    chapter_name: name,
                    chapter_uuid: zjid,
                    len: 0,
                    pages_url: Vec::new(),
                });
            }
            None => {
                let is_vip = a
                    .value()
                    .attr("onclick")
                    .map(|onclick| onclick.contains("'vip'"))
                    .unwrap_or(false);
                if is_vip {
                    vip_locked += 1;
                } else {
                    login_locked += 1;
                }
            }
        }
    }
    (chapters, login_locked, vip_locked)
}

/// 从登录页 HTML 里解析登录表单的提交地址（相对根目录）与 formhash。
/// Discuz 每次刷新页面都会换一组值，必须先 GET 再原样 POST 回去。
fn parse_login_form(html: &str) -> Result<(String, String)> {
    let doc = Html::parse_document(html);
    let form_sel = Selector::parse("form[action*='loginsubmit=yes']").unwrap();
    let form = doc
        .select(&form_sel)
        .next()
        .ok_or_else(|| anyhow!("登录页里没有找到登录表单，站点结构可能已改版"))?;
    let action = form
        .value()
        .attr("action")
        .ok_or_else(|| anyhow!("登录表单缺少 action 地址"))?
        .trim_start_matches('/')
        .to_string();
    // 本站登录表单内没有 formhash hidden input（页面里另一个 formhash 属于找回密码
    // 表单），formhash 拼在登录表单 action 的查询串里；hidden input 仅作老主题后备
    let formhash = Regex::new(r"formhash=([a-f0-9]+)")
        .unwrap()
        .captures(&action)
        .map(|c| c[1].to_string())
        .or_else(|| {
            form.select(&Selector::parse("input[name='formhash']").unwrap())
                .next()
                .and_then(|input| input.value().attr("value"))
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow!("登录表单缺少 formhash，无法提交"))?;
    Ok((action, formhash))
}

/// 从站点登录响应里抠出人话提示（成功欢迎语或失败原因），抠不到返回 None。
fn login_message(text: &str) -> Option<String> {
    // Discuz 提示页的两种形态：弹窗 div、ajax 回调注入的 JS 字符串
    let patterns = [
        r#"<div class="alert_[a-z]+">([^<]*)"#,
        r#"showError\('([^']*)'"#,
        r#"succeedlocation'\)\.innerHTML = '([^']*)'"#,
    ];
    for pattern in patterns {
        let Some(caps) = Regex::new(pattern).unwrap().captures(text) else {
            continue;
        };
        let msg = caps[1].trim().to_string();
        if !msg.is_empty() {
            return Some(msg);
        }
    }
    None
}

#[async_trait]
impl MangaSource for ZerobywSource {
    fn id(&self) -> &'static str {
        "zerobyw"
    }

    async fn on_selected(&self, cancelled: &AtomicBool) {
        self.login_guide(cancelled).await;
    }

    fn http(&self) -> &Client {
        &self.client
    }

    /// zerobyw 的 CDN 直链就是真实格式（.jpg/.png/.webp），按后缀落盘；
    /// 沿用基类默认的 `.webp` 会让这些 jpg 在看图软件里认不出来
    fn image_extension(&self, sample_page_url: Option<&str>) -> &'static str {
        image_extension_from_url(sample_page_url)
    }

    async fn search(&self, keyword: &str, cancelled: &AtomicBool) -> Result<Vec<ManGa_item>> {
        let list = self.fetch_search_results(keyword, cancelled).await?;

        println!("以下为搜索结果：");
        for (index, item) in list.iter().enumerate() {
            println!("{}.{}", index, item.name);
        }

        Ok(list)
    }

    async fn fetch_chapters(
        &self,
        manga_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ChapterContents>> {
        let meta = self.manga_meta(manga_id, cancelled).await?;
        if !meta.chapters.is_empty() {
            // 详情页的 mangaDownloadChapters 含站点标记为需登录 / VIP 的全部章节，
            // 这些章节的图片同样能从 CDN 取到，因此不再按锁定状态过滤
            return Ok(meta
                .chapters
                .iter()
                .map(|(zjid, zjname)| ChapterContents {
                    chapter_name: zjname.clone(),
                    chapter_uuid: zjid.clone(),
                    len: 0,
                    pages_url: Vec::new(),
                })
                .collect());
        }

        // 详情页改版时退回 Android 页面：只能列出未上锁章节，至少还能下载
        eprintln!(
            "[!] zerobyw：详情页里没有解析到 mangaDownloadChapters，退回 Android 页面解析（上锁章节会缺失）"
        );
        let url = format!("{}/Android/details/?kuid={manga_id}", self.base_website);
        let Some(html) = self
            .get_with_retry(
                || self.client.get(&url),
                "漫画详情页",
                3,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Ok(Vec::new());
        };

        let (chapters, login_locked, vip_locked) = parse_chapter_list_android(&html);
        if login_locked + vip_locked > 0 {
            eprintln!(
                "[!] zerobyw：Android 页面上还有 {} 个上锁章节没能列出",
                login_locked + vip_locked
            );
        }
        Ok(chapters)
    }

    async fn fetch_pages(
        &self,
        manga_id: &str,
        chapter_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<String>> {
        // 阅读页只靠 zjid 定位，但上锁章节要靠详情页里的漫画代码 + 章节目录名探测 CDN
        let meta = self.manga_meta(manga_id, cancelled).await?;
        let url = format!("{}/pc/view/index.php?zjid={chapter_id}", self.base_website);
        let referer = format!("{}/pc/details/", self.base_website);
        let Some(html) = self
            .get_with_retry(
                || self.client.get(&url).header(REFERER, &referer),
                "章节阅读页",
                3,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Ok(Vec::new());
        };

        let pages = extract_reader_images(&html);
        if !pages.is_empty() {
            return Ok(pages);
        }

        // 阅读页没有图片 = 站点在这一话做了付费墙（需登录 / VIP）。
        // 图片本体在 CDN 上并不鉴权，直接按漫画代码 + 章节目录名探测即可
        let Some(zjname) = meta.chapter_dir(chapter_id) else {
            return Err(anyhow!(
                "阅读页没有图片，详情页里也没有这一话的 CDN 目录名，无法在 CDN 上探测（章节可能已被删除）"
            ));
        };
        if meta.manga_code.is_empty() {
            return Err(anyhow!(
                "阅读页没有图片，详情页里也没有解析到 CDN 漫画代码，无法在 CDN 上探测"
            ));
        }

        self.probe_cdn_pages(&meta.manga_code, zjname, &referer, cancelled)
            .await
    }

    async fn find_by_name(&self, name: &str, cancelled: &AtomicBool) -> Result<Option<String>> {
        let list = self.fetch_search_results(name, cancelled).await?;
        Ok(list
            .into_iter()
            .find(|item| item.name == name)
            .map(|item| item.path_word))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_kuid_from_details_link() {
        assert_eq!(
            extract_kuid("/Android/details/?kuid=22912"),
            Some("22912".to_string())
        );
        assert_eq!(extract_kuid("/Android/details/?kuid=abc"), None);
        assert_eq!(extract_kuid("/Android/view/?zjid=1"), None);
    }

    #[test]
    fn parses_manga_meta_with_all_chapters() {
        // 真实详情页的形态：mangaDownloadChapters 是全部章节（含站点上锁的），
        // CDN 漫画代码来自封面图地址
        let html = r#"
            <img src="http://tupa.zerobyw33.com/manhua/t1a1ud8n1al/20/001.jpg" alt="封面">
            <script>
                const mangaDownloadChapters = [{"zjid":"707652","zjname":"1"},{"zjid":"707682","zjname":"601"}];
                window.app = { kuid: 22912, formhash: '81d8c08d', isGuest: true, canDownload: false };
            </script>
        "#;
        let meta = parse_manga_meta(html).unwrap();
        assert_eq!(meta.manga_code, "t1a1ud8n1al");
        assert_eq!(
            meta.chapters,
            vec![
                ("707652".to_string(), "1".to_string()),
                ("707682".to_string(), "601".to_string()),
            ]
        );
        // zjid -> CDN 目录名 的反查
        assert_eq!(meta.chapter_dir("707682"), Some("601"));
        assert_eq!(meta.chapter_dir("1"), None);
    }

    #[test]
    fn parses_manga_meta_tolerates_numeric_fields() {
        // 站点偶尔把 zjid/zjname 写成数字而不是字符串
        let html = r#"const mangaDownloadChapters = [{"zjid":707652,"zjname":1}];"#;
        let meta = parse_manga_meta(html).unwrap();
        assert_eq!(meta.chapters, vec![("707652".to_string(), "1".to_string())]);
    }

    #[test]
    fn parses_manga_meta_rejects_unrelated_page() {
        assert!(parse_manga_meta("<html><body>404</body></html>").is_none());
    }

    #[test]
    fn extracts_reader_images_in_order_and_deduplicated() {
        let html = r#"
            <img src="//tupa.zerobyw33.com/manhua/t1a1ud8n1al/1/001.jpg" class="manga-image">
            <img src="https://tupa.zerobyw33.com/manhua/t1a1ud8n1al/1/002.jpg" class="manga-image">
            <img src="//tupa.zerobyw33.com/manhua/t1a1ud8n1al/1/002.jpg" class="manga-image">
            <img src="/static/icon.png">
        "#;
        assert_eq!(
            extract_reader_images(html),
            vec![
                "https://tupa.zerobyw33.com/manhua/t1a1ud8n1al/1/001.jpg".to_string(),
                "https://tupa.zerobyw33.com/manhua/t1a1ud8n1al/1/002.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn extracts_reader_images_returns_empty_on_paywall_page() {
        // 上锁章节的阅读页只有锁提示，没有 manga-image
        let html = r#"<div>本话需要 VIP 才能阅读</div><img src="/static/lock.png">"#;
        assert!(extract_reader_images(html).is_empty());
    }

    #[test]
    fn builds_cdn_page_urls_with_zero_padded_page() {
        assert_eq!(
            cdn_page_url("t1a1ud8n1al", "601", 1, "jpg"),
            "https://tupa.zerobyw33.com/manhua/t1a1ud8n1al/601/001.jpg"
        );
        assert_eq!(
            cdn_page_url("t1a1ud8n1al", "601", 168, "png"),
            "https://tupa.zerobyw33.com/manhua/t1a1ud8n1al/601/168.png"
        );
        // 超过 3 位不截断
        assert_eq!(
            cdn_page_url("code", "1", 1000, "webp"),
            "https://tupa.zerobyw33.com/manhua/code/1/1000.webp"
        );
    }

    #[test]
    fn reads_extension_from_page_url() {
        assert_eq!(
            url_extension("https://tupa.zerobyw33.com/manhua/c/1/001.jpg"),
            Some("jpg".to_string())
        );
        assert_eq!(
            url_extension("https://tupa.zerobyw33.com/manhua/c/1/001.JPG?x=1"),
            Some("jpg".to_string())
        );
        assert_eq!(
            url_extension("https://s0.mangafunb.fun/1/x/e870c/1748014689320025.png.c1500x.jpg"),
            Some("jpg".to_string())
        );
        assert_eq!(url_extension("https://example.com/no-extension"), None);
    }

    #[test]
    fn parses_android_chapter_list_and_lock_reasons() {
        let html = r#"
            <div class="chapter-grid">
                <a class="chapter-item" href="/Android/view/?zjid=111">第1话</a>
                <a class="chapter-item" href="javascript:;" onclick="checkAuth(&#039;login&#039;)">第2话</a>
                <a class="chapter-item" href="javascript:;" onclick="checkAuth(&#039;vip&#039;)">第3话</a>
            </div>
        "#;
        let (chapters, login_locked, vip_locked) = parse_chapter_list_android(html);
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].chapter_uuid, "111");
        assert_eq!(chapters[0].chapter_name, "第1话");
        // onclick 里的 HTML 实体（&#039;）应被解码成 'vip' / 'login' 再判断
        assert_eq!(login_locked, 1);
        assert_eq!(vip_locked, 1);
    }

    #[test]
    fn parses_login_form_action_and_formhash() {
        let html = r#"
            <form method="post" name="login" id="loginform_LXJnY"
                action="member.php?mod=logging&amp;action=login&amp;loginsubmit=yes&amp;formhash=7826727e&amp;loginhash=LXJnY">
                <input type="hidden" name="referer" value="https://www.zerobyw33.com/./" />
                <input type="text" name="username" value="" />
                <input type="password" name="password" />
            </form>
            <form action="member.php?mod=lostpasswd&amp;lostpwsubmit=yes&amp;infloat=yes">
                <input type="hidden" name="formhash" value="deadbeef" />
            </form>
        "#;
        let (action, formhash) = parse_login_form(html).unwrap();
        // action 里的 &amp; 实体应被解码成 &，匹配到的是登录表单而非找回密码表单；
        // formhash 取自登录表单 action 查询串，而不是找回密码表单里的 hidden input
        assert_eq!(
            action,
            "member.php?mod=logging&action=login&loginsubmit=yes&formhash=7826727e&loginhash=LXJnY"
        );
        assert_eq!(formhash, "7826727e");
    }

    #[test]
    fn parses_login_form_falls_back_to_hidden_input() {
        // 老版 Discuz 主题把 formhash 放在表单 hidden input 里，action 不带
        let html = r#"
            <form action="member.php?mod=logging&amp;action=login&amp;loginsubmit=yes&amp;loginhash=LXJnY">
                <input type="hidden" name="formhash" value="7826727e" />
            </form>
        "#;
        let (_, formhash) = parse_login_form(html).unwrap();
        assert_eq!(formhash, "7826727e");
    }

    #[test]
    fn parses_login_form_rejects_page_without_form() {
        assert!(parse_login_form("<html><body>404</body></html>").is_err());
    }

    #[test]
    fn extracts_login_message_from_response() {
        let success = "succeedlocation').innerHTML = '欢迎您回来，新手上路 某用户，现在将转入登录前页面';";
        assert!(login_message(success).unwrap().contains("欢迎您回来"));
        assert_eq!(
            login_message(r#"<div class="alert_error">密码错误次数过多</div>"#),
            Some("密码错误次数过多".to_string())
        );
        assert_eq!(login_message("no message here"), None);
    }

    #[test]
    fn zerobyw_keeps_real_image_extension() {
        // CDN 直链就是真实 jpg，落盘成 .webp 会让看图软件认不出
        assert_eq!(
            image_extension_from_url(Some("https://tupa.zerobyw33.com/manhua/c/1/001.jpg")),
            "jpg"
        );
        assert_eq!(
            image_extension_from_url(Some("https://tupa.zerobyw33.com/manhua/c/1/001.png")),
            "png"
        );
        // 直链没有可识别后缀时按站点默认的 jpg
        assert_eq!(image_extension_from_url(Some("https://example.com/page")), "jpg");
        assert_eq!(image_extension_from_url(None), "jpg");
    }
}
