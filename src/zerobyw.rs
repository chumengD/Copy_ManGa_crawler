//! zerobyw（zero搬运网）站点适配器。
//!
//! 纯 HTML 站点，无加密、无 JSON 接口：
//! - 搜索：`GET /Android/souo/?keyword=...`
//! - 详情页：`/Android/details/?kuid=N`，章节列表为 `.chapter-grid a.chapter-item`
//! - 阅读页：`/Android/view/?zjid=N`，图片直链为 `img.manga-img` 的协议相对地址
//! - 登录：论坛式表单 `member.php?mod=logging&action=login`，需先取页面生成的
//!   formhash/loginhash 再 POST；成功后站点下发域级 cookie，`/Android/` 章节页据此解锁
//!
//! 锁定章节的 a 标签没有 zjid，按 onclick 里 checkAuth 的参数区分原因：
//! `login` 是登录可解锁，`vip` 是站点权限（登录也不解锁），都只提示不抓取。
//! 登录会话 cookie 持久化在 `download/.zerobyw_cookies.json`，下次启动自动恢复。

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use regex::Regex;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::{HeaderMap, REFERER};
use reqwest::{Client, Url};
use scraper::{Html, Selector};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::fs::{create_dir_all, remove_file, write};
use tokio::time::{sleep, Duration};

use crate::input_line;
use crate::input_number;
use crate::source::MangaSource;
use crate::types::{ChapterContents, ManGa_item};

/// 站点地址直接硬编码。该站域名尾部的数字会不定期变化，失效时改这里
pub const BASE_WEBSITE: &str = "https://www.zerobyw33.com";

/// 登录会话 cookie 的保存位置。检查更新扫描 download 目录时只认子目录，
/// 这个文件不会被误当成漫画。
fn cookie_file() -> PathBuf {
    Path::new("download").join(".zerobyw_cookies.json")
}

pub struct ZerobywSource {
    base_website: String,
    client: Client,
    cookie_jar: Arc<Jar>,
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
        // 恢复上次保存的登录会话；站点域名变了旧 cookie 自然对不上，
        // 引导里会检测到会话失效并重新登录
        if let Ok(text) = std::fs::read_to_string(cookie_file()) {
            if let Ok(pairs) = serde_json::from_str::<Vec<String>>(&text) {
                for pair in pairs {
                    cookie_jar.add_cookie_str(&pair, &url);
                }
            }
        }
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
            .danger_accept_invalid_certs(true)
            // 登录成功后站点下发的会话 cookie 要在后续章节请求里自动带上，
            // 没有cookie store 登录就只对当次响应生效
            .cookie_provider(cookie_jar.clone())
            .default_headers(headers)
            .build()?;
        Ok(Self {
            base_website: base_website.trim_end_matches('/').to_string(),
            client,
            cookie_jar,
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

            match request().send().await {
                Ok(res) if res.status().is_success() => return Ok(Some(res.text().await?)),
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

    /// 用账号密码登录 zerobyw。成功后登录 cookie 已存进自带 client 的 cookie store，
    /// 之后搜索/章节/图片请求会自动带上，上锁章节随之解锁。
    ///
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

    /// 启动时的登录引导：已保存的会话仍有效则直接恢复、不再打扰；
    /// 否则说明未登录的限制，引导用户输入账号密码登录，失败可重试，
    /// 也可一路跳过保持未登录状态。Ctrl+C / stdin 关闭（EOF）时静默退出。
    pub async fn login_guide(&self, cancelled: &AtomicBool) {
        println!();
        match self.session_valid(cancelled).await {
            Ok(true) => {
                println!("已恢复上次保存的 zerobyw 登录会话，全部章节均可下载");
                return;
            }
            Ok(false) => {
                // cookie 过期或无效：清掉旧文件，按未登录走引导
                let _ = remove_file(cookie_file()).await;
            }
            Err(e) => {
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                eprintln!("[!] zerobyw 登录状态检查失败：{e}，按未登录继续");
            }
        }

        println!("zerobyw 未登录时大部分章节上锁，每部漫画只有前几话可下载；");
        println!("用 zerobyw 账号登录（可在该站注册）后即可下载全部章节。");
        let Some(1) = input_number("1: 现在登录  0: 暂不登录\n", cancelled).await else {
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
                    println!("登录成功，zerobyw 全部章节均可下载");
                    return;
                }
                Err(e) => {
                    if cancelled.load(Ordering::SeqCst) {
                        return;
                    }
                    println!("登录失败：{e}");
                    if input_number("1: 重试登录  0: 跳过（仅能下载未上锁章节）\n", cancelled)
                        .await
                        != Some(1)
                    {
                        return;
                    }
                }
            }
        }
        println!("连续 3 次登录失败，已跳过登录（仅能下载未上锁章节）");
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

/// 解析详情页的章节列表，返回 (可读章节, 需登录数, VIP 数)。
/// 锁定章节的 a 标签没有 zjid，按 onclick 里 checkAuth 的参数区分锁定原因：
/// `login` 登录即可解锁，`vip` 是站点权限（登录也不解锁）。
fn parse_chapter_list(html: &str) -> (Vec<ChapterContents>, usize, usize) {
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

        let (chapters, login_locked, vip_locked) = parse_chapter_list(&html);
        if login_locked > 0 {
            eprintln!(
                "[!] zerobyw：有 {login_locked} 个章节需登录才能阅读，已跳过（可在启动时的登录引导里登录解锁）"
            );
        }
        if vip_locked > 0 {
            eprintln!(
                "[!] zerobyw：有 {vip_locked} 个 VIP 章节为站点权限限制（登录也不可读），已跳过"
            );
        }

        Ok(chapters)
    }

    async fn fetch_pages(
        &self,
        _manga_id: &str,
        chapter_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<String>> {
        // 阅读页只靠 zjid 定位，不需要漫画 ID
        let url = format!("{}/Android/view/?zjid={chapter_id}", self.base_website);
        let Some(html) = self
            .get_with_retry(
                || self.client.get(&url),
                "章节阅读页",
                3,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Ok(Vec::new());
        };

        let doc = Html::parse_document(&html);
        let img_sel = Selector::parse("img.manga-img").unwrap();
        let pages_url: Vec<String> = doc
            .select(&img_sel)
            .filter_map(|img| {
                // 部分页面图片走懒加载，src 缺失时退回 data-src
                let src = img
                    .value()
                    .attr("src")
                    .filter(|src| !src.trim().is_empty())
                    .or_else(|| img.value().attr("data-src"))?;
                Some(if let Some(rest) = src.strip_prefix("//") {
                    format!("https://{rest}")
                } else {
                    src.to_string()
                })
            })
            .collect();

        if pages_url.is_empty() {
            return Err(anyhow!(
                "阅读页中没有解析到图片（该章节可能需要登录后才能阅读）"
            ));
        }

        Ok(pages_url)
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
    fn parses_chapter_list_and_lock_reasons() {
        let html = r#"
            <div class="chapter-grid">
                <a class="chapter-item" href="/Android/view/?zjid=111">第1话</a>
                <a class="chapter-item" href="javascript:;" onclick="checkAuth(&#039;login&#039;)">第2话</a>
                <a class="chapter-item" href="javascript:;" onclick="checkAuth(&#039;vip&#039;)">第3话</a>
            </div>
        "#;
        let (chapters, login_locked, vip_locked) = parse_chapter_list(html);
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].chapter_uuid, "111");
        assert_eq!(chapters[0].chapter_name, "第1话");
        // onclick 里的 HTML 实体（&#039;）应被解码成 'vip' / 'login' 再判断
        assert_eq!(login_locked, 1);
        assert_eq!(vip_locked, 1);
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
}
