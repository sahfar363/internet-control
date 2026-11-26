#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Internet Control — usage meter v0.4.1.
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
//! v0.4 UI: a WebView2 (Edge) window showing a local page served by
//! the loopback HTTP server in `server.rs`. If the WebView2 runtime
//! is missing, the same page opens in the default browser instead.
//! The UI is a single window, offline — no internet needed for the
//! UI itself.

mod adapters;
mod capture;
mod connections;
mod log;
mod meter;
mod server;
mod startup;
mod state;
mod store;
mod webview;

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

    // Bridge thread: batches -> the JSON snapshot the UI polls.
    let bridge_shared = shared.clone();
    std::thread::spawn(move || {
        let mut bridge = state::Bridge::new(bridge_shared);
        while let Ok(batch) = rx.recv() {
            bridge.apply(batch);
        }
    });

    let port = match server::serve(shared.clone()) {
        Ok(port) => port,
        Err(e) => {
            // No console in a windows_subsystem build, so the log file
            // is the only record of this failure.
            log::log(&format!("FATAL: could not start the local UI server: {e}"));
            eprintln!("Could not start the local UI server: {e}");
            std::process::exit(1);
        }
    };
    let url = format!("http://127.0.0.1:{port}/");
    log::log(&format!("UI server listening on {url}"));

    // WebView2 window (Windows). On any failure — no WebView2 runtime,
    // no Edge, navigation failed — fall back to the default browser
    // on the same URL.
    match webview::run(&url) {
        Ok(()) => {
            log::log("window closed, saving config and exiting");
        }
        Err(e) => {
            log::log(&format!(
                "WebView2 unavailable ({e}); opening the UI in the default browser: {url}"
            ));
            eprintln!("WebView2 unavailable ({e}); opening the UI in the default browser: {url}");
            #[cfg(windows)]
            open_in_browser(&url);
            #[cfg(not(windows))]
            println!("Open this URL in a browser: {url}");
            // Park the process so the server (and the capture
            // handle, i.e. blocking) stays alive while the browser
            // tab is open. Ctrl+C / closing the terminal ends it.
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }

    // Window closed: blocking ends with the process (the capture
    // handle closes), and the config is saved.
    shared
        .store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .save();
}

/// Open `url` in the default browser (Windows fallback path).
/// `rundll32 url.dll,FileProtocolHandler` is the plain std-only way
/// to do it — no extra Win32 API surface needed.
#[cfg(windows)]
fn open_in_browser(url: &str) {
    let _ = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
}
