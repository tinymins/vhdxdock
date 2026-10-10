//! Opt-in, fixture-only application screenshots. No real disks are discovered,
//! created, attached or detached. Only this example's egui framebuffer is saved.
//!
//! cargo run --features ui-preview --example ui_preview -- mount D:\QA\mount.png
//! Scenarios: mount, build, eject, logs.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[allow(dead_code)]
#[path = "../src/app.rs"]
mod app;

use eframe::egui;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

struct Preview {
    app: app::DockApp,
    output: PathBuf,
    frames: u32,
    requested: bool,
    started: Instant,
}

impl eframe::App for Preview {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let screenshot = ctx.input(|input| {
            input.events.iter().find_map(|event| {
                if let egui::Event::Screenshot { image, .. } = event {
                    Some(image.clone())
                } else {
                    None
                }
            })
        });
        if let Some(screenshot) = screenshot {
            let pixels: Vec<u8> = screenshot
                .pixels
                .iter()
                .flat_map(|pixel| pixel.to_array())
                .collect();
            let result = image::save_buffer_with_format(
                &self.output,
                &pixels,
                screenshot.size[0] as u32,
                screenshot.size[1] as u32,
                image::ColorType::Rgba8,
                image::ImageFormat::Png,
            );
            if let Err(error) = result {
                let _ = std::fs::write(
                    self.output.with_extension("error.txt"),
                    format!("Screenshot save failed: {error}"),
                );
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.app.update(ctx, frame);
        self.frames += 1;
        if self.frames >= 4 && !self.requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            self.requested = true;
        }
        if self.started.elapsed() > Duration::from_secs(20) {
            let _ = std::fs::write(
                self.output.with_extension("error.txt"),
                "Screenshot timed out",
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(80));
    }
}

fn main() -> eframe::Result {
    let mut args = std::env::args_os().skip(1);
    let scenario = args
        .next()
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| "mount".into());
    let output = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("ui-{scenario}.png")));
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    // Documentation views show both mounted example disks without scrolling.
    let height = if matches!(scenario.as_str(), "mount" | "eject") {
        860.0
    } else {
        760.0
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_icon(app::application_icon())
            .with_inner_size([1100.0, height])
            .with_min_inner_size([860.0, 600.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "VhdxDock UI preview",
        options,
        Box::new(move |cc| {
            Ok(Box::new(Preview {
                app: app::DockApp::preview(&cc.egui_ctx, &scenario),
                output,
                frames: 0,
                requested: false,
                started: Instant::now(),
            }))
        }),
    )
}
