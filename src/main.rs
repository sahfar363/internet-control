#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Internet Control — usage meter v0.6.0.
//!
//! Windows 10/11 x64. Counts real download/upload by capturing packets
//! with WinDivert in intercept mode — the standard adapter counters
//! never move on PDAnet PPP adapters, so packet counting is the only
//! way. Intercept mode also lets the app block apps while it runs:
//! blocked apps' packets are dropped, never reinjected. When the app
//! closes, the handle closes and all traffic flows normally again.
//! Falls back to sniff mode (counting only), then to the v0.1 counter
//! mode, if WinDivert cannot be opened. Administrator rights required.
//!
//! v0.6 UI: native egui (complete Rust, `src/app.rs`) — back from the
//! WebView2 experiment. v0.4–v0.5.5 tried an Edge/WebView2 window, but
//! on Sahand's PC the embedded Edge process would not stay alive
//! (every attempt failed at Navigate with 0x8007139F, even with a
//! fresh profile, --disable-gpu and --no-sandbox), while Edge itself
//! works fine. The old egui window (v0.3.0) always opened fine, so
//! the UI is native egui again — same WinDivert engine, same
//! blocking (including terminating existing connections, v0.5),
//! Minimal view with the live graph (v0.5.2), dark/light theme
//! toggle, scrolling everywhere and a resizable Minimal window.
//! The UI is a single window, offline — no internet needed for the
//! UI itself.

mod adapters;
mod app;
mod capture;
mod connections;
mod log;
mod meter;
mod startup;
mod state;
mod store;

use std::sync::mpsc;
use std::sync::Arc;

fn main() {
    log::init();
    if let Some(p) = log::path() {
        log::log(&format!("log file: {}", p.display()));
    }
    log::log("Internet Control started");
    let store = store::Store::load();
    let shared = Arc::new(state::Shared::new(store));

    // Sampler thread: WinDivert capture + 100 ms batches.
    let (tx, rx) = mpsc::channel();
    let blocked_for_sampler = shared.blocked.clone();
    std::thread::spawn(move || adapters::sample_loop(tx, blocked_for_sampler));

    // Bridge thread: batches -> the snapshot the egui UI reads.
    let bridge_shared = shared.clone();
    std::thread::spawn(move || {
        let mut bridge = state::Bridge::new(bridge_shared);
        while let Ok(batch) = rx.recv() {
            bridge.apply(batch);
        }
    });

    // Native egui window. Resizable in both views, with a sensible
    // minimum size so the tables stay usable when small.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Internet Control — Usage Meter")
            .with_inner_size([980.0, 720.0])
            .with_min_inner_size([560.0, 420.0])
            .with_resizable(true),
        ..Default::default()
    };
    let shared_for_ui = shared.clone();
    if let Err(e) = eframe::run_native(
        "Internet Control — Usage Meter",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::MeterApp::new(shared_for_ui, cc)) as Box<dyn eframe::App>)
        }),
    ) {
        // No console in a windows_subsystem build, so the log file
        // is the only record of this failure.
        log::log(&format!("FATAL: could not open the egui window: {e}"));
        eprintln!("Could not open the egui window: {e}");
        std::process::exit(1);
    }

    // Window closed: blocking ends with the process (the capture
    // handle closes), and the config is saved.
    log::log("window closed, saving config and exiting");
    shared
        .store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .save();
}
