//! 앱 설정. ~/Library/Application Support/Podlift/config.json
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    /// 가져온 음악을 둘 폴더
    pub output_path: String,
    /// 영상(m4v·mp4·mov)도 가져올지
    pub include_video: bool,
    /// 중복 정리할 폴더 (비어 있으면 저장 폴더)
    pub dedupe_path: String,
    /// 폴더 정리 탭의 폴더 (비어 있으면 저장 폴더)
    pub organize_path: String,
}

impl Default for Config {
    fn default() -> Self {
        let base = dirs::audio_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Music")))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            output_path: base.join("Podlift").to_string_lossy().into_owned(),
            include_video: false,
            dedupe_path: String::new(),
            organize_path: String::new(),
        }
    }
}

fn config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("Podlift").join("config.json"))
}

impl Config {
    pub fn load() -> Self {
        config_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(f) = config_file() else { return Ok(()) };
        if let Some(dir) = f.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(f, serde_json::to_string_pretty(self).unwrap_or_default())
    }
}
