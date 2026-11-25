#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Internet Control — usage meter v0.2.
//!
//! Windows 10/11 x64. Counts real download/upload by capturing packets
//! with WinDivert (sniff mode) — the standard adapter counters never
//! move on PDAnet PPP adapters, so packet counting is the only way.
//! Falls back to the v0.1 counter mode if WinDivert cannot be opened.
//! Administrator rights are required in v0.2 (WinDivert driver).

mod adapters;
mod app;
mod capture;
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
