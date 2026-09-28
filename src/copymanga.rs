//! 拷贝漫画（copymanga）站点适配器。
//!
//! 该站的搜索接口返回明文 JSON；章节大纲、图片直链等接口返回 AES-CBC 密文：
//! 1. 从页面 HTML 中用正则提取密钥（如 `ccz` / `cct`）与 token
//! 2. 调用详情接口拿到 `results` 密文字段
//! 3. `decrypt_results`：密文前 16 字符作 IV，其余 hex 解码后 AES-128-CBC + PKCS7 解密

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, REFERER};
use reqwest::Client;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::{sleep, Duration};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use regex::Regex;

use crate::source::MangaSource;
use crate::types::{ChapterContents, ManGa_item, Response};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// 站点地址直接硬编码，不再依赖外部 config.toml
pub const BASE_WEBSITE: &str = "https://ios.copymanga.club";

/// 相邻网络请求之间的最小间隔，避免请求过快触发站点限流（Too Many Requests）
const REQUEST_DELAY: Duration = Duration::from_millis(1000);

pub struct CopyMangaSource {
    base_website: String,
    client: Client,
}

impl CopyMangaSource {
    pub fn new(base_website: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(REFERER, base_website.parse()?);
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
            .danger_accept_invalid_certs(true)
            .default_headers(headers)
            .build()?;
        Ok(Self {
            base_website: base_website.trim_end_matches('/').to_string(),
            client,
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

#[async_trait]
impl MangaSource for CopyMangaSource {
    fn id(&self) -> &'static str {
        "copymanga"
    }

    fn http(&self) -> &Client {
        &self.client
    }

    async fn search(&self, keyword: &str, cancelled: &AtomicBool) -> Result<Vec<ManGa_item>> {
        let base_url = format!("{}/api/kb/web/searchci/comics", self.base_website);
        let params = [
            ("offset", "0"),
            ("platform", "2"),
            ("limit", "12"),
            ("q", keyword),
            ("q_type", ""),
        ];

        let Some(resp_text) = self
            .get_with_retry(
                || self.client.get(&base_url).query(&params),
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
        println!("reponse：{:#?}", resp_json);

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
        let url = format!("{}/api/kb/web/searchci/comics", self.base_website);
        let params = [
            ("offset", "0"),
            ("platform", "2"),
            ("limit", "20"),
            ("q", name),
            ("q_type", ""),
        ];

        for _ in 0..3 {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(None);
            }

            match self.client.get(&url).query(&params).send().await {
                Ok(response) if response.status().is_success() => {
                    let text = response.text().await?;
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

    #[tokio::test]
    #[ignore = "需要联网且依赖站点可用性，默认跳过"]
    async fn search_works() {
        let source = CopyMangaSource::new(BASE_WEBSITE).unwrap();
        let cancelled = AtomicBool::new(false);
        let list = source.search("19", &cancelled).await.unwrap();
        assert!(!list.is_empty());
    }
}
