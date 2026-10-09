#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod app;

fn main() -> eframe::Result {
    #[cfg(windows)]
    if !ensure_elevated() {
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([860.0, 600.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "VhdxDock",
        options,
        Box::new(|cc| Ok(Box::new(app::DockApp::new(cc)))),
    )
}

/// Elevate only the interactive application, not Cargo's unit-test executables.
#[cfg(windows)]
fn ensure_elevated() -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::{w, PCWSTR},
        Win32::UI::{
            Shell::{IsUserAnAdmin, ShellExecuteW},
            WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK, SW_SHOWNORMAL},
        },
    };
    if unsafe { IsUserAnAdmin().as_bool() } {
        return true;
    }
    let Some(exe) = std::env::current_exe().ok() else {
        return false;
    };
    let path: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let directory: Vec<u16> = exe
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(path.as_ptr()),
            None,
            PCWSTR(directory.as_ptr()),
            SW_SHOWNORMAL,
        )
    };
    if (result.0 as isize) <= 32 {
        unsafe {
            MessageBoxW(
                None,
                w!("VhdxDock 需要管理员权限来创建和挂载虚拟磁盘。管理员启动被取消或失败。"),
                w!("VhdxDock"),
                MB_OK | MB_ICONERROR,
            );
        }
    }
    false
}
