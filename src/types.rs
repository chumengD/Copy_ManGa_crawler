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
    pub path_word: String
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

#[derive(Debug, Deserialize)]
pub struct Config {
    pub base_website: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChapterDetails {
    pub name: String,
    pub path_word: String,
    pub chapters: Vec<ChapterContents>,
}


#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChapterContents {
    pub chapter_name: String,
    pub chapter_uuid: String,
    pub len:usize,
    pub pages_url: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LocalManga {
    pub name: String,
    pub path_word: Option<String>,
    pub chapter_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MangaUpdate {
    pub name: String,
    pub path_word: String,
    pub online_chapters: Vec<ChapterContents>,
    pub new_chapters: Vec<ChapterContents>,
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

