use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use Copy_ManGa_downloader::{BASE_WEBSITE, CopyMangaSource, MangaSource, ZerobywSource, input_number};

const SETTINGS_PATH: &str = "download/.source_settings.json";

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    Copymanga,
    Zerobyw,
}

#[derive(Default, Deserialize, Serialize)]
struct Settings {
    default_source: SourceKind,
}

impl SourceKind {
    fn name(self) -> &'static str {
        match self {
            Self::Copymanga => "拷贝漫画",
            Self::Zerobyw => "zerobyw搬运网",
        }
    }

    pub fn create(self) -> Result<Arc<dyn MangaSource>> {
        match self {
            Self::Copymanga => Ok(Arc::new(CopyMangaSource::new(BASE_WEBSITE)?)),
            Self::Zerobyw => Ok(Arc::new(ZerobywSource::new()?)),
        }
    }
}

pub fn load_default() -> SourceKind {
    let settings = match std::fs::read(SETTINGS_PATH) {
        Ok(data) => serde_json::from_slice::<Settings>(&data).map_err(anyhow::Error::from),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SourceKind::default(),
        Err(e) => Err(e.into()),
    };
    match settings {
        Ok(settings) => settings.default_source,
        Err(e) => {
            eprintln!("读取默认源配置失败，使用拷贝漫画：{}", e);
            SourceKind::default()
        }
    }
}

pub async fn switch_source(source: &mut Arc<dyn MangaSource>, cancelled: &AtomicBool) -> Result<()> {
    println!("当前漫画源：{}", source.id());
    let save_default = loop {
        match input_number("1:本次换源  2:切换默认源  0:返回\n", cancelled).await {
            Some(1) => break false,
            Some(2) => break true,
            Some(0) | None => return Ok(()),
            _ => println!("⚠ 输入无效，请重新输入"),
        }
    };
    let kind = loop {
        match input_number("选择漫画源：1:拷贝漫画  2:zerobyw搬运网  0:返回\n", cancelled).await {
            Some(1) => break SourceKind::Copymanga,
            Some(2) => break SourceKind::Zerobyw,
            Some(0) | None => return Ok(()),
            _ => println!("⚠ 输入无效，请重新输入"),
        }
    };
    let selected = kind.create()?;
    if cancelled.load(Ordering::SeqCst) {
        return Ok(());
    }
    if save_default {
        let path = Path::new(SETTINGS_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).context("创建配置目录失败")?;
        let data = serde_json::to_vec_pretty(&Settings { default_source: kind })?;
        std::fs::write(path, data).context("保存默认源失败，当前源未切换")?;
    }
    *source = selected;
    let scope = if save_default {
        "，并保存为默认源"
    } else {
        "（仅本次运行生效）"
    };
    println!("已切换到{}{}", kind.name(), scope);
    source.on_selected(cancelled).await;
    Ok(())
}
