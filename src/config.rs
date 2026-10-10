use crate::models::VerifyMode;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub base_path: String,
    pub diff_path: String,
    pub drive_letter: Option<char>,
    pub source_path: String,
    pub output_path: String,
    pub volume_label: String,
    pub capacity_gib: u64,
    pub compress: bool,
    pub verify: VerifyMode,
    pub window_size: Option<[f32; 2]>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            base_path: String::new(),
            diff_path: String::new(),
            drive_letter: None,
            source_path: String::new(),
            output_path: String::new(),
            volume_label: String::new(),
            capacity_gib: 512,
            compress: true,
            verify: VerifyMode::Metadata,
            window_size: None,
        }
    }
}
impl AppConfig {
    pub fn load() -> Self {
        std::fs::read(data_dir().join("config.json"))
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) -> Result<()> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        let temp = dir.join(format!("config.{}.tmp", std::process::id()));
        let final_path = dir.join("config.json");
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&temp, bytes).context("保存配置失败")?;
        std::fs::rename(&temp, &final_path).context("更新配置文件失败")
    }
}

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn data_dir() -> PathBuf {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            let exe = exe_dir();
            if writable(&exe) {
                return exe;
            }
            let fallback = std::env::var_os("LOCALAPPDATA")
                .or_else(|| std::env::var_os("XDG_DATA_HOME"))
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("VhdxDock");
            let _ = std::fs::create_dir_all(&fallback);
            fallback
        })
        .clone()
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".vhdxdock-write-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_config_receives_defaults() {
        let c: AppConfig = serde_json::from_str("{\"base_path\":\"a.vhdx\"}").unwrap();
        assert_eq!(c.capacity_gib, 512);
        assert!(c.compress);
        assert!(c.volume_label.is_empty());
    }
    #[test]
    fn paths_roundtrip_without_shell_escaping() {
        let c = AppConfig {
            base_path: "\\\\NAS\\资料\\a ' $x.vhdx".into(),
            ..Default::default()
        };
        let d: AppConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(c.base_path, d.base_path);
    }

    #[test]
    fn custom_volume_label_roundtrips_without_trimming() {
        let c = AppConfig {
            volume_label: "  开发归档 ' $x  ".into(),
            ..Default::default()
        };
        let d: AppConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(c.volume_label, d.volume_label);
    }
}
