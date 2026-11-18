use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::BufRead;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Chapter {
    //下载图片时依据的结构，len是图片数量，pages_url是每张图片的链接，number是第几章，url是该话的链接
    pub number: usize,
    pub url: String,
    pub title: String,
    pub pages_url: Vec<String>,
    pub len: usize,
}

#[derive(Deserialize, Debug)]
pub struct Response {
    //搜索时用到的结构，用于储存搜索结果
    pub code: i32,
    pub message: String,
    pub results: SearchResult,
}

#[derive(Deserialize, Debug)]
pub struct SearchResult {
    pub list: Vec<ManGa_item>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ManGa_item {
    pub name: String,
    pub path_word: String,
    pub cover: String,
    pub author: Vec<Author>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Author {
    pub name: String,
    pub alias: Option<String>,
    pub path_word: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ErrorLog {
    pub chapter_title: String,
    pub error_message: String,
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

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct ChapterDetails {
    pub name: String,
    pub path_word: String,
    #[serde(default)]
    pub completed: Option<bool>,
    /// 显式手动标记优先于网站状态；旧 JSON 中缺失时为 None。
    #[serde(default)]
    pub completed_override: Option<bool>,
    /// 最近一次从网站取得的总话数，包含网站上锁定的章节。
    #[serde(default)]
    pub online_chapter_count: Option<usize>,
    /// 漫画来源站标识（copymanga / zerobyw ...）；旧版 JSON 无此字段，读入时为空串
    #[serde(default)]
    pub source: String,
    pub chapters: Vec<ChapterContents>,
}

impl ChapterDetails {
    pub fn migrate_legacy_metadata(&mut self) {
        if self.online_chapter_count.is_none() {
            // 旧版 true 只能来自手动标记；旧版 false 是默认值，无法证明网站仍在连载。
            if self.completed_override.is_none() {
                if self.completed == Some(true) {
                    self.completed_override = Some(true);
                } else {
                    self.completed = None;
                }
            }
            // zerobyw 的旧快照只有可读章节，不能据此推算网站总数。
            if self.source != "zerobyw" && !self.chapters.is_empty() {
                self.online_chapter_count = Some(self.chapters.len());
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChapterContents {
    pub chapter_name: String,
    pub chapter_uuid: String,
    pub len: usize,
    pub pages_url: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LocalManga {
    pub name: String,
    pub path_word: Option<String>,
    pub completed: Option<bool>,
    pub completed_override: Option<bool>,
    pub online_chapter_count: Option<usize>,
    /// 漫画所属源；旧版下载的 JSON 没有该字段时为 None
    pub source: Option<String>,
    pub chapter_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MangaUpdate {
    pub name: String,
    pub path_word: String,
    pub source: String,
    pub completed: Option<bool>,
    pub online_chapter_count: usize,
    pub local_chapter_count: usize,
    pub online_chapters: Vec<ChapterContents>,
    pub new_chapters: Vec<ChapterContents>,
}

/// 指定漫画检查时拉到的线上章节信息（含落盘快照用的完整大纲）
#[derive(Debug, Clone)]
pub struct MangaOutline {
    pub name: String,
    pub path_word: String,
    pub source: String,
    pub completed: Option<bool>,
    pub online_chapter_count: usize,
    pub online_chapters: Vec<ChapterContents>,
}

/// 站点一次详情请求返回的状态、总话数与可读章节。
#[derive(Debug, Default)]
pub struct MangaDetails {
    pub completed: Option<bool>,
    pub online_chapter_count: usize,
    pub chapters: Vec<ChapterContents>,
}

pub fn completion_label(completed: Option<bool>) -> &'static str {
    match completed {
        Some(true) => "已完结",
        Some(false) => "连载中",
        None => "状态未知",
    }
}

pub fn chapter_progress(local: usize, online: Option<usize>) -> String {
    format!(
        "{}/{}",
        local,
        online
            .map(|n| n.to_string())
            .unwrap_or_else(|| "未知".into())
    )
}

/// run() 的退出原因：正常完成一部漫画，还是被 Ctrl+C 取消
pub enum RunOutcome {
    Completed,
    Cancelled,
}

pub enum InputOutcome {
    Line(String),
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_defaults_and_manual_completion() {
        let json = r#"{"name":"旧漫画","path_word":"id","completed":false,"chapters":[{"chapter_name":"1","chapter_uuid":"1","len":0,"pages_url":[]}]}"#;
        let mut old: ChapterDetails = serde_json::from_str(json).unwrap();
        old.migrate_legacy_metadata();
        assert_eq!(old.completed, None);
        assert_eq!(old.online_chapter_count, Some(1));
        assert_eq!(old.completed_override, None);

        let mut old: ChapterDetails = serde_json::from_str(json).unwrap();
        old.completed = Some(true);
        old.source = "zerobyw".into();
        old.migrate_legacy_metadata();
        assert_eq!(old.completed_override, Some(true));
        assert_eq!(old.online_chapter_count, None);
        assert_eq!(chapter_progress(3, None), "3/未知");
        assert_eq!(chapter_progress(3, Some(100)), "3/100");
        assert_eq!(completion_label(None), "状态未知");
    }
}
