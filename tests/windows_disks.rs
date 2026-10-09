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
    let original_device = virtual_disk::physical_path(&diff)?;
    let repeated = backend::mount(MountRequest {
        base: base.clone(),
        diff: diff.clone(),
        drive_letter: None,
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

fn create_independent_base(scratch: &mut Scratch, name: &str, sentinel: &[u8]) -> Result<PathBuf> {
    let partial = scratch.image(&format!("{name}.vhdx.partial"));
    let base = scratch.image(&format!("{name}.vhdx"));
    virtual_disk::create_dynamic(&partial, 128 * 1024 * 1024)?;
    let root = backend::initialize_new_virtual_disk(&partial, true)?;
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
    })?;
    let root_a = mounted_root(&diff_a)?;
    fs::write(root_a.join("child-a.txt"), b"writes A")?;
    let rows = backend::mount(MountRequest {
        base: base_b.clone(),
        diff: diff_b.clone(),
        drive_letter: None,
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
