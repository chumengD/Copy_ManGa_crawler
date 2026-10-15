//! 拷贝漫画（copymanga）站点适配器。
//!
//! 搜索走独立 API 域名上的 v3 明文 JSON 接口，且必须携带 `version` / `platform`
//! 请求头（官方客户端版本门槛），否则返回 200 但列表为空；章节大纲、图片直链等
//! 接口仍走网页主站，返回 AES-CBC 密文：
//! 1. 从页面 HTML 中用正则提取密钥（如 `ccz` / `cct`）与 token
//! 2. 调用详情接口拿到 `results` 密文字段
//! 3. `decrypt_results`：密文前 16 字符作 IV，其余 hex 解码后 AES-128-CBC + PKCS7 解密

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, REFERER};
use reqwest::Client;
use serde_json::Value;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::time::{sleep, timeout, Duration};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use regex::Regex;

use crate::source::MangaSource;
use crate::types::{ChapterContents, ManGa_item, Response};
use crate::wait_cancelled;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// 站点地址直接硬编码，不再依赖外部 config.toml
pub const BASE_WEBSITE: &str = "https://ios.copymanga.club";

/// 搜索接口的 API 域名，与网页主站分离。
/// 官方会不定期更换该域名（命名含年月，如 202601 = 2026-01），失效时可到
/// github.com/copy-manga/copymanga 发布页或 lanyeeee/copymanga-downloader 查最新地址
pub const API_BASE_WEBSITE: &str = "https://api.copy202601.com";

/// v3 接口的客户端版本门槛，两个头缺一不可，缺失时接口返回 200 但列表为空
const SEARCH_API_HEADERS: [(&str, &str); 2] = [("version", "2025.08.15"), ("platform", "1")];

/// 相邻网络请求之间的最小间隔，避免请求过快触发站点限流（Too Many Requests）
const REQUEST_DELAY: Duration = Duration::from_millis(1000);

pub struct CopyMangaSource {
    base_website: String,
    client: Client,
    /// 链路测速得出的地址族偏好：true = IPv6 优先。解析器与测速逻辑共享，
    /// 测速完成后立即生效（见 prepare_for_download）
    prefer_ipv6: Arc<AtomicBool>,
}

impl CopyMangaSource {
    pub fn new(base_website: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(REFERER, base_website.parse()?);
        // 初始偏好 IPv6：Cloudflare 的 v6 入口普遍没有 v4 拥挤；
        // 每次批量下载前会实测两条链路的吞吐并纠正（见 prepare_for_download）
        let prefer_ipv6 = Arc::new(AtomicBool::new(true));
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
            .danger_accept_invalid_certs(true)
            // 图片 CDN（mangafuna/mangafunb 系）挂在 Cloudflare 后面，IPv4/IPv6 是
            // 两条独立链路。hyper 只按解析顺序连第一个能连上的地址、不做传输测速，
            // 所以由解析器按测速结果决定顺序，见 FamilyOrderResolver
            .dns_resolver(Arc::new(FamilyOrderResolver {
                prefer_ipv6: prefer_ipv6.clone(),
            }))
            // 连接黑洞时限制单次卡死时长；正文流停滞时快速报错进入重试，
            // 而不是静默挂满外层的单页 60s 预算
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .default_headers(headers)
            .build()?;
        Ok(Self {
            base_website: base_website.trim_end_matches('/').to_string(),
            client,
            prefer_ipv6,
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
            // 而不是等当前请求自然超时（read_timeout 最长 30s）才轮到标志检查
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

    /// 获取解密后的章节大纲 JSON payload。
    /// 返回 `Ok(None)` 表示等待请求期间被取消。
    async fn fetch_chapter_payload(
        &self,
        manga_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Option<Value>> {
        let page_url = format!("{}/comic/{manga_id}", self.base_website);

        let Some(html) = self
            .get_with_retry(
                || self.client.get(&page_url),
                "漫画详情页",
                5,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Ok(None);
        };
        let (key, dnts) = extract_page_secrets(&html)?;

        // 连续请求详情页与章节接口之间歇一下
        sleep(REQUEST_DELAY).await;

        let api_url = format!("{}/comicdetail/{manga_id}/chapters", self.base_website);
        let Some(body) = self
            .get_with_retry(
                || {
                    self.client
                        .get(&api_url)
                        .header("dnts", &dnts)
                        .header("Referer", &page_url)
                },
                "章节接口",
                5,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Ok(None);
        };

        let body_json: Value = serde_json::from_str(&body)
            .with_context(|| format!("章节接口返回不是合法 JSON: {body}"))?;
        if body_json.get("code").and_then(Value::as_i64) != Some(200) {
            return Err(anyhow!("章节接口返回异常: {body_json}"));
        }

        let results = body_json
            .get("results")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("章节接口返回缺少 results 字段"))?;
        let payload = decrypt_results(results, &key)?;

        let total = payload["groups"]
            .as_object()
            .map(|groups| {
                groups
                    .values()
                    .filter_map(|group| group["chapters"].as_array())
                    .map(|chapters| chapters.len())
                    .sum()
            })
            .unwrap_or(0);
        if total == 0 {
            eprintln!(
                "[!] 接口正常但返回空章节列表 —— 当前出口 IP 大概率被站点软限流，可稍后重试或更换出口 IP。"
            );
        }

        Ok(Some(payload))
    }
}

/// DNS 解析器：按测速共享状态决定 IPv4/IPv6 谁排在解析结果前面。
///
/// hyper 连接时按解析顺序选第一个能连上的地址，且只比"谁先连上"不比"谁传得快"；
/// 两条链路通常都能秒连，快慢只能靠传输测速分辨（见 prepare_for_download）。
/// 落选的地址族不会丢：hyper 自带 happy-eyeballs，首选族 300ms 内没连上时
/// 备选族会并行开连自动兜底（对没有 IPv6 的用户这层兜底就是正常路径）。
struct FamilyOrderResolver {
    prefer_ipv6: Arc<AtomicBool>,
}

impl Resolve for FamilyOrderResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let prefer_ipv6 = self.prefer_ipv6.load(Ordering::Relaxed);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            Ok(Box::new(order_addrs(addrs, prefer_ipv6).into_iter()) as Addrs)
        })
    }
}

/// 把 IPv6 / IPv4 分组排序，prefer_ipv6 决定谁在前（各自保持原有相对顺序）
fn order_addrs(addrs: Vec<SocketAddr>, prefer_ipv6: bool) -> Vec<SocketAddr> {
    let (v6, v4): (Vec<SocketAddr>, Vec<SocketAddr>) =
        addrs.into_iter().partition(|addr| addr.is_ipv6());
    if prefer_ipv6 {
        v6.into_iter().chain(v4).collect()
    } else {
        v4.into_iter().chain(v6).collect()
    }
}

/// 链路测速的单次下载窗口：窗口内能拉到多少字节算多少（窗口内拉完按实际耗时算）。
/// 几秒足够让健康链路（秒级下完一张图）和劣化链路（几十 KB/s）拉开数量级差距。
const PROBE_WINDOW: Duration = Duration::from_secs(6);

/// 用指定地址族（"0.0.0.0" 仅 IPv4，"::" 仅 IPv6）下载样本图，返回实测吞吐（字节/秒）。
/// 地址族不可用（没有 IPv6 路由等）会立刻失败，由调用方按 0 分处理。
async fn measure_family_speed(base_website: &str, sample_url: &str, bind: &str) -> Result<f64> {
    let mut headers = HeaderMap::new();
    headers.insert(REFERER, base_website.parse()?);
    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
        .danger_accept_invalid_certs(true)
        .default_headers(headers)
        .local_address(bind.parse::<IpAddr>()?)
        .connect_timeout(Duration::from_secs(5))
        .build()?;

    let started = Instant::now();
    let response = client.get(sample_url).send().await?;
    let mut stream = response.bytes_stream();
    let received = Arc::new(AtomicU64::new(0));
    let counter = received.clone();
    // 超时把 future 连同流一起丢弃，已收字数保存在共享计数器里
    let _ = timeout(PROBE_WINDOW, async move {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    counter.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                }
                Err(_) => break,
            }
        }
    })
    .await;

    let elapsed = started.elapsed().as_secs_f64().max(0.05);
    Ok(received.load(Ordering::Relaxed) as f64 / elapsed)
}

/// v4 领先不足该倍数视为持平（保持 v6 优先），避免两条链路都健康时被噪声翻转
const PROBE_TIE_FACTOR: f64 = 1.3;

/// 比较两条链路的实测吞吐，返回是否应优先 IPv6
fn should_prefer_ipv6(v4_speed: f64, v6_speed: f64) -> bool {
    if v6_speed >= v4_speed {
        true
    } else {
        v4_speed < v6_speed * PROBE_TIE_FACTOR
    }
}

/// 把字节/秒格式化成可读速度
fn format_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.0 {
        "不可用".to_string()
    } else if bytes_per_sec < 1024.0 {
        format!("{:.0} 字节/秒", bytes_per_sec)
    } else if bytes_per_sec < 1024.0 * 1024.0 {
        format!("{:.0} KB/s", bytes_per_sec / 1024.0)
    } else {
        format!("{:.1} MB/s", bytes_per_sec / 1024.0 / 1024.0)
    }
}

#[async_trait]
impl MangaSource for CopyMangaSource {
    fn id(&self) -> &'static str {
        "copymanga"
    }

    fn http(&self) -> &Client {
        &self.client
    }

    /// 下载前的链路测速：拿第一话的真实图片直链，对 IPv4 / IPv6 并发各拉一次，
    /// 按实测吞吐决定本次下载走哪条链路（写进共享偏好，解析器立即生效）。
    /// 测速是尽力而为：全失败或被取消时保持现有偏好，不影响下载。
    async fn prepare_for_download(&self, sample_page_url: Option<&str>, cancelled: &AtomicBool) {
        let Some(url) = sample_page_url else {
            return;
        };
        if cancelled.load(Ordering::SeqCst) {
            return;
        }

        println!(
            "正在给图片 CDN 测速：IPv4 / IPv6 并发各下载一页样本（最多 {} 秒）",
            PROBE_WINDOW.as_secs()
        );
        let (v4_measured, v6_measured) = tokio::join!(
            measure_family_speed(&self.base_website, url, "0.0.0.0"),
            measure_family_speed(&self.base_website, url, "::"),
        );
        if cancelled.load(Ordering::SeqCst) {
            return;
        }
        let v4_speed = v4_measured.unwrap_or(0.0);
        let v6_speed = v6_measured.unwrap_or(0.0);

        // 两条链路都没拿到样本（样本图失效、站点不可达等）：保持现状不折腾
        if v4_speed <= 0.0 && v6_speed <= 0.0 {
            println!("两条链路都没能下载样本图，跳过测速，保持 IPv6 优先");
            return;
        }

        let prefer_ipv6 = should_prefer_ipv6(v4_speed, v6_speed);
        self.prefer_ipv6.store(prefer_ipv6, Ordering::Relaxed);
        println!(
            "测速结果：IPv4 {} / IPv6 {}，本次下载优先走 {}",
            format_speed(v4_speed),
            format_speed(v6_speed),
            if prefer_ipv6 { "IPv6" } else { "IPv4" }
        );
    }

    async fn search(&self, keyword: &str, cancelled: &AtomicBool) -> Result<Vec<ManGa_item>> {
        let base_url = format!("{API_BASE_WEBSITE}/api/v3/search/comic");
        let params = [
            ("offset", "0"),
            ("platform", "1"),
            ("limit", "12"),
            ("q", keyword),
            ("q_type", ""),
        ];

        let Some(resp_text) = self
            .get_with_retry(
                || {
                    let mut request = self.client.get(&base_url).query(&params);
                    for (name, value) in SEARCH_API_HEADERS {
                        request = request.header(name, value);
                    }
                    request
                },
                "搜索",
                5,
                REQUEST_DELAY,
                cancelled,
            )
            .await?
        else {
            return Ok(Vec::new());
        };

        let resp_json: Response = serde_json::from_str(&resp_text)?;

        println!("以下为搜索结果(仅列举至多12项)：");
        for (index, item) in resp_json.results.list.iter().enumerate() {
            println!("{}.{}", index, item.name);
        }

        Ok(resp_json.results.list)
    }

    async fn fetch_chapters(
        &self,
        manga_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ChapterContents>> {
        let Some(payload) = self.fetch_chapter_payload(manga_id, cancelled).await? else {
            return Ok(Vec::new());
        };

        Ok(extract_chapter_contents(&payload))
    }

    async fn fetch_pages(
        &self,
        manga_id: &str,
        chapter_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<String>> {
        // 章节页 HTML 里带 AES 密钥 `cct` 和加密内容 `contentKey`，
        // 解密后是一个 `[{ "url": "..." }, ...]` 数组。
        let page_url = format!(
            "{}/comic/{manga_id}/chapter/{chapter_id}",
            self.base_website
        );

        let Some(html) = self
            .get_with_retry(
                || self.client.get(&page_url),
                "章节目录页",
                5,
                Duration::from_secs(2),
                cancelled,
            )
            .await?
        else {
            return Ok(Vec::new());
        };

        let (cct, content_key) = extract_chapter_secrets(&html)?;
        let pages_url: Vec<String> =
            serde_json::from_value::<Vec<Value>>(decrypt_results(&content_key, &cct)?)?
                .into_iter()
                .filter_map(|page| {
                    page.get("url")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect();

        Ok(pages_url)
    }

    async fn find_by_name(&self, name: &str, cancelled: &AtomicBool) -> Result<Option<String>> {
        let url = format!("{API_BASE_WEBSITE}/api/v3/search/comic");
        let params = [
            ("offset", "0"),
            ("platform", "1"),
            ("limit", "20"),
            ("q", name),
            ("q_type", ""),
        ];

        for _ in 0..3 {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(None);
            }

            let request = || {
                let mut rb = self.client.get(&url).query(&params);
                for (header, value) in SEARCH_API_HEADERS {
                    rb = rb.header(header, value);
                }
                rb
            };

            let response = tokio::select! {
                _ = wait_cancelled(cancelled) => return Ok(None),
                res = request().send() => res,
            };

            match response {
                Ok(response) if response.status().is_success() => {
                    let text = tokio::select! {
                        _ = wait_cancelled(cancelled) => return Ok(None),
                        text = response.text() => text?,
                    };
                    let result: Response = serde_json::from_str(&text)?;
                    return Ok(result
                        .results
                        .list
                        .into_iter()
                        .find(|item| item.name == name)
                        .map(|item| item.path_word));
                }
                Ok(response) => {
                    println!("搜索漫画 ID 失败，状态码: {}，正在重试...", response.status());
                }
                Err(e) => {
                    println!("搜索漫画 ID 发生错误: {e}，正在重试...");
                }
            }

            sleep(Duration::from_secs(2)).await;
        }

        Err(anyhow!("搜索漫画 ID 失败：重试 3 次后仍然没有成功响应"))
    }
}

fn decrypt_results(results: &str, key: &str) -> Result<Value> {
    let iv = results
        .get(..16)
        .ok_or_else(|| anyhow!("results 长度不足 16 字符，无法取出 IV"))?
        .as_bytes();
    let hex_ct = results
        .get(16..)
        .ok_or_else(|| anyhow!("results 缺少密文部分"))?;
    let mut buf = hex::decode(hex_ct).context("章节密文 hex 解码失败")?;

    if buf.is_empty() || buf.len() % 16 != 0 {
        bail!("章节密文长度（{} 字节）不是 16 的倍数", buf.len());
    }
    let plaintext = Aes128CbcDec::new_from_slices(key.as_bytes(), iv)
        .map_err(|_| anyhow!("AES-128 密钥/IV 长度错误"))?
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow!("PKCS7 padding 校验失败（密钥可能不对）"))?;

    serde_json::from_slice(plaintext).context("解密结果不是合法 JSON")
}

fn extract_page_secrets(html: &str) -> Result<(String, String)> {
    let ccz_re = Regex::new(r"var\s+ccz\s*=\s*'([^']+)'").unwrap();
    let dnt_re = Regex::new(r#"id="dnt"[^>]*value="([^"]*)""#).unwrap();
    let key = ccz_re
        .captures(html)
        .map(|c| c[1].to_string())
        .ok_or_else(|| anyhow!("页面中未找到 AES 密钥 ccz（站点可能已更新加密方案）"))?;
    let dnts = dnt_re
        .captures(html)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "3".to_string());
    Ok((key, dnts))
}

fn extract_chapter_secrets(html: &str) -> Result<(String, String)> {
    let cct_re = Regex::new(r"var\s+cct\s*=\s*'([^']+)'").unwrap();
    let content_key_re = Regex::new(r"var\s+contentKey\s*=\s*'([^']+)'").unwrap();

    let cct = cct_re
        .captures(html)
        .map(|c| c[1].to_string())
        .ok_or_else(|| anyhow!("阅读页中未找到 AES 密钥 cct（站点可能已更新加密方案）"))?;
    let content_key = content_key_re
        .captures(html)
        .map(|c| c[1].to_string())
        .ok_or_else(|| anyhow!("阅读页中未找到 contentKey"))?;

    Ok((cct, content_key))
}

fn extract_chapter_contents(details: &Value) -> Vec<ChapterContents> {
    let mut chapters = Vec::new();

    let Some(groups) = details.get("groups").and_then(Value::as_object) else {
        return chapters;
    };

    for group in groups.values() {
        let Some(group_chapters) = group.get("chapters").and_then(Value::as_array) else {
            continue;
        };

        for chapter in group_chapters {
            let Some(name) = chapter.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(uuid) = chapter.get("id").and_then(Value::as_str) else {
                continue;
            };

            chapters.push(ChapterContents {
                chapter_name: name.to_string(),
                chapter_uuid: uuid.to_string(),
                len: 0,
                pages_url: Vec::new(),
            });
        }
    }

    chapters
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_chapter_name_and_uuid() {
        let details = json!({
            "groups": {
                "default": {
                    "chapters": [
                        {"id": "uuid-01", "name": "第01话"},
                        {"id": "uuid-02", "name": "第02话"}
                    ]
                }
            }
        });

        let chapters = extract_chapter_contents(&details);

        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].chapter_name, "第01话");
        assert_eq!(chapters[0].chapter_uuid, "uuid-01");
        assert_eq!(chapters[1].chapter_name, "第02话");
        assert_eq!(chapters[1].chapter_uuid, "uuid-02");
    }

    #[test]
    fn order_addrs_respects_family_preference() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        let v4 = |a: [u8; 4]| SocketAddr::new(IpAddr::V4(Ipv4Addr::from(a)), 443);
        let v6 = |seg: u16| {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, seg)), 443)
        };
        // Windows 下解析结果常 v4 在前
        let raw = vec![
            v4([104, 21, 23, 76]),
            v6(0x174c),
            v4([172, 67, 209, 202]),
            v6(0xd1ca),
        ];

        let v6_first = order_addrs(raw.clone(), true);
        assert!(v6_first[0].is_ipv6() && v6_first[1].is_ipv6());
        assert!(v6_first[2].is_ipv4() && v6_first[3].is_ipv4());

        let v4_first = order_addrs(raw, false);
        assert!(v4_first[0].is_ipv4() && v4_first[1].is_ipv4());
        assert!(v4_first[2].is_ipv6() && v4_first[3].is_ipv6());

        // 只有单一地址族时无论偏好如何都保持原样
        let only_v4 = vec![v4([1, 2, 3, 4])];
        assert_eq!(order_addrs(only_v4.clone(), true), only_v4);
        assert_eq!(order_addrs(only_v4, false), vec![v4([1, 2, 3, 4])]);
        assert!(order_addrs(Vec::new(), true).is_empty());
    }

    #[test]
    fn speed_comparison_respects_tie_band() {
        // v6 更快或差距在持平带内：保持 v6 优先
        assert!(should_prefer_ipv6(100.0, 200.0));
        assert!(should_prefer_ipv6(110.0, 100.0));
        // v4 领先超过 1.3 倍才翻转
        assert!(!should_prefer_ipv6(200.0, 100.0));
        // 一条链路不可用时另一条直接胜出
        assert!(should_prefer_ipv6(0.0, 1.0));
        assert!(!should_prefer_ipv6(1.0, 0.0));
    }

    #[tokio::test]
    #[ignore = "需要联网且依赖站点可用性，默认跳过"]
    async fn search_works() {
        let source = CopyMangaSource::new(BASE_WEBSITE).unwrap();
        let cancelled = AtomicBool::new(false);
        let list = source.search("19", &cancelled).await.unwrap();
        assert!(!list.is_empty());
    }
}
