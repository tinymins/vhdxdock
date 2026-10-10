//! Folder-to-VHDX creation. Incomplete images always remain beside the requested
//! output as `.partial`; an existing image is never opened for formatting.
use crate::models::{BuildRequest, BuildResult, Progress, VerifyMode};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    kind: EntryKind,
    bytes: u64,
    modified: Option<SystemTime>,
    target: Option<PathBuf>,
}

#[derive(Default)]
struct Manifest {
    entries: BTreeMap<PathBuf, Entry>,
    files: u64,
    bytes: u64,
}

#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("操作已取消；未完成镜像和日志已保留")
    }
}

impl std::error::Error for Cancelled {}

/// Only a bare cancellation permits the UI to close automatically. Context
/// around it can describe a cleanup failure and must remain visible.
pub fn is_clean_cancellation(error: &anyhow::Error) -> bool {
    let outer: &(dyn std::error::Error + 'static) = error.as_ref();
    outer.is::<Cancelled>()
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    Ok(())
}

/// An empty input follows the output filename; explicit labels are preserved.
/// Validate before scanning or creating an image so invalid input leaves no partial disk.
pub fn resolve_volume_label(output: &Path, input: &str) -> Result<String> {
    let label = if input.is_empty() {
        output
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("无法从输出文件名获取卷标，请手动填写卷标")?
    } else {
        input
    };
    validate_volume_label(label)?;
    Ok(label.to_owned())
}

pub(crate) fn validate_volume_label(label: &str) -> Result<()> {
    if label.trim().is_empty() {
        bail!("卷标不能只包含空白字符，请填写名称或留空使用输出文件名");
    }
    if label.encode_utf16().count() > 32 {
        bail!("NTFS 卷标最多 32 个 UTF-16 字符，请缩短卷标（留空时使用输出文件名）");
    }
    if label.chars().any(char::is_control) {
        bail!("卷标不能包含换行、制表符或其他控制字符");
    }
    Ok(())
}

fn capacity_bytes(gib: u64, logical_bytes: u64, entries: usize) -> Result<u64> {
    let capacity = gib
        .checked_mul(1024 * 1024 * 1024)
        .context("虚拟容量数值过大")?;
    // Budget for allocation rounding, MFT records, and volume metadata. This is
    // conservative, not a guarantee about every possible NTFS file layout.
    let per_entry = u64::try_from(entries)?
        .checked_mul(8192)
        .context("文件数量过大")?;
    let required = logical_bytes
        .checked_add(per_entry)
        .and_then(|v| v.checked_add(256 * 1024 * 1024))
        .context("源目录大小溢出")?;
    if capacity < required {
        bail!(
            "虚拟容量不足：至少需要约 {:.2} GiB（含文件系统预留），当前为 {gib} GiB",
            required as f64 / 1073741824.0
        );
    }
    Ok(capacity)
}

fn is_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn scan(
    root: &Path,
    cancel: &AtomicBool,
    notify: &impl Fn(Progress),
    phase: &str,
    excluded_root_names: &[&str],
) -> Result<Manifest> {
    let mut manifest = Manifest::default();
    let mut walk = walkdir::WalkDir::new(root).follow_links(false).into_iter();
    let mut last = std::time::Instant::now();
    while let Some(item) = walk.next() {
        check_cancel(cancel)?;
        let item = item.with_context(|| format!("无法扫描 {}", root.display()))?;
        let relative = item.path().strip_prefix(root)?.to_owned();
        if relative.as_os_str().is_empty() {
            continue;
        }
        if item.depth() == 1
            && excluded_root_names.iter().any(|name| {
                item.file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(name)
            })
        {
            if item.file_type().is_dir() {
                walk.skip_current_dir();
            }
            continue;
        }
        let metadata = fs::symlink_metadata(item.path())
            .with_context(|| format!("读取文件信息失败：{}", item.path().display()))?;
        let entry = if is_reparse(&metadata) {
            if item.file_type().is_dir() {
                walk.skip_current_dir();
            }
            let target = fs::read_link(item.path()).with_context(|| {
                format!(
                    "无法读取链接（不支持此类重解析点）：{}",
                    item.path().display()
                )
            })?;
            Entry {
                kind: EntryKind::Link,
                bytes: 0,
                modified: metadata.modified().ok(),
                target: Some(target),
            }
        } else if metadata.is_dir() {
            Entry {
                kind: EntryKind::Directory,
                bytes: 0,
                modified: metadata.modified().ok(),
                target: None,
            }
        } else if metadata.is_file() {
            manifest.files = manifest.files.checked_add(1).context("文件数溢出")?;
            manifest.bytes = manifest
                .bytes
                .checked_add(metadata.len())
                .context("文件大小溢出")?;
            Entry {
                kind: EntryKind::File,
                bytes: metadata.len(),
                modified: metadata.modified().ok(),
                target: None,
            }
        } else {
            bail!("不支持的文件类型：{}", item.path().display());
        };
        manifest.entries.insert(relative, entry);
        if last.elapsed() >= Duration::from_millis(500) {
            notify(Progress {
                phase: phase.into(),
                message: format!("已扫描 {} 个文件", manifest.files),
                files: manifest.files,
                bytes: manifest.bytes,
                ..Default::default()
            });
            last = std::time::Instant::now();
        }
    }
    Ok(manifest)
}

fn source_unchanged(before: &Manifest, after: &Manifest) -> Result<()> {
    if before.entries != after.entries {
        bail!("源目录在制作过程中发生变化，请停止编辑后重新制作；未完成镜像已保留");
    }
    Ok(())
}

fn compare_manifests(source: &Manifest, target: &Manifest) -> Result<()> {
    for (path, expected) in &source.entries {
        let actual = target
            .entries
            .get(path)
            .with_context(|| format!("镜像缺少：{}", path.display()))?;
        // NTFS reparse-point timestamps can be adjusted when creating a link;
        // link identity is checked by target, directories by existence/type.
        if actual.kind != expected.kind
            || actual.bytes != expected.bytes
            || actual.target != expected.target
            || (expected.kind == EntryKind::File && actual.modified != expected.modified)
        {
            bail!("镜像文件信息不匹配：{}", path.display());
        }
    }
    for path in target.entries.keys() {
        if !source.entries.contains_key(path) {
            bail!("镜像中出现源目录没有的项目：{}", path.display());
        }
    }
    Ok(())
}

fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("无法读取：{}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        check_cancel(cancel)?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn verify_hashes(
    source: &Path,
    target: &Path,
    manifest: &Manifest,
    cancel: &AtomicBool,
    notify: &impl Fn(Progress),
) -> Result<()> {
    let mut files = 0;
    let mut bytes = 0;
    let mut last = std::time::Instant::now();
    for (relative, entry) in &manifest.entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        check_cancel(cancel)?;
        if hash_file(&source.join(relative), cancel)? != hash_file(&target.join(relative), cancel)?
        {
            bail!("SHA-256 校验不一致：{}", relative.display());
        }
        files += 1;
        bytes += entry.bytes;
        if last.elapsed() >= Duration::from_millis(500) || files == manifest.files {
            notify(Progress {
                phase: "内容校验".into(),
                message: relative.display().to_string(),
                files,
                bytes,
                total_files: manifest.files,
                total_bytes: manifest.bytes,
            });
            last = std::time::Instant::now();
        }
    }
    Ok(())
}

fn copy_exit_ok(code: i32) -> bool {
    (0..8).contains(&code)
}
fn verify_exit_ok(code: i32) -> bool {
    code == 0 || code == 2
}

fn output_inside_source(source: &Path, output_parent: &Path) -> bool {
    output_parent
        .ancestors()
        .any(|ancestor| crate::paths::same_path(ancestor, source))
}

fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("文件已存在，不会覆盖：{}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("无法检查输出文件：{}", path.display())),
    }
}

// Resolve existing junction/symlink ancestors without creating directories.
fn directory_scope_path(path: &Path) -> Result<PathBuf> {
    let mut existing = crate::paths::resolve(path)?;
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(&existing) {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    existing
                        .file_name()
                        .context("日志路径没有可访问的父目录")?
                        .to_os_string(),
                );
                if !existing.pop() {
                    bail!("日志路径没有可访问的父目录");
                }
            }
            Err(error) => return Err(error).context("无法验证日志目录位置"),
        }
    }
}

fn choose_log_directory(source: &Path, preferred: &Path, fallback: &Path) -> Result<PathBuf> {
    let preferred = directory_scope_path(preferred)?;
    if !output_inside_source(source, &preferred) {
        return Ok(preferred);
    }
    let fallback = directory_scope_path(fallback)?;
    if output_inside_source(source, &fallback) {
        bail!("日志目录和临时备用日志目录都位于源文件夹内部；请修改源目录范围或 TEMP 环境变量。尚未创建镜像。");
    }
    Ok(fallback)
}

#[cfg(windows)]
fn available_bytes(directory: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{core::PCWSTR, Win32::Storage::FileSystem::GetDiskFreeSpaceExW};
    let wide: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut available), None, None) }
        .context("无法查询输出位置剩余空间")?;
    Ok(available)
}

#[cfg(windows)]
fn robocopy_path(path: &Path) -> PathBuf {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
    if wide.starts_with(&prefix) {
        if wide.len() >= 8
            && wide[4..8]
                .iter()
                .zip("UNC\\".encode_utf16())
                .all(|(a, b)| *a <= 0x7f && (*a as u8).to_ascii_uppercase() == b as u8)
        {
            let mut normal: Vec<u16> = r"\\".encode_utf16().collect();
            normal.extend_from_slice(&wide[8..]);
            return PathBuf::from(OsString::from_wide(&normal));
        }
        if wide.len() >= 6
            && wide[4] <= 0x7f
            && (wide[4] as u8).is_ascii_alphabetic()
            && wide[5] == b':' as u16
        {
            return PathBuf::from(OsString::from_wide(&wide[4..]));
        }
    }
    path.to_owned()
}

#[cfg(windows)]
struct RobocopyWorker {
    child: std::process::Child,
    finished: bool,
}
#[cfg(windows)]
impl RobocopyWorker {
    fn stop(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        let _ = self.child.kill();
        self.child.wait().context("无法等待 Robocopy 停止")?;
        self.finished = true;
        Ok(())
    }
}
#[cfg(windows)]
impl Drop for RobocopyWorker {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(windows)]
fn run_robocopy(
    source: &Path,
    target: &Path,
    log: &Path,
    verify: bool,
    cancel: &AtomicBool,
    notify: &impl Fn(Progress),
    manifest: &Manifest,
) -> Result<()> {
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
        thread,
    };
    let mut cmd = Command::new("robocopy.exe");
    // Robocopy handles long paths itself but rejects explicit extended-length
    // directory prefixes (ERROR 123). canonicalize() adds those on Windows.
    cmd.arg(robocopy_path(source))
        .arg(robocopy_path(target))
        .args([
            "/E",
            "/COPY:DAT",
            "/DCOPY:DAT",
            "/SL",
            "/SJ",
            "/R:2",
            "/W:2",
            "/NP",
            "/NFL",
            "/NDL",
        ]);
    if verify {
        cmd.args(["/L", "/XX", "/R:0", "/W:0"]);
    } else {
        cmd.arg("/MT:16");
    }
    let mut log_arg = std::ffi::OsString::from("/UNILOG:");
    log_arg.push(log.as_os_str());
    cmd.arg(log_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000);
    let mut worker = RobocopyWorker {
        child: cmd.spawn().context("无法启动 Windows Robocopy")?,
        finished: false,
    };
    let phase = if verify {
        "文件信息校验"
    } else {
        "复制"
    };
    let mut last = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            worker.stop()?;
            check_cancel(cancel)?;
        }
        // If polling errors or a callback panics, the worker guard stops and
        // reaps Robocopy before the caller attempts to detach its image.
        if let Some(status) = worker
            .child
            .try_wait()
            .context("查询 Robocopy 进程状态失败")?
        {
            worker.finished = true;
            let code = status.code().context("Robocopy 未返回正常退出码")?;
            if !(if verify {
                verify_exit_ok(code)
            } else {
                copy_exit_ok(code)
            }) {
                bail!(
                    "Robocopy {phase}失败（返回码 {code}），日志：{}",
                    log.display()
                );
            }
            return Ok(());
        }
        if last.elapsed() >= Duration::from_secs(1) {
            notify(Progress {
                phase: phase.into(),
                message: format!("正在{phase}；详细日志：{}", log.display()),
                total_files: manifest.files,
                total_bytes: manifest.bytes,
                ..Default::default()
            });
            last = std::time::Instant::now();
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(windows)]
struct AttachedGuard {
    path: PathBuf,
    attached: bool,
}
#[cfg(windows)]
impl AttachedGuard {
    fn detach(&mut self) -> Result<()> {
        crate::virtual_disk::detach(&self.path)?;
        self.attached = false;
        Ok(())
    }
    fn cleanup_error(&mut self, error: anyhow::Error) -> anyhow::Error {
        match crate::virtual_disk::inspect(&self.path) {
            Ok(info) if !info.attached => {
                self.attached = false;
                error
            }
            _ => match self.detach() {
                Ok(()) => error,
                Err(cleanup) => error.context(format!(
                    "清理时无法卸载未完成镜像 {}：{cleanup:#}；请关闭占用程序后手动卸载",
                    self.path.display()
                )),
            },
        }
    }
}
#[cfg(windows)]
impl Drop for AttachedGuard {
    fn drop(&mut self) {
        if self.attached {
            let _ = crate::virtual_disk::detach(&self.path);
        }
    }
}

#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{core::PCWSTR, Win32::Storage::FileSystem::MoveFileW};
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe { MoveFileW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr())) }
        .context("完成文件改名失败（不会覆盖已有文件）")?;
    Ok(())
}

/// Creates a new image. The callback is called from the worker thread; the GUI
/// should forward events to its own event queue.
pub fn build(
    request: BuildRequest,
    cancel: Arc<AtomicBool>,
    progress: impl Fn(Progress) + Send + 'static,
) -> Result<BuildResult> {
    #[cfg(not(windows))]
    {
        let _ = (request, cancel, progress);
        bail!("制作 VHDX 仅支持 Windows")
    }
    #[cfg(windows)]
    {
        build_windows(request, cancel, progress)
    }
}

#[cfg(windows)]
fn build_windows(
    request: BuildRequest,
    cancel: Arc<AtomicBool>,
    progress: impl Fn(Progress) + Send + 'static,
) -> Result<BuildResult> {
    use crate::{paths, virtual_disk};
    check_cancel(&cancel)?;
    let source = fs::canonicalize(paths::resolve(&request.source)?).context("无法访问源文件夹")?;
    if !source.is_dir() {
        bail!("源路径必须是文件夹");
    }
    let output = paths::resolve(&request.output)?;
    if !output
        .extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("vhdx"))
    {
        bail!("制作仅输出 .vhdx 镜像");
    }
    let volume_label = resolve_volume_label(&output, &request.volume_label)?;
    let parent = output.parent().context("输出路径必须包含目录")?;
    // Canonicalize the existing parent rather than the absent output. This also
    // detects a destination reached through a junction inside the source.
    let resolved_parent = fs::canonicalize(parent).context("输出目录不存在或无法访问")?;
    if output_inside_source(&source, &resolved_parent) {
        bail!("输出镜像不能放在源文件夹内部");
    }
    let partial = paths::partial_path(&output);
    let mut checksum_name = output.as_os_str().to_owned();
    checksum_name.push(".sha256");
    let checksum = PathBuf::from(checksum_name);
    for path in [&output, &partial, &checksum] {
        ensure_absent(path)?;
    }
    // Resolve/create logging before the source manifest: probing the portable
    // config directory may touch a directory inside the selected source.
    let log_dir = choose_log_directory(
        &source,
        &crate::config::data_dir().join("logs"),
        &std::env::temp_dir().join("VhdxDockLogs"),
    )?;
    fs::create_dir_all(&log_dir)?;
    progress(Progress {
        phase: "扫描".into(),
        message: "正在统计源目录，链接不会被展开".into(),
        ..Default::default()
    });
    let manifest = scan(&source, &cancel, &progress, "扫描", &[])?;
    let capacity = capacity_bytes(request.capacity_gib, manifest.bytes, manifest.entries.len())?;
    let free_bytes = available_bytes(&resolved_parent)?;
    let conservative = manifest
        .bytes
        .checked_add(
            (manifest.entries.len() as u64)
                .checked_mul(8192)
                .context("大小溢出")?,
        )
        .and_then(|n| n.checked_add(256 * 1024 * 1024))
        .context("大小溢出")?;
    if free_bytes < conservative {
        bail!(
            "输出盘剩余空间不足：需要约 {:.2} GiB（按未压缩数据估算），剩余 {:.2} GiB",
            conservative as f64 / 1073741824.0,
            free_bytes as f64 / 1073741824.0
        );
    }
    let id = format!(
        "build-{}-{}",
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_millis(),
        std::process::id()
    );
    let copy_log = log_dir.join(format!("{id}-copy.log"));
    let verify_log = log_dir.join(format!("{id}-verify.log"));
    let mut summary = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(log_dir.join(format!("{id}.log")))?;
    writeln!(
        summary,
        "Source: {}\nOutput: {}\nVolume label: {}\nFiles: {}\nLogical bytes: {}",
        source.display(),
        output.display(),
        volume_label,
        manifest.files,
        manifest.bytes
    )?;
    for (relative, entry) in &manifest.entries {
        if let Some(target) = &entry.target {
            writeln!(
                summary,
                "Link preserved: {} -> {} (target data is not followed)",
                relative.display(),
                target.display()
            )?;
        }
    }
    progress(Progress {
        phase: "创建镜像".into(),
        message: partial.display().to_string(),
        total_files: manifest.files,
        total_bytes: manifest.bytes,
        ..Default::default()
    });
    check_cancel(&cancel)?;
    virtual_disk::create_dynamic(&partial, capacity)?;
    // Establish cleanup before initialization, which can fail after attachment.
    let mut guard = AttachedGuard {
        path: partial.clone(),
        attached: true,
    };
    let copying = (|| -> Result<()> {
        let target =
            crate::backend::initialize_new_virtual_disk(&partial, request.compress, &volume_label)?;
        check_cancel(&cancel)?;
        run_robocopy(
            &source, &target, &copy_log, false, &cancel, &progress, &manifest,
        )?;
        progress(Progress {
            phase: "复制完成".into(),
            message: "开始复查文件".into(),
            files: manifest.files,
            bytes: manifest.bytes,
            total_files: manifest.files,
            total_bytes: manifest.bytes,
        });
        run_robocopy(
            &source,
            &target,
            &verify_log,
            true,
            &cancel,
            &progress,
            &manifest,
        )?;
        let mut excluded = Vec::new();
        for name in ["System Volume Information", "$RECYCLE.BIN"] {
            if !manifest
                .entries
                .keys()
                .any(|p| p.as_os_str().to_string_lossy().eq_ignore_ascii_case(name))
            {
                if target.join(name).exists() {
                    writeln!(summary, "Ignored Windows system directory: {name}")?;
                }
                excluded.push(name);
            }
        }
        let copied = scan(&target, &cancel, &progress, "文件信息校验", &excluded)?;
        compare_manifests(&manifest, &copied)?;
        if request.verify == VerifyMode::Sha256 {
            verify_hashes(&source, &target, &manifest, &cancel, &progress)?;
        }
        progress(Progress {
            phase: "复查源目录".into(),
            message: "确认制作期间源文件未变化".into(),
            ..Default::default()
        });
        let after = scan(&source, &cancel, &progress, "复查源目录", &[])?;
        source_unchanged(&manifest, &after)?;
        Ok(())
    })();
    if let Err(error) = copying {
        return Err(guard.cleanup_error(error));
    }
    guard.detach()?;
    progress(Progress {
        phase: "镜像 SHA-256".into(),
        message: "正在读取已卸载镜像生成校验值".into(),
        ..Default::default()
    });
    let digest = hash_file(&partial, &cancel)?;
    check_cancel(&cancel)?;
    let image_bytes = fs::metadata(&partial)?.len();
    let mut permissions = fs::metadata(&partial)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&partial, permissions)?;
    // MoveFileW fails if the destination appeared while the worker was running.
    rename_no_replace(&partial, &output)?;
    let save_checksum = (|| -> Result<()> {
        let mut hash_output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&checksum)?;
        writeln!(
            hash_output,
            "{}  {}",
            digest,
            output.file_name().context("缺少文件名")?.to_string_lossy()
        )?;
        hash_output.sync_all()?;
        Ok(())
    })();
    save_checksum.with_context(|| {
        format!(
            "完整镜像已保存在 {}，但无法写入 SHA-256 文件；无需重新制作镜像",
            output.display()
        )
    })?;
    writeln!(
        summary,
        "Completed: {}\nSHA-256: {}",
        output.display(),
        digest
    )?;
    progress(Progress {
        phase: "完成".into(),
        message: output.display().to_string(),
        files: manifest.files,
        bytes: manifest.bytes,
        total_files: manifest.files,
        total_bytes: manifest.bytes,
    });
    Ok(BuildResult {
        output,
        image_bytes,
        logical_bytes: manifest.bytes,
        files: manifest.files,
        sha256: digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn volume_label_uses_output_name_or_preserves_custom_unicode() {
        let output = Path::new("backup/剑三归档-base.vhdx");
        assert_eq!(resolve_volume_label(output, "").unwrap(), "剑三归档-base");
        let custom = "剑三 ' $ 归档";
        assert_eq!(resolve_volume_label(output, custom).unwrap(), custom);
        let long_output = PathBuf::from(format!("{}.vhdx", "a".repeat(33)));
        assert!(resolve_volume_label(&long_output, "").is_err());
        assert_eq!(resolve_volume_label(&long_output, "归档").unwrap(), "归档");
    }

    #[test]
    fn volume_label_limits_utf16_length_and_rejects_control_characters() {
        let output = Path::new("base.vhdx");
        for valid in ["汉".repeat(32), "🚢".repeat(16)] {
            assert_eq!(resolve_volume_label(output, &valid).unwrap(), valid);
        }
        for invalid in [
            "汉".repeat(33),
            "🚢".repeat(17),
            " \t ".into(),
            "bad\0label".into(),
            "bad\nlabel".into(),
            "   ".into(),
        ] {
            assert!(resolve_volume_label(output, &invalid).is_err());
        }
    }

    #[test]
    fn cancellation_is_clean_only_without_cleanup_context() {
        let cancelled = check_cancel(&AtomicBool::new(true)).unwrap_err();
        assert!(is_clean_cancellation(&cancelled));
        assert!(!is_clean_cancellation(
            &cancelled.context("清理时无法卸载镜像")
        ));
        assert!(!is_clean_cancellation(&anyhow::anyhow!("操作失败")));
    }
    #[cfg(windows)]
    #[test]
    fn robocopy_converts_extended_directory_paths() {
        assert_eq!(
            robocopy_path(Path::new(r"\\?\D:\资料 ' $\source")),
            PathBuf::from(r"D:\资料 ' $\source")
        );
        assert_eq!(
            robocopy_path(Path::new(r"\\?\UNC\NAS\共享\source")),
            PathBuf::from(r"\\NAS\共享\source")
        );
        assert_eq!(
            robocopy_path(Path::new(r"\\?\unc\NAS\共享\source")),
            PathBuf::from(r"\\NAS\共享\source")
        );
        assert_eq!(
            robocopy_path(Path::new(r"\\NAS\共享\source")),
            PathBuf::from(r"\\NAS\共享\source")
        );
    }
    #[cfg(windows)]
    #[test]
    fn robocopy_canonical_unicode_directory_regression_without_admin() {
        let scratch = tempfile::tempdir().unwrap();
        let source = scratch.path().join("源文件夹 ' $ 中文");
        let target = scratch.path().join("目标文件夹 ' $ 中文");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("说明 ' $ 内容.txt"), b"literal fixture").unwrap();
        let source = fs::canonicalize(source).unwrap();
        let target = fs::canonicalize(target).unwrap();
        let log_root = fs::canonicalize(scratch.path()).unwrap();
        let cancel = AtomicBool::new(false);
        let manifest = scan(&source, &cancel, &|_| {}, "scan", &[]).unwrap();
        run_robocopy(
            &source,
            &target,
            &log_root.join("copy.log"),
            false,
            &cancel,
            &|_| {},
            &manifest,
        )
        .unwrap();
        assert_eq!(
            fs::read(target.join("说明 ' $ 内容.txt")).unwrap(),
            b"literal fixture"
        );
        run_robocopy(
            &source,
            &target,
            &log_root.join("verify.log"),
            true,
            &cancel,
            &|_| {},
            &manifest,
        )
        .unwrap();
    }
    #[test]
    fn capacity_accounts_for_overhead_and_overflow() {
        assert!(capacity_bytes(0, 0, 0).is_err());
        assert!(capacity_bytes(1, 1024 * 1024 * 1024, 0).is_err());
        assert!(capacity_bytes(u64::MAX, 0, 0).is_err());
        assert!(capacity_bytes(512, u64::MAX, 1).is_err());
        assert_eq!(
            capacity_bytes(512, 37 * 1024 * 1024 * 1024, 333_000).unwrap(),
            512 * 1024 * 1024 * 1024
        );
    }
    #[test]
    fn robocopy_statuses_are_not_regular_process_statuses() {
        for code in 0..8 {
            assert!(copy_exit_ok(code));
        }
        for code in [-1, 8, 16] {
            assert!(!copy_exit_ok(code));
        }
        assert!(verify_exit_ok(0));
        assert!(verify_exit_ok(2));
        for code in [1, 3, 4, 8] {
            assert!(!verify_exit_ok(code));
        }
    }
    #[test]
    fn refuses_output_inside_source_but_allows_sibling() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let nested = source.join("backups");
        let sibling = root.path().join("source-copy");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(&sibling).unwrap();
        assert!(output_inside_source(&source, &source));
        assert!(output_inside_source(&source, &nested));
        assert!(!output_inside_source(&source, &sibling));
    }
    #[test]
    fn never_accepts_existing_output() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("archive.vhdx");
        ensure_absent(&path).unwrap();
        fs::write(&path, b"existing").unwrap();
        assert!(ensure_absent(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing");
    }
    #[test]
    fn logging_falls_back_without_creating_inside_source() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        let preferred = source.join("tool/logs");
        let fallback = root.path().join("temporary/VhdxDockLogs");
        assert_eq!(
            choose_log_directory(&source, &preferred, &fallback).unwrap(),
            directory_scope_path(&fallback).unwrap()
        );
        assert!(!preferred.exists());
        assert!(!fallback.exists());
        assert!(choose_log_directory(&source, &preferred, &source.join("temp/logs")).is_err());
    }
    #[test]
    fn logging_uses_preferred_when_outside_source() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        let preferred = root.path().join("tool/logs");
        assert_eq!(
            choose_log_directory(&source, &preferred, &source.join("unused")).unwrap(),
            directory_scope_path(&preferred).unwrap()
        );
    }
    #[test]
    fn manifest_comparison_rejects_missing_and_extra_files() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"abc").unwrap();
        let cancel = AtomicBool::new(false);
        let source = scan(root.path(), &cancel, &|_| {}, "scan", &[]).unwrap();
        let mut target = scan(root.path(), &cancel, &|_| {}, "scan", &[]).unwrap();
        compare_manifests(&source, &target).unwrap();
        target.entries.remove(Path::new("a"));
        assert!(compare_manifests(&source, &target).is_err());
        target
            .entries
            .insert(PathBuf::from("a"), source.entries[Path::new("a")].clone());
        target.entries.insert(
            PathBuf::from("extra"),
            source.entries[Path::new("a")].clone(),
        );
        assert!(compare_manifests(&source, &target).is_err());
    }
    #[test]
    fn system_directory_exclusion_is_only_at_root() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("System Volume Information")).unwrap();
        fs::create_dir_all(root.path().join("nested/System Volume Information")).unwrap();
        fs::write(root.path().join("System Volume Information/ignored"), b"x").unwrap();
        fs::write(
            root.path().join("nested/System Volume Information/kept"),
            b"x",
        )
        .unwrap();
        let scanned = scan(
            root.path(),
            &AtomicBool::new(false),
            &|_| {},
            "scan",
            &["System Volume Information"],
        )
        .unwrap();
        assert_eq!(scanned.files, 1);
        assert!(scanned
            .entries
            .contains_key(Path::new("nested/System Volume Information/kept")));
    }
    #[test]
    fn scan_includes_hidden_files_and_empty_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::create_dir(dir.path().join("empty")).unwrap();
        fs::write(dir.path().join(".git/config"), b"abc").unwrap();
        let cancel = AtomicBool::new(false);
        let scanned = scan(dir.path(), &cancel, &|_| {}, "scan", &[]).unwrap();
        assert_eq!(scanned.files, 1);
        assert_eq!(scanned.bytes, 3);
        assert!(scanned.entries.contains_key(Path::new("empty")));
        assert!(scanned.entries.contains_key(Path::new(".git/config")));
    }
    #[test]
    fn scanning_and_hashing_obey_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        fs::write(&path, b"abc").unwrap();
        let cancel = AtomicBool::new(true);
        assert!(scan(dir.path(), &cancel, &|_| {}, "scan", &[]).is_err());
        assert!(hash_file(&path, &cancel).is_err());
    }
    #[test]
    fn hash_and_content_verification_detect_changes() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::write(source.path().join("a"), b"abc").unwrap();
        fs::write(target.path().join("a"), b"xyz").unwrap();
        let cancel = AtomicBool::new(false);
        assert_eq!(
            hash_file(&source.path().join("a"), &cancel).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let before = scan(source.path(), &cancel, &|_| {}, "scan", &[]).unwrap();
        assert!(verify_hashes(source.path(), target.path(), &before, &cancel, &|_| {}).is_err());
        fs::write(source.path().join("b"), b"new").unwrap();
        let after = scan(source.path(), &cancel, &|_| {}, "scan", &[]).unwrap();
        assert!(source_unchanged(&before, &after).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn links_are_not_followed_outside_source() {
        let source = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"not included").unwrap();
        std::os::unix::fs::symlink(outside.path(), source.path().join("external")).unwrap();
        fs::write(source.path().join("local"), b"included").unwrap();
        let scanned = scan(source.path(), &AtomicBool::new(false), &|_| {}, "scan", &[]).unwrap();
        assert_eq!(scanned.files, 1);
        assert_eq!(scanned.entries.len(), 2);
        assert_eq!(scanned.entries[Path::new("external")].kind, EntryKind::Link);
    }
}
