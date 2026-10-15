use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use std::sync::atomic::AtomicBool;

use crate::types::{ChapterContents, ManGa_item};

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

    /// 单页图片写盘时用的扩展名（不含点），由该话首张图片直链推导。
    /// 默认 `webp`：拷贝漫画的 CDN 直链虽然以 `.jpg` 结尾，实际返回的是 webp 内容，
    /// 沿用旧命名以免与已下载的文件重名。
    fn image_extension(&self, _sample_page_url: Option<&str>) -> &'static str {
        "webp"
    }

    /// 批量下载开始前的一次性准备：第一话的某个图片直链会作为样本传入，
    /// 实现方可用它做链路测速等优化（如拷贝漫画按实测吞吐挑选 IPv4/IPv6）。
    /// 默认无操作。
    async fn prepare_for_download(&self, _sample_page_url: Option<&str>, _cancelled: &AtomicBool) {}

    /// 按关键词搜索，返回搜索结果列表。取消时返回空列表（调用方自行检查 cancelled 标志）。
    async fn search(&self, keyword: &str, cancelled: &AtomicBool) -> Result<Vec<ManGa_item>>;

    /// 拉取某部漫画的完整线上章节列表（不含图片直链）。取消时返回空列表。
    async fn fetch_chapters(
        &self,
        manga_id: &str,
        cancelled: &AtomicBool,
    ) -> Result<Vec<ChapterContents>>;

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
