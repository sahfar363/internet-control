#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Internet Control — usage meter v0.1.
//!
//! Windows 10/11 x64. Lists all network adapters, polls their 64-bit
//! octet counters every 100 ms, and shows real download/upload for the
//! selected adapter. No admin rights needed for the meter.

mod adapters;
mod app;
mod meter;
mod store;

use std::sync::mpsc;

fn main() -> eframe::Result<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || adapters::sample_loop(tx));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 680.0])
            .with_title("Internet Control — Usage Meter"),
        ..Default::default()
    };
    eframe::run_native(
        "Internet Control — Usage Meter",
        options,
        Box::new(|_cc| Ok(Box::new(app::MeterApp::new(rx)))),
    )
}
