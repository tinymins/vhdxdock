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
pub fn relocate(_: &Path, _: &Path) -> Result<()> {
    anyhow::bail!("重新定位功能仅支持 Windows")
}
#[cfg(not(windows))]
pub fn initialize_new_virtual_disk(_: &Path, _: bool) -> Result<PathBuf> {
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

    const DISCOVER: &str = r#"
$rows = @()
$pageRoots = @(Get-CimInstance Win32_PageFileUsage -ErrorAction Stop | ForEach-Object { [IO.Path]::GetPathRoot($_.Name) })
foreach ($candidate in @($req.candidates)) {
    $physical = [string]$candidate.physical
    $image = [string]$candidate.image
    $protected = $true
    $readOnly = $true
    $volumes = @()
    $warning = $null
    try {
        if ($physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw '无法识别物理设备名称' }
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
            foreach ($access in @($partition.AccessPaths)) {
                if ($access -and $access -notlike '\\?\Volume{*') {
                    $volumes += [string]$access
                    if ($pageRoots -contains [string]$access) { $protected = $true }
                }
            }
        }
        if ($protected) { $warning = '系统、启动或分页文件所在磁盘，禁止卸载' }
    } catch {
        $warning = '无法验证磁盘安全状态：' + $_.Exception.Message
        $protected = $true
    }
    if (-not $image) { $image = $physical; $protected = $true; $warning = '无法查询基础文件路径，禁止卸载' }
    $rows += [pscustomobject]@{
        image_path = $image; parent_path = $null; volumes = @($volumes | Select-Object -Unique)
        kind = '未知'; read_only = $readOnly; can_eject = (-not $protected); warning = $warning
    }
}
ConvertTo-Json -InputObject @($rows) -Depth 5 -Compress
"#;

    pub fn list_mounted() -> Result<Vec<MountedImage>> {
        let candidates = virtual_disk::attached_paths()?;
        let payload = json!({"candidates": candidates.iter().map(|p| json!({"physical": p.physical, "image": p.image})).collect::<Vec<_>>()});
        let value = process::powershell(DISCOVER, &payload)?;
        let mut rows: Vec<MountedImage> =
            serde_json::from_value(value).context("Windows 返回的挂载列表无效")?;
        for row in &mut rows {
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
}
if ($eligible.Count -eq 0) { throw '镜像没有 Windows 可识别的文件系统' }
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
ConvertTo-Json -InputObject @{ok=$true} -Compress
"#;

    pub fn mount(request: MountRequest) -> Result<Vec<MountedImage>> {
        let base = paths::resolve(&request.base)?;
        let diff = paths::resolve(&request.diff)?;
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
            bail!(
                "差分镜像仍指向 {}；请使用重新定位基础镜像",
                actual_parent.display()
            )
        }
        if child.attached {
            return verify_mount_rows(mounted, &diff);
        }
        virtual_disk::attach(&diff)?;
        let assigned = (|| -> Result<()> {
            let physical = virtual_disk::physical_path(&diff)?;
            process::powershell(
                ASSIGN_VOLUMES,
                &json!({"physical": physical, "letter": request.drive_letter.map(|c| c.to_ascii_uppercase().to_string())}),
            )?;
            Ok(())
        })();
        if let Err(error) = assigned {
            if let Err(detach_error) = virtual_disk::detach(&diff) {
                bail!("{error:#}；挂载回滚失败，请刷新并手动卸载：{detach_error:#}")
            }
            return Err(error);
        }
        verify_mount_rows(list_mounted()?, &diff)
            .context("镜像已连接，但挂载状态未通过检查；请刷新查看实际状态")
    }
    fn ensure_finalized(path: &Path) -> Result<()> {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("partial"))
        {
            bail!(
                ".partial 是尚未完成的镜像，不能挂载或重新定位：{}",
                path.display()
            )
        }
        Ok(())
    }
    fn verify_mount_rows(rows: Vec<MountedImage>, diff: &Path) -> Result<Vec<MountedImage>> {
        let row = rows
            .iter()
            .find(|r| paths::same_path(&r.image_path, diff))
            .context("Windows 未返回目标差分镜像的挂载记录")?;
        if row.volumes.is_empty() {
            bail!("差分镜像没有可打开的卷")
        }
        if row.read_only {
            bail!("差分镜像处于只读状态")
        }
        if let Some(warning) = &row.warning {
            bail!("差分镜像状态无法确认：{warning}")
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
    fn verify_eject_roots(roots: &[String], image: &MountedImage) -> Result<()> {
        if !image.volumes.is_empty() && roots.is_empty() {
            bail!("已挂载磁盘未返回任何卷路径，拒绝绕过卷锁卸载")
        }
        Ok(())
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
        let roots = process::powershell(
            EJECT_ROOTS,
            &json!({"physical": virtual_disk::physical_path(&path)?, "expectedVolumes": image.volumes}),
        )?;
        let roots: Vec<String> = serde_json::from_value(roots)?;
        verify_eject_roots(&roots, &image)?;
        virtual_disk::safe_detach(&path, &roots)
    }
    pub fn relocate(diff: &Path, base: &Path) -> Result<()> {
        let diff = paths::resolve(diff)?;
        let base = paths::resolve(base)?;
        ensure_finalized(&base)?;
        ensure_finalized(&diff)?;
        paths::ensure_local(&diff)?;
        if paths::same_path(&diff, &base) {
            bail!("基础镜像与差分不能是同一个文件")
        }
        virtual_disk::relocate_parent(&diff, &base)
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
$formatArgs = @{FileSystem='NTFS'; AllocationUnitSize=4096; NewFileSystemLabel='VhdxDock'; Confirm=$false; ErrorAction='Stop'}
if ([bool]$req.compress) { $formatArgs.Compress = $true }
$partition | Format-Volume @formatArgs | Out-Null
$partition = Get-Partition -DiskNumber $number -PartitionNumber $partition.PartitionNumber -ErrorAction Stop
if (-not $partition.DriveLetter) { throw '新分区未获得盘符' }
ConvertTo-Json -InputObject @{root=([string]$partition.DriveLetter + ':\')} -Compress
"#;
    pub fn initialize_new_virtual_disk(partial: &Path, compress: bool) -> Result<PathBuf> {
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
                &json!({"physical": physical, "uniqueId": unique_id, "expectedSize": info.virtual_size, "compress": compress}),
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
        fn partial_mount_and_relocation_are_rejected_before_disk_operations() {
            for (base, diff) in [
                ("base.vhdx.partial", "diff.vhdx"),
                ("base.vhdx", "diff.vhdx.PARTIAL"),
            ] {
                let error = mount(MountRequest {
                    base: base.into(),
                    diff: diff.into(),
                    drive_letter: None,
                })
                .unwrap_err();
                assert!(error.to_string().contains(".partial"));
                let error = relocate(Path::new(diff), Path::new(base)).unwrap_err();
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
            assert!(verify_mount_rows(vec![ambiguous], path).is_err());
        }
        #[test]
        fn powershell_worker_scripts_parse_without_execution() {
            for script in [DISCOVER, ASSIGN_VOLUMES, INITIALIZE, EJECT_ROOTS] {
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
