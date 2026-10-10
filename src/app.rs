use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, OnceLock,
    },
    time::{Duration, Instant},
};
use vhdxdock::{
    backend, builder,
    config::{self, AppConfig},
    models::*,
    paths,
};

const ACCENT: Color32 = Color32::from_rgb(13, 115, 119);
const TEXT: Color32 = Color32::from_rgb(30, 44, 60);
const MUTED: Color32 = Color32::from_rgb(100, 116, 139);
const BORDER: Color32 = Color32::from_rgb(221, 228, 235);
const CARD_PADDING: i8 = 18;
const CONTROL_HEIGHT: f32 = 34.0;

/// Decode the embedded artwork once and share it with the window and brand.
pub fn application_icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    Arc::clone(ICON.get_or_init(|| {
        Arc::new(
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                .expect("embedded application icon must be a valid PNG"),
        )
    }))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tab {
    Mount,
    Build,
    Logs,
}

enum Event {
    Disks {
        action: String,
        result: Result<Vec<MountedImage>, String>,
    },
    Progress(Progress),
    Built(Result<BuildResult, BuildFailure>),
    Opened(Result<(), String>),
}

struct BuildFailure {
    message: String,
    clean_cancel: bool,
}

enum LogCommand {
    Write(String),
    Pause(mpsc::Sender<()>),
    Resume,
}

struct EjectConfirmation {
    image: MountedImage,
    focus_cancel: bool,
}

pub struct DockApp {
    settings: AppConfig,
    brand_texture: Option<egui::TextureHandle>,
    tab: Tab,
    disks: Vec<MountedImage>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    disk_busy: Option<String>,
    building: bool,
    cancel: Arc<AtomicBool>,
    progress: Progress,
    build_started: Option<Instant>,
    build_result: Option<BuildResult>,
    logs: VecDeque<String>,
    log_tx: Option<mpsc::Sender<LogCommand>>,
    notice: Option<(String, bool)>,
    diff_manual: bool,
    last_auto_base: String,
    eject: Option<EjectConfirmation>,
    open_volumes: Option<Vec<String>>,
    close_confirmation: bool,
    close_focus_cancel: bool,
    exit_when_idle: bool,
    config_dirty: bool,
    last_save: Instant,
    #[cfg(feature = "ui-preview")]
    preview_mode: bool,
}

impl DockApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_style(&cc.egui_ctx);
        let settings = AppConfig::load();
        if let Some([width, height]) = settings.window_size {
            cc.egui_ctx
                .send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(
                    width.max(900.0),
                    height.max(650.0),
                )));
        }
        let mut app = Self::with_settings(settings);
        app.log_tx = Some(start_log_writer());
        app.log("VhdxDock 已启动");
        app.refresh();
        app
    }

    fn with_settings(settings: AppConfig) -> Self {
        let (tx, rx) = mpsc::channel();
        let diff_manual = !settings.diff_path.trim().is_empty()
            && !Self::resolved(&settings.base_path)
                .and_then(|base| paths::default_diff(&base).map_err(|e| e.to_string()))
                .ok()
                .zip(Self::resolved(&settings.diff_path).ok())
                .is_some_and(|(default, actual)| paths::same_path(&default, &actual));
        let last_auto_base = settings.base_path.clone();
        Self {
            settings,
            brand_texture: None,
            tab: Tab::Mount,
            disks: Vec::new(),
            tx,
            rx,
            disk_busy: None,
            building: false,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Progress::default(),
            build_started: None,
            build_result: None,
            logs: VecDeque::new(),
            log_tx: None,
            notice: None,
            diff_manual,
            last_auto_base,
            eject: None,
            open_volumes: None,
            close_confirmation: false,
            close_focus_cancel: false,
            exit_when_idle: false,
            config_dirty: false,
            last_save: Instant::now(),
            #[cfg(feature = "ui-preview")]
            preview_mode: false,
        }
    }

    /// Fixture-only constructor used by the opt-in screenshot example. It never
    /// discovers disks, persists configuration or starts disk/logging workers.
    #[cfg(feature = "ui-preview")]
    #[allow(dead_code)] // Called by the separately compiled screenshot example.
    pub fn preview(ctx: &egui::Context, scenario: &str) -> Self {
        install_style(ctx);
        let settings = AppConfig {
            base_path: r"\\NAS\backup\Archive.vhdx".into(),
            diff_path: r"D:\VhdxDock\diffs\Archive-diff.vhdx".into(),
            source_path: r"D:\Data\Archive".into(),
            output_path: r"D:\Images\Archive.vhdx".into(),
            ..Default::default()
        };
        let mut app = Self::with_settings(settings);
        app.preview_mode = true;
        app.disks = vec![
            MountedImage {
                image_path: PathBuf::from(r"D:\VhdxDock\diffs\Archive-diff.vhdx"),
                parent_path: Some(PathBuf::from(r"\\NAS\backup\Archive.vhdx")),
                volumes: vec![r"F:\".into()],
                kind: "差分 VHDX".into(),
                read_only: false,
                can_eject: true,
                warning: None,
            },
            MountedImage {
                image_path: PathBuf::from(r"D:\VhdxDock\diffs\Photos-diff.vhd"),
                parent_path: Some(PathBuf::from(r"\\NAS\backup\Photos.vhd")),
                volumes: vec![r"G:\".into()],
                kind: "差分 VHD".into(),
                read_only: false,
                can_eject: true,
                warning: None,
            },
        ];
        if scenario == "folder" {
            app.settings.mount_mode = MountMode::Folder;
            app.settings.mount_folder = r"D:\Mounts\Archive".into();
            app.disks[0].volumes = vec![r"D:\Mounts\Archive\".into()];
        }
        if scenario == "build" {
            app.tab = Tab::Build;
        }
        if scenario == "logs" {
            app.tab = Tab::Logs;
            app.logs = [
                "VhdxDock 已启动",
                "已刷新挂载列表：2 个镜像",
                r"基础镜像：\\NAS\backup\Archive.vhdx",
                r"本地差分：D:\VhdxDock\diffs\Archive-diff.vhdx",
                "镜像已挂载，修改将保存在差分盘中",
            ]
            .into_iter()
            .map(String::from)
            .collect();
        }
        if scenario == "eject" {
            app.eject = Some(EjectConfirmation {
                image: app.disks[0].clone(),
                focus_cancel: true,
            });
        }
        app
    }

    fn operations_allowed(&self) -> bool {
        #[cfg(feature = "ui-preview")]
        if self.preview_mode {
            return false;
        }
        true
    }

    fn log(&mut self, message: impl Into<String>) {
        let message = message.into();
        if let Some(writer) = &self.log_tx {
            let _ = writer.send(LogCommand::Write(message.clone()));
        }
        self.logs.push_back(message);
        while self.logs.len() > 400 {
            self.logs.pop_front();
        }
    }

    fn report(&mut self, message: impl Into<String>, error: bool) {
        let message = message.into();
        self.log(message.clone());
        self.notice = Some((message, error));
    }

    fn save(&mut self) {
        // A portable executable may live inside the folder being archived.
        // Keep that source stable until the builder's final verification ends.
        if !self.operations_allowed() || self.building {
            return;
        }
        if let Err(error) = self.settings.save() {
            self.log(format!("配置保存失败：{error:#}"));
        }
        self.config_dirty = false;
        self.last_save = Instant::now();
    }

    fn resolved(input: &str) -> Result<PathBuf, String> {
        if input.trim().is_empty() {
            return Err("请先填写路径。".into());
        }
        paths::resolve(Path::new(input.trim())).map_err(|e| format!("路径无效：{e:#}"))
    }

    fn set_default_diff(&mut self) {
        if !self.diff_manual && !self.settings.base_path.trim().is_empty() {
            // A partially typed base path is normal while editing this field.
            if let Ok(path) = Self::resolved(&self.settings.base_path)
                .and_then(|base| paths::default_diff(&base).map_err(|e| format!("{e:#}")))
            {
                self.settings.diff_path = path
                    .strip_prefix(config::exe_dir())
                    .map(|relative| format!(".{}{}", std::path::MAIN_SEPARATOR, relative.display()))
                    .unwrap_or_else(|_| path.display().to_string());
            }
        }
        self.last_auto_base = self.settings.base_path.clone();
        self.config_dirty = true;
    }

    fn refresh(&mut self) {
        if !self.operations_allowed() {
            return;
        }
        if self.disk_busy.is_some() {
            return;
        }
        self.disk_busy = Some("正在读取已挂载镜像".into());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = backend::list_mounted().map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Disks {
                action: "已刷新挂载列表".into(),
                result,
            });
        });
    }

    fn mount_request(&self) -> Result<MountRequest, String> {
        Ok(MountRequest {
            base: Self::resolved(&self.settings.base_path)?,
            diff: Self::resolved(&self.settings.diff_path)?,
            drive_letter: match self.settings.mount_mode {
                MountMode::DriveLetter => self.settings.drive_letter,
                MountMode::Folder => None,
            },
            mount_folder: match self.settings.mount_mode {
                MountMode::DriveLetter => None,
                MountMode::Folder => Some(Self::resolved(&self.settings.mount_folder)?),
            },
        })
    }

    fn mount(&mut self) {
        if !self.operations_allowed() {
            return;
        }
        let request = match self.mount_request() {
            Ok(request) => request,
            Err(error) => {
                self.report(error, true);
                return;
            }
        };
        self.save();
        self.disk_busy = Some("正在检查父镜像、创建差分并挂载".into());
        self.notice = None;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = backend::mount(request).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Disks {
                action: "镜像已挂载，修改将保存在差分盘中".into(),
                result,
            });
        });
    }

    fn unmount(&mut self, path: PathBuf) {
        if !self.operations_allowed() {
            return;
        }
        self.disk_busy = Some("正在卸载镜像".into());
        self.notice = None;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = backend::unmount(&path)
                .and_then(|_| backend::list_mounted())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Disks {
                action: "镜像已卸载，差分中的修改已保留".into(),
                result,
            });
        });
    }

    fn build_request(&self) -> Result<BuildRequest, String> {
        let source = Self::resolved(&self.settings.source_path)?;
        let output = Self::resolved(&self.settings.output_path)?;
        let volume_label = builder::resolve_volume_label(&output, &self.settings.volume_label)
            .map_err(|error| format!("{error:#}"))?;
        Ok(BuildRequest {
            source,
            output,
            volume_label,
            capacity_gib: self.settings.capacity_gib,
            compress: self.settings.compress,
            verify: self.settings.verify,
        })
    }

    fn start_build(&mut self) {
        if !self.operations_allowed() || self.exit_when_idle {
            return;
        }
        let request = match self.build_request() {
            Ok(request) => request,
            Err(error) => {
                self.report(error, true);
                return;
            }
        };
        self.save();
        self.cancel = Arc::new(AtomicBool::new(false));
        self.building = true;
        self.build_started = Some(Instant::now());
        self.build_result = None;
        self.progress = Progress {
            phase: "准备制作".into(),
            message: "正在检查路径与输出位置".into(),
            ..Default::default()
        };
        self.notice = None;
        self.log(format!(
            "开始制作：{} → {}",
            request.source.display(),
            request.output.display()
        ));
        let log_paused = self.log_tx.as_ref().and_then(|writer| {
            let (ack, receiver) = mpsc::channel();
            writer.send(LogCommand::Pause(ack)).ok().map(|_| receiver)
        });
        let cancel = self.cancel.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // The acknowledgement is queued behind all prior log writes. The
            // builder must not scan until the portable session log is stable.
            if let Some(ack) = log_paused {
                let _ = ack.recv();
            }
            let progress_tx = tx.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                builder::build(request, cancel, move |progress| {
                    let _ = progress_tx.send(Event::Progress(progress));
                })
            }))
            .map_err(|panic| {
                let detail = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("未知内部错误");
                BuildFailure {
                    message: format!("制作任务异常终止：{detail}；请检查未完成镜像的挂载状态。"),
                    clean_cancel: false,
                }
            })
            .and_then(|result| {
                result.map_err(|error| BuildFailure {
                    clean_cancel: builder::is_clean_cancellation(&error),
                    message: format!("{error:#}"),
                })
            });
            let _ = tx.send(Event::Built(result));
        });
    }

    /// Returns whether an earlier cancel-and-exit request can safely continue.
    /// Failures, including unsuccessful cleanup, must remain visible to users.
    fn finish_build(&mut self, result: Result<BuildResult, BuildFailure>) -> bool {
        self.building = false;
        self.close_confirmation = false;
        if let Some(writer) = &self.log_tx {
            let _ = writer.send(LogCommand::Resume);
        }
        let safe_to_exit = match result {
            Ok(result) => {
                self.report(
                    format!(
                        "制作完成：{}（{}）",
                        result.output.display(),
                        paths::format_bytes(result.image_bytes)
                    ),
                    false,
                );
                self.build_result = Some(result);
                true
            }
            Err(error) => {
                self.report(error.message, !error.clean_cancel);
                error.clean_cancel
            }
        };
        if !safe_to_exit {
            self.exit_when_idle = false;
        }
        self.exit_when_idle && safe_to_exit
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        for _ in 0..2_000 {
            let Ok(event) = self.rx.try_recv() else {
                break;
            };
            match event {
                Event::Disks { action, result } => {
                    self.disk_busy = None;
                    match result {
                        Ok(disks) => {
                            self.disks = disks;
                            self.report(action, false);
                        }
                        Err(error) => self.report(error, true),
                    }
                }
                Event::Progress(progress) => {
                    if progress.phase != self.progress.phase {
                        self.log(format!("{}：{}", progress.phase, progress.message));
                    }
                    self.progress = progress;
                }
                Event::Built(result) => {
                    let close = self.finish_build(result);
                    if self.config_dirty || close {
                        self.save();
                    }
                    if close {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                Event::Opened(result) => {
                    if let Err(error) = result {
                        self.report(error, true);
                    }
                }
            }
        }
    }

    fn open_disk(&mut self, volumes: Vec<String>) {
        if volumes.len() > 1 {
            self.open_volumes = Some(volumes);
        } else if let Some(volume) = volumes.into_iter().next() {
            self.open_path(volume);
        }
    }

    fn open_path(&self, path: String) {
        if !self.operations_allowed() {
            return;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            #[cfg(windows)]
            let result = std::process::Command::new("explorer.exe")
                .arg(&path)
                .spawn();
            #[cfg(not(windows))]
            let result = std::process::Command::new("xdg-open").arg(&path).spawn();
            let result = result
                .map(|_| ())
                .map_err(|e| format!("无法打开 {path}：{e}"));
            let _ = tx.send(Event::Opened(result));
        });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let brand_texture = self.brand_texture.get_or_insert_with(|| {
            let icon = application_icon();
            ui.ctx().load_texture(
                "vhdxdock-brand",
                egui::ColorImage::from_rgba_unmultiplied(
                    [icon.width as usize, icon.height as usize],
                    &icon.rgba,
                ),
                egui::TextureOptions::LINEAR,
            )
        });
        let brand_height = ui
            .painter()
            .layout_no_wrap("VhdxDock".into(), egui::FontId::proportional(27.0), TEXT)
            .size()
            .y
            + ui.text_style_height(&egui::TextStyle::Body)
            + ui.spacing().item_spacing.y;
        ui.allocate_ui_with_layout(
            Vec2::new(ui.available_width(), brand_height.max(50.0)),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                brand_icon(ui, brand_texture);
                ui.vertical(|ui| {
                    ui.heading(RichText::new("VhdxDock").size(27.0).color(TEXT));
                    ui.label(RichText::new("镜像归档 · 本地差分 · 随时接入").color(MUTED));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new("WINDOWS  /  VHD + VHDX")
                            .size(11.0)
                            .color(MUTED),
                    );
                });
            },
        );
        ui.add_space(18.0);
        control_row(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for (tab, title) in [(Tab::Mount, "挂载镜像"), (Tab::Build, "制作镜像")] {
                if tab_button(ui, title, self.tab == tab).clicked() {
                    self.tab = tab;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if tab_button(ui, "日志", self.tab == Tab::Logs).clicked() {
                    self.tab = Tab::Logs;
                }
            });
        });
        ui.add_space(18.0);
    }

    fn mount_page(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.label(RichText::new("接入基础镜像").strong().size(17.0));
            ui.label(
                RichText::new("从 NAS 或本地读取基础镜像，修改保存到本地差分盘。").color(MUTED),
            );
            ui.add_space(15.0);
            ui.add_enabled_ui(self.disk_busy.is_none(), |ui| {
                let base_changed =
                    path_input(ui, "基础镜像", &mut self.settings.base_path, Browse::Image);
                if base_changed {
                    self.set_default_diff();
                }
                ui.add_space(9.0);
                if path_input(
                    ui,
                    "本地差分镜像",
                    &mut self.settings.diff_path,
                    Browse::SaveDiff,
                ) {
                    self.diff_manual = !self.settings.diff_path.trim().is_empty();
                    self.config_dirty = true;
                }
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("实际路径：").size(12.0).color(MUTED));
                    let preview = Self::resolved(&self.settings.diff_path)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "选择基础镜像后自动生成".into());
                    ui.label(RichText::new(preview).size(12.0).color(MUTED));
                });
                control_row(ui, |ui| {
                    ui.label(
                        RichText::new("相对路径以软件所在目录为起点")
                            .size(12.0)
                            .color(MUTED),
                    );
                    if ui.add(button("恢复默认差分路径")).clicked() {
                        self.diff_manual = false;
                        self.set_default_diff();
                    }
                });
                ui.add_space(13.0);
                control_row(ui, |ui| {
                    ui.label("挂载位置");
                    let previous = self.settings.mount_mode;
                    ui.selectable_value(
                        &mut self.settings.mount_mode,
                        MountMode::DriveLetter,
                        "盘符",
                    );
                    ui.selectable_value(&mut self.settings.mount_mode, MountMode::Folder, "文件夹");
                    self.config_dirty |= previous != self.settings.mount_mode;
                });
                if self.settings.mount_mode == MountMode::Folder {
                    self.config_dirty |= path_input(
                        ui,
                        "挂载文件夹",
                        &mut self.settings.mount_folder,
                        Browse::MountFolder,
                    );
                    ui.label(
                        RichText::new("选择本地 NTFS 上已有的空文件夹；不会自动创建目录。")
                            .size(12.0)
                            .color(MUTED),
                    );
                    ui.add_space(6.0);
                }
                control_row(ui, |ui| {
                    if self.settings.mount_mode == MountMode::DriveLetter {
                        ui.label("盘符");
                        let old_letter = self.settings.drive_letter;
                        let selected = self
                            .settings
                            .drive_letter
                            .map(|d| format!("{d}:"))
                            .unwrap_or_else(|| "自动".into());
                        egui::ComboBox::from_id_salt("drive_letter")
                            .selected_text(selected)
                            .width(85.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.settings.drive_letter, None, "自动");
                                for letter in 'A'..='Z' {
                                    let used = self
                                        .disks
                                        .iter()
                                        .flat_map(|disk| &disk.volumes)
                                        .any(|volume| {
                                            volume.to_uppercase().starts_with(&format!("{letter}:"))
                                        });
                                    ui.add_enabled_ui(
                                        !used || self.settings.drive_letter == Some(letter),
                                        |ui| {
                                            ui.selectable_value(
                                                &mut self.settings.drive_letter,
                                                Some(letter),
                                                format!(
                                                    "{letter}:{}",
                                                    if used { "  已使用" } else { "" }
                                                ),
                                            );
                                        },
                                    );
                                }
                            });
                        self.config_dirty |= old_letter != self.settings.drive_letter;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let enabled = !self.settings.base_path.trim().is_empty()
                            && !self.settings.diff_path.trim().is_empty()
                            && (self.settings.mount_mode == MountMode::DriveLetter
                                || !self.settings.mount_folder.trim().is_empty());
                        if ui.add_enabled(enabled, primary("挂载", 106.0)).clicked() {
                            self.mount();
                        }
                        ui.label(
                            RichText::new("差分不存在时自动创建")
                                .size(12.0)
                                .color(MUTED),
                        );
                    });
                });
            });
        });
        ui.add_space(14.0);
        self.mounted_list(ui);
    }

    fn logs_page(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("操作日志").strong().size(17.0));
                ui.label(RichText::new(format!("{} 条", self.logs.len())).color(MUTED));
            });
            ui.add_space(8.0);
            control_row(ui, |ui| {
                if ui.add(button("清空显示")).clicked() {
                    self.logs.clear();
                }
                if ui.add(button("打开日志目录")).clicked() {
                    self.open_path(config::data_dir().join("logs").display().to_string());
                }
            });
            ui.add_space(8.0);
            let footer = "清空显示仅清除当前列表，磁盘日志文件保留。";
            let footer_height = ui
                .painter()
                .layout_no_wrap(footer.into(), egui::FontId::proportional(12.0), MUTED)
                .rect
                .height();
            let notice_space = self.notice.as_ref().map_or(0.0, |(message, _)| {
                let height = ui
                    .painter()
                    .layout(
                        message.clone(),
                        egui::TextStyle::Body.resolve(ui.style()),
                        TEXT,
                        ui.available_width(),
                    )
                    .rect
                    .height();
                height + 24.0 + 12.0 + ui.spacing().item_spacing.y
            });
            let footer_space = footer_height
                + 8.0
                + 2.0 * ui.spacing().item_spacing.y
                + f32::from(CARD_PADDING)
                + 1.0;
            let list_height =
                (ui.clip_rect().bottom() - ui.cursor().top() - footer_space - notice_space)
                    .max(0.0);
            egui::ScrollArea::vertical()
                .id_salt("logs_page_list")
                .min_scrolled_height(0.0)
                .max_height(list_height)
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.logs.is_empty() {
                        ui.label(RichText::new("暂无操作日志").color(MUTED));
                    } else {
                        for message in &self.logs {
                            ui.label(RichText::new(message).size(12.0).color(MUTED));
                        }
                    }
                });
            ui.add_space(8.0);
            ui.label(RichText::new(footer).size(12.0).color(MUTED));
        });
    }

    fn mounted_list(&mut self, ui: &mut egui::Ui) {
        let mut open = None;
        let mut eject = None;
        card(ui, |ui| {
            control_row(ui, |ui| {
                ui.label(RichText::new("已挂载镜像").strong().size(17.0));
                ui.label(RichText::new(format!("{} 个", self.disks.len())).color(MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(self.disk_busy.is_none(), button("刷新"))
                        .clicked()
                    {
                        self.refresh();
                    }
                    if self.disk_busy.is_some() {
                        ui.spinner();
                    }
                });
            });
            ui.add_space(10.0);
            if self.disks.is_empty() {
                ui.add_space(15.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("尚无已挂载镜像").color(MUTED).size(16.0));
                    ui.label(
                        RichText::new("挂载后将在这里显示，可同时管理多个磁盘")
                            .color(MUTED)
                            .size(12.0),
                    );
                });
                ui.add_space(20.0);
            } else {
                let available = ui.available_width();
                let location_width = (available * 0.22).clamp(110.0, 180.0);
                let actions_width = 2.0 * CONTROL_HEIGHT + ui.spacing().item_spacing.x;
                let path_width =
                    ((available - location_width - actions_width - 36.0) / 2.0).max(100.0);
                egui::Grid::new("mounted_images")
                    .num_columns(4)
                    .spacing([12.0, 12.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for header in ["挂载位置 / 状态", "挂载镜像", "基础镜像", "操作"]
                        {
                            ui.label(RichText::new(header).size(12.0).color(MUTED));
                        }
                        ui.end_row();
                        for disk in &self.disks {
                            ui.vertical(|ui| {
                                ui.set_width(location_width);
                                truncated_path(
                                    ui,
                                    &if disk.volumes.is_empty() {
                                        "无挂载位置".into()
                                    } else {
                                        disk.volumes.join("  ")
                                    },
                                    location_width,
                                );
                                ui.label(
                                    RichText::new(if disk.read_only {
                                        "只读"
                                    } else {
                                        "可读写"
                                    })
                                    .size(11.0)
                                    .color(if disk.read_only { MUTED } else { ACCENT }),
                                );
                            });
                            ui.vertical(|ui| {
                                truncated_path(
                                    ui,
                                    &disk.image_path.display().to_string(),
                                    path_width,
                                );
                                ui.label(RichText::new(&disk.kind).color(MUTED).size(11.0));
                                if let Some(warning) = &disk.warning {
                                    ui.label(
                                        RichText::new("需注意")
                                            .color(Color32::from_rgb(160, 95, 20))
                                            .size(11.0),
                                    )
                                    .on_hover_text(warning);
                                }
                            });
                            truncated_path(
                                ui,
                                &disk
                                    .parent_path
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "直接挂载".into()),
                                path_width,
                            );
                            ui.allocate_ui_with_layout(
                                Vec2::new(
                                    2.0 * CONTROL_HEIGHT + ui.spacing().item_spacing.x,
                                    CONTROL_HEIGHT,
                                ),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    if icon_button(
                                        ui,
                                        Icon::Folder,
                                        !disk.volumes.is_empty(),
                                        "打开磁盘",
                                    )
                                    .clicked()
                                    {
                                        open = Some(disk.volumes.clone());
                                    }
                                    if icon_button(
                                        ui,
                                        Icon::Eject,
                                        disk.can_eject && self.disk_busy.is_none(),
                                        if disk.can_eject {
                                            "卸载磁盘"
                                        } else {
                                            "此磁盘受保护，不能在此卸载"
                                        },
                                    )
                                    .clicked()
                                    {
                                        eject = Some(disk.clone());
                                    }
                                },
                            );
                            ui.end_row();
                        }
                    });
            }
            ui.label(
                RichText::new("关闭软件后磁盘继续挂载；弹出保留差分中的修改。")
                    .size(12.0)
                    .color(MUTED),
            );
        });
        if let Some(volumes) = open {
            self.open_disk(volumes);
        }
        if let Some(image) = eject {
            self.eject = Some(EjectConfirmation {
                image,
                focus_cancel: true,
            });
        }
    }

    fn build_page(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.label(RichText::new("将文件夹封装为基础镜像").strong().size(17.0));
            ui.label(
                RichText::new("文件夹内容直接放在镜像根目录。制作期间请停止修改源文件。")
                    .color(MUTED),
            );
            ui.add_space(10.0);
            ui.add_enabled_ui(!self.building, |ui| {
                self.config_dirty |= path_input(
                    ui,
                    "源文件夹",
                    &mut self.settings.source_path,
                    Browse::Folder,
                );
                ui.add_space(8.0);
                self.config_dirty |= path_input(
                    ui,
                    "输出镜像",
                    &mut self.settings.output_path,
                    Browse::SaveVhdx,
                );
                ui.label(
                    RichText::new(
                        "全程写入指定位置，不占用软件所在盘；制作中为 .vhdx.partial，校验卸载后去掉 .partial。",
                    )
                    .size(12.0)
                    .color(MUTED),
                );
                ui.add_space(6.0);
                ui.label(RichText::new("卷标").strong().size(13.0));
                self.config_dirty |= ui
                    .add_sized(
                        [ui.available_width(), CONTROL_HEIGHT],
                        egui::TextEdit::singleline(&mut self.settings.volume_label)
                            .vertical_align(egui::Align::Center)
                            .min_size(Vec2::new(0.0, CONTROL_HEIGHT))
                            .hint_text("留空使用输出镜像文件名（去掉 .vhdx）"),
                    )
                    .changed();
                ui.horizontal_wrapped(|ui| {
                    if !self.settings.output_path.trim().is_empty()
                        || !self.settings.volume_label.is_empty()
                    {
                        match builder::resolve_volume_label(
                            Path::new(self.settings.output_path.trim()),
                            &self.settings.volume_label,
                        ) {
                            Ok(label) => {
                                ui.label(
                                    RichText::new(format!("实际卷标：{label}"))
                                        .size(12.0)
                                        .color(MUTED),
                                );
                            }
                            Err(error) => {
                                ui.label(
                                    RichText::new(error.to_string())
                                        .size(12.0)
                                        .color(Color32::from_rgb(164, 46, 41)),
                                );
                            }
                        }
                    } else {
                        ui.label(
                            RichText::new("挂载后在资源管理器中显示的磁盘名称。")
                                .size(12.0)
                                .color(MUTED),
                        );
                    }
                    ui.label(
                        RichText::new("最多 32 个 UTF-16 字符")
                            .size(12.0)
                            .color(MUTED),
                    )
                    .on_hover_text("大多数汉字和字母计为 1 个；部分符号（如 emoji）计为 2 个。");
                });
                ui.add_space(6.0);
                control_row(ui, |ui| {
                    ui.label("虚拟容量");
                    self.config_dirty |= ui
                        .add(
                            egui::DragValue::new(&mut self.settings.capacity_gib)
                                .range(1..=65_536)
                                .speed(16.0)
                                .suffix(" GiB"),
                        )
                        .changed();
                    ui.add_space(20.0);
                    self.config_dirty |= ui
                        .checkbox(&mut self.settings.compress, "NTFS 压缩")
                        .changed();
                });
                ui.label(
                    RichText::new("动态镜像按写入量增长；虚拟容量是盘内空间上限。")
                        .size(12.0)
                        .color(MUTED),
                );
                ui.add_space(6.0);
                control_row(ui, |ui| {
                    ui.label("校验方式");
                    let old_verify = self.settings.verify;
                    egui::ComboBox::from_id_salt("verify_mode")
                        .width(205.0)
                        .selected_text(match self.settings.verify {
                            VerifyMode::Metadata => "文件信息比较",
                            VerifyMode::Sha256 => "文件内容 SHA-256",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.settings.verify,
                                VerifyMode::Metadata,
                                "文件信息比较",
                            );
                            ui.selectable_value(
                                &mut self.settings.verify,
                                VerifyMode::Sha256,
                                "文件内容 SHA-256",
                            );
                        });
                    self.config_dirty |= old_verify != self.settings.verify;
                });
                ui.label(
                    RichText::new(if self.settings.verify == VerifyMode::Metadata {
                        "比较路径、大小和时间；完成后另外生成整个镜像的 SHA-256。"
                    } else {
                        "逐文件校验内容，耗时更长；完成后另外生成整个镜像的 SHA-256。"
                    })
                    .size(12.0)
                    .color(MUTED),
                );
            });
            ui.add_space(10.0);
            control_row(ui, |ui| {
                if ui
                    .add_enabled(
                        !self.building
                            && !self.exit_when_idle
                            && !self.settings.source_path.trim().is_empty()
                            && !self.settings.output_path.trim().is_empty(),
                        primary("开始制作", 124.0),
                    )
                    .clicked()
                {
                    self.start_build();
                }
                let cancelling = self.cancel.load(Ordering::Relaxed);
                if ui
                    .add_enabled(
                        self.building && !cancelling,
                        button(if cancelling && self.building {
                            "正在取消…"
                        } else {
                            "取消"
                        })
                        .min_size(Vec2::new(85.0, CONTROL_HEIGHT)),
                    )
                    .clicked()
                {
                    self.cancel.store(true, Ordering::Relaxed);
                    self.log("已请求取消，正在等待后台任务停止并卸载镜像。");
                }
                ui.label(
                    RichText::new("失败或取消时保留 .partial 和日志")
                        .size(12.0)
                        .color(MUTED),
                );
            });
        });
        ui.add_space(14.0);
        if self.building || self.build_result.is_some() {
            card(ui, |ui| {
                if self.building {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(&self.progress.phase).strong().size(16.0));
                        if let Some(started) = self.build_started {
                            let secs = started.elapsed().as_secs();
                            ui.label(
                                RichText::new(format!(
                                    "已用时 {:02}:{:02}:{:02}",
                                    secs / 3600,
                                    secs / 60 % 60,
                                    secs % 60
                                ))
                                .color(MUTED),
                            );
                        }
                    });
                    ui.label(&self.progress.message);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "已处理文件：{}{}",
                                self.progress.files,
                                if self.progress.total_files > 0 {
                                    format!(" / {}", self.progress.total_files)
                                } else {
                                    String::new()
                                }
                            ))
                            .color(MUTED),
                        );
                        ui.add_space(15.0);
                        ui.label(
                            RichText::new(format!(
                                "已处理数据：{}{}",
                                paths::format_bytes(self.progress.bytes),
                                if self.progress.total_bytes > 0 {
                                    format!(" / {}", paths::format_bytes(self.progress.total_bytes))
                                } else {
                                    String::new()
                                }
                            ))
                            .color(MUTED),
                        );
                    });
                    if self.exit_when_idle {
                        ui.label(RichText::new("后台清理完成后自动退出，请稍候。").color(ACCENT));
                    }
                } else if let Some(result) = &self.build_result {
                    ui.label(
                        RichText::new("基础镜像已完成")
                            .strong()
                            .size(17.0)
                            .color(ACCENT),
                    );
                    ui.label(result.output.display().to_string());
                    ui.label(format!(
                        "{} 个文件 · 源数据 {} · 镜像大小 {}",
                        result.files,
                        paths::format_bytes(result.logical_bytes),
                        paths::format_bytes(result.image_bytes)
                    ));
                    ui.label(
                        RichText::new(format!("SHA-256：{}", result.sha256))
                            .size(11.0)
                            .color(MUTED),
                    );
                    if ui.add(button("填入挂载页面")).clicked() {
                        self.settings.base_path = result.output.display().to_string();
                        self.diff_manual = false;
                        self.set_default_diff();
                        self.tab = Tab::Mount;
                    }
                }
            });
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.close_confirmation {
            let mut dismiss = false;
            let mut cancel_and_exit = false;
            egui::Modal::new(egui::Id::new("close_build_confirmation")).show(ctx, |ui| {
                ui.set_width(430.0);
                ui.heading("镜像仍在制作中");
                ui.add_space(10.0);
                ui.label("可以返回继续等待，或取消制作并在后台清理完成后退出。");
                ui.label(RichText::new("未完成镜像保留为 .partial，源文件不受影响。").color(MUTED));
                ui.add_space(15.0);
                control_row(ui, |ui| {
                    let response = ui.add(button("返回"));
                    if self.close_focus_cancel {
                        response.request_focus();
                        self.close_focus_cancel = false;
                    }
                    dismiss = response.clicked();
                    cancel_and_exit = ui.add(button("取消制作并退出")).clicked();
                });
            });
            if dismiss {
                self.close_confirmation = false;
            }
            if cancel_and_exit {
                self.close_confirmation = false;
                self.exit_when_idle = true;
                self.cancel.store(true, Ordering::Relaxed);
                if !self.building {
                    self.save();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            return;
        }
        if let Some(confirmation) = &mut self.eject {
            let mut dismiss = false;
            let mut confirm = false;
            egui::Modal::new(egui::Id::new("eject_confirmation")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(format!(
                            "确认卸载 {}？",
                            if confirmation.image.volumes.is_empty() {
                                "此镜像".into()
                            } else {
                                confirmation.image.volumes.join("、")
                            }
                        ))
                        .heading(),
                    )
                    .wrap(),
                );
                ui.add_space(10.0);
                ui.label("挂载镜像：");
                ui.add(
                    egui::Label::new(confirmation.image.image_path.display().to_string()).wrap(),
                );
                ui.add_space(10.0);
                ui.label("卸载后已保存的修改仍保留，请先保存并关闭盘内文件。");
                ui.add_space(16.0);
                control_row(ui, |ui| {
                    let response = ui.add(button("取消"));
                    if confirmation.focus_cancel {
                        response.request_focus();
                        confirmation.focus_cancel = false;
                    }
                    dismiss = response.clicked();
                    confirm = ui
                        .add_enabled(self.disk_busy.is_none(), primary("卸载", 80.0))
                        .clicked();
                });
            });
            if confirm {
                let image = self.eject.take().unwrap().image;
                self.unmount(image.image_path);
            } else if dismiss {
                self.eject = None;
            }
        }
        if let Some(volumes) = &self.open_volumes {
            let mut chosen = None;
            let mut dismiss = false;
            egui::Modal::new(egui::Id::new("open_volume")).show(ctx, |ui| {
                ui.set_width(340.0);
                ui.heading("选择要打开的卷");
                ui.add_space(12.0);
                for volume in volumes {
                    if ui.add(button(volume).wrap()).clicked() {
                        chosen = Some(volume.clone());
                    }
                }
                ui.add_space(12.0);
                dismiss = ui.add(button("取消")).clicked();
            });
            if let Some(volume) = chosen {
                self.open_volumes = None;
                self.open_path(volume);
            } else if dismiss {
                self.open_volumes = None;
            }
        }
    }
}

impl eframe::App for DockApp {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.poll_events(ctx);
        if ctx.input(|input| input.viewport().close_requested()) {
            if self.building {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if !self.exit_when_idle {
                    self.close_confirmation = true;
                    self.close_focus_cancel = true;
                }
            } else {
                self.save();
            }
        }
        if let Some(rect) = ctx.input(|input| input.viewport().inner_rect) {
            let size = [rect.width(), rect.height()];
            if self.settings.window_size != Some(size) {
                self.settings.window_size = Some(size);
                self.config_dirty = true;
            }
        }
        if self.tab == Tab::Mount && self.disk_busy.is_none() {
            let dropped = ctx.input(|input| input.raw.dropped_files.clone());
            if let Some(path) = dropped
                .into_iter()
                .filter_map(|file| file.path)
                .find(|path| {
                    path.extension().is_some_and(|extension| {
                        extension.eq_ignore_ascii_case("vhd")
                            || extension.eq_ignore_ascii_case("vhdx")
                    })
                })
            {
                self.settings.base_path = path.display().to_string();
                self.set_default_diff();
            }
        }
        if self.last_auto_base != self.settings.base_path {
            self.set_default_diff();
        }
        egui::TopBottomPanel::bottom("status_bar")
            .frame(
                egui::Frame::new()
                    .fill(Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(24, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(action) = &self.disk_busy {
                        ui.spinner();
                        ui.label(RichText::new(action).color(ACCENT));
                    } else {
                        ui.label(
                            RichText::new(if self.building {
                                "制作任务运行中"
                            } else {
                                "就绪"
                            })
                            .color(MUTED),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .size(11.0)
                                .color(MUTED),
                        );
                    });
                });
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(245, 247, 250))
                    .inner_margin(24),
            )
            .show(ctx, |ui| {
                self.header(ui);
                egui::ScrollArea::vertical()
                    .id_salt(match self.tab {
                        Tab::Mount => "mount_page_scroll",
                        Tab::Build => "build_page_scroll",
                        Tab::Logs => "logs_page_scroll",
                    })
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        match self.tab {
                            Tab::Mount => self.mount_page(ui),
                            Tab::Build => self.build_page(ui),
                            Tab::Logs => self.logs_page(ui),
                        }
                        if let Some((message, error)) = &self.notice {
                            ui.add_space(12.0);
                            egui::Frame::new()
                                .fill(if *error {
                                    Color32::from_rgb(255, 240, 239)
                                } else {
                                    Color32::from_rgb(232, 245, 242)
                                })
                                .corner_radius(7.0)
                                .inner_margin(12)
                                .show(ui, |ui| {
                                    ui.label(RichText::new(message).color(if *error {
                                        Color32::from_rgb(164, 46, 41)
                                    } else {
                                        ACCENT
                                    }));
                                });
                        }
                    });
            });
        self.dialogs(ctx);
        if self.config_dirty && !self.building && self.last_save.elapsed() >= Duration::from_secs(2)
        {
            self.save();
        }
        if self.building || self.disk_busy.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}

fn primary(label: &str, width: f32) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(Color32::WHITE))
        .fill(ACCENT)
        .min_size(Vec2::new(width, CONTROL_HEIGHT))
        .corner_radius(6.0)
}

fn tab_button(ui: &mut egui::Ui, title: &str, selected: bool) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(title).size(14.0).color(if selected {
            Color32::WHITE
        } else {
            TEXT
        }))
        .fill(if selected { ACCENT } else { Color32::WHITE })
        .stroke(Stroke::new(1.0_f32, if selected { ACCENT } else { BORDER }))
        .min_size(Vec2::new(124.0, CONTROL_HEIGHT))
        .corner_radius(7.0),
    )
}

fn start_log_writer() -> mpsc::Sender<LogCommand> {
    let (tx, rx) = mpsc::channel::<LogCommand>();
    std::thread::spawn(move || {
        let directory = config::data_dir().join("logs");
        if std::fs::create_dir_all(&directory).is_err() {
            return;
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(format!("session-{timestamp}-{}.log", std::process::id())))
        else {
            return;
        };
        run_log_writer(file, rx);
    });
    tx
}

fn run_log_writer(mut file: std::fs::File, commands: mpsc::Receiver<LogCommand>) {
    use std::io::Write;
    let mut paused = false;
    for command in commands {
        match command {
            LogCommand::Write(message) if !paused => {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let _ = writeln!(file, "[{timestamp}] {message}");
            }
            LogCommand::Write(_) => {}
            LogCommand::Pause(ack) => {
                paused = true;
                let _ = file.flush();
                let _ = ack.send(());
            }
            LogCommand::Resume => paused = false,
        }
    }
}

fn card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(Color32::WHITE)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(10.0)
        .inner_margin(CARD_PADDING)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            content(ui);
        });
}

fn button(label: &str) -> egui::Button<'_> {
    egui::Button::new(label).min_size(Vec2::new(0.0, CONTROL_HEIGHT))
}

/// Interactive rows know their height before the first label is placed. Keep
/// this local so ordinary text-only rows retain their compact line spacing.
fn control_row<R>(
    ui: &mut egui::Ui,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), CONTROL_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            let text_height = ui
                .text_style_height(&egui::TextStyle::Button)
                .max(ui.spacing().icon_width);
            ui.spacing_mut().interact_size.y = CONTROL_HEIGHT;
            ui.spacing_mut().button_padding.y = ((CONTROL_HEIGHT - text_height) * 0.5).max(0.0);
            content(ui)
        },
    )
}

enum Browse {
    Image,
    SaveDiff,
    Folder,
    MountFolder,
    SaveVhdx,
}

fn path_input(ui: &mut egui::Ui, label: &str, value: &mut String, kind: Browse) -> bool {
    ui.label(RichText::new(label).strong().size(13.0));
    let mut changed = false;
    control_row(ui, |ui| {
        changed |= ui
            .add_sized(
                [ui.available_width() - 70.0, CONTROL_HEIGHT],
                egui::TextEdit::singleline(value)
                    .vertical_align(egui::Align::Center)
                    .min_size(Vec2::new(0.0, CONTROL_HEIGHT))
                    .hint_text(match kind {
                        Browse::Image => r"本地路径或 \\NAS\共享\镜像.vhdx",
                        Browse::SaveDiff => r".\diffs\镜像-diff.vhdx",
                        Browse::Folder => r"E:\需要归档的文件夹",
                        Browse::MountFolder => r"D:\Mounts\Archive",
                        Browse::SaveVhdx => r"D:\Backup\镜像-base.vhdx",
                    }),
            )
            .changed();
        if ui
            .add_sized([62.0, CONTROL_HEIGHT], button("浏览"))
            .clicked()
        {
            let mut dialog = rfd::FileDialog::new();
            if !value.trim().is_empty() {
                if let Ok(path) = DockApp::resolved(value) {
                    if path.is_dir() {
                        dialog = dialog.set_directory(path);
                    } else {
                        if let Some(parent) = path.parent() {
                            dialog = dialog.set_directory(parent);
                        }
                        if let Some(name) = path.file_name() {
                            dialog = dialog.set_file_name(name.to_string_lossy());
                        }
                    }
                }
            }
            let chosen = match kind {
                Browse::Image => dialog.add_filter("虚拟硬盘", &["vhdx", "vhd"]).pick_file(),
                Browse::SaveDiff => dialog.add_filter("虚拟硬盘", &["vhdx", "vhd"]).save_file(),
                Browse::Folder | Browse::MountFolder => dialog.pick_folder(),
                Browse::SaveVhdx => dialog.add_filter("VHDX 镜像", &["vhdx"]).save_file(),
            };
            if let Some(path) = chosen {
                *value = path.display().to_string();
                changed = true;
            }
        }
    });
    changed
}

fn truncated_path(ui: &mut egui::Ui, path: &str, width: f32) {
    ui.allocate_ui_with_layout(
        Vec2::new(width, 23.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.add(egui::Label::new(path).truncate().halign(egui::Align::Min))
                .on_hover_text(path);
        },
    );
}

#[derive(Clone, Copy)]
enum Icon {
    Folder,
    Eject,
}

fn icon_button(ui: &mut egui::Ui, icon: Icon, enabled: bool, tooltip: &str) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let (rect, response) =
            ui.allocate_exact_size(Vec2::splat(CONTROL_HEIGHT), egui::Sense::click());
        let color = if !enabled {
            Color32::from_rgb(183, 194, 204)
        } else if response.hovered() {
            ACCENT
        } else {
            MUTED
        };
        if response.hovered() && enabled {
            ui.painter()
                .rect_filled(rect, 5.0, Color32::from_rgb(231, 242, 243));
        }
        let painter = ui.painter();
        let center = rect.center();
        let point = |x: f32, y: f32| center + Vec2::new(x, y);
        let stroke = Stroke::new(1.7_f32, color);
        match icon {
            Icon::Folder => {
                painter.add(egui::Shape::closed_line(
                    vec![
                        point(-9.0, -5.0),
                        point(-9.0, 7.0),
                        point(9.0, 7.0),
                        point(9.0, -3.0),
                        point(-1.0, -3.0),
                        point(-4.0, -6.0),
                        point(-9.0, -6.0),
                    ],
                    stroke,
                ));
                painter.line_segment([point(-8.0, -1.0), point(8.0, -1.0)], stroke);
            }
            Icon::Eject => {
                painter.add(egui::Shape::closed_line(
                    vec![point(0.0, -8.0), point(-8.0, 2.0), point(8.0, 2.0)],
                    stroke,
                ));
                painter.rect_stroke(
                    egui::Rect::from_min_max(point(-8.0, 6.0), point(8.0, 9.0)),
                    0.0,
                    stroke,
                    egui::StrokeKind::Inside,
                );
            }
        }
        response.on_hover_text(tooltip)
    })
    .inner
}

fn brand_icon(ui: &mut egui::Ui, texture: &egui::TextureHandle) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(50.0), egui::Sense::hover());
    ui.painter().image(
        texture.id(),
        rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
}

fn install_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let windows_dir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let candidates = [
        windows_dir.join("Fonts/msyh.ttc"),
        windows_dir.join("Fonts/msyh.ttf"),
        windows_dir.join("Fonts/simhei.ttf"),
        PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
        PathBuf::from("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"),
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".into());
            break;
        }
    }
    ctx.set_fonts(fonts);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.selection.bg_fill = Color32::from_rgb(199, 231, 229);
    style.visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(247, 249, 251);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.spacing.item_spacing = Vec2::new(8.0, 7.0);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(1100.0, 900.0),
            )),
            events,
            ..Default::default()
        }
    }

    fn rendered_text(shapes: &[egui::epaint::ClippedShape]) -> String {
        fn collect(shape: &egui::Shape, text: &mut String) {
            match shape {
                egui::Shape::Text(shape) => {
                    text.push_str(&shape.galley.job.text);
                    text.push('\n');
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, text);
                    }
                }
                _ => {}
            }
        }
        let mut text = String::new();
        for shape in shapes {
            collect(&shape.shape, &mut text);
        }
        text
    }

    fn text_rect(shapes: &[egui::epaint::ClippedShape], text: &str) -> egui::Rect {
        shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(shape) if shape.galley.job.text == text => Some(
                    egui::Rect::from_min_size(shape.pos, shape.galley.rect.size()),
                ),
                _ => None,
            })
            .unwrap_or_else(|| panic!("Missing UI label: {text}"))
    }

    fn pointer_events(position: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(position),
            egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            },
        ]
    }

    fn draw_header(
        ctx: &egui::Context,
        app: &mut DockApp,
        width: f32,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let mut raw = input(events);
        raw.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            Vec2::new(width, 760.0),
        ));
        ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.header(ui));
        })
    }

    #[test]
    fn logs_tab_is_right_aligned_and_real_clicks_preserve_tasks() {
        for width in [1100.0, 1280.0] {
            let ctx = egui::Context::default();
            let mut app = DockApp::with_settings(AppConfig::default());
            app.building = true;
            app.disk_busy = Some("正在挂载".into());
            app.notice = Some(("已有提示".into(), true));
            app.logs.push_back("已有日志".into());
            let cancel = app.cancel.clone();
            let output = draw_header(&ctx, &mut app, width, vec![]);
            let mount = text_rect(&output.shapes, "挂载镜像");
            let build = text_rect(&output.shapes, "制作镜像");
            let logs = text_rect(&output.shapes, "日志");
            assert!((mount.center().y - logs.center().y).abs() < 1.0);
            assert!((build.center().y - logs.center().y).abs() < 1.0);
            assert!(
                logs.center().x > width - 100.0,
                "Logs must sit at the right edge"
            );
            assert!(mount.right() < build.left() && build.right() < logs.left());
            for (target, rect) in [(Tab::Logs, logs), (Tab::Build, build), (Tab::Mount, mount)] {
                let _ = draw_header(&ctx, &mut app, width, pointer_events(rect.center(), true));
                let _ = draw_header(&ctx, &mut app, width, pointer_events(rect.center(), false));
                assert_eq!(app.tab, target);
            }
            assert!(app.building);
            assert_eq!(app.disk_busy.as_deref(), Some("正在挂载"));
            assert_eq!(app.notice, Some(("已有提示".into(), true)));
            assert!(Arc::ptr_eq(&cancel, &app.cancel));
            assert!(!app.cancel.load(Ordering::Relaxed));
            assert_eq!(app.logs.len(), 1);
        }
    }

    #[test]
    fn logs_page_clear_only_changes_memory_and_keeps_logger_untouched() {
        let ctx = egui::Context::default();
        let mut app = DockApp::with_settings(AppConfig::default());
        app.tab = Tab::Logs;
        app.logs.push_back("测试会话日志内容".into());
        let (writer, commands) = mpsc::channel();
        app.log_tx = Some(writer);
        let render = |app: &mut DockApp, events| {
            ctx.run(input(events), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.logs_page(ui));
            })
        };
        let output = render(&mut app, vec![]);
        let text = rendered_text(&output.shapes);
        assert!(text.contains("测试会话日志内容"));
        assert!(text.contains("打开日志目录"));
        assert!(text.contains("磁盘日志文件保留"));
        let clear = text_rect(&output.shapes, "清空显示").center();
        let _ = render(&mut app, pointer_events(clear, true));
        let output = render(&mut app, pointer_events(clear, false));
        assert!(app.logs.is_empty());
        assert!(rendered_text(&output.shapes).contains("暂无操作日志"));
        assert!(
            matches!(commands.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "Clearing the displayed list must not send disk logger operations"
        );
    }

    #[test]
    fn logs_footer_and_notice_stay_inside_the_visible_page() {
        for height in [600.0, 760.0, 820.0] {
            for show_notice in [false, true] {
                let ctx = egui::Context::default();
                install_style(&ctx);
                let mut app = DockApp::with_settings(AppConfig::default());
                app.tab = Tab::Logs;
                app.logs = (0..100).map(|index| format!("操作日志 {index}")).collect();
                let message = "已有错误提示，请检查镜像路径后重试。".repeat(8);
                if show_notice {
                    app.notice = Some((message.clone(), true));
                }
                let mut raw = input(vec![]);
                raw.screen_rect = Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1100.0, height),
                ));
                let output = ctx.run(raw, |ctx| {
                    egui::TopBottomPanel::bottom("test_status")
                        .exact_height(34.0)
                        .show(ctx, |_| {});
                    egui::CentralPanel::default()
                        .frame(egui::Frame::new().inner_margin(24))
                        .show(ctx, |ui| {
                            app.header(ui);
                            egui::ScrollArea::vertical()
                                .id_salt("test_logs_container")
                                .show(ui, |ui| {
                                    app.logs_page(ui);
                                    if let Some((message, _)) = &app.notice {
                                        ui.add_space(12.0);
                                        egui::Frame::new().inner_margin(12).show(ui, |ui| {
                                            ui.label(message);
                                        });
                                    }
                                });
                        });
                });
                let assert_visible = |text: &str| {
                    let (shape, clip) = output
                        .shapes
                        .iter()
                        .find_map(|clipped| match &clipped.shape {
                            egui::Shape::Text(shape) if shape.galley.job.text == text => {
                                Some((shape, clipped.clip_rect))
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| panic!("Expected visible text: {text}"));
                    assert!(
                        shape.pos.y + shape.galley.rect.max.y <= clip.bottom(),
                        "Text clipped at height {height}, notice={show_notice}: {text}"
                    );
                };
                assert_visible("清空显示仅清除当前列表，磁盘日志文件保留。");
                if show_notice {
                    assert_visible(&message);
                }
            }
        }
    }

    fn disk(letter: &str, name: &str) -> MountedImage {
        MountedImage {
            image_path: PathBuf::from(format!(r"D:\Diff\{name}.vhdx")),
            parent_path: Some(PathBuf::from(format!(r"\\NAS\backup\{name}.vhdx"))),
            volumes: vec![format!("{letter}:\\")],
            kind: "差分盘".into(),
            read_only: false,
            can_eject: true,
            warning: None,
        }
    }

    #[test]
    fn selecting_mount_mode_changes_the_request_and_preserves_diff_and_drive() {
        for width in [860.0, 900.0] {
            let ctx = egui::Context::default();
            install_style(&ctx);
            let mut app = DockApp::with_settings(AppConfig {
                base_path: "archive.vhdx".into(),
                diff_path: "diffs/archive-diff.vhdx".into(),
                drive_letter: Some('Q'),
                mount_folder: "mounts/archive".into(),
                ..Default::default()
            });
            let original_diff = app.settings.diff_path.clone();
            let render = |app: &mut DockApp, events| {
                let mut raw = input(events);
                raw.screen_rect = Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(width, 900.0),
                ));
                ctx.run(raw, |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.mount_page(ui));
                })
            };
            let initial = render(&mut app, vec![]);
            let folder = text_rect(&initial.shapes, "文件夹").center();
            let _ = render(&mut app, pointer_events(folder, true));
            let selected = render(&mut app, pointer_events(folder, false));
            assert_eq!(app.settings.mount_mode, MountMode::Folder);
            assert!(app.config_dirty);
            assert!(rendered_text(&selected.shapes).contains("已有的空文件夹"));
            assert!(text_rect(&selected.shapes, "挂载").right() < width);
            let folder_request = app.mount_request().unwrap();
            assert_eq!(folder_request.drive_letter, None);
            assert_eq!(
                folder_request.mount_folder,
                Some(DockApp::resolved("mounts/archive").unwrap())
            );
            assert_eq!(
                folder_request.diff,
                DockApp::resolved(&original_diff).unwrap()
            );
            let drive = text_rect(&selected.shapes, "盘符").center();
            let _ = render(&mut app, pointer_events(drive, true));
            let _ = render(&mut app, pointer_events(drive, false));
            assert_eq!(app.settings.mount_mode, MountMode::DriveLetter);
            let drive_request = app.mount_request().unwrap();
            assert_eq!(drive_request.drive_letter, Some('Q'));
            assert!(drive_request.mount_folder.is_none());
            assert_eq!(app.settings.diff_path, original_diff);
            assert_eq!(app.settings.mount_folder, "mounts/archive");
            app.settings.mount_mode = MountMode::Folder;
            app.settings.mount_folder.clear();
            assert!(app.mount_request().is_err());
        }
    }

    #[test]
    fn long_folder_mounts_keep_table_actions_and_confirmation_inside_the_window() {
        let ctx = egui::Context::default();
        install_style(&ctx);
        let mut app = DockApp::with_settings(AppConfig::default());
        let folder = format!(r"D:\Mounts\{}\", "ArchiveDocuments".repeat(12));
        let mut mounted = disk("F", "archive");
        mounted.volumes = vec![folder.clone()];
        app.disks = vec![mounted.clone()];
        let mut raw = input(vec![]);
        raw.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            Vec2::new(860.0, 900.0),
        ));
        let output = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.mounted_list(ui));
        });
        assert!(text_rect(&output.shapes, "操作").right() < 860.0);
        assert!(rendered_text(&output.shapes).contains("挂载位置 / 状态"));
        app.eject = Some(EjectConfirmation {
            image: mounted,
            focus_cancel: true,
        });
        // egui sizes a newly opened Area in an invisible first pass.
        let _ = ctx.run(input(vec![]), |ctx| app.dialogs(ctx));
        let output = ctx.run(input(vec![]), |ctx| app.dialogs(ctx));
        let heading = text_rect(&output.shapes, &format!("确认卸载 {folder}？"));
        assert!(
            heading.width() <= 461.0,
            "Long mount paths must wrap in the modal"
        );
        assert!(app.disk_busy.is_none());
    }

    #[test]
    fn mounted_rows_render_separate_disks_and_parents() {
        let ctx = egui::Context::default();
        let mut app = DockApp::with_settings(AppConfig::default());
        app.disks = vec![disk("F", "alpha"), disk("G", "beta")];
        let output = ctx.run(input(vec![]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.mount_page(ui));
        });
        let text = rendered_text(&output.shapes);
        assert_eq!(
            text.lines().filter(|line| *line == "基础镜像").count(),
            2,
            "One base input plus one table heading must remain: {text}"
        );
        assert_eq!(
            text.lines().filter(|line| *line == "本地差分镜像").count(),
            1,
            "{text}"
        );
        assert!(!text.contains("重新定位"), "{text}");
        assert!(!text.contains("验证并更新父路径"), "{text}");
        assert!(text.contains("F:\\"), "{text}");
        assert!(text.contains("G:\\"), "{text}");
        assert!(text.contains(r"D:\Diff\alpha.vhdx"), "{text}");
        assert!(text.contains(r"\\NAS\backup\beta.vhdx"), "{text}");
    }

    #[test]
    fn eject_confirmation_enter_cancels_without_starting_unmount() {
        let ctx = egui::Context::default();
        let mut app = DockApp::with_settings(AppConfig::default());
        app.eject = Some(EjectConfirmation {
            image: disk("F", "alpha"),
            focus_cancel: true,
        });
        let _ = ctx.run(input(vec![]), |ctx| app.dialogs(ctx));
        assert!(ctx.memory(|memory| memory.focused()).is_some());
        let _ = ctx.run(
            input(vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }]),
            |ctx| app.dialogs(ctx),
        );
        assert!(
            app.eject.is_none(),
            "Enter should activate the default Cancel button"
        );
        assert!(
            app.disk_busy.is_none(),
            "Cancel must never start disk operations"
        );
    }

    #[test]
    fn builder_has_single_output_and_partial_explanation() {
        let ctx = egui::Context::default();
        let mut app = DockApp::with_settings(AppConfig::default());
        let output = ctx.run(input(vec![]), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.build_page(ui));
        });
        let text = rendered_text(&output.shapes);
        assert_eq!(
            text.lines().filter(|line| *line == "输出镜像").count(),
            1,
            "{text}"
        );
        assert!(text.contains(".vhdx.partial"), "{text}");
        assert!(!text.contains("本地临时目录"), "{text}");
        assert!(text.contains("卷标"), "{text}");
        assert!(
            text.contains("留空使用输出镜像文件名（去掉 .vhdx）"),
            "{text}"
        );
    }

    #[test]
    fn build_request_passes_custom_volume_label_verbatim() {
        let mut app = DockApp::with_settings(AppConfig {
            source_path: "source".into(),
            output_path: "archive.vhdx".into(),
            volume_label: "  开发归档 ' $x  ".into(),
            ..Default::default()
        });
        assert_eq!(
            app.build_request().unwrap().volume_label,
            "  开发归档 ' $x  "
        );
        app.settings.volume_label.clear();
        assert_eq!(app.build_request().unwrap().volume_label, "archive");
        assert!(
            app.settings.volume_label.is_empty(),
            "The automatic label must not overwrite the user's empty setting"
        );
        app.settings.volume_label = " ".into();
        assert!(
            app.build_request().is_err(),
            "Invalid labels must be reported before starting the worker"
        );
        app.settings.volume_label = "A".repeat(33);
        assert!(app.build_request().is_err());
    }

    #[test]
    fn volume_label_and_start_button_fit_default_window() {
        for width in [1100.0, 1280.0] {
            let ctx = egui::Context::default();
            install_style(&ctx);
            let mut app = DockApp::with_settings(AppConfig {
                source_path: "source".into(),
                output_path: "Archive.vhdx".into(),
                ..Default::default()
            });
            let mut raw = input(vec![]);
            raw.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(width, 760.0),
            ));
            let output = ctx.run(raw, |ctx| {
                egui::TopBottomPanel::bottom("status_bar")
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(24, 10)))
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("就绪");
                        });
                    });
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().inner_margin(24))
                    .show(ctx, |ui| {
                        app.header(ui);
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            app.build_page(ui);
                        });
                    });
            });
            let text = rendered_text(&output.shapes);
            assert!(text.contains("实际卷标：Archive"), "{text}");
            assert!(text.contains("最多 32 个 UTF-16 字符"), "{text}");
            let (button, clip) = output
                .shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(shape) if shape.galley.job.text == "开始制作" => {
                        Some((shape, clipped.clip_rect))
                    }
                    _ => None,
                })
                .expect("Start button must be rendered");
            let button_bottom = button.pos.y + (button.galley.rect.height() + CONTROL_HEIGHT) * 0.5;
            assert!(
                button_bottom <= clip.bottom(),
                "Start button is clipped at window width {width}: bottom={button_bottom}, clip={clip:?}"
            );
        }
    }

    #[test]
    fn manual_diff_survives_base_changes_and_reload() {
        let settings = AppConfig {
            base_path: "first.vhdx".into(),
            diff_path: "custom/work.vhdx".into(),
            ..Default::default()
        };
        let mut app = DockApp::with_settings(settings);
        assert!(app.diff_manual);
        app.settings.base_path = "second.vhdx".into();
        app.set_default_diff();
        assert_eq!(app.settings.diff_path, "custom/work.vhdx");
        app.diff_manual = false;
        app.set_default_diff();
        assert_eq!(
            DockApp::resolved(&app.settings.diff_path).unwrap(),
            config::exe_dir().join("diffs").join("second-diff.vhdx")
        );
        let reloaded = DockApp::with_settings(app.settings);
        assert!(
            !reloaded.diff_manual,
            "Automatically generated paths should remain automatic across restarts"
        );
    }

    #[test]
    fn cancellation_request_does_not_hide_cleanup_failure_or_exit() {
        let mut app = DockApp::with_settings(AppConfig::default());
        app.building = true;
        app.exit_when_idle = true;
        app.cancel.store(true, Ordering::Relaxed);
        let close = app.finish_build(Err(BuildFailure {
            message: "清理时无法卸载未完成镜像；请手动卸载".into(),
            clean_cancel: false,
        }));
        assert!(
            !close,
            "A cleanup failure must never close the error window"
        );
        assert!(!app.exit_when_idle);
        assert!(!app.building);
        assert!(
            app.notice.as_ref().is_some_and(|(_, error)| *error),
            "Failure must be shown in red even if cancellation was requested"
        );
    }

    #[test]
    fn clean_cancellation_can_complete_cancel_and_exit() {
        let mut app = DockApp::with_settings(AppConfig::default());
        app.building = true;
        app.exit_when_idle = true;
        app.cancel.store(true, Ordering::Relaxed);
        let close = app.finish_build(Err(BuildFailure {
            message: "操作已取消；未完成镜像和日志已保留".into(),
            clean_cancel: true,
        }));
        assert!(close);
        assert!(!app.building);
        assert!(app.notice.as_ref().is_some_and(|(_, error)| !error));
    }

    #[test]
    fn config_save_is_deferred_while_builder_reads_source() {
        let mut app = DockApp::with_settings(AppConfig::default());
        app.building = true;
        app.config_dirty = true;
        let last_save = app.last_save;
        app.save();
        assert!(
            app.config_dirty,
            "Changes must remain pending until the build ends"
        );
        assert_eq!(app.last_save, last_save);
    }

    #[test]
    fn logger_acknowledges_prior_writes_and_stays_stable_until_resume() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.log");
        let file = std::fs::File::create(&path).unwrap();
        let (tx, rx) = mpsc::channel();
        let writer = std::thread::spawn(move || run_log_writer(file, rx));
        let pause = |tx: &mpsc::Sender<LogCommand>| {
            let (ack, receiver) = mpsc::channel();
            tx.send(LogCommand::Pause(ack)).unwrap();
            receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        };
        tx.send(LogCommand::Write("before scan".into())).unwrap();
        pause(&tx);
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(before.contains("before scan"));
        tx.send(LogCommand::Write("during scan".into())).unwrap();
        pause(&tx);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "Paused GUI logging must not mutate an archived source"
        );
        tx.send(LogCommand::Resume).unwrap();
        tx.send(LogCommand::Write("after verification".into()))
            .unwrap();
        pause(&tx);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("after verification"));
        assert!(!after.contains("during scan"));
        drop(tx);
        writer.join().unwrap();
    }
}
