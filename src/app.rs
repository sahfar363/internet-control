//! egui UI for the meter (v0.3: per-app list, blocking, Minimal view).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Stroke, Vec2};

use crate::adapters::{default_route_luid, AdapterReading, AppReading, SampleBatch};
use crate::capture::CaptureStatus;
use crate::meter::{format_bytes, format_rate, RateSmoother};
use crate::store::Store;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HISTORY: usize = 600; // 600 samples @ 10 Hz = 60 seconds.

/// Download blue / upload orange — the v0.2 colours, kept in v0.3.
const DOWN_COLOR: Color32 = Color32::from_rgb(80, 180, 255);
const UP_COLOR: Color32 = Color32::from_rgb(255, 170, 60);

/// How the per-app list is sorted. Clicking a rate column sorts by
/// Rate (down+up rate), clicking a total column sorts by Total
/// (cumulative down+up bytes), clicking App sorts by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    Rate,
    Total,
    Name,
}

/// Sort per-app rows in place. Rate/Total sort descending (busiest
/// first); Name sorts ascending. Pure — unit-tested below.
pub fn sort_apps(apps: &mut [AppReading], mode: SortMode) {
    match mode {
        SortMode::Rate => apps.sort_by(|a, b| {
            (b.down_bps + b.up_bps)
                .partial_cmp(&(a.down_bps + a.up_bps))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.display_name.cmp(&b.display_name))
        }),
        SortMode::Total => apps.sort_by(|a, b| {
            (b.down_total + b.up_total)
                .cmp(&(a.down_total + a.up_total))
                .then_with(|| a.display_name.cmp(&b.display_name))
        }),
        SortMode::Name => apps.sort_by(|a, b| a.display_name.cmp(&b.display_name)),
    }
}

pub struct MeterApp {
    rx: Receiver<SampleBatch>,
    store: Store,
    adapters: Vec<AdapterReading>,
    apps: Vec<AppReading>,
    selected_luid: Option<u64>,
    /// (down_bps, up_bps) history for the selected adapter — smoothed
    /// values, so the graph is as calm as the top numbers.
    history: VecDeque<(f64, f64)>,
    /// Smoothed display rates (v0.3); session totals stay exact.
    down_bps: f64,
    up_bps: f64,
    down_smoother: RateSmoother,
    up_smoother: RateSmoother,
    /// Per-app smoothers, keyed by exe path.
    app_smoothers: HashMap<String, (RateSmoother, RateSmoother)>,
    session_down: u64,
    session_up: u64,
    peak_down: f64,
    peak_up: f64,
    last_batch: Option<Instant>,
    last_save: Instant,
    error: Option<String>,
    /// How the numbers are measured (WinDivert intercept/sniff, or
    /// the counter fallback with the reason). Shown under the heading.
    capture_status: CaptureStatus,
    /// Shared blocked set (lowercased exe paths), read by the capture
    /// thread on every packet. The list is remembered in config so the
    /// same apps are blocked again next time the app opens — but
    /// blocking itself only applies while this app is running.
    blocked: Arc<RwLock<HashSet<String>>>,
    /// Minimal view: only DOWNLOAD/UPLOAD, session totals and IP.
    minimal: bool,
    run_on_startup: bool,
    sort_mode: SortMode,
    /// Note shown when a Block click cannot take effect (sniff mode).
    block_note: Option<String>,
}

impl MeterApp {
    pub fn new(rx: Receiver<SampleBatch>, blocked: Arc<RwLock<HashSet<String>>>) -> Self {
        let store = Store::load();
        // Restore the saved selection if that adapter still exists later;
        // otherwise the default-route adapter is picked on first batch.
        let selected_luid = store.config.selected_luid;
        let run_on_startup = store.config.run_on_startup;
        // `mut` is only needed on Windows (the refresh below); the
        // allow keeps the Linux build warning-free.
        #[allow(unused_mut)]
        let mut app = Self {
            rx,
            store,
            adapters: Vec::new(),
            apps: Vec::new(),
            selected_luid,
            history: VecDeque::with_capacity(HISTORY),
            down_bps: 0.0,
            up_bps: 0.0,
            down_smoother: RateSmoother::new(),
            up_smoother: RateSmoother::new(),
            app_smoothers: HashMap::new(),
            session_down: 0,
            session_up: 0,
            peak_down: 0.0,
            peak_up: 0.0,
            last_batch: None,
            last_save: Instant::now(),
            error: None,
            capture_status: CaptureStatus::default(),
            blocked,
            minimal: false,
            run_on_startup,
            sort_mode: SortMode::default(),
            block_note: None,
        };
        // If "Run on startup" was on, refresh the Run key if it is
        // missing or points elsewhere (the exe may have moved since
        // it was set). A failure here is not fatal — the checkbox
        // still shows the saved state.
        #[cfg(windows)]
        if run_on_startup && !crate::startup::is_run_on_startup() {
            if let Err(e) = crate::startup::set_run_on_startup(true) {
                app.error = Some(format!("Run on startup could not be refreshed: {e}"));
            }
        }
        app
    }

    fn selected(&self) -> Option<&AdapterReading> {
        self.selected_luid
            .and_then(|luid| self.adapters.iter().find(|a| a.info.luid == luid))
    }

    fn selected_name(&self) -> String {
        self.selected()
            .map(|a| a.info.name.clone())
            .unwrap_or_else(|| "— (waiting for adapters…)".to_string())
    }

    fn selected_ip(&self) -> String {
        self.selected()
            .and_then(|a| a.info.ipv4.clone())
            .unwrap_or_else(|| "—".to_string())
    }

    fn is_blocked(&self, exe_path: &str) -> bool {
        self.blocked
            .read()
            .map(|set| set.contains(&exe_path.to_lowercase()))
            .unwrap_or(false)
    }

    /// Toggle block/unblock for one app. Blocking only applies while
    /// this app is running (WinDivert intercept drops the packets);
    /// the list is saved so it is restored next launch.
    fn toggle_block(&mut self, exe_path: &str) {
        if !self.capture_status.can_block() {
            self.block_note = Some(
                "Blocking needs WinDivert intercept mode. The app is in \
                 sniff/counter mode, so Block does nothing right now."
                    .to_string(),
            );
            return;
        }
        self.block_note = None;
        let lower = exe_path.to_lowercase();
        let mut blocked = self.is_blocked(exe_path);
        if let Ok(mut set) = self.blocked.write() {
            if blocked {
                set.remove(&lower);
                blocked = false;
            } else {
                set.insert(lower);
                blocked = true;
            }
        }
        if blocked {
            if !self
                .store
                .config
                .blocked_exes
                .iter()
                .any(|e| e.eq_ignore_ascii_case(exe_path))
            {
                self.store.config.blocked_exes.push(exe_path.to_string());
            }
        } else {
            self.store
                .config
                .blocked_exes
                .retain(|e| !e.eq_ignore_ascii_case(exe_path));
        }
        self.store.save();
    }

    fn pick_default(&mut self) {
        if self.selected_luid.is_some() && self.selected().is_some() {
            return;
        }
        // 1. Saved adapter, if it is back.
        if let Some(saved) = self.store.config.selected_luid {
            if self.adapters.iter().any(|a| a.info.luid == saved) {
                self.selected_luid = Some(saved);
                return;
            }
        }
        // 2. Adapter on the default IPv4 route.
        if let Some(luid) = default_route_luid() {
            if self.adapters.iter().any(|a| a.info.luid == luid) {
                self.select(luid);
                return;
            }
        }
        // 3. Operational adapter with the highest recent traffic.
        if let Some(best) = self
            .adapters
            .iter()
            .filter(|a| a.info.oper_status == 1)
            .max_by(|a, b| {
                (a.down_bps + a.up_bps)
                    .partial_cmp(&(b.down_bps + b.up_bps))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        {
            self.select(best.info.luid);
        }
    }

    fn select(&mut self, luid: u64) {
        self.selected_luid = Some(luid);
        self.history.clear();
        self.down_bps = 0.0;
        self.up_bps = 0.0;
        self.down_smoother = RateSmoother::new();
        self.up_smoother = RateSmoother::new();
        if let Some(a) = self.adapters.iter().find(|a| a.info.luid == luid) {
            self.store.config.selected_luid = Some(luid);
            self.store.config.selected_name = Some(a.info.name.clone());
            self.store.save();
        }
    }

    fn drain(&mut self) {
        while let Ok(batch) = self.rx.try_recv() {
            self.last_batch = Some(Instant::now());
            self.capture_status = batch.capture_status.clone();
            self.adapters = batch.adapters;
            self.apps = batch.apps;
            self.pick_default();
            // Copy the selected reading out first (borrow ends here) so
            // the fields below can be assigned.
            let sel = self
                .selected()
                .map(|s| (s.down_bps, s.up_bps, s.down_delta, s.up_delta));
            if let Some((raw_down, raw_up, down_delta, up_delta)) = sel {
                // Smoothed for display + graph; totals stay exact.
                self.down_bps = self.down_smoother.update(raw_down);
                self.up_bps = self.up_smoother.update(raw_up);
                self.session_down += down_delta;
                self.session_up += up_delta;
                // Daily totals are for the selected adapter only.
                self.store.today.down_bytes += down_delta;
                self.store.today.up_bytes += up_delta;
                if self.down_bps > self.peak_down {
                    self.peak_down = self.down_bps;
                }
                if self.up_bps > self.peak_up {
                    self.peak_up = self.up_bps;
                }
                self.history.push_back((self.down_bps, self.up_bps));
                if self.history.len() > HISTORY {
                    self.history.pop_front();
                }
            }
            // Smooth the per-app rates too, keyed by exe path.
            for app in &self.apps {
                let (down_s, up_s) = self
                    .app_smoothers
                    .entry(app.exe_path.clone())
                    .or_insert_with(|| (RateSmoother::new(), RateSmoother::new()));
                down_s.update(app.down_bps);
                up_s.update(app.up_bps);
            }
        }
        if self.last_save.elapsed() > Duration::from_secs(5) {
            self.store.save();
            self.last_save = Instant::now();
        }
        #[cfg(not(windows))]
        {
            self.error = Some(
                "Non-Windows build: adapter enumeration is a stub, so no live \
                 data is shown. Build for Windows (x86_64-pc-windows-msvc) \
                 for real measurements."
                    .to_string(),
            );
        }
    }

    /// Rows for the per-app table: apps with traffic this session,
    /// plus blocked apps with no traffic (so they can be unblocked),
    /// sorted by the current sort mode.
    fn app_rows(&self) -> Vec<AppReading> {
        let mut rows: Vec<AppReading> = self.apps.clone();
        for exe in &self.store.config.blocked_exes {
            if !rows.iter().any(|a| a.exe_path.eq_ignore_ascii_case(exe)) {
                rows.push(AppReading {
                    exe_path: exe.clone(),
                    display_name: crate::adapters::display_name_for(exe),
                    ..Default::default()
                });
            }
        }
        sort_apps(&mut rows, self.sort_mode);
        rows
    }

    fn draw_top_numbers(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("DOWNLOAD").small().color(Color32::GRAY));
                ui.label(
                    RichText::new(format_rate(self.down_bps))
                        .size(32.0)
                        .color(DOWN_COLOR),
                );
            });
            ui.add_space(32.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("UPLOAD").small().color(Color32::GRAY));
                ui.label(
                    RichText::new(format_rate(self.up_bps))
                        .size(32.0)
                        .color(UP_COLOR),
                );
            });
        });
    }

    fn draw_graph(&self, ui: &mut egui::Ui) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 120.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, Color32::from_gray(24));
        if self.history.len() < 2 {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Waiting for samples…",
                egui::FontId::default(),
                Color32::GRAY,
            );
            return;
        }
        let max_v = self
            .history
            .iter()
            .flat_map(|(d, u)| [*d, *u])
            .fold(1.0f64, f64::max);
        let plot = |values: &dyn Fn(&(f64, f64)) -> f64, color: Color32| {
            let points: Vec<egui::Pos2> = self
                .history
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let x = rect.left() + rect.width() * (i as f32 / (HISTORY - 1) as f32);
                    let y = rect.bottom() - rect.height() * (values(p) / max_v) as f32;
                    egui::Pos2::new(x, y)
                })
                .collect();
            painter.add(egui::Shape::line(points, Stroke::new(1.5_f32, color)));
        };
        plot(&|p| p.0, DOWN_COLOR);
        plot(&|p| p.1, UP_COLOR);
        painter.text(
            rect.left_top() + Vec2::new(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            format!("peak in graph: {}", format_rate(max_v)),
            egui::FontId::proportional(11.0),
            Color32::GRAY,
        );
    }

    fn draw_adapters(&mut self, ui: &mut egui::Ui) {
        ui.heading("Adapters (all — including virtual / VPN / PDAnet)");
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .show(ui, |ui| {
                egui::Grid::new("adapters").striped(true).show(ui, |ui| {
                    ui.label(RichText::new("Adapter").strong());
                    ui.label(RichText::new("Type").strong());
                    ui.label(RichText::new("Status").strong());
                    ui.label(RichText::new("IP").strong());
                    ui.label(RichText::new("Down").strong());
                    ui.label(RichText::new("Up").strong());
                    ui.label(RichText::new("Total").strong());
                    ui.label("");
                    ui.end_row();
                    // Clone the rows we need so `select` can borrow mutably.
                    struct Row {
                        luid: u64,
                        name: String,
                        type_name: String,
                        status: String,
                        ip: String,
                        down: String,
                        up: String,
                        total: String,
                        is_sel: bool,
                    }
                    let rows: Vec<Row> = self
                        .adapters
                        .iter()
                        .map(|a| Row {
                            luid: a.info.luid,
                            name: a.info.name.clone(),
                            type_name: a.info.type_name.clone(),
                            status: a.info.status_text.clone(),
                            ip: a.info.ipv4.clone().unwrap_or_else(|| "—".to_string()),
                            down: format_rate(a.down_bps),
                            up: format_rate(a.up_bps),
                            total: format_bytes(a.info.in_octets + a.info.out_octets),
                            is_sel: Some(a.info.luid) == self.selected_luid,
                        })
                        .collect();
                    let mut to_select: Option<u64> = None;
                    for row in rows {
                        if row.is_sel {
                            ui.label(RichText::new(format!("▶ {}", row.name)).strong());
                        } else {
                            ui.label(&row.name);
                        }
                        ui.label(&row.type_name);
                        ui.label(&row.status);
                        ui.label(&row.ip);
                        ui.label(&row.down);
                        ui.label(&row.up);
                        ui.label(&row.total);
                        if row.is_sel {
                            ui.label("selected");
                        } else if ui.button("Select").clicked() {
                            to_select = Some(row.luid);
                        }
                        ui.end_row();
                    }
                    if let Some(luid) = to_select {
                        self.select(luid);
                    }
                });
            });
    }

    fn draw_apps(&mut self, ui: &mut egui::Ui) {
        ui.heading("Apps (click a column to sort — Rate or Total)");
        if self.apps.is_empty() && self.capture_status.is_active() {
            ui.label(
                RichText::new(
                    "Per-app attribution is starting — apps appear here as they connect. \
                     (Flows that started before this app launched appear after they reconnect.)",
                )
                .small()
                .color(Color32::GRAY),
            );
        }
        if !self.capture_status.can_block() && self.capture_status.is_active() {
            ui.label(
                RichText::new(
                    "Blocking is unavailable in sniff mode — restart the app so WinDivert \
                     can open in intercept mode.",
                )
                .small()
                .color(Color32::YELLOW),
            );
        }
        if let Some(note) = &self.block_note {
            ui.label(RichText::new(note).color(Color32::YELLOW));
        }
        let rows = self.app_rows();
        // Smoothed rates for display, keyed by exe path.
        let smooth: HashMap<String, (f64, f64)> = rows
            .iter()
            .map(|a| {
                let (d, u) = self
                    .app_smoothers
                    .get(&a.exe_path)
                    .map(|(ds, us)| (ds.value(), us.value()))
                    .unwrap_or((a.down_bps, a.up_bps));
                (a.exe_path.clone(), (d, u))
            })
            .collect();
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .show(ui, |ui| {
                egui::Grid::new("apps").striped(true).show(ui, |ui| {
                    // Clickable headers: rate columns sort by Rate, total
                    // columns by Total, App by name.
                    if ui.button("App").clicked() {
                        self.sort_mode = SortMode::Name;
                    }
                    if ui.button("Down").clicked() {
                        self.sort_mode = SortMode::Rate;
                    }
                    if ui.button("Up").clicked() {
                        self.sort_mode = SortMode::Rate;
                    }
                    if ui.button("Down total").clicked() {
                        self.sort_mode = SortMode::Total;
                    }
                    if ui.button("Up total").clicked() {
                        self.sort_mode = SortMode::Total;
                    }
                    ui.label("");
                    ui.end_row();
                    let mut to_toggle: Option<String> = None;
                    for row in &rows {
                        let blocked = self.is_blocked(&row.exe_path);
                        let (down, up) = smooth
                            .get(&row.exe_path)
                            .copied()
                            .unwrap_or((row.down_bps, row.up_bps));
                        if blocked {
                            ui.label(
                                RichText::new(format!("⛔ {}", row.display_name))
                                    .strong()
                                    .color(Color32::from_rgb(255, 120, 120)),
                            );
                        } else {
                            ui.label(&row.display_name);
                        }
                        ui.label(format_rate(down));
                        ui.label(format_rate(up));
                        ui.label(format_bytes(row.down_total));
                        ui.label(format_bytes(row.up_total));
                        if blocked {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("BLOCKED")
                                        .small()
                                        .color(Color32::from_rgb(255, 120, 120)),
                                );
                                if ui.button("Unblock").clicked() {
                                    to_toggle = Some(row.exe_path.clone());
                                }
                            });
                        } else if ui.button("Block").clicked() {
                            to_toggle = Some(row.exe_path.clone());
                        }
                        ui.end_row();
                    }
                    if let Some(exe) = to_toggle {
                        self.toggle_block(&exe);
                    }
                });
            });
        ui.label(
            RichText::new(
                "Blocking only works while this app is running. Close it (or restart the PC \
                 without it) and every blocked app gets its internet back automatically — \
                 no firewall rules are created.",
            )
            .small()
            .color(Color32::GRAY),
        );
    }
}

impl eframe::App for MeterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        // ~10 Hz repaint so the numbers and graph stay live.
        ctx.request_repaint_after(Duration::from_millis(100));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(format!("Internet Control — Usage Meter  v{VERSION}"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if self.minimal { "Full view" } else { "Minimal" };
                    if ui.button(label).clicked() {
                        self.minimal = !self.minimal;
                    }
                });
            });

            if self.minimal {
                // Minimal view: only the big numbers, session totals
                // and the selected adapter's IP. Nothing else.
                self.draw_top_numbers(ui);
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("Session: ↓ {}  ↑ {}", format_bytes(self.session_down), format_bytes(self.session_up)));
                });
                ui.add_space(4.0);
                ui.label(format!("IP: {}", self.selected_ip()));
                ui.label(RichText::new(format!("Adapter: {}", self.selected_name())).small().color(Color32::GRAY));
                return;
            }

            ui.label(format!("Selected adapter: {}", self.selected_name()));
            // Measurement source: green when WinDivert capture is live,
            // yellow when it fell back to the (possibly frozen) counters.
            let source = self.capture_status.label();
            if self.capture_status.is_active() {
                ui.label(RichText::new(source).color(Color32::from_rgb(120, 220, 130)));
            } else {
                ui.label(RichText::new(source).color(Color32::YELLOW));
            }
            // Run on startup (HKCU Run key). The app needs admin for
            // WinDivert anyway, so a UAC prompt at login is expected.
            let mut startup = self.run_on_startup;
            if ui.checkbox(&mut startup, "Run on startup").changed() {
                #[cfg(windows)]
                match crate::startup::set_run_on_startup(startup) {
                    Ok(()) => {
                        self.run_on_startup = startup;
                        self.store.config.run_on_startup = startup;
                        self.store.save();
                    }
                    Err(e) => {
                        self.error = Some(format!("Run on startup failed: {e}"));
                    }
                }
                #[cfg(not(windows))]
                {
                    self.error = Some("Run on startup is only available on Windows.".to_string());
                }
            }
            ui.add_space(4.0);

            self.draw_top_numbers(ui);

            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Session: ↓ {}  ↑ {}", format_bytes(self.session_down), format_bytes(self.session_up)));
                ui.label(format!("  Today: ↓ {}  ↑ {}", format_bytes(self.store.today.down_bytes), format_bytes(self.store.today.up_bytes)));
                ui.label(format!("  Peak: ↓ {}  ↑ {}", format_rate(self.peak_down), format_rate(self.peak_up)));
            });
            ui.label(
                "Totals are for the selected adapter only — never the sum of all \
                 adapters, so VPN + PDAnet carrying the same bytes are not counted twice.",
            )
            .on_hover_text("If your PC has a VPN adapter and a PDAnet adapter, the same packet crosses both. Summing them would double your usage.");

            ui.add_space(6.0);
            self.draw_graph(ui);

            ui.add_space(6.0);
            self.draw_adapters(ui);

            ui.add_space(6.0);
            self.draw_apps(ui);

            if let Some(err) = &self.error {
                ui.add_space(6.0);
                ui.label(RichText::new(err).color(Color32::YELLOW));
            }
            ui.add_space(4.0);
            ui.label(
                "Administrator rights are required in v0.3 (WinDivert driver). \
                 Verification: download a known 100 MB file — the session total \
                 should read ~100 MB, not 0 and not ~200 MB.",
            );
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.store.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AppReading;

    fn app(name: &str, down_bps: f64, up_bps: f64, down_total: u64, up_total: u64) -> AppReading {
        AppReading {
            exe_path: format!("C:\\Apps\\{name}"),
            display_name: name.to_string(),
            down_bps,
            up_bps,
            down_delta: 0,
            up_delta: 0,
            down_total,
            up_total,
        }
    }

    #[test]
    fn sort_by_rate_descending() {
        let mut rows = vec![
            app("slow.exe", 1_000.0, 0.0, 999_999, 0),
            app("fast.exe", 500_000.0, 500_000.0, 1, 0),
            app("mid.exe", 10_000.0, 20_000.0, 50, 0),
        ];
        sort_apps(&mut rows, SortMode::Rate);
        assert_eq!(rows[0].display_name, "fast.exe");
        assert_eq!(rows[1].display_name, "mid.exe");
        assert_eq!(rows[2].display_name, "slow.exe");
    }

    #[test]
    fn sort_by_total_descending() {
        let mut rows = vec![
            app("a.exe", 999_999.0, 0.0, 100, 0),
            app("b.exe", 1.0, 0.0, 50_000, 40_000),
            app("c.exe", 1.0, 0.0, 1_000, 2_000),
        ];
        sort_apps(&mut rows, SortMode::Total);
        // b: 90_000 total beats a: 100, even though a is fastest now.
        assert_eq!(rows[0].display_name, "b.exe");
        assert_eq!(rows[1].display_name, "c.exe");
        assert_eq!(rows[2].display_name, "a.exe");
    }

    #[test]
    fn sort_by_name_ascending() {
        let mut rows = vec![
            app("zebra.exe", 0.0, 0.0, 0, 0),
            app("apple.exe", 0.0, 0.0, 0, 0),
            app("mango.exe", 0.0, 0.0, 0, 0),
        ];
        sort_apps(&mut rows, SortMode::Name);
        assert_eq!(rows[0].display_name, "apple.exe");
        assert_eq!(rows[1].display_name, "mango.exe");
        assert_eq!(rows[2].display_name, "zebra.exe");
    }

    #[test]
    fn sort_is_stable_for_equal_rates() {
        let mut rows = vec![
            app("b.exe", 100.0, 0.0, 0, 0),
            app("a.exe", 100.0, 0.0, 0, 0),
        ];
        sort_apps(&mut rows, SortMode::Rate);
        // Ties break by name, so the order is deterministic.
        assert_eq!(rows[0].display_name, "a.exe");
    }
}
