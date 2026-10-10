#![cfg(windows)]
//! Opt-in, elevated integration tests. Every writable object belongs to this
//! test's fresh tempfile directory; existing images are never selected.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
use vhdxdock::{backend, models::MountRequest, paths, process, virtual_disk};

struct Scratch {
    dir: tempfile::TempDir,
    images: Vec<PathBuf>,
}
impl Scratch {
    fn new() -> Result<Self> {
        Ok(Self {
            dir: tempfile::tempdir()?,
            images: Vec::new(),
        })
    }
    fn image(&mut self, name: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        self.images.push(path.clone());
        path
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        for path in self.images.iter().rev() {
            if path.exists() {
                if virtual_disk::physical_path(path).is_ok() {
                    let _ = virtual_disk::detach(path);
                }
                if let Ok(meta) = fs::metadata(path) {
                    let mut permissions = meta.permissions();
                    // Windows-only: clear FILE_ATTRIBUTE_READONLY on our own
                    // temporary fixtures so tempfile can remove them.
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(false);
                    let _ = fs::set_permissions(path, permissions);
                }
            }
        }
    }
}
fn digest(path: &Path) -> Result<Vec<u8>> {
    Ok(Sha256::digest(fs::read(path)?).to_vec())
}
fn enable_test() -> Result<()> {
    anyhow::ensure!(
        std::env::var("VHDXDOCK_RUN_DISK_TESTS").as_deref() == Ok("1"),
        "set VHDXDOCK_RUN_DISK_TESTS=1 to permit scratch-disk tests"
    );
    let result = process::powershell(
        r#"
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
ConvertTo-Json -InputObject @{admin=$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)} -Compress
"#,
        &serde_json::json!({}),
    )?;
    anyhow::ensure!(
        result["admin"].as_bool() == Some(true),
        "scratch-disk tests require an elevated Windows process"
    );
    Ok(())
}
fn mounted_root(diff: &Path) -> Result<PathBuf> {
    backend::list_mounted()?
        .into_iter()
        .find(|row| paths::same_path(&row.image_path, diff))
        .and_then(|row| row.volumes.first().cloned())
        .map(PathBuf::from)
        .context("scratch child was attached but no drive root was discovered")
}

#[test]
#[ignore = "requires Windows elevation and VHDXDOCK_RUN_DISK_TESTS=1; only fresh scratch images are modified"]
fn scratch_differencing_lifecycle_preserves_base() -> Result<()> {
    enable_test()?;
    let mut scratch = Scratch::new()?;
    let partial = scratch.image("archive.vhdx.partial");
    let base = scratch.image("archive.vhdx");
    let diff = scratch.image("archive-diff.vhdx");
    let moved = scratch.image("moved.vhdx");
    let unrelated = scratch.image("unrelated.vhdx");
    virtual_disk::create_dynamic(&partial, 128 * 1024 * 1024)?;
    let root = backend::initialize_new_virtual_disk(&partial, true, "Scratch archive")?;
    fs::write(root.join("base-sentinel.txt"), b"immutable base fixture")?;
    virtual_disk::detach(&partial)?;
    fs::rename(&partial, &base)?;
    let mut readonly = fs::metadata(&base)?.permissions();
    readonly.set_readonly(true);
    fs::set_permissions(&base, readonly)?;
    let base_hash = digest(&base)?;

    backend::mount(MountRequest {
        base: base.clone(),
        diff: diff.clone(),
        drive_letter: None,
        mount_folder: None,
    })?;
    let root = mounted_root(&diff)?;
    assert_eq!(
        fs::read(root.join("base-sentinel.txt"))?,
        b"immutable base fixture"
    );
    fs::write(root.join("child-only.txt"), b"persistent local writes")?;
    let original_device = virtual_disk::physical_path(&diff)?;
    let repeated = backend::mount(MountRequest {
        base: base.clone(),
        diff: diff.clone(),
        drive_letter: None,
        mount_folder: None,
    })?;
    assert_eq!(
        repeated
            .iter()
            .filter(|row| paths::same_path(&row.image_path, &diff))
            .count(),
        1,
        "repeated mount created a duplicate disk record"
    );
    assert_eq!(
        virtual_disk::physical_path(&diff)?,
        original_device,
        "repeated mount changed the physical device"
    );
    assert_eq!(
        mounted_root(&diff)?,
        root,
        "repeated mount changed the volume root"
    );
    assert_eq!(
        fs::read(root.join("child-only.txt"))?,
        b"persistent local writes",
        "repeated mount lost local changes"
    );
    backend::unmount(&diff)?;
    assert_eq!(digest(&base)?, base_hash, "base changed after guest write");

    backend::mount(MountRequest {
        base: base.clone(),
        diff: diff.clone(),
        drive_letter: None,
        mount_folder: None,
    })?;
    let root = mounted_root(&diff)?;
    assert_eq!(
        fs::read(root.join("child-only.txt"))?,
        b"persistent local writes"
    );
    // A live file handle must prevent normal eject; no forced detach is used.
    let busy = fs::File::open(root.join("child-only.txt"))?;
    assert!(
        backend::unmount(&diff).is_err(),
        "busy volume was forcibly ejected"
    );
    drop(busy);
    backend::unmount(&diff)?;

    virtual_disk::create_dynamic(&unrelated, 128 * 1024 * 1024)?;
    assert!(virtual_disk::validate_parent(&diff, &unrelated).is_err());
    let unrelated_hash = digest(&unrelated)?;
    // A real move removes the original parent path. Automatic recovery must
    // still work, and selecting an unrelated replacement must not mutate diff.
    fs::rename(&base, &moved)?;
    assert!(!base.exists(), "the old parent path must really be absent");
    assert_eq!(digest(&moved)?, base_hash);
    let diff_before_rejected_mount = digest(&diff)?;
    assert!(
        backend::mount(MountRequest {
            base: unrelated.clone(),
            diff: diff.clone(),
            drive_letter: None,
            mount_folder: None,
        })
        .is_err(),
        "mount accepted an unrelated replacement parent"
    );
    assert_eq!(
        digest(&diff)?,
        diff_before_rejected_mount,
        "rejected replacement parent changed the difference image"
    );
    assert_eq!(digest(&unrelated)?, unrelated_hash);
    // Supplying the correct moved base is sufficient: no separate relocate
    // operation is part of the application workflow anymore.
    backend::mount(MountRequest {
        base: moved.clone(),
        diff: diff.clone(),
        drive_letter: None,
        mount_folder: None,
    })?;
    let root = mounted_root(&diff)?;
    assert_eq!(
        fs::read(root.join("base-sentinel.txt"))?,
        b"immutable base fixture",
        "automatic parent recovery changed the base data"
    );
    assert_eq!(
        fs::read(root.join("child-only.txt"))?,
        b"persistent local writes"
    );
    let recovered_parent = virtual_disk::inspect(&diff)?
        .parent
        .context("automatically recovered difference has no parent path")?;
    assert!(paths::same_path(&recovered_parent, &moved));
    backend::unmount(&diff)?;
    assert!(!base.exists());
    assert_eq!(digest(&moved)?, base_hash);

    let old_base = scratch.image("legacy.vhd");
    let old_diff = scratch.image("legacy-diff.vhd");
    let old_moved = scratch.image("legacy-moved.vhd");
    let old_unrelated = scratch.image("legacy-unrelated.vhd");
    virtual_disk::create_dynamic(&old_base, 64 * 1024 * 1024)?;
    let old_hash = digest(&old_base)?;
    virtual_disk::create_difference(&old_base, &old_diff)?;
    virtual_disk::validate_parent(&old_diff, &old_base)?;
    assert_eq!(digest(&old_base)?, old_hash);
    // Legacy VHD fixtures are intentionally RAW, so exercise only the native
    // parent operation; backend::mount correctly requires a filesystem.
    fs::rename(&old_base, &old_moved)?;
    assert!(!old_base.exists());
    virtual_disk::create_dynamic(&old_unrelated, 64 * 1024 * 1024)?;
    let old_diff_hash = digest(&old_diff)?;
    assert!(virtual_disk::relocate_parent(&old_diff, &old_unrelated).is_err());
    assert_eq!(digest(&old_diff)?, old_diff_hash);
    virtual_disk::relocate_parent(&old_diff, &old_moved)?;
    virtual_disk::validate_parent(&old_diff, &old_moved)?;
    let old_parent = virtual_disk::inspect(&old_diff)?
        .parent
        .context("relocated legacy VHD difference has no parent path")?;
    assert!(paths::same_path(&old_parent, &old_moved));
    assert_eq!(digest(&old_moved)?, old_hash);
    Ok(())
}

fn create_independent_base(scratch: &mut Scratch, name: &str, sentinel: &[u8]) -> Result<PathBuf> {
    let partial = scratch.image(&format!("{name}.vhdx.partial"));
    let base = scratch.image(&format!("{name}.vhdx"));
    virtual_disk::create_dynamic(&partial, 128 * 1024 * 1024)?;
    let root = backend::initialize_new_virtual_disk(&partial, true, "Scratch archive")?;
    fs::write(root.join("base-sentinel.txt"), sentinel)?;
    virtual_disk::detach(&partial)?;
    fs::rename(&partial, &base)?;
    let mut permissions = fs::metadata(&base)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&base, permissions)?;
    Ok(base)
}

#[test]
#[ignore = "requires Windows elevation and VHDXDOCK_RUN_DISK_TESTS=1; only independent fresh scratch images are modified"]
fn two_independent_mounts_eject_only_the_requested_disk() -> Result<()> {
    enable_test()?;
    let mut scratch = Scratch::new()?;
    // Each base is independently initialized, giving its GPT disk and volumes
    // distinct identities. Copying one base would introduce signature conflicts.
    let base_a = create_independent_base(&mut scratch, "independent-a", b"base A")?;
    let base_b = create_independent_base(&mut scratch, "independent-b", b"base B")?;
    let diff_a = scratch.image("independent-a-diff.vhdx");
    let diff_b = scratch.image("independent-b-diff.vhdx");
    let hash_a = digest(&base_a)?;
    let hash_b = digest(&base_b)?;
    backend::mount(MountRequest {
        base: base_a.clone(),
        diff: diff_a.clone(),
        drive_letter: None,
        mount_folder: None,
    })?;
    let root_a = mounted_root(&diff_a)?;
    fs::write(root_a.join("child-a.txt"), b"writes A")?;
    let rows = backend::mount(MountRequest {
        base: base_b.clone(),
        diff: diff_b.clone(),
        drive_letter: None,
        mount_folder: None,
    })?;
    assert_eq!(
        rows.iter()
            .filter(|row| paths::same_path(&row.image_path, &diff_a))
            .count(),
        1
    );
    assert_eq!(
        rows.iter()
            .filter(|row| paths::same_path(&row.image_path, &diff_b))
            .count(),
        1
    );
    let root_b = mounted_root(&diff_b)?;
    let device_b = virtual_disk::physical_path(&diff_b)?;
    assert_ne!(virtual_disk::physical_path(&diff_a)?, device_b);
    assert_ne!(root_a, root_b);
    assert_eq!(fs::read(root_a.join("base-sentinel.txt"))?, b"base A");
    assert_eq!(fs::read(root_b.join("base-sentinel.txt"))?, b"base B");
    fs::write(root_b.join("child-b.txt"), b"writes B")?;

    backend::unmount(&diff_a)?;
    let rows = backend::list_mounted()?;
    assert!(
        !rows
            .iter()
            .any(|row| paths::same_path(&row.image_path, &diff_a)),
        "ejected disk A remained in the mounted list"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| paths::same_path(&row.image_path, &diff_b))
            .count(),
        1,
        "ejecting A affected B's mounted record"
    );
    assert_eq!(virtual_disk::physical_path(&diff_b)?, device_b);
    assert_eq!(fs::read(root_b.join("child-b.txt"))?, b"writes B");
    fs::write(root_b.join("after-a-eject.txt"), b"B stays writable")?;
    backend::unmount(&diff_b)?;
    assert_eq!(digest(&base_a)?, hash_a);
    assert_eq!(digest(&base_b)?, hash_b);
    Ok(())
}

fn ensure_ntfs_scratch(folder: &Path) -> Result<()> {
    let value = process::powershell(
        r#"
$root = [IO.Path]::GetPathRoot([string]$req.path)
if ($root -notmatch '^[A-Za-z]:\\$') { throw 'Folder-mount scratch root must be on a local drive' }
$volume = Get-Volume -DriveLetter ([char]$root.Substring(0,1)) -ErrorAction Stop
ConvertTo-Json -InputObject @{filesystem=[string]$volume.FileSystemType} -Compress
"#,
        &serde_json::json!({"path": folder}),
    )?;
    anyhow::ensure!(
        value["filesystem"].as_str() == Some("NTFS"),
        "folder-mount tests require their dedicated tempfile directory on NTFS"
    );
    Ok(())
}

fn assert_no_scratch_drive_letters(diff: &Path) -> Result<()> {
    let physical = virtual_disk::physical_path(diff)?;
    let partitions = process::powershell(
        r#"
if ([string]$req.physical -notmatch '^\\\\\.\\PhysicalDrive(\d+)$') { throw 'Invalid scratch device' }
$number = [int]$Matches[1]
ConvertTo-Json -InputObject @(Get-Partition -DiskNumber $number -ErrorAction Stop | ForEach-Object {
    @{letter=[string]$_.DriveLetter; access_paths=@($_.AccessPaths)}
}) -Depth 4 -Compress
"#,
        &serde_json::json!({"physical": physical}),
    )?;
    let partitions = partitions
        .as_array()
        .context("expected scratch partition array")?;
    anyhow::ensure!(
        !partitions.is_empty(),
        "scratch folder image has no partitions"
    );
    for partition in partitions {
        let letter = partition["letter"].as_str().unwrap_or_default();
        anyhow::ensure!(
            letter.trim_matches('\0').is_empty(),
            "folder-mounted scratch image unexpectedly has a drive letter: {partition}"
        );
    }
    Ok(())
}

fn assert_empty_ordinary_directory(folder: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    let metadata = fs::symlink_metadata(folder)?;
    anyhow::ensure!(
        metadata.is_dir(),
        "mount folder must remain an ordinary directory"
    );
    anyhow::ensure!(
        metadata.file_attributes() & 0x400 == 0,
        "unmount left a reparse point behind: {}",
        folder.display()
    );
    anyhow::ensure!(
        fs::read_dir(folder)?.next().is_none(),
        "unmount did not restore an empty mount folder"
    );
    Ok(())
}

fn assert_requires_eject(error: anyhow::Error) {
    let message = format!("{error:#}");
    assert!(
        message.contains("卸载") || message.contains("弹出"),
        "changing an attached target must explain that eject is required: {message}"
    );
}

#[test]
#[ignore = "requires Windows elevation and VHDXDOCK_RUN_DISK_TESTS=1; only fresh scratch NTFS folder mounts are modified"]
fn scratch_folder_mount_is_persistent_and_cleans_its_mount_point() -> Result<()> {
    enable_test()?;
    let mut scratch = Scratch::new()?;
    ensure_ntfs_scratch(scratch.dir.path())?;
    let base = create_independent_base(&mut scratch, "folder-base", b"folder immutable base")
        .context("folder lifecycle: create first scratch base")?;
    let diff = scratch.image("folder-diff.vhdx");
    let base_hash = digest(&base)?;
    let folder = scratch.dir.path().join("挂载目录 ' $ 中文");
    let other_folder = scratch.dir.path().join("other empty target");
    fs::create_dir(&folder)?;
    fs::create_dir(&other_folder)?;
    let conflict_diff = scratch.image("rejected-conflicting-mode-diff.vhdx");
    assert!(
        backend::mount(MountRequest {
            base: base.clone(),
            diff: conflict_diff.clone(),
            drive_letter: Some('Z'),
            mount_folder: Some(folder.clone()),
        })
        .is_err(),
        "request containing both a drive letter and folder must be rejected"
    );
    assert_empty_ordinary_directory(&folder)?;
    assert!(virtual_disk::physical_path(&conflict_diff).is_err());
    let request = || MountRequest {
        base: base.clone(),
        diff: diff.clone(),
        drive_letter: None,
        mount_folder: Some(folder.clone()),
    };
    let rows = backend::mount(request()).context("folder lifecycle: first folder mount")?;
    let row = rows
        .iter()
        .find(|row| paths::same_path(&row.image_path, &diff))
        .context("folder-mounted diff missing from discovery")?;
    assert!(
        row.volumes
            .iter()
            .any(|path| paths::same_path(Path::new(path), &folder)),
        "mounted list does not expose the requested folder: {:?}",
        row.volumes
    );
    assert_eq!(
        fs::read(folder.join("base-sentinel.txt"))?,
        b"folder immutable base"
    );
    assert_no_scratch_drive_letters(&diff)?;
    fs::write(folder.join("folder-only.txt"), b"persistent folder overlay")?;
    let device = virtual_disk::physical_path(&diff)?;
    let repeated =
        backend::mount(request()).context("folder lifecycle: repeat same folder mount")?;
    assert_eq!(virtual_disk::physical_path(&diff)?, device);
    assert_eq!(
        repeated
            .iter()
            .filter(|row| paths::same_path(&row.image_path, &diff))
            .count(),
        1
    );
    assert_no_scratch_drive_letters(&diff)?;
    assert_requires_eject(
        backend::mount(MountRequest {
            base: base.clone(),
            diff: diff.clone(),
            drive_letter: None,
            mount_folder: Some(other_folder.clone()),
        })
        .expect_err("attached diff must not silently switch mount folders"),
    );
    assert_requires_eject(
        backend::mount(MountRequest {
            base: base.clone(),
            diff: diff.clone(),
            drive_letter: None,
            mount_folder: None,
        })
        .expect_err("attached diff must not silently switch to a drive letter"),
    );
    assert_eq!(
        fs::read(folder.join("folder-only.txt"))?,
        b"persistent folder overlay"
    );
    assert_empty_ordinary_directory(&other_folder)?;

    // An independently initialized second image avoids duplicate GPT IDs and
    // proves ejecting a folder mount does not affect another folder volume.
    let base_b = create_independent_base(&mut scratch, "folder-base-b", b"second folder base")
        .context("folder lifecycle: create independent second base")?;
    let hash_b = digest(&base_b)?;
    let diff_b = scratch.image("folder-diff-b.vhdx");
    let folder_b = scratch.dir.path().join("independent folder B");
    fs::create_dir(&folder_b)?;
    backend::mount(MountRequest {
        base: base_b.clone(),
        diff: diff_b.clone(),
        drive_letter: None,
        mount_folder: Some(folder_b.clone()),
    })
    .context("folder lifecycle: mount independent second folder")?;
    let device_b = virtual_disk::physical_path(&diff_b)?;
    assert_ne!(device, device_b);
    assert_no_scratch_drive_letters(&diff_b)?;
    assert_eq!(
        fs::read(folder_b.join("base-sentinel.txt"))?,
        b"second folder base"
    );
    fs::write(folder_b.join("b-only.txt"), b"independent folder writes")?;

    let busy = fs::File::open(folder.join("folder-only.txt"))?;
    assert!(
        backend::unmount(&diff).is_err(),
        "folder volume with a live file handle was forcibly ejected"
    );
    assert_eq!(
        fs::read(folder.join("folder-only.txt"))?,
        b"persistent folder overlay"
    );
    drop(busy);
    backend::unmount(&diff)
        .context("folder lifecycle: first clean unmount after dropping busy handle")?;
    assert_empty_ordinary_directory(&folder)?;
    assert_eq!(digest(&base)?, base_hash);
    assert_eq!(virtual_disk::physical_path(&diff_b)?, device_b);
    assert_eq!(
        fs::read(folder_b.join("b-only.txt"))?,
        b"independent folder writes"
    );
    fs::write(
        folder_b.join("after-a-eject.txt"),
        b"second folder stays writable",
    )?;
    // Folder mode may persist NoDefaultDriveLetter in the child partition.
    // After eject, switching back to automatic letter mode must explicitly
    // restore a usable letter, while preserving the same local difference.
    backend::mount(MountRequest {
        base: base.clone(),
        diff: diff.clone(),
        drive_letter: None,
        mount_folder: None,
    })
    .context("folder lifecycle: switch from folder to automatic drive letter")?;
    let letter_root = mounted_root(&diff)?;
    let letter_text = letter_root.to_string_lossy();
    let letter_bytes = letter_text.as_bytes();
    assert!(
        letter_bytes.len() >= 3
            && letter_bytes[0].is_ascii_alphabetic()
            && letter_bytes[1] == b':'
            && letter_bytes[2] == b'\\',
        "automatic letter mode did not restore a drive root after folder mode: {}",
        letter_root.display()
    );
    assert_eq!(
        fs::read(letter_root.join("folder-only.txt"))?,
        b"persistent folder overlay"
    );
    fs::write(
        letter_root.join("written-via-letter.txt"),
        b"writes after folder to letter switch",
    )?;
    assert_empty_ordinary_directory(&folder)?;
    backend::unmount(&diff).context("folder lifecycle: unmount automatic drive letter")?;
    assert_eq!(digest(&base)?, base_hash);
    backend::mount(request())
        .context("folder lifecycle: remount original folder after drive-letter mode")?;
    assert_no_scratch_drive_letters(&diff)?;
    assert_eq!(
        fs::read(folder.join("folder-only.txt"))?,
        b"persistent folder overlay"
    );
    assert_eq!(
        fs::read(folder.join("written-via-letter.txt"))?,
        b"writes after folder to letter switch",
        "switching back to folder mode lost writes made through the drive letter"
    );
    fs::write(folder.join("after-remount.txt"), b"folder remains writable")?;
    backend::unmount(&diff).context("folder lifecycle: final unmount of first folder")?;
    assert_empty_ordinary_directory(&folder)?;
    assert_eq!(digest(&base)?, base_hash);
    assert_eq!(virtual_disk::physical_path(&diff_b)?, device_b);
    assert_eq!(
        fs::read(folder_b.join("after-a-eject.txt"))?,
        b"second folder stays writable"
    );
    backend::unmount(&diff_b)
        .context("folder lifecycle: final unmount of independent second folder")?;
    assert_empty_ordinary_directory(&folder_b)?;
    assert_eq!(digest(&base_b)?, hash_b);

    let rejected_diff = scratch.image("rejected-nonempty-diff.vhdx");
    let nonempty = scratch.dir.path().join("nonempty target");
    fs::create_dir(&nonempty)?;
    fs::write(nonempty.join("keep.txt"), b"pre-existing host data")?;
    assert!(
        backend::mount(MountRequest {
            base: base.clone(),
            diff: rejected_diff.clone(),
            drive_letter: None,
            mount_folder: Some(nonempty.clone())
        })
        .is_err(),
        "nonempty host directory was accepted as a mount target"
    );
    assert_eq!(
        fs::read(nonempty.join("keep.txt"))?,
        b"pre-existing host data"
    );
    assert!(
        virtual_disk::physical_path(&rejected_diff).is_err(),
        "rejected nonempty folder left a child attached"
    );
    if rejected_diff.exists() {
        assert!(!virtual_disk::inspect(&rejected_diff)?.attached);
    }

    // Both the link and its target are inside this test's scratch directory.
    // Some Windows runners do not grant symlink creation even when elevated.
    let link_target = scratch.dir.path().join("link target");
    let linked_folder = scratch.dir.path().join("directory symlink");
    fs::create_dir(&link_target)?;
    match std::os::windows::fs::symlink_dir(&link_target, &linked_folder) {
        Ok(()) => {
            let linked_diff = scratch.image("rejected-symlink-diff.vhdx");
            assert!(
                backend::mount(MountRequest {
                    base: base.clone(),
                    diff: linked_diff.clone(),
                    drive_letter: None,
                    mount_folder: Some(linked_folder.clone())
                })
                .is_err(),
                "existing directory symlink was accepted as a mount target"
            );
            // The target is deliberately empty: rejection must be about the
            // existing reparse point, not merely nonempty-directory validation.
            assert_empty_ordinary_directory(&link_target)?;
            assert!(fs::symlink_metadata(&linked_folder)?
                .file_type()
                .is_symlink());
            assert!(
                virtual_disk::physical_path(&linked_diff).is_err(),
                "rejected symlink left a child attached"
            );
            if linked_diff.exists() {
                assert!(!virtual_disk::inspect(&linked_diff)?.attached);
            }
            fs::remove_dir(&linked_folder)?;
        }
        Err(error) => {
            eprintln!("Scratch directory-symlink negative case unavailable on this runner: {error}")
        }
    }
    assert_eq!(digest(&base)?, base_hash);
    Ok(())
}
