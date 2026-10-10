//! Explicitly opted-in, elevated Windows integration tests. Each case formats
//! only a brand-new image under its dedicated temporary directory.
#![cfg(windows)]

use anyhow::{bail, ensure, Context, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc, Mutex},
};
use tempfile::TempDir;
use vhdxdock::{
    backend, builder,
    models::{BuildRequest, DiskKind, MountRequest, MountedImage, VerifyMode},
    paths, process, virtual_disk,
};

static BUILDER_TEST_LOCK: Mutex<()> = Mutex::new(());

fn require_explicit_permission_and_admin() -> Result<()> {
    ensure!(
        std::env::var("VHDXDOCK_RUN_DISK_TESTS").as_deref() == Ok("1"),
        "Disk integration tests are disabled. On an elevated Windows scratch/CI machine, set VHDXDOCK_RUN_DISK_TESTS=1 and run cargo test --test windows_builder -- --ignored. These tests create and format only dedicated temporary images."
    );
    let value = process::powershell(
        r#"
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
ConvertTo-Json -InputObject @{admin=$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)} -Compress
"#,
        &json!({}),
    )?;
    ensure!(value["admin"].as_bool() == Some(true), "Disk integration tests require an elevated Administrator token; no scratch image has been created.");
    Ok(())
}

struct ScratchImages {
    directory: Option<TempDir>,
    images: Vec<PathBuf>,
}

impl ScratchImages {
    fn new() -> Result<Self> {
        Ok(Self {
            directory: Some(
                tempfile::Builder::new()
                    .prefix("VhdxDock-builder-")
                    .tempdir()?,
            ),
            images: Vec::new(),
        })
    }

    fn root(&self) -> &Path {
        self.directory
            .as_ref()
            .expect("scratch directory exists")
            .path()
    }

    fn track(&mut self, path: PathBuf) {
        assert!(
            path.starts_with(self.root()),
            "test cleanup must only touch its dedicated scratch directory"
        );
        self.images.push(path);
    }
}

impl Drop for ScratchImages {
    fn drop(&mut self) {
        let mut detached = true;
        for image in self.images.iter().rev() {
            if let Ok(info) = virtual_disk::inspect(image) {
                if info.attached {
                    // This fallback is deliberately restricted to images made
                    // and tracked by this test, never a discovered system disk.
                    if let Err(error) = virtual_disk::detach(image) {
                        eprintln!(
                            "Scratch image cleanup could not detach {}: {error:#}",
                            image.display()
                        );
                        detached = false;
                    }
                }
            }
        }
        if !detached {
            if let Some(directory) = self.directory.take() {
                eprintln!(
                    "Scratch files retained for manual detachment: {}",
                    directory.keep().display()
                );
            }
            return;
        }
        // The builder intentionally seals its base as read-only. Unseal only
        // these dedicated test files so TempDir can remove them afterwards.
        for image in &self.images {
            if let Ok(metadata) = fs::metadata(image) {
                let mut permissions = metadata.permissions();
                // Windows-only test cleanup: remove the DOS read-only attribute
                // from this scratch image, without changing Unix access modes.
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = fs::set_permissions(image, permissions);
            }
        }
    }
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn mounted_volume(rows: &[MountedImage], diff: &Path) -> Result<PathBuf> {
    let row = rows
        .iter()
        .find(|row| paths::same_path(&row.image_path, diff))
        .context("new scratch differential image was absent from the mounted list")?;
    ensure!(
        row.can_eject,
        "scratch disk unexpectedly protected: {:?}",
        row.warning
    );
    ensure!(!row.read_only, "differential scratch disk must be writable");
    let root = row
        .volumes
        .iter()
        .find(|volume| Path::new(volume).is_dir())
        .context("scratch differential image did not expose an accessible volume")?;
    Ok(PathBuf::from(root))
}

fn diagnose_scratch_mount(scratch_root: &Path, diff: &Path) {
    if !diff.starts_with(scratch_root) {
        eprintln!("Refusing diagnostics for image outside this test's scratch directory");
        return;
    }
    eprintln!("Scratch mount failure image: {}", diff.display());
    match virtual_disk::inspect(diff) {
        Ok(info) => eprintln!("Scratch native image state: {info:?}"),
        Err(error) => eprintln!("Scratch native image query failed: {error:#}"),
    }
    let physical = match virtual_disk::physical_path(diff) {
        Ok(physical) => physical,
        Err(error) => {
            eprintln!("Scratch image physical-device query failed: {error:#}");
            return;
        }
    };
    // Diagnostic queries are restricted to the native physical-device mapping
    // of this test's own diff. This script makes no storage state changes.
    let diagnostic = process::powershell(
        r#"
if ([string]$req.physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw 'Invalid scratch physical-device path' }
$number = [int]$Matches[1]
$disk = Get-Disk -Number $number -ErrorAction Stop
$rows = @()
$partitionError = $null
try {
    foreach ($partition in @(Get-Partition -DiskNumber $number -ErrorAction Stop)) {
        $volumes = @()
        $volumeError = $null
        try {
            $volumes = @($partition | Get-Volume -ErrorAction Stop | Select-Object DriveLetter, Path, UniqueId, FileSystem, FileSystemType, FileSystemLabel, HealthStatus, OperationalStatus, Size, SizeRemaining)
        } catch { $volumeError = $_.Exception.Message }
        $rows += [pscustomobject]@{
            partition = ($partition | Select-Object DiskNumber, PartitionNumber, DriveLetter, AccessPaths, Type, GptType, Guid, Offset, Size, IsBoot, IsSystem, IsHidden, IsReadOnly, NoDefaultDriveLetter)
            volumes = @($volumes)
            volume_error = $volumeError
        }
    }
} catch { $partitionError = $_.Exception.Message }
ConvertTo-Json -InputObject @{
    image = [string]$req.image
    physical = [string]$req.physical
    disk = ($disk | Select-Object Number, FriendlyName, UniqueId, Path, Location, BusType, PartitionStyle, Size, IsOffline, IsReadOnly, IsBoot, IsSystem, OperationalStatus, HealthStatus)
    partitions = @($rows)
    partition_error = $partitionError
} -Depth 8 -Compress
"#,
        &json!({"physical": physical, "image": diff}),
    );
    match diagnostic {
        Ok(value) => eprintln!(
            "Scratch Get-Disk/Get-Partition/Get-Volume diagnostics:\n{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        ),
        Err(error) => eprintln!("Scratch storage diagnostics failed: {error:#}"),
    }
}

fn mount_scratch(
    request: MountRequest,
    scratch_root: &Path,
    context: &str,
) -> Result<Vec<MountedImage>> {
    let diff = request.diff.clone();
    match backend::mount(request) {
        Ok(rows) => Ok(rows),
        Err(error) => {
            diagnose_scratch_mount(scratch_root, &diff);
            Err(error).context(context.to_owned())
        }
    }
}

fn scenario(verify: VerifyMode, compress: bool, volume_label: &str) -> Result<()> {
    require_explicit_permission_and_admin()?;
    let _serial = BUILDER_TEST_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("builder test lock poisoned"))?;
    let mut scratch = ScratchImages::new()?;
    let source = scratch.root().join("源文件夹 ' $ 中文");
    let output_dir = scratch.root().join("输出目录 ' $ 中文");
    fs::create_dir_all(source.join(".git"))?;
    fs::create_dir_all(source.join("empty directory"))?;
    fs::create_dir_all(&output_dir)?;
    let chinese_file = "说明 ' $ 内容.txt";
    let contents = "中文源码\n$(this remains literal)\n".as_bytes();
    fs::write(source.join(chinese_file), contents)?;
    fs::write(
        source.join(".git/config"),
        b"[core]\nrepositoryformatversion = 0\n",
    )?;
    fs::write(source.join("hidden.dat"), b"hidden fixture")?;
    process::powershell(
        r#"
foreach ($path in @($req.paths)) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    $item.Attributes = $item.Attributes -bor [IO.FileAttributes]::Hidden
}
ConvertTo-Json -InputObject @{ok=$true} -Compress
"#,
        &json!({"paths": [source.join(".git"), source.join("hidden.dat")]}),
    )?;
    let base = output_dir.join("镜像 ' $ base.vhdx");
    let expected_label = if volume_label.is_empty() {
        base.file_stem()
            .context("base filename stem")?
            .to_str()
            .context("UTF-8 test fixture filename")?
            .to_owned()
    } else {
        volume_label.to_owned()
    };
    ensure!(
        expected_label.encode_utf16().count() <= 32,
        "test fixture volume label exceeds NTFS limit"
    );
    let partial = paths::partial_path(&base);
    let diff = output_dir.join("镜像 ' $ base-diff.vhdx");
    scratch.track(base.clone());
    scratch.track(partial.clone());
    scratch.track(diff.clone());
    let phases = Arc::new(Mutex::new(Vec::new()));
    let updates = phases.clone();
    let result = builder::build(
        BuildRequest {
            source: source.clone(),
            output: base.clone(),
            capacity_gib: 1,
            compress,
            verify,
            volume_label: volume_label.to_owned(),
        },
        Arc::new(AtomicBool::new(false)),
        move |progress| {
            updates
                .lock()
                .expect("phase list lock")
                .push(progress.phase)
        },
    )
    .context("scratch folder-to-VHDX build failed")?;
    ensure!(base.is_file(), "final image missing");
    ensure!(
        !partial.exists(),
        "successful build must remove its .partial suffix"
    );
    ensure!(
        result.files == 3,
        "unexpected source file count: {}",
        result.files
    );
    ensure!(result.output == base, "result output path differs");
    let info = virtual_disk::inspect(&base)?;
    ensure!(
        info.kind == DiskKind::Dynamic && !info.attached && info.parent.is_none(),
        "builder must produce an unmounted dynamic base"
    );
    ensure!(
        info.virtual_size == 1024 * 1024 * 1024,
        "unexpected virtual capacity"
    );
    ensure!(
        fs::metadata(&base)?.permissions().readonly(),
        "completed base must be read-only"
    );
    let initial_hash = sha256(&base)?;
    ensure!(
        result.sha256 == initial_hash,
        "builder's image SHA-256 does not match independent hash"
    );
    let mut checksum_path = base.as_os_str().to_owned();
    checksum_path.push(".sha256");
    ensure!(
        fs::read_to_string(PathBuf::from(checksum_path))?.starts_with(&initial_hash),
        "checksum sidecar missing or invalid"
    );
    {
        let phases = phases.lock().expect("phase list lock");
        ensure!(
            phases.iter().any(|phase| phase == "完成"),
            "builder did not report completion"
        );
        if verify == VerifyMode::Sha256 {
            ensure!(
                phases.iter().any(|phase| phase == "内容校验"),
                "SHA-256 mode did not perform per-file verification"
            );
        }
    }
    let request = || MountRequest {
        base: base.clone(),
        diff: diff.clone(),
        drive_letter: None,
    };
    let rows = mount_scratch(
        request(),
        scratch.root(),
        "creating/mounting scratch differencing disk failed",
    )?;
    let root = mounted_volume(&rows, &diff)?;
    // Query only partitions belonging to this fixture's newly mounted diff.
    // It inherits the NTFS label written into the sealed base image.
    let labels = process::powershell(
        r#"
if ([string]$req.physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw 'Invalid scratch physical-device path' }
$number = [int]$Matches[1]
$labels = @(Get-Partition -DiskNumber $number -ErrorAction Stop | Get-Volume -ErrorAction SilentlyContinue | ForEach-Object { [string]$_.FileSystemLabel })
ConvertTo-Json -InputObject @($labels) -Compress
"#,
        &json!({"physical": virtual_disk::physical_path(&diff)?}),
    ).context("querying inherited scratch NTFS volume label failed")?;
    let labels = labels
        .as_array()
        .context("volume-label query must return an array")?;
    ensure!(
        labels.len() == 1,
        "expected one scratch NTFS volume, got {labels:?}"
    );
    ensure!(
        labels[0].as_str() == Some(expected_label.as_str()),
        "inherited volume label mismatch: expected {expected_label:?}, got {:?}",
        labels[0]
    );
    ensure!(
        fs::read(root.join(chinese_file))? == contents,
        "source content was not copied flat to image root"
    );
    ensure!(
        !root
            .join(source.file_name().context("source file name")?)
            .exists(),
        "image contains an unwanted outer source folder"
    );
    ensure!(
        root.join("empty directory").is_dir(),
        "empty directory missing"
    );
    ensure!(
        fs::read(root.join(".git/config"))? == b"[core]\nrepositoryformatversion = 0\n",
        "hidden .git data missing"
    );
    ensure!(
        fs::read(root.join("hidden.dat"))? == b"hidden fixture",
        "hidden file missing"
    );
    let overlay_file = root.join("overlay-only ' $ 中文.txt");
    fs::write(&overlay_file, b"persisted in difference only")?;
    fs::write(root.join(chinese_file), b"changed in overlay")?;
    backend::unmount(&diff).context("safe scratch disk unload failed")?;
    ensure!(
        sha256(&base)? == initial_hash,
        "base changed after differential writes"
    );
    let rows = mount_scratch(
        request(),
        scratch.root(),
        "remount of existing scratch difference failed",
    )?;
    let root = mounted_volume(&rows, &diff)?;
    ensure!(
        fs::read(root.join("overlay-only ' $ 中文.txt"))? == b"persisted in difference only",
        "difference did not preserve created file"
    );
    ensure!(
        fs::read(root.join(chinese_file))? == b"changed in overlay",
        "difference did not preserve modified file"
    );
    ensure!(
        fs::read(source.join(chinese_file))? == contents,
        "source fixture was changed"
    );
    backend::unmount(&diff)?;
    ensure!(
        !virtual_disk::inspect(&diff)?.attached,
        "scratch difference is still mounted"
    );
    if sha256(&base)? != initial_hash {
        bail!("base SHA-256 changed after remount lifecycle");
    }
    Ok(())
}

#[test]
#[ignore = "Requires explicit VHDXDOCK_RUN_DISK_TESTS=1 and elevated Windows; formats dedicated scratch VHDX files only"]
fn metadata_builder_flat_copy_and_persistent_overlay() -> Result<()> {
    scenario(VerifyMode::Metadata, false, "剑三 ' $ 归档")
}

#[test]
#[ignore = "Requires explicit VHDXDOCK_RUN_DISK_TESTS=1 and elevated Windows; formats dedicated scratch VHDX files only"]
fn sha256_compressed_builder_flat_copy_and_persistent_overlay() -> Result<()> {
    scenario(VerifyMode::Sha256, true, "")
}
