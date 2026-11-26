#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Internet Control — usage meter v0.3.
//!
//! Windows 10/11 x64. Counts real download/upload by capturing packets
//! with WinDivert in intercept mode — the standard adapter counters
//! never move on PDAnet PPP adapters, so packet counting is the only
//! way. Intercept mode also lets the app block apps while it runs:
//! blocked apps' packets are dropped, never reinjected. When the app
//! closes, the handle closes and all traffic flows normally again.
//! Falls back to sniff mode (counting only), then to the v0.1 counter
//! mode, if WinDivert cannot be opened. Administrator rights required.

mod adapters;
mod app;
mod capture;
mod meter;
mod startup;
mod store;

use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};

fn main() -> eframe::Result<()> {
    let (tx, rx) = mpsc::channel();
    // Blocked apps (lowercased exe paths), shared with the capture
    // thread and restored from config; blocking only applies while
    // this app is running.
    let store = store::Store::load();
    let blocked: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(
        store
            .config
            .blocked_exes
            .iter()
            .map(|e| e.to_lowercase())
            .collect(),
    ));
    let blocked_for_sampler = blocked.clone();
    std::thread::spawn(move || adapters::sample_loop(tx, blocked_for_sampler));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 680.0])
            .with_title("Internet Control — Usage Meter"),
        ..Default::default()
    };
    eframe::run_native(
        "Internet Control — Usage Meter",
        options,
        Box::new(|_cc| Ok(Box::new(app::MeterApp::new(rx, blocked)))),
    )
}
