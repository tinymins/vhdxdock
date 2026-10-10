//! Native virtual-disk operations. Parent stores are never opened read/write.
use crate::models::DiskInfo;
use anyhow::Result;
use std::path::Path;

#[cfg(windows)]
pub use native::*;

#[cfg(not(windows))]
fn unsupported<T>() -> Result<T> {
    anyhow::bail!("此功能仅支持 Windows")
}
#[cfg(not(windows))]
pub fn inspect(_: &Path) -> Result<DiskInfo> {
    unsupported()
}
#[cfg(not(windows))]
pub fn create_dynamic(_: &Path, _: u64) -> Result<()> {
    unsupported()
}
#[cfg(not(windows))]
pub fn create_difference(_: &Path, _: &Path) -> Result<()> {
    unsupported()
}
#[cfg(not(windows))]
pub fn attach(_: &Path) -> Result<()> {
    unsupported()
}
#[cfg(not(windows))]
pub fn detach(_: &Path) -> Result<()> {
    unsupported()
}
#[cfg(not(windows))]
pub fn physical_path(_: &Path) -> Result<String> {
    unsupported()
}
#[cfg(not(windows))]
pub fn canonical_physical_device(_: &str) -> Result<String> {
    unsupported()
}
#[cfg(not(windows))]
pub fn relocate_parent(_: &Path, _: &Path) -> Result<()> {
    unsupported()
}
#[cfg(not(windows))]
pub fn validate_parent(_: &Path, _: &Path) -> Result<()> {
    unsupported()
}

#[derive(Debug)]
pub struct AttachedPath {
    pub enumerated: String,
    pub physical: String,
    pub image: Option<std::path::PathBuf>,
    pub warning: Option<String>,
}
#[cfg(not(windows))]
pub fn attached_paths() -> Result<Vec<AttachedPath>> {
    unsupported()
}

#[cfg(windows)]
mod native {
    use super::*;
    use crate::{
        models::{DiskKind, ImageFormat},
        paths,
    };
    use anyhow::{bail, Context};
    use std::{
        mem::{offset_of, size_of},
        os::windows::ffi::OsStrExt,
        path::PathBuf,
    };
    use windows::{
        core::{BOOL, PCWSTR, PWSTR},
        Win32::{
            Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, WIN32_ERROR},
            Storage::{
                FileSystem::{
                    CreateFileW, GetVolumeNameForVolumeMountPointW, FILE_ATTRIBUTE_NORMAL,
                    FILE_DEVICE_DISK, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
                },
                Vhd::*,
            },
            System::{
                Ioctl::{
                    FSCTL_LOCK_VOLUME, IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER,
                },
                IO::DeviceIoControl,
            },
        },
    };

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    /// Elevation alone does not enable SeManageVolumePrivilege. Serialize the
    /// temporary process-token adjustment and restore the previous token state.
    struct ManageVolumePrivilege {
        token: Handle,
        previous: windows::Win32::Security::TOKEN_PRIVILEGES,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    static PRIVILEGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    impl ManageVolumePrivilege {
        fn enable() -> Result<Self> {
            use windows::Win32::{
                Foundation::{GetLastError, ERROR_NOT_ALL_ASSIGNED, LUID},
                Security::*,
                System::Threading::{GetCurrentProcess, OpenProcessToken},
            };
            let lock = PRIVILEGE_LOCK
                .lock()
                .map_err(|_| anyhow::anyhow!("权限操作锁异常"))?;
            let mut raw = HANDLE::default();
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                    &mut raw,
                )?;
            }
            let token = Handle(raw);
            let mut luid = LUID::default();
            unsafe {
                LookupPrivilegeValueW(PCWSTR::null(), SE_MANAGE_VOLUME_NAME, &mut luid)?;
            }
            let new = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let mut previous = TOKEN_PRIVILEGES::default();
            let mut len = 0;
            unsafe {
                AdjustTokenPrivileges(
                    token.0,
                    false,
                    Some(&new),
                    size_of::<TOKEN_PRIVILEGES>() as u32,
                    Some(&mut previous),
                    Some(&mut len),
                )?;
                if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                    bail!("当前进程缺少磁盘管理权限，请以管理员身份运行 VhdxDock")
                }
            }
            Ok(Self {
                token,
                previous,
                _lock: lock,
            })
        }
    }
    impl Drop for ManageVolumePrivilege {
        fn drop(&mut self) {
            unsafe {
                let _ = windows::Win32::Security::AdjustTokenPrivileges(
                    self.token.0,
                    false,
                    Some(&self.previous),
                    0,
                    None,
                    None,
                );
            }
        }
    }
    fn check(code: WIN32_ERROR, action: &str) -> Result<()> {
        if code.0 == 0 {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "{action}: {} (Win32 {})",
                std::io::Error::from_raw_os_error(code.0 as i32),
                code.0
            ))
        }
    }
    fn wide(path: &Path) -> Result<Vec<u16>> {
        let mut v: Vec<u16> = path.as_os_str().encode_wide().collect();
        if v.contains(&0) {
            bail!("路径含有空字符")
        }
        v.push(0);
        Ok(v)
    }
    fn storage(format: ImageFormat) -> VIRTUAL_STORAGE_TYPE {
        VIRTUAL_STORAGE_TYPE {
            DeviceId: match format {
                ImageFormat::Vhd => VIRTUAL_STORAGE_TYPE_DEVICE_VHD,
                ImageFormat::Vhdx => VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
            },
            VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
        }
    }
    fn open(path: &Path, writable: bool, no_parents: bool) -> Result<Handle> {
        let p = wide(path)?;
        let params = OPEN_VIRTUAL_DISK_PARAMETERS {
            Version: OPEN_VIRTUAL_DISK_VERSION_2,
            Anonymous: OPEN_VIRTUAL_DISK_PARAMETERS_0 {
                Version2: OPEN_VIRTUAL_DISK_PARAMETERS_0_1 {
                    GetInfoOnly: BOOL(0),
                    ReadOnly: BOOL(i32::from(!writable)),
                    ..Default::default()
                },
            },
        };
        let mut handle = HANDLE::default();
        unsafe {
            check(
                OpenVirtualDisk(
                    &storage(paths::image_format(path)?),
                    PCWSTR(p.as_ptr()),
                    VIRTUAL_DISK_ACCESS_NONE,
                    if no_parents {
                        OPEN_VIRTUAL_DISK_FLAG_NO_PARENTS
                    } else {
                        OPEN_VIRTUAL_DISK_FLAG_NONE
                    },
                    Some(&params),
                    &mut handle,
                ),
                "打开虚拟磁盘",
            )?;
        }
        Ok(Handle(handle))
    }
    fn info(
        handle: &Handle,
        version: GET_VIRTUAL_DISK_INFO_VERSION,
    ) -> Result<GET_VIRTUAL_DISK_INFO> {
        let mut value = GET_VIRTUAL_DISK_INFO {
            Version: version,
            ..Default::default()
        };
        let mut len = size_of::<GET_VIRTUAL_DISK_INFO>() as u32;
        unsafe {
            check(
                GetVirtualDiskInformation(handle.0, &mut len, &mut value, None),
                "查询虚拟磁盘信息",
            )?;
        }
        Ok(value)
    }
    fn parent_path(handle: &Handle) -> Result<PathBuf> {
        // u64 storage provides the required structure alignment; the trailing WCHAR array is variable length.
        let mut buffer = vec![0u64; 32768];
        let ptr = buffer.as_mut_ptr().cast::<GET_VIRTUAL_DISK_INFO>();
        let mut len = (buffer.len() * 8) as u32;
        unsafe {
            (*ptr).Version = GET_VIRTUAL_DISK_INFO_PARENT_LOCATION;
            check(
                GetVirtualDiskInformation(handle.0, &mut len, ptr, None),
                "查询父镜像路径",
            )?;
            if len as usize > buffer.len() * 8 {
                bail!("父镜像路径响应长度无效")
            }
            let start = std::ptr::addr_of!((*ptr).Anonymous.ParentLocation.ParentLocationBuffer)
                .cast::<u16>();
            let offset = start as usize - buffer.as_ptr() as usize;
            let chars =
                std::slice::from_raw_parts(start, (len as usize).saturating_sub(offset) / 2);
            let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
            if end == 0 {
                bail!("差分盘未提供父镜像路径")
            }
            // With unresolved parents Windows returns a MULTI_SZ; its first entry is sufficient for diagnostics.
            Ok(PathBuf::from(String::from_utf16_lossy(&chars[..end])))
        }
    }
    fn physical_optional(handle: &Handle) -> Result<Option<String>> {
        let mut buffer = vec![0u16; 32768];
        let mut len = (buffer.len() * 2) as u32;
        let status =
            unsafe { GetVirtualDiskPhysicalPath(handle.0, &mut len, PWSTR(buffer.as_mut_ptr())) };
        // ERROR_DEV_NOT_EXIST is returned for an unattached image (covered by
        // create-only VHD and VHDX fixtures). Other errors are not proof that
        // the disk is detached, especially access denial or device failures.
        if status == windows::Win32::Foundation::ERROR_DEV_NOT_EXIST {
            return Ok(None);
        }
        check(status, "查询虚拟磁盘设备")?;
        if len as usize > buffer.len() * 2 {
            bail!("虚拟磁盘设备响应长度无效")
        }
        let end = buffer
            .iter()
            .position(|&c| c == 0)
            .context("虚拟磁盘设备路径未终止")?;
        if end == 0 {
            bail!("虚拟磁盘设备路径为空，无法确认挂载状态")
        }
        Ok(Some(
            String::from_utf16(&buffer[..end]).context("虚拟磁盘设备路径编码无效")?,
        ))
    }
    fn physical(handle: &Handle) -> Result<String> {
        physical_optional(handle)?.context("虚拟磁盘尚未挂载")
    }
    pub fn inspect(path: &Path) -> Result<DiskInfo> {
        let h = open(path, false, true)?;
        let kind = match unsafe {
            info(&h, GET_VIRTUAL_DISK_INFO_PROVIDER_SUBTYPE)?
                .Anonymous
                .ProviderSubtype
        } {
            2 => DiskKind::Fixed,
            3 => DiskKind::Dynamic,
            4 => DiskKind::Differencing,
            _ => DiskKind::Unknown,
        };
        let virtual_size = unsafe {
            info(&h, GET_VIRTUAL_DISK_INFO_SIZE)?
                .Anonymous
                .Size
                .VirtualSize
        };
        let parent = if kind == DiskKind::Differencing {
            Some(parent_path(&h)?)
        } else {
            None
        };
        let attached = physical_optional(&h)?.is_some();
        Ok(DiskInfo {
            path: path.to_path_buf(),
            format: paths::image_format(path)?,
            kind,
            parent,
            virtual_size,
            attached,
        })
    }
    fn create(path: &Path, size: u64, parent: Option<&Path>) -> Result<()> {
        if path.exists() {
            bail!("目标镜像已存在：{}", path.display())
        }
        let format = paths::image_format(path)?;
        let p = wide(path)?;
        let parent_wide = parent.map(wide).transpose()?;
        let params = CREATE_VIRTUAL_DISK_PARAMETERS {
            Version: CREATE_VIRTUAL_DISK_VERSION_2,
            Anonymous: CREATE_VIRTUAL_DISK_PARAMETERS_0 {
                Version2: CREATE_VIRTUAL_DISK_PARAMETERS_0_1 {
                    MaximumSize: size,
                    ParentPath: parent_wide
                        .as_ref()
                        .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
                    ParentVirtualStorageType: parent.map(|_| storage(format)).unwrap_or_default(),
                    ..Default::default()
                },
            },
        };
        let mut h = HANDLE::default();
        unsafe {
            check(
                CreateVirtualDisk(
                    &storage(format),
                    PCWSTR(p.as_ptr()),
                    VIRTUAL_DISK_ACCESS_NONE,
                    None,
                    CREATE_VIRTUAL_DISK_FLAG_NONE,
                    0,
                    &params,
                    None,
                    &mut h,
                ),
                "创建虚拟磁盘",
            )?;
        }
        drop(Handle(h));
        Ok(())
    }
    pub fn create_dynamic(path: &Path, bytes: u64) -> Result<()> {
        if bytes < 16 * 1024 * 1024 || !bytes.is_multiple_of(512) {
            bail!("虚拟容量必须至少 16 MiB，且为 512 字节的倍数")
        }
        create(path, bytes, None)
    }
    pub fn create_difference(base: &Path, diff: &Path) -> Result<()> {
        if paths::image_format(base)? != paths::image_format(diff)? {
            bail!("基础镜像与差分格式必须一致")
        }
        if paths::same_path(base, diff) {
            bail!("基础镜像和差分不能是同一个文件")
        }
        // CreateVirtualDisk opens the parent as backing storage. No metadata or merge flags are enabled.
        create(diff, 0, Some(base))?;
        validate_parent(diff, base)
    }
    pub fn validate_parent(diff: &Path, base: &Path) -> Result<()> {
        if paths::image_format(base)? != paths::image_format(diff)? {
            bail!("基础镜像与差分格式必须一致")
        }
        if paths::same_path(diff, base) {
            bail!("基础镜像和差分不能是同一个文件")
        }
        let child = open(diff, false, true)?;
        let parent = open(base, false, true)?;
        let subtype = unsafe {
            info(&child, GET_VIRTUAL_DISK_INFO_PROVIDER_SUBTYPE)?
                .Anonymous
                .ProviderSubtype
        };
        if subtype != 4 {
            bail!("待挂载镜像必须是差分镜像")
        }
        let child_size = unsafe { info(&child, GET_VIRTUAL_DISK_INFO_SIZE)?.Anonymous.Size };
        let parent_size = unsafe { info(&parent, GET_VIRTUAL_DISK_INFO_SIZE)?.Anonymous.Size };
        if child_size.VirtualSize != parent_size.VirtualSize
            || child_size.SectorSize != parent_size.SectorSize
        {
            bail!("父镜像容量或逻辑扇区大小与差分不匹配")
        }
        let expected = unsafe {
            info(&child, GET_VIRTUAL_DISK_INFO_PARENT_IDENTIFIER)?
                .Anonymous
                .ParentIdentifier
        };
        let actual = unsafe {
            info(&parent, GET_VIRTUAL_DISK_INFO_IDENTIFIER)?
                .Anonymous
                .Identifier
        };
        if expected != actual {
            bail!("父镜像身份不匹配；基础镜像可能不是原始副本，或已被修改")
        }
        Ok(())
    }
    pub fn attach(path: &Path) -> Result<()> {
        let _privilege = ManageVolumePrivilege::enable()?;
        let h = open(path, true, false)?;
        unsafe {
            check(
                AttachVirtualDisk(
                    h.0,
                    None,
                    ATTACH_VIRTUAL_DISK_FLAG_PERMANENT_LIFETIME
                        | ATTACH_VIRTUAL_DISK_FLAG_NO_DRIVE_LETTER,
                    0,
                    None,
                    None,
                ),
                "挂载虚拟磁盘",
            )
        }
    }
    pub fn detach(path: &Path) -> Result<()> {
        let _privilege = ManageVolumePrivilege::enable()?;
        let h = open(path, false, false)?;
        unsafe {
            check(
                DetachVirtualDisk(h.0, DETACH_VIRTUAL_DISK_FLAG_NONE, 0),
                "卸载虚拟磁盘",
            )
        }
    }
    /// Lock every filesystem volume before detach; busy volumes remain attached.
    pub fn safe_detach(path: &Path, roots: &[String]) -> Result<()> {
        let mut locks = Vec::new();
        let mut names = std::collections::HashSet::new();
        for root in roots {
            let root_wide = wide(Path::new(root))?;
            let mut volume = vec![0u16; 1024];
            unsafe {
                GetVolumeNameForVolumeMountPointW(PCWSTR(root_wide.as_ptr()), &mut volume)
                    .with_context(|| format!("无法识别卷 {root}"))?;
            }
            let end = volume.iter().position(|&c| c == 0).context("卷路径无效")?;
            let name = String::from_utf16_lossy(&volume[..end])
                .trim_end_matches('\\')
                .to_owned();
            if !names.insert(name.clone()) {
                continue;
            }
            let name_wide = wide(Path::new(&name))?;
            let handle = Handle(unsafe {
                CreateFileW(
                    PCWSTR(name_wide.as_ptr()),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
                .with_context(|| format!("无法打开卷 {root}，请关闭占用它的程序"))?
            });
            let mut returned = 0;
            unsafe {
                DeviceIoControl(
                    handle.0,
                    FSCTL_LOCK_VOLUME,
                    None,
                    0,
                    None,
                    0,
                    Some(&mut returned),
                    None,
                )
                .with_context(|| format!("磁盘 {root} 正在使用，请保存文件并关闭占用程序后重试"))?;
            }
            locks.push(handle);
        }
        // Handles remain alive during detach. Drop releases locks on errors.
        detach(path)
    }
    pub fn physical_path(path: &Path) -> Result<String> {
        physical(&open(path, false, false)?)
    }
    /// Resolve a device path's actual disk number rather than parsing its name.
    pub fn canonical_physical_device(device: &str) -> Result<String> {
        let p = wide(Path::new(device))?;
        let handle = Handle(unsafe {
            CreateFileW(
                PCWSTR(p.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .context("打开枚举的虚拟磁盘设备")?
        });
        let mut number = STORAGE_DEVICE_NUMBER::default();
        let mut returned = 0;
        unsafe {
            DeviceIoControl(
                handle.0,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                None,
                0,
                Some(std::ptr::addr_of_mut!(number).cast()),
                size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                Some(&mut returned),
                None,
            )
            .context("查询枚举设备的真实磁盘编号")?;
        }
        if returned as usize != size_of::<STORAGE_DEVICE_NUMBER>()
            || number.DeviceType != FILE_DEVICE_DISK.0
            || !matches!(number.PartitionNumber, 0 | u32::MAX)
        {
            bail!(
                "枚举设备未返回完整的整盘磁盘标识：类型 {}，分区 {}，返回字节 {}",
                number.DeviceType,
                number.PartitionNumber,
                returned
            )
        }
        Ok(format!(r"\\.\PhysicalDrive{}", number.DeviceNumber))
    }
    pub fn relocate_parent(diff: &Path, base: &Path) -> Result<()> {
        if inspect(diff)?.attached {
            bail!("差分盘已挂载，请先弹出后重试挂载")
        }
        if paths::image_format(base)? != paths::image_format(diff)? {
            bail!("基础镜像与差分格式必须一致")
        }
        validate_parent(diff, base)?;
        let h = open(diff, true, true)?;
        let p = wide(base)?;
        let value = SET_VIRTUAL_DISK_INFO {
            Version: SET_VIRTUAL_DISK_INFO_PARENT_PATH,
            Anonymous: SET_VIRTUAL_DISK_INFO_0 {
                ParentFilePath: PCWSTR(p.as_ptr()),
            },
        };
        unsafe {
            check(SetVirtualDiskInformation(h.0, &value), "更新父镜像路径")?;
        }
        drop(h);
        // Opening the complete chain lets Windows perform its own linkage validation.
        let _ = open(diff, false, false).context("父路径已更新，但 Windows 无法打开完整父链")?;
        Ok(())
    }
    fn dependency_image(physical: &str) -> Result<Option<PathBuf>> {
        let p = wide(Path::new(physical))?;
        let h = Handle(unsafe {
            CreateFileW(
                PCWSTR(p.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )?
        });
        let mut buffer = vec![0u64; 32768];
        let ptr = buffer.as_mut_ptr().cast::<STORAGE_DEPENDENCY_INFO>();
        unsafe {
            (*ptr).Version = STORAGE_DEPENDENCY_INFO_VERSION_2;
            check(
                GetStorageDependencyInformation(
                    h.0,
                    GET_STORAGE_DEPENDENCY_FLAG_DISK_HANDLE
                        | GET_STORAGE_DEPENDENCY_FLAG_HOST_VOLUMES,
                    (buffer.len() * 8) as u32,
                    ptr,
                    None,
                ),
                "查询磁盘镜像依赖",
            )?;
            let offset = offset_of!(STORAGE_DEPENDENCY_INFO, Anonymous);
            let max_entries =
                (buffer.len() * 8 - offset) / size_of::<STORAGE_DEPENDENCY_INFO_TYPE_2>();
            let count = (*ptr).NumberEntries as usize;
            if count > max_entries {
                bail!("虚拟磁盘依赖响应长度无效")
            }
            let entries = std::slice::from_raw_parts(
                std::ptr::addr_of!((*ptr).Anonymous).cast::<STORAGE_DEPENDENCY_INFO_TYPE_2>(),
                count,
            );
            if let Some(entry) = entries
                .iter()
                .filter(|e| {
                    matches!(
                        e.VirtualStorageType.DeviceId,
                        VIRTUAL_STORAGE_TYPE_DEVICE_VHD | VIRTUAL_STORAGE_TYPE_DEVICE_VHDX
                    )
                })
                .min_by_key(|e| e.AncestorLevel)
            {
                let read = |p: PWSTR| -> Result<String> {
                    if p.is_null() {
                        return Ok(String::new());
                    }
                    let addr = p.0 as usize;
                    let start = buffer.as_ptr() as usize;
                    let end = start + buffer.len() * 8;
                    if addr < start || addr >= end || !addr.is_multiple_of(2) {
                        bail!("依赖路径指针无效")
                    }
                    let chars = std::slice::from_raw_parts(p.0, (end - addr) / 2);
                    let n = chars
                        .iter()
                        .position(|&c| c == 0)
                        .context("依赖路径未终止")?;
                    Ok(String::from_utf16_lossy(&chars[..n]))
                };
                let host = read(entry.HostVolumeName)?;
                let relative = read(entry.DependentVolumeRelativePath)?;
                if !host.is_empty() && !relative.is_empty() {
                    return Ok(Some(PathBuf::from(format!(
                        "{}\\{}",
                        host.trim_end_matches('\\'),
                        relative.trim_start_matches('\\')
                    ))));
                }
            }
        }
        Ok(None)
    }
    fn enumerated_image_path(enumerated: &str) -> Option<PathBuf> {
        let path = Path::new(enumerated);
        paths::image_format(path).ok().map(|_| path.to_path_buf())
    }
    fn resolve_enumerated(enumerated: String) -> AttachedPath {
        // Despite the API name, Windows returns backing image filenames here.
        // Resolve those with the VHD API before passing anything to Storage.
        if let Some(image) = enumerated_image_path(&enumerated) {
            return match physical_path(&image) {
                Ok(physical) => AttachedPath {
                    enumerated,
                    physical,
                    image: Some(image),
                    warning: None,
                },
                Err(error) => AttachedPath {
                    physical: enumerated.clone(),
                    enumerated,
                    image: Some(image),
                    warning: Some(format!("无法解析已枚举镜像的设备：{error:#}")),
                },
            };
        }
        // Retain device/interface fallback for unknown enumeration variants.
        // Failed resolution remains a protected row rather than disappearing.
        let image = dependency_image(&enumerated).ok().flatten();
        match canonical_physical_device(&enumerated) {
            Ok(physical) => AttachedPath {
                enumerated,
                physical,
                image,
                warning: None,
            },
            Err(error) => AttachedPath {
                physical: enumerated.clone(),
                enumerated,
                image,
                warning: Some(format!("无法解析已枚举设备：{error:#}")),
            },
        }
    }
    pub fn attached_paths() -> Result<Vec<AttachedPath>> {
        let mut chars = vec![0u16; 32768];
        let mut bytes = (chars.len() * 2) as u32;
        unsafe {
            let code =
                GetAllAttachedVirtualDiskPhysicalPaths(&mut bytes, PWSTR(chars.as_mut_ptr()));
            if code.0 == 122 {
                chars.resize(bytes as usize / 2 + 1, 0);
                check(
                    GetAllAttachedVirtualDiskPhysicalPaths(&mut bytes, PWSTR(chars.as_mut_ptr())),
                    "枚举虚拟磁盘",
                )?;
            } else {
                check(code, "枚举虚拟磁盘")?;
            }
        }
        Ok(chars
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(|s| {
                let enumerated = String::from_utf16_lossy(s);
                resolve_enumerated(enumerated)
            })
            .collect())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use sha2::{Digest, Sha256};
        fn digest(path: &Path) -> Vec<u8> {
            Sha256::digest(std::fs::read(path).unwrap()).to_vec()
        }
        #[test]
        fn native_enumeration_distinguishes_image_names_from_devices() {
            for name in [
                r"C:\Temp\archive-diff.vhdx",
                r"D:\备份\base.VHD",
                r"E:\build.vhdx.partial",
            ] {
                assert_eq!(enumerated_image_path(name), Some(PathBuf::from(name)));
            }
            assert!(enumerated_image_path(r"\\.\PhysicalDrive2").is_none());
            assert!(enumerated_image_path(r"\\?\scsi#disk#fixture").is_none());
            let missing =
                resolve_enumerated(r"C:\vhdxdock-nonexistent-fixture\missing.vhdx".into());
            assert!(missing.image.is_some());
            assert!(
                missing.warning.is_some(),
                "unresolved entries must remain protected, not disappear"
            );
        }

        #[test]
        fn scratch_children_preserve_parent_and_relocate_safely() {
            // Create-only fixture: never attaches, initializes, or formats disks.
            let temp = tempfile::tempdir().unwrap();
            for ext in ["vhd", "vhdx"] {
                let base = temp.path().join(format!("base.{ext}"));
                let child = temp.path().join(format!("child.{ext}"));
                let relocated = temp.path().join(format!("moved.{ext}"));
                let unrelated = temp.path().join(format!("unrelated.{ext}"));
                create_dynamic(&base, 64 * 1024 * 1024).unwrap();
                assert!(physical_optional(&open(&base, false, true).unwrap())
                    .unwrap()
                    .is_none());
                if ext == "vhdx" {
                    // The VHDX linkage is its active header DataWriteGuid,
                    // not the persistent guest-visible VirtualDiskId.
                    let bytes = std::fs::read(&base).unwrap();
                    let a = &bytes[64 * 1024..64 * 1024 + 4096];
                    let b = &bytes[128 * 1024..128 * 1024 + 4096];
                    let seq = |h: &[u8]| u64::from_le_bytes(h[8..16].try_into().unwrap());
                    let header = if seq(a) > seq(b) { a } else { b };
                    assert_eq!(&header[..4], b"head");
                    let raw = &header[32..48];
                    let linkage = windows::core::GUID::from_values(
                        u32::from_le_bytes(raw[..4].try_into().unwrap()),
                        u16::from_le_bytes(raw[4..6].try_into().unwrap()),
                        u16::from_le_bytes(raw[6..8].try_into().unwrap()),
                        raw[8..16].try_into().unwrap(),
                    );
                    let h = open(&base, false, true).unwrap();
                    assert_eq!(
                        unsafe {
                            info(&h, GET_VIRTUAL_DISK_INFO_IDENTIFIER)
                                .unwrap()
                                .Anonymous
                                .Identifier
                        },
                        linkage
                    );
                }
                let initial_hash = digest(&base);
                create_difference(&base, &child).unwrap();
                assert_eq!(digest(&base), initial_hash, "creating child changed parent");
                assert_eq!(inspect(&child).unwrap().kind, DiskKind::Differencing);
                validate_parent(&child, &base).unwrap();
                // The old parent is deliberately absent: metadata inspection
                // and validation must not require resolving its previous path.
                std::fs::rename(&base, &relocated).unwrap();
                assert!(!base.exists());
                assert_eq!(inspect(&child).unwrap().kind, DiskKind::Differencing);
                validate_parent(&child, &relocated).unwrap();
                relocate_parent(&child, &relocated).unwrap();
                assert!(paths::same_path(
                    &inspect(&child).unwrap().parent.unwrap(),
                    &relocated
                ));
                assert_eq!(
                    digest(&relocated),
                    initial_hash,
                    "relocating child changed parent"
                );
                create_dynamic(&unrelated, 64 * 1024 * 1024).unwrap();
                let child_before_rejection = digest(&child);
                assert!(relocate_parent(&child, &unrelated).is_err());
                assert_eq!(
                    digest(&child),
                    child_before_rejection,
                    "rejected relocation modified child"
                );
            }
        }

        #[test]
        fn changed_vhdx_data_linkage_is_rejected_before_relocation() {
            // Only this newly-created, unattached scratch file is changed.
            // Keep its persistent VirtualDiskId and size, but update both valid
            // headers' DataWriteGuid as a compliant writer would before data
            // changes. This distinguishes data linkage from persistent identity.
            let temp = tempfile::tempdir().unwrap();
            let base = temp.path().join("base.vhdx");
            let child = temp.path().join("child.vhdx");
            let changed = temp.path().join("changed.vhdx");
            create_dynamic(&base, 64 * 1024 * 1024).unwrap();
            create_difference(&base, &child).unwrap();
            let persistent = unsafe {
                info(
                    &open(&base, false, true).unwrap(),
                    GET_VIRTUAL_DISK_INFO_VIRTUAL_DISK_ID,
                )
                .unwrap()
                .Anonymous
                .VirtualDiskId
            };
            let mut bytes = std::fs::read(&base).unwrap();
            for start in [64 * 1024, 128 * 1024] {
                let header = &mut bytes[start..start + 4096];
                assert_eq!(&header[..4], b"head");
                header[32] ^= 0x80;
                header[4..8].fill(0);
                let mut crc = u32::MAX;
                for byte in header.iter() {
                    crc ^= u32::from(*byte);
                    for _ in 0..8 {
                        crc = (crc >> 1) ^ (0x82f6_3b78 & 0u32.wrapping_sub(crc & 1));
                    }
                }
                header[4..8].copy_from_slice(&(!crc).to_le_bytes());
            }
            std::fs::write(&changed, bytes).unwrap();
            let changed_handle = open(&changed, false, true).unwrap();
            assert_eq!(
                unsafe {
                    info(&changed_handle, GET_VIRTUAL_DISK_INFO_VIRTUAL_DISK_ID)
                        .unwrap()
                        .Anonymous
                        .VirtualDiskId
                },
                persistent,
                "fixture must preserve persistent identity"
            );
            drop(changed_handle);
            let before = digest(&child);
            assert!(validate_parent(&child, &changed).is_err());
            assert!(relocate_parent(&child, &changed).is_err());
            assert_eq!(
                digest(&child),
                before,
                "wrong parent must not update child metadata"
            );
        }

        #[test]
        fn resized_vhd_parent_is_rejected_before_relocation() {
            // Legacy VHD keeps its identity when expanded; capacity must be
            // checked independently before changing a child's parent locator.
            let temp = tempfile::tempdir().unwrap();
            let base = temp.path().join("base.vhd");
            let child = temp.path().join("child.vhd");
            create_dynamic(&base, 64 * 1024 * 1024).unwrap();
            create_difference(&base, &child).unwrap();
            let h = open(&base, true, true).unwrap();
            let params = EXPAND_VIRTUAL_DISK_PARAMETERS {
                Version: EXPAND_VIRTUAL_DISK_VERSION_1,
                Anonymous: EXPAND_VIRTUAL_DISK_PARAMETERS_0 {
                    Version1: EXPAND_VIRTUAL_DISK_PARAMETERS_0_0 {
                        NewSize: 128 * 1024 * 1024,
                    },
                },
            };
            check(
                unsafe { ExpandVirtualDisk(h.0, EXPAND_VIRTUAL_DISK_FLAG_NONE, &params, None) },
                "扩大测试镜像",
            )
            .unwrap();
            drop(h);
            let before = digest(&child);
            let error = validate_parent(&child, &base).unwrap_err();
            assert!(error.to_string().contains("容量"), "{error:#}");
            assert!(relocate_parent(&child, &base).is_err());
            assert_eq!(digest(&child), before);
        }

        #[test]
        fn native_path_rejects_embedded_nul_before_api() {
            assert!(wide(Path::new("bad\0name.vhdx")).is_err());
            let unicode = wide(Path::new("备份.vhdx")).unwrap();
            assert_eq!(unicode.last(), Some(&0));
        }

        #[test]
        #[ignore = "read-only live Windows discovery probe"]
        fn read_only_discovery_probe() {
            for disk in attached_paths().unwrap() {
                println!("{disk:?}");
            }
        }
    }
}
