//! Path rules shared by the UI and storage workers.
use crate::models::ImageFormat;
use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

/// Relative paths always refer to the executable's directory, never the shell cwd.
pub fn resolve(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("路径不能为空");
    }
    #[cfg(windows)]
    if !path.is_absolute()
        && (path.has_root() || matches!(path.components().next(), Some(Component::Prefix(_))))
    {
        bail!("请使用完整路径或 .\\ 相对路径，不能使用盘符相对路径");
    }
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        crate::config::exe_dir().join(path)
    };
    // Do not canonicalize nonexistent output files, and do not turn mapped shares into cwd paths.
    let mut result = PathBuf::new();
    for component in full.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    bail!("路径超出根目录");
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}

pub fn image_format(path: &Path) -> Result<ImageFormat> {
    let extension = path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("partial") {
        let stem = path.file_stem().context("无效镜像名称")?;
        return image_format(Path::new(stem));
    }
    match extension.to_ascii_lowercase().as_str() {
        "vhd" => Ok(ImageFormat::Vhd),
        "vhdx" => Ok(ImageFormat::Vhdx),
        _ => bail!("仅支持 .vhd 和 .vhdx 镜像"),
    }
}

pub fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".partial");
    PathBuf::from(name)
}

pub fn default_diff(base: &Path) -> Result<PathBuf> {
    let format = image_format(base)?;
    let stem = base.file_stem().context("基础镜像缺少文件名")?;
    let mut name = stem.to_os_string();
    name.push(match format {
        ImageFormat::Vhd => "-diff.vhd",
        ImageFormat::Vhdx => "-diff.vhdx",
    });
    Ok(crate::config::exe_dir().join(name))
}

fn comparison_key(path: &Path) -> String {
    let absolute = resolve(path).unwrap_or_else(|_| path.to_path_buf());
    let canonical = std::fs::canonicalize(&absolute).unwrap_or(absolute);
    let raw = canonical.to_string_lossy().replace('/', "\\");
    let key = if let Some(rest) = raw.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{rest}")
    } else {
        raw.strip_prefix("\\\\?\\").unwrap_or(&raw).to_owned()
    };
    key.trim_end_matches('\\').to_lowercase()
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    comparison_key(a) == comparison_key(b)
}

pub fn ensure_local(path: &Path) -> Result<()> {
    let original = path.to_string_lossy();
    if original.starts_with("\\\\") && !original.starts_with("\\\\?\\")
        || original.to_ascii_uppercase().starts_with("\\\\?\\UNC\\")
    {
        bail!("差分镜像必须位于本地磁盘，不能使用网络共享");
    }
    let path = resolve(path)?;
    let text = path.to_string_lossy();
    if text.starts_with("\\\\") && !text.starts_with("\\\\?\\")
        || text.to_ascii_uppercase().starts_with("\\\\?\\UNC\\")
    {
        bail!("差分镜像必须位于本地磁盘，不能使用网络共享");
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetDriveTypeW;
        let mut root = PathBuf::new();
        for c in path.components() {
            match c {
                Component::Prefix(_) | Component::RootDir => root.push(c.as_os_str()),
                _ => break,
            }
        }
        let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
        // Win32 DRIVE_REMOTE = 4; GetDriveTypeW returns a raw u32.
        if unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) } == 4 {
            bail!("差分镜像不能位于映射的网络驱动器");
        }
    }
    Ok(())
}

pub fn format_bytes(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", units[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_keeps_explicit_image_type() {
        assert_eq!(
            image_format(Path::new("镜像.vhdx.partial")).unwrap(),
            ImageFormat::Vhdx
        );
        assert_eq!(
            partial_path(Path::new("a.vhdx")),
            PathBuf::from("a.vhdx.partial")
        );
    }
    #[test]
    fn old_vhd_diff_keeps_format() {
        assert!(default_diff(Path::new("archive.vhd"))
            .unwrap()
            .ends_with("archive-diff.vhd"));
    }
    #[test]
    fn rejects_unsupported() {
        assert!(image_format(Path::new("a.iso")).is_err());
    }
    #[test]
    fn resolves_against_executable() {
        assert_eq!(
            resolve(Path::new("./a.vhdx")).unwrap(),
            crate::config::exe_dir().join("a.vhdx")
        );
    }
    #[test]
    fn rejects_network_child() {
        assert!(ensure_local(Path::new("\\\\NAS\\share\\diff.vhdx")).is_err());
    }
    #[test]
    fn formats_binary_capacity() {
        assert_eq!(format_bytes(512 * 1024 * 1024 * 1024), "512.00 GiB");
    }
}
