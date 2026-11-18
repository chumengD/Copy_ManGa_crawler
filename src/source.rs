use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use std::sync::atomic::AtomicBool;

use crate::types::{ChapterContents, ManGa_item, MangaDetails};

/// 漫画源适配器统一接口：每个站点实现一份「怎么搜索、怎么列章节、怎么拿图片直链」，
/// 菜单/下载/更新检查等主流程只面向该接口，新增站点时无需改动主流程。
#[async_trait]
pub trait MangaSource: Send + Sync {
    /// 源标识：写入 `<漫画名>.json` 的 source 字段，检查更新时据此区分本地漫画属于哪个站。
    /// 旧版 JSON 没有该字段时按拷贝漫画处理（向后兼容）。
    fn id(&self) -> &'static str;

    /// 源被选中后立即执行的引导（如 zerobyw 的登录引导）。默认无操作。
    /// 引导允许被跳过（用户拒绝 / Ctrl+C / stdin 关闭），失败不得中断主流程。
    async fn on_selected(&self, _cancelled: &AtomicBool) {}

    /// 拉取图片直链与下载图片共用的 HTTP 客户端（各源自带 UA / Referer / 证书策略）。
    fn http(&self) -> &Client;

    /// 按关键词搜索，返回搜索结果列表。取消时返回空列表（调用方自行检查 cancelled 标志）。
    async fn search(&self, keyword: &str, cancelled: &AtomicBool) -> Result<Vec<ManGa_item>>;

    /// 拉取某部漫画的完整线上章节列表（不含图片直链）。取消时返回空列表。
    async fn fetch_chapters(
        &self,
        manga_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ChapterContents>>;

    /// 拉取网站状态和总话数。无状态信息的源返回 None，不默认当作连载。
    async fn fetch_details(&self, manga_id: &str, cancelled: &AtomicBool) -> Result<MangaDetails> {
        let chapters = self.fetch_chapters(manga_id, cancelled).await?;
        Ok(MangaDetails {
            completed: None,
            online_chapter_count: chapters.len(),
            chapters,
        })
    }

    /// 拉取某一话的图片直链。取消时返回空列表。
    async fn fetch_pages(
        &self,
        manga_id: &str,
        chapter_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<String>>;

    /// 检查更新时按漫画名精确反查 manga_id；源不支持该能力时返回 Err。
    async fn find_by_name(&self, name: &str, cancelled: &AtomicBool) -> Result<Option<String>>;
}

/// 只接受明确的状态值，避免把简介中的“完结”等字样误判为状态。
pub(crate) fn parse_completion_text(text: &str) -> Option<bool> {
    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let value = text
        .trim_start_matches("状态")
        .trim_start_matches("狀態")
        .trim_start_matches([':', '：']);
    match value {
        "完结" | "已完结" | "完結" | "已完結" | "已完" => Some(true),
        "连载" | "连载中" | "連載" | "連載中" | "未完结" | "未完結" => Some(false),
        _ => None,
    }
}

pub(crate) fn parse_html_completion(html: &str) -> Option<bool> {
    let doc = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse("span, p, li, div, dd, a").unwrap();
    doc.select(&selector).find_map(|element| {
        let text = element.text().collect::<String>();
        let text = text.trim();
        let class = element.value().attr("class").unwrap_or("");
        if text.starts_with("状态")
            || text.starts_with("狀態")
            || class.contains("status")
            || class.contains("comicParticulars-right-txt")
        {
            parse_completion_text(text)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_explicit_status_fields() {
        assert_eq!(
            parse_html_completion(
                r#"<nav><a>已完结</a></nav>
            <li><span>狀態：</span><span class="comicParticulars-right-txt">連載中</span></li>"#
            ),
            Some(false)
        );
        assert_eq!(
            parse_html_completion("<div>状态：<span>已完结</span></div>"),
            Some(true)
        );
        assert_eq!(parse_html_completion("<p>这是已完结漫画的续作</p>"), None);
        assert_eq!(
            parse_html_completion("<div class='manga-status'>暂停</div>"),
            None
        );
    }
}
