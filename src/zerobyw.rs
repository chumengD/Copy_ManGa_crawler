//! zerobyw（zero搬运网）站点适配器。
//!
//! 纯 HTML 站点，无加密、无 JSON 接口：
//! - 搜索：`GET /Android/souo/?keyword=...`
//! - 详情页：`/Android/details/?kuid=N`，章节列表为 `.chapter-grid a.chapter-item`
//! - 阅读页：`/Android/view/?zjid=N`，图片直链为 `img.manga-img` 的协议相对地址
//!
//! 注意：未登录状态下每部漫画只有前几话未上锁，其余章节的 a 标签是
//! `javascript:;` + locked 样式，直接构造 zjid 访问阅读页也会被服务端拦截。
//! 适配器只抓取未上锁的章节。

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use regex::Regex;
use reqwest::header::{HeaderMap, REFERER};
use reqwest::Client;
use scraper::{Html, Selector};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::{sleep, Duration};

use crate::source::MangaSource;
use crate::types::{ChapterContents, ManGa_item};

/// 站点地址直接硬编码。该站域名尾部的数字会不定期变化，失效时改这里
pub const BASE_WEBSITE: &str = "https://www.zerobyw33.com";

pub struct ZerobywSource {
    base_website: String,
    client: Client,
}

impl ZerobywSource {
    pub fn new() -> Result<Self> {
        Self::with_base_website(BASE_WEBSITE)
    }

    pub fn with_base_website(base_website: &str) -> Result<Self> {
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

#[async_trait]
impl MangaSource for ZerobywSource {
    fn id(&self) -> &'static str {
        "zerobyw"
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

        let doc = Html::parse_document(&html);
        let item_sel = Selector::parse(".chapter-grid a.chapter-item").unwrap();
        let zjid_re = Regex::new(r"zjid=(\d+)").unwrap();

        let mut chapters = Vec::new();
        let mut locked = 0usize;
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
                // href 是 javascript:; 的锁定章节：没有 zjid，未登录无法阅读
                None => locked += 1,
            }
        }

        if locked > 0 {
            eprintln!(
                "[!] zerobyw：有 {locked} 个章节未登录不可读，已跳过；当前仅能下载未上锁的章节"
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
}
