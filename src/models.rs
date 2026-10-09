use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormat {
    Vhd,
    Vhdx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiskKind {
    Fixed,
    Dynamic,
    Differencing,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub path: PathBuf,
    pub format: ImageFormat,
    pub kind: DiskKind,
    pub parent: Option<PathBuf>,
    pub virtual_size: u64,
    pub attached: bool,
}

#[derive(Debug, Clone)]
pub struct MountRequest {
    pub base: PathBuf,
    pub diff: PathBuf,
    pub drive_letter: Option<char>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountedImage {
    pub image_path: PathBuf,
    pub parent_path: Option<PathBuf>,
    pub volumes: Vec<String>,
    pub kind: String,
    pub read_only: bool,
    pub can_eject: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VerifyMode {
    #[default]
    Metadata,
    Sha256,
}

#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub source: PathBuf,
    pub output: PathBuf,
    pub capacity_gib: u64,
    pub compress: bool,
    pub verify: VerifyMode,
}

#[derive(Debug, Clone)]
pub struct BuildResult {
    pub output: PathBuf,
    pub image_bytes: u64,
    pub logical_bytes: u64,
    pub files: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub phase: String,
    pub message: String,
    pub files: u64,
    pub bytes: u64,
    pub total_files: u64,
    pub total_bytes: u64,
}
