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
    let root = backend::initialize_new_virtual_disk(&partial, true)?;
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
    })?;
    let root = mounted_root(&diff)?;
    assert_eq!(
        fs::read(root.join("base-sentinel.txt"))?,
        b"immutable base fixture"
    );
    fs::write(root.join("child-only.txt"), b"persistent local writes")?;
    backend::unmount(&diff)?;
    assert_eq!(digest(&base)?, base_hash, "base changed after guest write");

    backend::mount(MountRequest {
        base: base.clone(),
        diff: diff.clone(),
        drive_letter: None,
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
    assert!(backend::relocate(&diff, &unrelated).is_err());
    fs::copy(&base, &moved)?;
    backend::relocate(&diff, &moved)?;
    backend::mount(MountRequest {
        base: moved.clone(),
        diff: diff.clone(),
        drive_letter: None,
    })?;
    let root = mounted_root(&diff)?;
    assert_eq!(
        fs::read(root.join("child-only.txt"))?,
        b"persistent local writes"
    );
    backend::unmount(&diff)?;
    assert_eq!(digest(&base)?, base_hash);
    assert_eq!(digest(&moved)?, base_hash);

    let old_base = scratch.image("legacy.vhd");
    let old_diff = scratch.image("legacy-diff.vhd");
    virtual_disk::create_dynamic(&old_base, 64 * 1024 * 1024)?;
    let old_hash = digest(&old_base)?;
    virtual_disk::create_difference(&old_base, &old_diff)?;
    virtual_disk::validate_parent(&old_diff, &old_base)?;
    assert_eq!(digest(&old_base)?, old_hash);
    Ok(())
}
