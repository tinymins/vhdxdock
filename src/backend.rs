//! Application-level disk orchestration. Destructive initialization is restricted
//! to the exact physical device returned for a newly-created RAW virtual image.
use crate::models::{MountRequest, MountedImage};
use anyhow::Result;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
pub fn mount(_: MountRequest) -> Result<Vec<MountedImage>> {
    anyhow::bail!("挂载功能仅支持 Windows")
}
#[cfg(not(windows))]
pub fn unmount(_: &Path) -> Result<()> {
    anyhow::bail!("卸载功能仅支持 Windows")
}
#[cfg(not(windows))]
pub fn list_mounted() -> Result<Vec<MountedImage>> {
    Ok(Vec::new())
}
#[cfg(not(windows))]
pub fn initialize_new_virtual_disk(_: &Path, _: bool, _: &str) -> Result<PathBuf> {
    anyhow::bail!("制作功能仅支持 Windows")
}

#[cfg(windows)]
pub use implementation::*;

#[cfg(windows)]
mod implementation {
    use super::*;
    use crate::{models::DiskKind, paths, process, virtual_disk};
    use anyhow::{bail, Context};
    use serde_json::json;
    use std::os::windows::{ffi::OsStrExt, fs::MetadataExt};

    fn wide_path(path: &Path) -> Result<Vec<u16>> {
        let mut text: Vec<_> = path.as_os_str().encode_wide().collect();
        if text.contains(&0) {
            bail!("路径含有空字符")
        }
        text.push(0);
        Ok(text)
    }
    fn access_root(path: &Path) -> String {
        format!("{}\\", path.to_string_lossy().trim_end_matches('\\'))
    }
    fn is_letter_root(path: &str) -> bool {
        let text = path.trim_end_matches('\\').as_bytes();
        text.len() == 2 && text[0].is_ascii_alphabetic() && text[1] == b':'
    }
    fn volume_name(path: &Path) -> Result<String> {
        use windows::{
            core::PCWSTR, Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW,
        };
        let text = wide_path(Path::new(&access_root(path)))?;
        let mut output = [0u16; 1024];
        unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(text.as_ptr()), &mut output) }
            .with_context(|| format!("无法确认挂载路径的卷身份：{}", path.display()))?;
        Ok(String::from_utf16(
            &output[..output
                .iter()
                .position(|&c| c == 0)
                .context("卷路径未终止")?],
        )?)
    }
    fn same_volume(a: &str, b: &str) -> bool {
        a.trim_end_matches('\\')
            .eq_ignore_ascii_case(b.trim_end_matches('\\'))
    }
    fn same_access_path(a: &Path, b: &Path) -> bool {
        let key = |p: &Path| {
            p.to_string_lossy()
                .replace('/', "\\")
                .trim_start_matches(r"\\?\")
                .trim_end_matches('\\')
                .to_lowercase()
        };
        key(a) == key(b)
    }
    fn validate_mount_folder(folder: &Path, already_mounted_here: bool) -> Result<()> {
        use windows::{
            core::PCWSTR,
            Win32::Storage::FileSystem::{
                GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW,
            },
        };
        paths::ensure_local(folder).context("挂载文件夹必须位于本地磁盘")?;
        if folder.file_name().is_none() {
            bail!("不能挂载到磁盘根目录")
        }
        let parent = folder.parent().context("不能挂载到磁盘根目录")?;
        // Inspect the directory entries themselves, never canonicalize across
        // a junction/symlink into an unexpected host or a mounted image.
        for (index, ancestor) in folder.ancestors().enumerate() {
            let metadata = std::fs::symlink_metadata(ancestor)
                .with_context(|| format!("挂载文件夹必须已存在：{}", ancestor.display()))?;
            if !metadata.is_dir() {
                bail!("挂载目标及其父路径必须是目录")
            }
            if metadata.file_attributes() & 0x400 != 0 && !(index == 0 && already_mounted_here) {
                bail!("挂载文件夹或父目录是重解析点，不能作为挂载目标")
            }
        }
        let text = wide_path(parent)?;
        let mut host = [0u16; 32768];
        unsafe { GetVolumePathNameW(PCWSTR(text.as_ptr()), &mut host) }
            .context("无法确认挂载文件夹的宿主卷")?;
        if !matches!(unsafe { GetDriveTypeW(PCWSTR(host.as_ptr())) }, 2 | 3) {
            bail!("挂载文件夹必须位于本地磁盘")
        }
        let mut filesystem = [0u16; 64];
        unsafe {
            GetVolumeInformationW(
                PCWSTR(host.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut filesystem),
            )
        }
        .context("无法读取挂载文件夹的文件系统")?;
        let end = filesystem
            .iter()
            .position(|&c| c == 0)
            .context("文件系统名称无效")?;
        if String::from_utf16(&filesystem[..end])? != "NTFS" {
            bail!("挂载文件夹的宿主文件系统必须是 NTFS")
        }
        if !already_mounted_here && std::fs::read_dir(folder)?.next().transpose()?.is_some() {
            bail!("挂载文件夹必须为空；不会删除已有内容")
        }
        Ok(())
    }

    /// Read only the reparse entry, including after its target volume detached.
    fn folder_reparse_volume(folder: &Path) -> Result<Option<String>> {
        use windows::{
            core::PCWSTR,
            Win32::{
                Foundation::CloseHandle,
                Storage::FileSystem::{
                    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
                },
                System::{Ioctl::FSCTL_GET_REPARSE_POINT, IO::DeviceIoControl},
            },
        };
        if std::fs::symlink_metadata(folder)?.file_attributes() & 0x400 == 0 {
            return Ok(None);
        }
        let path = wide_path(folder)?;
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }?;
        let mut buffer = [0u8; 16384];
        let mut returned = 0;
        let result = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_GET_REPARSE_POINT,
                None,
                0,
                Some(buffer.as_mut_ptr().cast()),
                buffer.len() as u32,
                Some(&mut returned),
                None,
            )
        };
        unsafe {
            let _ = CloseHandle(handle);
        }
        result.context("无法读取挂载文件夹的重解析目标")?;
        let u16_at = |n| u16::from_le_bytes([buffer[n], buffer[n + 1]]) as usize;
        if returned < 16 || u32::from_le_bytes(buffer[..4].try_into()?) != 0xa0000003 {
            bail!("挂载路径不是卷挂载点，拒绝移除")
        }
        let start = 16usize
            .checked_add(u16_at(8))
            .context("重解析目标偏移溢出")?;
        let end = start
            .checked_add(u16_at(10))
            .context("重解析目标长度溢出")?;
        if end > returned as usize || !start.is_multiple_of(2) || !end.is_multiple_of(2) {
            bail!("重解析目标数据无效")
        }
        let chars: Vec<_> = buffer[start..end]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let target = String::from_utf16(&chars)?;
        let suffix = target
            .strip_prefix(r"\??\Volume{")
            .context("重解析目标不是卷 GUID，拒绝移除")?;
        Ok(Some(format!(r"\\?\Volume{{{suffix}")))
    }
    fn cleanup_folder_mounts(folders: &[(PathBuf, String)]) -> Result<()> {
        use windows::{core::PCWSTR, Win32::Storage::FileSystem::DeleteVolumeMountPointW};
        for (folder, expected) in folders {
            if let Some(actual) = folder_reparse_volume(folder)? {
                if !same_volume(&actual, expected) {
                    bail!("挂载文件夹已指向其他卷，拒绝移除：{}", folder.display())
                }
                let path = wide_path(Path::new(&access_root(folder)))?;
                unsafe { DeleteVolumeMountPointW(PCWSTR(path.as_ptr())) }
                    .with_context(|| format!("无法移除文件夹挂载点：{}", folder.display()))?;
                if folder_reparse_volume(folder)?.is_some() {
                    bail!("文件夹挂载点仍存在：{}", folder.display())
                }
            }
        }
        Ok(())
    }

    const DISCOVER: &str = r#"
$rows = @()
$pageRoots = @(Get-CimInstance Win32_PageFileUsage -ErrorAction Stop | ForEach-Object { [IO.Path]::GetPathRoot($_.Name) })
foreach ($candidate in @($req.candidates)) {
    $physical = [string]$candidate.physical
    $image = [string]$candidate.image
    $protected = $true
    $readOnly = $true
    $volumes = @()
    $bindings = @()
    $warning = $null
    try {
        if ($candidate.normalizationError) { throw ('无法规范化枚举设备：' + $candidate.enumeratedPhysical + '；' + $candidate.normalizationError) }
        if ($physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw ('无法识别物理设备名称：' + $physical) }
        $disk = Get-Disk -Number ([int]$Matches[1]) -ErrorAction Stop
        try {
            $diskImage = Get-DiskImage -DevicePath $physical -ErrorAction Stop
            if ($diskImage.ImagePath) { $image = [string]$diskImage.ImagePath }
        } catch { }
        $protected = [bool]($disk.IsBoot -or $disk.IsSystem)
        $readOnly = [bool]$disk.IsReadOnly
        $partitions = @()
        if ($disk.PartitionStyle -ne 'RAW') { $partitions = @(Get-Partition -DiskNumber $disk.Number -ErrorAction Stop) }
        foreach ($partition in $partitions) {
            if ($partition.IsBoot -or $partition.IsSystem) { $protected = $true }
            $openPaths = @($partition.AccessPaths)
            $guidPaths = @($partition.AccessPaths | Where-Object { ([string]$_).StartsWith('\\?\Volume{', [StringComparison]::OrdinalIgnoreCase) })
            # AccessPaths can lag behind DriveLetter after Add-PartitionAccessPath.
            # Cached letters must still map to this exact disk and partition.
            if ($partition.DriveLetter -and ([string]$partition.DriveLetter) -match '^[A-Za-z]$') {
                $letterRoot = [string]$partition.DriveLetter + ':\'
                if (Test-Path -LiteralPath $letterRoot -PathType Container) {
                    $mapped = @(Get-Partition -DriveLetter ([char]$partition.DriveLetter) -ErrorAction Stop)
                    if ($mapped.Count -ne 1 -or $mapped[0].DiskNumber -ne $disk.Number -or $mapped[0].PartitionNumber -ne $partition.PartitionNumber) { throw '盘符已指向其他磁盘或分区，请刷新后重试' }
                    $openPaths += $letterRoot
                }
            }
            foreach ($access in $openPaths) {
                if ($access -and $access -notlike '\\?\Volume{*' -and (Test-Path -LiteralPath ([string]$access) -PathType Container)) {
                    $volumes += [string]$access
                    if ($guidPaths.Count -ne 1) { throw '分区未提供唯一的卷 GUID，无法验证挂载路径' }
                    $bindings += @{root=[string]$access;guid=[string]$guidPaths[0]}
                    if ($pageRoots -contains [string]$access) { $protected = $true }
                }
            }
        }
        if ($protected) { $warning = '系统、启动或分页文件所在磁盘，禁止卸载' }
    } catch {
        $warning = '无法验证磁盘安全状态：' + $_.Exception.Message + ' [' + $_.FullyQualifiedErrorId + '] ' + $_.InvocationInfo.PositionMessage
        $protected = $true
    }
    if (-not $image) { $image = $physical; $protected = $true; $warning = '无法查询基础文件路径，禁止卸载' }
    $rows += [pscustomobject]@{
        image_path = $image; parent_path = $null; volumes = @($volumes | Select-Object -Unique)
        bindings = @($bindings)
        kind = '未知'; read_only = $readOnly; can_eject = (-not $protected); warning = $warning
    }
}
ConvertTo-Json -InputObject @($rows) -Depth 5 -Compress
"#;

    pub fn list_mounted() -> Result<Vec<MountedImage>> {
        let candidates = virtual_disk::attached_paths()?;
        let payload = json!({"candidates": candidates.iter().map(|p| {
            let (physical, error) = match virtual_disk::canonical_physical_device(&p.physical) {
                Ok(device) => (device, p.warning.clone()),
                Err(error) => (p.physical.clone(), Some(format!("{error:#}"))),
            };
            json!({"physical": physical, "enumeratedPhysical":p.enumerated, "normalizationError":error, "image": p.image})
        }).collect::<Vec<_>>()});
        let value = process::powershell(DISCOVER, &payload)?;
        let mut rows: Vec<MountedImage> =
            serde_json::from_value(value.clone()).context("Windows 返回的挂载列表无效")?;
        for (index, row) in rows.iter_mut().enumerate() {
            for root in &row.volumes {
                let identity = (|| -> Result<()> {
                    let bindings = value[index]["bindings"]
                        .as_array()
                        .context("缺少挂载路径身份信息")?;
                    let expected = bindings
                        .iter()
                        .find(|binding| binding["root"].as_str() == Some(root))
                        .and_then(|binding| binding["guid"].as_str())
                        .context("挂载路径没有对应的分区卷身份")?;
                    if !same_volume(&volume_name(Path::new(root))?, expected) {
                        bail!("挂载路径已经指向其他卷")
                    }
                    Ok(())
                })();
                if let Err(error) = identity {
                    row.can_eject = false;
                    row.warning = Some(format!("无法验证挂载路径 {}：{error:#}", root));
                }
            }
            match virtual_disk::inspect(&row.image_path) {
                Ok(info) => {
                    row.parent_path = info.parent;
                    row.kind = match info.kind {
                        DiskKind::Differencing => "差分",
                        DiskKind::Fixed => "直接挂载（固定）",
                        DiskKind::Dynamic => "直接挂载（动态）",
                        DiskKind::Unknown => "未知",
                    }
                    .into();
                    if !info.attached {
                        row.can_eject = false;
                        row.warning = Some("挂载状态已变化，请刷新".into());
                    }
                }
                Err(err) => {
                    row.can_eject = false;
                    row.warning = Some(format!("无法验证镜像：{err:#}"));
                }
            }
            if row
                .image_path
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".partial")
            {
                row.can_eject = false;
                row.warning = Some("正在制作的镜像，使用制作页面取消任务".into());
            }
        }
        Ok(rows)
    }

    const ASSIGN_VOLUMES: &str = r#"
if ([string]$req.physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw '无法识别虚拟磁盘设备' }
$disk = Get-Disk -Number ([int]$Matches[1]) -ErrorAction Stop
if ($disk.IsBoot -or $disk.IsSystem) { throw '拒绝操作系统或启动磁盘' }
if ($disk.PartitionStyle -eq 'RAW') { throw '镜像尚未分区；挂载功能不会格式化磁盘' }
if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false -ErrorAction Stop }
$disk = Get-Disk -Number $disk.Number -ErrorAction Stop
if ($disk.IsReadOnly) { throw '差分盘处于只读状态，无法提供可写挂载' }
$eligible = @()
foreach ($p in @(Get-Partition -DiskNumber $disk.Number -ErrorAction Stop)) {
    if ($p.IsBoot -or $p.IsSystem) { throw '分区安全检查失败' }
    $v = $p | Get-Volume -ErrorAction SilentlyContinue
    if ($v -and $v.FileSystemType -and [string]$v.FileSystemType -notin @('Unknown','RAW')) { $eligible += $p }
    elseif ($req.folder -and [string]$p.Type -ne 'Reserved') { throw '文件夹模式无法确认所有数据分区，拒绝挂载' }
}
if ($eligible.Count -eq 0) { throw '镜像没有 Windows 可识别的文件系统' }
if ($req.folder) {
    if ($eligible.Count -ne 1) { throw '文件夹模式只支持一个数据分区；多分区镜像请使用盘符模式' }
    $p = $eligible[0]
    if ($disk.PartitionStyle -eq 'GPT') { $p | Set-Partition -NoDefaultDriveLetter $true -ErrorAction Stop }
    $p | Add-PartitionAccessPath -AccessPath ([string]$req.folder) -ErrorAction Stop
    $p = Get-Partition -DiskNumber $disk.Number -PartitionNumber $p.PartitionNumber -ErrorAction Stop
    $letters = @($p.AccessPaths | Where-Object { [string]$_ -match '^[A-Za-z]:\\$' })
    if ($p.DriveLetter -and ([string]$p.DriveLetter) -match '^[A-Za-z]$') { $letters += ([string]$p.DriveLetter + ':\') }
    foreach ($letterPath in @($letters | Select-Object -Unique)) {
        $p | Remove-PartitionAccessPath -AccessPath $letterPath -ErrorAction Stop
    }
    $p = Get-Partition -DiskNumber $disk.Number -PartitionNumber $p.PartitionNumber -ErrorAction Stop
    if ($p.DriveLetter -or @($p.AccessPaths | Where-Object { [string]$_ -match '^[A-Za-z]:\\$' }).Count) { throw '文件夹挂载仍有盘符，拒绝报告成功' }
    ConvertTo-Json -InputObject @{ok=$true;partitions=@($p | Select-Object PartitionNumber,DriveLetter,AccessPaths)} -Depth 5 -Compress
    return
}
$first = $true
foreach ($p in $eligible) {
    if ($first -and $req.letter) {
        $letter = [char][string]$req.letter
        if ($p.DriveLetter -and $p.DriveLetter -ne $letter) { throw '磁盘已有其他盘符，请先卸载后重试' }
        if (-not $p.DriveLetter) {
            if (Get-PSDrive -Name ([string]$letter) -ErrorAction SilentlyContinue) { throw '所选盘符已被占用' }
            $p | Set-Partition -NewDriveLetter $letter -ErrorAction Stop
        }
    } elseif (-not $p.DriveLetter) {
        $p | Add-PartitionAccessPath -AssignDriveLetter -ErrorAction Stop
    }
    $first = $false
}
ConvertTo-Json -InputObject @{ok=$true;partitions=@(Get-Partition -DiskNumber $disk.Number -ErrorAction Stop | Select-Object PartitionNumber,DriveLetter,AccessPaths)} -Depth 5 -Compress
"#;

    const MOUNT_DIAGNOSTICS: &str = r#"
$snapshot = @{physical=[string]$req.physical;disk=$null;partitions=@();error=$null}
try {
    if ([string]$req.physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw '设备路径无效' }
    $disk = Get-Disk -Number ([int]$Matches[1]) -ErrorAction Stop
    $snapshot.disk = $disk | Select-Object Number,UniqueId,PartitionStyle,IsOffline,IsReadOnly,BusType,Size
    foreach ($p in @(Get-Partition -DiskNumber $disk.Number -ErrorAction Stop)) {
        $row = @{partitionNumber=$p.PartitionNumber;driveLetter=[string]$p.DriveLetter;letterCode=[int][char]$p.DriveLetter;accessPaths=@($p.AccessPaths);volume=$null;volumeError=$null;rootAccessible=$false}
        if ($p.DriveLetter) { $row.rootAccessible = Test-Path -LiteralPath ([string]$p.DriveLetter + ':\') -PathType Container }
        try { $row.volume = $p | Get-Volume -ErrorAction Stop | Select-Object DriveLetter,Path,UniqueId,FileSystemType,HealthStatus,OperationalStatus } catch { $row.volumeError = $_.Exception.Message }
        $snapshot.partitions += $row
    }
} catch { $snapshot.error=$_.Exception.Message }
ConvertTo-Json -InputObject $snapshot -Depth 7 -Compress
"#;

    const VERIFY_FOLDER: &str = r#"
if ([string]$req.physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw '设备路径无效' }
$disk = Get-Disk -Number ([int]$Matches[1]) -ErrorAction Stop
$data = @(Get-Partition -DiskNumber $disk.Number -ErrorAction Stop | Where-Object { [string]$_.Type -ne 'Reserved' })
if ($data.Count -ne 1) { throw '文件夹挂载必须只有一个数据分区' }
if ($data[0].DriveLetter -or @($data[0].AccessPaths | Where-Object { [string]$_ -match '^[A-Za-z]:\\$' }).Count) { throw '文件夹挂载仍有盘符' }
ConvertTo-Json -InputObject @{ok=$true} -Compress
"#;

    pub fn mount(request: MountRequest) -> Result<Vec<MountedImage>> {
        if request.mount_folder.is_some() && request.drive_letter.is_some() {
            bail!("文件夹模式不能同时指定盘符")
        }
        let base = paths::resolve(&request.base)?;
        let diff = paths::resolve(&request.diff)?;
        let folder = request
            .mount_folder
            .as_deref()
            .map(paths::resolve)
            .transpose()?;
        ensure_finalized(&base)?;
        ensure_finalized(&diff)?;
        paths::ensure_local(&diff)?;
        if paths::same_path(&base, &diff) {
            bail!("基础镜像与差分不能是同一个文件")
        }
        if paths::image_format(&base)? != paths::image_format(&diff)? {
            bail!("基础镜像与差分格式必须相同")
        }
        if let Some(letter) = request.drive_letter {
            if !letter.is_ascii_alphabetic() {
                bail!("盘符必须为 A 到 Z")
            }
        }
        virtual_disk::inspect(&base).context("无法访问基础镜像")?;
        let mounted = list_mounted()?;
        if let Some(folder) = &folder {
            let already_here = mounted
                .iter()
                .find(|r| paths::same_path(&r.image_path, &diff))
                .is_some_and(|row| {
                    row.volumes
                        .iter()
                        .any(|root| same_access_path(Path::new(root), folder))
                });
            validate_mount_folder(folder, already_here)?;
        }
        // A backing parent can be loaded by a child without being directly
        // attached. Only a discovered row for the base itself is a direct mount.
        if let Some(row) = mounted
            .iter()
            .find(|r| paths::same_path(&r.image_path, &base))
        {
            if !row.read_only || row.warning.is_some() {
                bail!("基础镜像已直接挂载为可写或无法确认只读，请先卸载基础镜像")
            }
        }
        if !diff.exists() {
            if let Some(parent) = diff.parent() {
                std::fs::create_dir_all(parent).context("无法创建差分目录")?;
            }
            virtual_disk::create_difference(&base, &diff)?;
        }
        let child = virtual_disk::inspect(&diff)?;
        if child.kind != DiskKind::Differencing {
            bail!("所选本地文件不是差分镜像，不能作为写入层")
        }
        virtual_disk::validate_parent(&diff, &base)?;
        let actual_parent = child.parent.context("差分镜像没有父路径")?;
        let actual_parent = if actual_parent.is_absolute() {
            actual_parent
        } else {
            diff.parent()
                .context("差分路径无父目录")?
                .join(actual_parent)
        };
        if !paths::same_path(&actual_parent, &base) {
            if child.attached {
                bail!("差分盘已挂载，请先弹出，再用新的基础镜像地址挂载")
            }
            // The selected base is authoritative. Only change the child locator
            // after identity validation; never rewrite or ignore the base identity.
            virtual_disk::relocate_parent(&diff, &base)
                .context("无法自动更新差分盘中的基础镜像路径")?;
            virtual_disk::validate_parent(&diff, &base)?;
        }
        if child.attached {
            let rows = wait_for_mount_rows(&diff)?;
            verify_requested_location(&rows, &diff, folder.as_deref(), request.drive_letter)
                .context("差分盘已经挂载在其他位置或模式，请先弹出后重试挂载")?;
            if folder.is_some() {
                process::powershell(
                    VERIFY_FOLDER,
                    &json!({"physical":virtual_disk::physical_path(&diff)?}),
                )?;
            }
            return Ok(rows);
        }
        virtual_disk::attach(&diff)?;
        let assigned = (|| -> Result<Vec<MountedImage>> {
            let physical = virtual_disk::physical_path(&diff)?;
            if let Some(folder) = &folder {
                validate_mount_folder(folder, false)?;
            }
            let assignment = process::powershell(
                ASSIGN_VOLUMES,
                &json!({"physical": physical, "letter": request.drive_letter.map(|c| c.to_ascii_uppercase().to_string()), "folder":folder.as_deref().map(access_root)}),
            )?;
            let rows = wait_for_mount_rows(&diff).with_context(|| {
                format!("镜像已连接，但挂载状态未通过检查。分配挂载路径后的快照：{assignment}")
            })?;
            verify_requested_location(&rows, &diff, folder.as_deref(), request.drive_letter)?;
            if folder.is_some() {
                process::powershell(
                    VERIFY_FOLDER,
                    &json!({"physical":virtual_disk::physical_path(&diff)?}),
                )?;
            }
            Ok(rows)
        })();
        match assigned {
            Ok(rows) => Ok(rows),
            Err(error) => {
                if let Err(detach_error) = detach_checked(&diff, &[]) {
                    bail!("{error:#}；挂载回滚失败，请刷新并手动卸载：{detach_error:#}")
                }
                Err(error)
            }
        }
    }

    fn verify_requested_location(
        rows: &[MountedImage],
        diff: &Path,
        folder: Option<&Path>,
        letter: Option<char>,
    ) -> Result<()> {
        let row = rows
            .iter()
            .find(|r| paths::same_path(&r.image_path, diff))
            .context("找不到目标差分镜像")?;
        if let Some(folder) = folder {
            if row.volumes.len() != 1 || !same_access_path(Path::new(&row.volumes[0]), folder) {
                bail!("差分盘的挂载位置与所选文件夹不一致，或仍然存在盘符/其他挂载点")
            }
        } else if let Some(letter) = letter {
            let expected = format!("{}:\\", letter.to_ascii_uppercase());
            if !row
                .volumes
                .iter()
                .any(|root| root.eq_ignore_ascii_case(&expected))
            {
                bail!("差分盘的盘符与所选盘符不一致")
            }
        } else if !row.volumes.iter().any(|root| is_letter_root(root)) {
            bail!("差分盘当前使用文件夹挂载，请先弹出后切换盘符模式")
        }
        Ok(())
    }
    fn wait_for_mount_rows(diff: &Path) -> Result<Vec<MountedImage>> {
        let mut last = anyhow::anyhow!("Windows 尚未返回挂载状态");
        let mut last_rows = serde_json::Value::Null;
        for attempt in 0..8 {
            let result = match list_mounted() {
                Ok(rows) => {
                    last_rows = serde_json::to_value(&rows)
                        .unwrap_or_else(|error| json!({"snapshotError":error.to_string()}));
                    verify_mount_rows(rows, diff)
                }
                Err(error) => {
                    last_rows = json!({"listError":format!("{error:#}")});
                    Err(error)
                }
            };
            match result {
                Ok(rows) => return Ok(rows),
                Err(error) => last = error,
            }
            if attempt < 7 {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
        let diagnostic = virtual_disk::physical_path(diff)
            .and_then(|physical| {
                process::powershell(MOUNT_DIAGNOSTICS, &json!({"physical":physical}))
            })
            .unwrap_or_else(|error| json!({"diagnosticError":format!("{error:#}")}));
        let enumerated = virtual_disk::attached_paths()
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|p| json!({"enumerated":p.enumerated,"physical":p.physical,"image":p.image,"warning":p.warning}))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Err(last)
            .with_context(|| format!("等待卷就绪后仍未通过挂载检查；最后挂载列表：{last_rows}；枚举设备：{}；实时存储快照：{diagnostic}",json!(enumerated)))
    }
    fn ensure_finalized(path: &Path) -> Result<()> {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("partial"))
        {
            bail!(".partial 是尚未完成的镜像，不能挂载：{}", path.display())
        }
        Ok(())
    }
    fn verify_mount_rows(rows: Vec<MountedImage>, diff: &Path) -> Result<Vec<MountedImage>> {
        let row = rows
            .iter()
            .find(|r| paths::same_path(&r.image_path, diff))
            .context("Windows 未返回目标差分镜像的挂载记录")?;
        if let Some(warning) = &row.warning {
            bail!("差分镜像状态无法确认：{warning}")
        }
        if row.volumes.is_empty() {
            bail!("差分镜像没有可打开的卷")
        }
        if row.read_only {
            bail!("差分镜像处于只读状态")
        }
        Ok(rows)
    }
    const EJECT_ROOTS: &str = r#"
if ([string]$req.physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw '设备路径无效' }
$disk = Get-Disk -Number ([int]$Matches[1]) -ErrorAction Stop
if ($disk.IsBoot -or $disk.IsSystem) { throw '禁止卸载系统磁盘' }
$pageRoots = @(Get-CimInstance Win32_PageFileUsage -ErrorAction Stop | ForEach-Object { [IO.Path]::GetPathRoot($_.Name) })
$roots = @()
$partitions = @()
if ($disk.PartitionStyle -ne 'RAW') { $partitions = @(Get-Partition -DiskNumber $disk.Number -ErrorAction Stop) }
foreach ($partition in $partitions) {
    if ($partition.IsBoot -or $partition.IsSystem) { throw '禁止卸载系统分区' }
    # AccessPaths contains GUID roots as well as letters and mounted folders.
    # Get-Volume errors must never silently remove a volume from the lock set.
    $accessPaths = @($partition.AccessPaths | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) })
    if ($partition.DriveLetter -and ([string]$partition.DriveLetter) -match '^[A-Za-z]$') {
        $letterRoot = [string]$partition.DriveLetter + ':\'
        if (Test-Path -LiteralPath $letterRoot -PathType Container) {
            $mapped = @(Get-Partition -DriveLetter ([char]$partition.DriveLetter) -ErrorAction Stop)
            if ($mapped.Count -ne 1 -or $mapped[0].DiskNumber -ne $disk.Number -or $mapped[0].PartitionNumber -ne $partition.PartitionNumber) { throw '盘符已指向其他磁盘或分区，拒绝卸载' }
            $accessPaths += $letterRoot
        }
    }
    if ($accessPaths.Count -eq 0 -and [string]$partition.Type -ne 'Reserved') { throw '无法确认分区卷路径，拒绝卸载' }
    foreach ($access in $accessPaths) {
        if (-not ([string]$access).EndsWith('\')) { throw '卷路径未以反斜杠结束，拒绝卸载' }
        if ($pageRoots -contains [string]$access) { throw '禁止卸载分页文件所在磁盘' }
        $roots += [string]$access
    }
}
foreach ($expected in @($req.expectedVolumes)) {
    if ($roots -notcontains [string]$expected) { throw '磁盘卷路径发生变化，拒绝卸载，请刷新后重试' }
}
ConvertTo-Json -InputObject @($roots | Select-Object -Unique) -Compress
"#;
    fn verify_eject_roots(roots: &[String], expected_volumes: &[String]) -> Result<()> {
        if !expected_volumes.is_empty() && roots.is_empty() {
            bail!("已挂载磁盘未返回任何卷路径，拒绝绕过卷锁卸载")
        }
        Ok(())
    }
    fn detach_checked(path: &Path, expected_volumes: &[String]) -> Result<()> {
        let roots = process::powershell(
            EJECT_ROOTS,
            &json!({"physical": virtual_disk::physical_path(path)?, "expectedVolumes": expected_volumes}),
        )?;
        let roots: Vec<String> = serde_json::from_value(roots)?;
        verify_eject_roots(&roots, expected_volumes)?;
        let guid_roots: Vec<_> = roots
            .iter()
            .filter(|root| root.to_ascii_lowercase().starts_with(r"\\?\volume{"))
            .collect();
        let mut folders = Vec::new();
        for root in &roots {
            if root.to_ascii_lowercase().starts_with(r"\\?\volume{") {
                continue;
            }
            let actual = volume_name(Path::new(root))?;
            if !guid_roots
                .iter()
                .any(|expected| same_volume(expected, &actual))
            {
                bail!("卷路径已指向目标磁盘以外的卷，拒绝卸载：{root}")
            }
            if !is_letter_root(root) {
                let folder = PathBuf::from(root.trim_end_matches('\\'));
                let target =
                    folder_reparse_volume(&folder)?.context("文件夹挂载点已消失，请刷新后重试")?;
                if !same_volume(&target, &actual) {
                    bail!("文件夹挂载点身份不匹配，拒绝卸载")
                }
                folders.push((folder, actual));
            }
        }
        // Busy handles fail before any mount point is removed. Reparse targets
        // remain readable without traversing the now-detached target volume.
        virtual_disk::safe_detach(path, &roots)?;
        cleanup_folder_mounts(&folders)
            .context("镜像已经弹出，但文件夹挂载点清理失败；目录和数据未删除")
    }
    pub fn unmount(path: &Path) -> Result<()> {
        let path = paths::resolve(path)?;
        let image = list_mounted()?
            .into_iter()
            .find(|r| paths::same_path(&r.image_path, &path))
            .context("未找到该镜像的已挂载磁盘，请刷新")?;
        if !image.can_eject {
            bail!(
                "{}",
                image.warning.unwrap_or_else(|| "磁盘不允许安全卸载".into())
            )
        }
        detach_checked(&path, &image.volumes)
    }
    const INITIALIZE: &str = r#"
if ([string]$req.physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw '无法识别新镜像设备' }
$number = [int]$Matches[1]
$disk = Get-Disk -Number $number -ErrorAction Stop
if ($disk.IsBoot -or $disk.IsSystem -or $disk.PartitionStyle -ne 'RAW') { throw '只允许初始化空白的新虚拟磁盘' }
if ([string]$disk.BusType -ne 'File Backed Virtual') { throw '目标不是文件支持的虚拟磁盘，停止格式化' }
if ([uint64]$disk.Size -ne [uint64]$req.expectedSize) { throw '磁盘容量与新镜像不符，停止格式化' }
if ($disk.IsOffline) { $disk | Set-Disk -IsOffline $false -ErrorAction Stop }
if ($disk.IsReadOnly) { throw '新镜像为只读，停止格式化' }
# Re-query the device just before initialization; do not select a disk by drive letter or free-space heuristics.
$disk = Get-Disk -Number $number -ErrorAction Stop
if ($disk.IsBoot -or $disk.IsSystem -or $disk.PartitionStyle -ne 'RAW' -or [string]$disk.UniqueId -ne [string]$req.uniqueId) { throw '磁盘身份或状态发生变化，停止格式化' }
$partition = $disk | Initialize-Disk -PartitionStyle GPT -PassThru -ErrorAction Stop | New-Partition -UseMaximumSize -AssignDriveLetter -ErrorAction Stop
$formatArgs = @{FileSystem='NTFS'; AllocationUnitSize=4096; NewFileSystemLabel=[string]$req.volumeLabel; Confirm=$false; ErrorAction='Stop'}
if ([bool]$req.compress) { $formatArgs.Compress = $true }
$partition | Format-Volume @formatArgs | Out-Null
$partition = Get-Partition -DiskNumber $number -PartitionNumber $partition.PartitionNumber -ErrorAction Stop
if (-not $partition.DriveLetter) { throw '新分区未获得盘符' }
$volume = $partition | Get-Volume -ErrorAction Stop
if ([string]$volume.FileSystemLabel -cne [string]$req.volumeLabel) { throw '格式化后的卷标与指定名称不一致，停止制作' }
ConvertTo-Json -InputObject @{root=([string]$partition.DriveLetter + ':\')} -Compress
"#;
    pub fn initialize_new_virtual_disk(
        partial: &Path,
        compress: bool,
        volume_label: &str,
    ) -> Result<PathBuf> {
        crate::builder::validate_volume_label(volume_label)?;
        if !partial
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".vhdx.partial")
        {
            bail!("制作初始化只接受新建的 .vhdx.partial 镜像")
        }
        let info = virtual_disk::inspect(partial)?;
        if info.attached || info.kind != DiskKind::Dynamic || info.parent.is_some() {
            bail!("镜像必须是尚未挂载的新动态基础镜像")
        }
        virtual_disk::attach(partial)?;
        let result = (|| -> Result<PathBuf> {
            let physical = virtual_disk::physical_path(partial)?;
            let identity = process::powershell(
                r#"
if ([string]$req.physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw '设备路径无效' }
$disk = Get-Disk -Number ([int]$Matches[1]) -ErrorAction Stop
if ($disk.IsBoot -or $disk.IsSystem -or $disk.PartitionStyle -ne 'RAW') { throw '目标不是空白的新磁盘' }
ConvertTo-Json -InputObject @{uniqueId=[string]$disk.UniqueId} -Compress
"#,
                &json!({"physical": physical}),
            )?;
            let unique_id = identity["uniqueId"]
                .as_str()
                .filter(|v| !v.is_empty())
                .context("虚拟磁盘未提供唯一标识")?;
            // Reconfirm native image-to-device mapping after querying storage state.
            if virtual_disk::physical_path(partial)? != physical {
                bail!("虚拟磁盘设备身份发生变化")
            }
            let value = process::powershell(
                INITIALIZE,
                &json!({"physical": physical, "uniqueId": unique_id, "expectedSize": info.virtual_size, "compress": compress, "volumeLabel": volume_label}),
            )?;
            let root = value["root"]
                .as_str()
                .context("格式化完成，但无法获取盘符")?;
            Ok(PathBuf::from(root))
        })();
        if result.is_err() {
            let _ = virtual_disk::detach(partial);
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn folder_and_letter_conflict_is_rejected_before_image_access() {
            let error = mount(MountRequest {
                base: "missing-base.vhdx".into(),
                diff: "missing-diff.vhdx".into(),
                drive_letter: Some('F'),
                mount_folder: Some("missing-folder".into()),
            })
            .unwrap_err();
            assert!(error.to_string().contains("不能同时指定盘符"));
        }
        #[test]
        fn folder_preflight_requires_existing_empty_plain_local_directory() {
            let temp = tempfile::tempdir().unwrap();
            let folder = temp.path().join("挂载 ' $ 文件夹");
            assert!(validate_mount_folder(&folder, false).is_err());
            std::fs::create_dir(&folder).unwrap();
            validate_mount_folder(&folder, false).unwrap();
            assert!(folder_reparse_volume(&folder).unwrap().is_none());
            std::fs::write(folder.join("keep.txt"), b"must remain").unwrap();
            assert!(validate_mount_folder(&folder, false).is_err());
            cleanup_folder_mounts(&[(folder.clone(), r"\\?\Volume{unused}\".into())]).unwrap();
            assert_eq!(
                std::fs::read(folder.join("keep.txt")).unwrap(),
                b"must remain"
            );
            let root = folder.ancestors().last().unwrap();
            assert!(validate_mount_folder(root, false).is_err());
            assert!(validate_mount_folder(Path::new(r"\\nas\share\folder"), false).is_err());
        }
        #[test]
        fn folder_preflight_and_cleanup_refuse_directory_junctions() {
            let temp = tempfile::tempdir().unwrap();
            let target = temp.path().join("target");
            let junction = temp.path().join("junction");
            std::fs::create_dir_all(target.join("empty-child")).unwrap();
            std::fs::write(target.join("keep.txt"), b"must remain").unwrap();
            process::powershell(r#"
New-Item -ItemType Junction -Path ([string]$req.link) -Target ([string]$req.target) -ErrorAction Stop | Out-Null
ConvertTo-Json -InputObject @{ok=$true} -Compress
"#, &json!({"link":junction,"target":target})).unwrap();
            assert!(validate_mount_folder(&junction, false).is_err());
            assert!(validate_mount_folder(&junction.join("empty-child"), false).is_err());
            // A directory junction shares the mount-point tag but points to a
            // filesystem path, so it must never pass as a volume-GUID mount.
            assert!(folder_reparse_volume(&junction).is_err());
            assert!(
                cleanup_folder_mounts(&[(junction.clone(), r"\\?\Volume{unused}\".into())])
                    .is_err()
            );
            assert_eq!(
                std::fs::read(target.join("keep.txt")).unwrap(),
                b"must remain"
            );
            std::fs::remove_dir(junction).unwrap(); // Remove only our junction entry.
        }
        #[test]
        fn repeated_mount_must_match_requested_mode_and_location() {
            let diff = Path::new("fixture-diff.vhdx");
            let mut row = MountedImage {
                image_path: diff.into(),
                parent_path: None,
                volumes: vec![r"C:\mounts\one\".into()],
                kind: "差分".into(),
                read_only: false,
                can_eject: true,
                warning: None,
            };
            assert!(verify_requested_location(
                &[row.clone()],
                diff,
                Some(Path::new(r"c:\mounts\one")),
                None
            )
            .is_ok());
            assert!(verify_requested_location(
                &[row.clone()],
                diff,
                Some(Path::new(r"C:\mounts\two")),
                None
            )
            .is_err());
            assert!(verify_requested_location(&[row.clone()], diff, None, None).is_err());
            row.volumes.push(r"F:\".into());
            assert!(verify_requested_location(
                &[row.clone()],
                diff,
                Some(Path::new(r"C:\mounts\one")),
                None
            )
            .is_err());
            assert!(verify_requested_location(&[row.clone()], diff, None, Some('G')).is_err());
            assert!(verify_requested_location(&[row], diff, None, Some('f')).is_ok());
        }
        #[test]
        fn folder_assignment_refuses_ambiguous_partitions_and_removes_cached_letter() {
            let mock = r#"
function Get-Disk { [pscustomobject]@{Number=999;IsBoot=$false;IsSystem=$false;IsReadOnly=$false;IsOffline=$false;PartitionStyle='GPT'} }
function Get-Partition {
    param($DiskNumber,$PartitionNumber)
    $paths = @('\\?\Volume{fixture}\')
    if ($script:folderAdded) { $paths += [string]$req.folder }
    if (-not $script:letterRemoved) { $paths += 'F:\' }
    foreach ($n in 1..([int]$req.count)) { [pscustomobject]@{DiskNumber=999;PartitionNumber=$n;Type='Basic';IsBoot=$false;IsSystem=$false;DriveLetter=$(if ($script:letterRemoved) {$null} else {'F'});AccessPaths=$paths} }
}
function Get-Volume { [pscustomobject]@{FileSystemType='NTFS'} }
function Set-Partition { param($NoDefaultDriveLetter) if (-not $NoDefaultDriveLetter) { throw 'must disable default letters' } }
function Add-PartitionAccessPath { param($AccessPath,$AssignDriveLetter) if ($AssignDriveLetter -or $AccessPath -ne $req.folder) { throw 'unexpected drive assignment' }; $script:folderAdded=$true }
function Remove-PartitionAccessPath { param($AccessPath) if ($AccessPath -ne 'F:\') { throw 'only fixture cached letter may be removed' }; $script:letterRemoved=$true }
"#;
            let script = format!("{mock}\n{ASSIGN_VOLUMES}");
            let request = |count| json!({"physical":r"\\.\PhysicalDrive999","folder":r"C:\mounts\fixture\","count":count});
            let result = process::powershell(&script, &request(1)).unwrap();
            assert_eq!(
                result["partitions"][0]["DriveLetter"],
                serde_json::Value::Null
            );
            assert!(result["partitions"][0]["AccessPaths"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p.as_str() != Some(r"F:\")));
            assert!(process::powershell(&script, &request(2)).is_err());
        }
        #[test]
        fn discovery_letter_fallback_requires_an_accessible_root() {
            let mock = r#"
function Get-CimInstance { @() }
function Get-Disk { [pscustomobject]@{Number=999;IsBoot=$false;IsSystem=$false;IsReadOnly=$false;PartitionStyle='GPT'} }
function Get-DiskImage { [pscustomobject]@{ImagePath='test-diff.vhdx'} }
function Get-Partition {
    param($DriveLetter,$DiskNumber)
    if ($DriveLetter) { [pscustomobject]@{DiskNumber=$req.mappedDisk;PartitionNumber=$req.mappedPartition} }
    else { [pscustomobject]@{DiskNumber=999;PartitionNumber=2;Type='Basic';DriveLetter=[char]'F';AccessPaths=@('\\?\Volume{fixture}\');IsBoot=$false;IsSystem=$false} }
}
function Test-Path {
    param($LiteralPath,$PathType)
    if ($PathType -ne 'Container' -or $LiteralPath -ne 'F:\') { throw 'root accessibility check was bypassed or malformed' }
    [bool]$req.accessible
}
"#;
            let script = format!("{mock}\n{DISCOVER}");
            let request = |accessible, mapped_disk, mapped_partition| json!({"accessible":accessible,"mappedDisk":mapped_disk,"mappedPartition":mapped_partition,"physical":r"\\.\PhysicalDrive999","expectedVolumes":["F:\\"],"candidates":[{"physical":r"\\.\PhysicalDrive999","image":"test-diff.vhdx"}]});
            let ready = process::powershell(&script, &request(true, 999, 2)).unwrap();
            assert_eq!(ready[0]["volumes"], json!(["F:\\"]));
            let unavailable = process::powershell(&script, &request(false, 999, 2)).unwrap();
            assert_eq!(unavailable[0]["volumes"], json!([]));
            let eject = format!("{mock}\n{EJECT_ROOTS}");
            assert!(process::powershell(&eject, &request(true, 999, 2)).is_ok());
            for (disk, partition) in [(1000, 2), (999, 3)] {
                let wrong = process::powershell(&script, &request(true, disk, partition)).unwrap();
                assert_eq!(wrong[0]["volumes"], json!([]));
                assert_eq!(wrong[0]["can_eject"], json!(false));
                assert!(process::powershell(&eject, &request(true, disk, partition)).is_err());
            }
        }
        #[test]
        fn partial_mount_is_rejected_before_disk_operations() {
            for (base, diff) in [
                ("base.vhdx.partial", "diff.vhdx"),
                ("base.vhdx", "diff.vhdx.PARTIAL"),
            ] {
                let error = mount(MountRequest {
                    base: base.into(),
                    diff: diff.into(),
                    drive_letter: None,
                    mount_folder: None,
                })
                .unwrap_err();
                assert!(error.to_string().contains(".partial"));
            }
        }
        #[test]
        fn eject_refuses_missing_or_incomplete_volume_discovery() {
            // Mock storage queries: test the actual safety script without
            // opening, attaching, locking, or detaching a real disk.
            let mock = r#"
function Get-Disk { [pscustomobject]@{Number=999;IsBoot=$false;IsSystem=$false;PartitionStyle=$req.style} }
function Get-CimInstance { @() }
function Get-Partition {
    if ($req.style -eq 'RAW') { throw 'RAW disks must not enumerate partitions' }
    foreach ($paths in @($req.partitions)) {
        [pscustomobject]@{IsBoot=$false;IsSystem=$false;Type='Basic';AccessPaths=@($paths)}
    }
}
"#;
            let script = format!("{mock}\n{EJECT_ROOTS}");
            let physical = r"\\.\PhysicalDrive999";
            assert!(process::powershell(&script, &json!({"physical":physical,"style":"GPT","partitions":[[]],"expectedVolumes":["F:\\"]})).is_err());
            assert!(process::powershell(&script, &json!({"physical":physical,"style":"GPT","partitions":[["F:\\"],[]],"expectedVolumes":["F:\\"]})).is_err());
            let valid = process::powershell(&script, &json!({"physical":physical,"style":"GPT","partitions":[["F:\\"]],"expectedVolumes":["F:\\"]})).unwrap();
            assert_eq!(valid, json!(["F:\\"]));
            let raw = process::powershell(
                &script,
                &json!({"physical":physical,"style":"RAW","partitions":[],"expectedVolumes":[]}),
            )
            .unwrap();
            assert_eq!(raw, json!([]));
        }
        #[test]
        fn mount_result_requires_identified_writable_volume() {
            let path = Path::new("test-diff.vhdx");
            assert!(verify_mount_rows(Vec::new(), path).is_err());
            let row = MountedImage {
                image_path: path.into(),
                parent_path: None,
                volumes: vec!["F:\\".into()],
                kind: "差分".into(),
                read_only: false,
                can_eject: true,
                warning: None,
            };
            assert!(verify_mount_rows(vec![row.clone()], path).is_ok());
            let mut unreadable = row.clone();
            unreadable.read_only = true;
            assert!(verify_mount_rows(vec![unreadable], path).is_err());
            let mut ambiguous = row;
            ambiguous.warning = Some("无法验证磁盘状态".into());
            ambiguous.volumes.clear();
            let error = verify_mount_rows(vec![ambiguous], path).unwrap_err();
            assert!(
                error.to_string().contains("无法验证磁盘状态"),
                "an empty volume list must not hide the discovery failure"
            );
        }
        #[test]
        fn powershell_worker_scripts_parse_without_execution() {
            for script in [
                DISCOVER,
                ASSIGN_VOLUMES,
                INITIALIZE,
                EJECT_ROOTS,
                MOUNT_DIAGNOSTICS,
                VERIFY_FOLDER,
            ] {
                process::powershell(r#"
$tokens = $null; $parseErrors = $null
[System.Management.Automation.Language.Parser]::ParseInput([string]$req.script, [ref]$tokens, [ref]$parseErrors) | Out-Null
if ($parseErrors.Count) { throw ($parseErrors | Out-String) }
ConvertTo-Json -InputObject @{ok=$true} -Compress
"#, &json!({"script": script})).unwrap();
            }
        }
    }
}
