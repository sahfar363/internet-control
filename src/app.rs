//! Native egui UI for the meter (v0.6: complete Rust, no WebView2/Edge).
//!
//! Reads the snapshot the bridge thread maintains in [`Shared`]
//! ([`StateJson`]) and writes user actions back through the same
//! request slots the old HTTP API used (select / minimal / view /
//! sort / startup) plus the block logic (shared blocked set +
//! store + SetTcpEntry termination, as the old `/api/block` did).
//!
//! Layout (clean and minimal):
//! * Top bar: title, opacity slider, Dark/Light theme toggle,
//!   Minimal toggle, run-on-startup checkbox.
//! * Big DOWNLOAD / UPLOAD rates + session totals + the carrying
//!   adapter's name and IP — shown in BOTH normal and minimal view.
//! * Tabs (full view): Overview (graph + totals + capture status),
//!   Apps (per-app list with Block/Unblock), Adapters (adapter list
//!   + diagnostics). Minimal view (v0.7): the window itself shrinks
//!     to a really small size — just the rates, the opacity slider and
//!     the IP, no graph, no tabs.
//! * Everything scrolls: the whole panel is a vertical ScrollArea,
//!   so nothing is cut off when the window is short; the tables
//!   scroll horizontally when the window is narrow. The window is
//!   resizable in both views (minimum size set in `main.rs`).
//! * Fixed columns: numeric columns have fixed widths and use a
//!   monospace font, so values never shift left/right as numbers
//!   change (the v0.3 complaint). The app list keeps the bridge's
//!   stable order — re-sorted only on header click or view toggle.

use std::sync::Arc;
use std::time::Duration;

use egui::{Color32, RichText, Stroke, Vec2};

use crate::meter::{format_bytes, format_rate};
use crate::opacity::{OPACITY_MAX, OPACITY_MIN};
use crate::state::{
    adapter_mode_after_click, AdapterSelection, AppView, BlockState, Shared, SortMode, StateJson,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Download blue / upload orange — the v0.2 colours, kept ever since.
const DOWN_COLOR: Color32 = Color32::from_rgb(80, 180, 255);
const UP_COLOR: Color32 = Color32::from_rgb(255, 170, 60);

/// Really-small Minimal window (v0.7): just the rates, the opacity
/// slider and the IP. The window itself shrinks to this size when
/// Minimal is clicked — v0.6 only hid panels and left the window big.
pub const MINIMAL_SIZE: [f32; 2] = [360.0, 190.0];
/// Smallest the Minimal window can be dragged to.
pub const MINIMAL_MIN_SIZE: [f32; 2] = [300.0, 150.0];
/// Normal view size / minimum (set in `main.rs` too — keep in sync).
pub const NORMAL_SIZE: [f32; 2] = [980.0, 720.0];
pub const NORMAL_MIN_SIZE: [f32; 2] = [560.0, 420.0];

/// Fixed widths (px) for the numeric columns: values never shift
/// left/right as numbers change, because the column width is
/// constant and the text is monospace.
const RATE_COL_W: f32 = 92.0;
const TOTAL_COL_W: f32 = 92.0;
/// Width of the Block/Unblock + badge cell in the apps table.
const ACTION_COL_W: f32 = 128.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Apps,
    Adapters,
}

/// Label of the theme toggle button: it shows the CURRENT theme.
pub fn theme_button_label(dark: bool) -> &'static str {
    if dark {
        "Theme: Dark"
    } else {
        "Theme: Light"
    }
}

/// Label of the per-app block button.
pub fn block_button_label(blocked: bool) -> &'static str {
    if blocked {
        "Unblock"
    } else {
        "Block"
    }
}

/// Badge text for a blocked row. Empty when not blocked.
pub fn block_badge(state: BlockState) -> &'static str {
    match state {
        BlockState::Blocked => "BLOCKED",
        BlockState::BlockedNewConnectionsOnly => "NEW CONNECTIONS ONLY",
        BlockState::NotBlocked => "",
    }
}

/// Scale for the graph: the largest rate in either history, at
/// least 1.0 so the lines never divide by zero. Pure — unit-tested.
pub fn graph_max(down: &[f64], up: &[f64]) -> f64 {
    down.iter().chain(up.iter()).fold(1.0f64, |m, &v| m.max(v))
}

pub struct MeterApp {
    shared: Arc<Shared>,
    tab: Tab,
    dark: bool,
    /// Window opacity 0.2..=1.0 (v0.7), loaded from config.
    opacity: f32,
    /// Opacity the window currently has; `None` until first applied.
    applied_opacity: Option<f32>,
    /// Minimal state the window size currently reflects. Starts
    /// false (the window opens at full size) — the first frame
    /// shrinks it if the saved config says Minimal.
    minimal: bool,
    /// Last full-view window size, so leaving Minimal restores it.
    normal_size: Option<Vec2>,
}

impl MeterApp {
    pub fn new(shared: Arc<Shared>, cc: &eframe::CreationContext<'_>) -> Self {
        let (dark, opacity) = shared
            .store
            .lock()
            .map(|s| (s.config.dark_theme, s.config.opacity))
            .unwrap_or((true, crate::opacity::OPACITY_DEFAULT));
        cc.egui_ctx.set_visuals(if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        Self {
            shared,
            tab: Tab::Overview,
            dark,
            opacity: crate::opacity::clamp_opacity(opacity),
            applied_opacity: None,
            minimal: false,
            normal_size: None,
        }
    }

    fn snapshot(&self) -> StateJson {
        self.shared
            .snapshot
            .read()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    fn set_theme(&mut self, ctx: &egui::Context, dark: bool) {
        self.dark = dark;
        ctx.set_visuals(if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        if let Ok(mut store) = self.shared.store.lock() {
            store.config.dark_theme = dark;
            store.save();
        }
    }

    /// Save a new opacity (v0.7). The value is applied to the window
    /// at the end of `update` (see `applied_opacity`), so the slider
    /// only has to store it.
    fn set_opacity(&mut self, value: f32) {
        self.opacity = crate::opacity::clamp_opacity(value);
        if let Ok(mut store) = self.shared.store.lock() {
            store.config.opacity = self.opacity;
            store.save();
        }
    }

    /// Opacity slider (v0.7): 0.2..=1.0 with a percent label. Used in
    /// the top bar (normal view) and under the rates (Minimal view).
    fn opacity_slider(&mut self, ui: &mut egui::Ui, width: f32) {
        ui.label("Opacity");
        let resp = ui.add_sized(
            Vec2::new(width, 18.0),
            egui::Slider::new(&mut self.opacity, OPACITY_MIN..=OPACITY_MAX).show_value(false),
        );
        ui.label(format!("{:.0}%", self.opacity * 100.0));
        if resp.changed() {
            let value = self.opacity;
            self.set_opacity(value);
        }
    }

    /// Send a new adapter mode to the bridge (v0.7). It applies on
    /// the next sample tick (~100 ms) and is saved to config.
    fn set_adapter_mode(&mut self, mode: AdapterSelection) {
        if let Ok(mut req) = self.shared.adapter_mode_request.lock() {
            *req = Some(mode);
        }
    }

    /// Toggle block/unblock for one app — the same steps the old
    /// `/api/block` endpoint did. Blocking only applies while this
    /// app is running (WinDivert intercept drops the packets); the
    /// list is saved so it is restored next launch. A Block click
    /// also terminates the app's existing TCP connections at once
    /// (v0.5) and records the note shown under the list.
    fn set_block(&mut self, exe_path: &str, block: bool, can_block: bool) {
        if block && !can_block {
            if let Ok(mut n) = self.shared.block_note.lock() {
                *n = Some(
                    "Blocking needs WinDivert intercept mode. The app is in \
                     sniff/counter mode, so Block does nothing right now."
                        .to_string(),
                );
            }
            return;
        }
        let lower = exe_path.to_lowercase();
        if let Ok(mut set) = self.shared.blocked.write() {
            if block {
                set.insert(lower);
            } else {
                set.remove(&lower);
            }
        }
        {
            let mut store = self.shared.store.lock().unwrap_or_else(|e| e.into_inner());
            if block {
                if !store
                    .config
                    .blocked_exes
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(exe_path))
                {
                    store.config.blocked_exes.push(exe_path.to_string());
                }
            } else {
                store
                    .config
                    .blocked_exes
                    .retain(|e| !e.eq_ignore_ascii_case(exe_path));
            }
            store.save();
        }
        if block {
            let report = crate::connections::terminate_app_connections(exe_path);
            let note = report.summarize(&crate::adapters::display_name_for(exe_path));
            crate::log::log(&format!("Block {exe_path}: {note}"));
            if let Ok(mut n) = self.shared.block_note.lock() {
                *n = Some(note);
            }
        } else if let Ok(mut n) = self.shared.block_note.lock() {
            *n = None;
        }
    }

    fn draw_top_strip(&self, ui: &mut egui::Ui, snap: &StateJson) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("DOWNLOAD").small().color(Color32::GRAY));
                ui.label(RichText::new(&snap.top.down).size(32.0).color(DOWN_COLOR));
            });
            ui.add_space(32.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("UPLOAD").small().color(Color32::GRAY));
                ui.label(RichText::new(&snap.top.up).size(32.0).color(UP_COLOR));
            });
        });
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "Session: ↓ {}  ↑ {}",
                format_bytes(snap.session.down),
                format_bytes(snap.session.up)
            ));
            ui.label(format!(
                "  Today: ↓ {}  ↑ {}",
                format_bytes(snap.today.down),
                format_bytes(snap.today.up)
            ));
            ui.label(format!(
                "  Peak: ↓ {}  ↑ {}",
                snap.peak_down_fmt, snap.peak_up_fmt
            ));
        });
        ui.add_space(2.0);
        // The carrying adapter's IP — shown in BOTH normal and
        // minimal view (v0.6 requirement).
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("IP: {}", snap.selected_ip)).strong());
            ui.label(format!("  Adapter: {}", snap.selected_name));
        });
        if let Some(note) = &snap.top.note {
            ui.label(RichText::new(note).small().color(Color32::YELLOW));
        }
    }

    /// The really-small Minimal view (v0.7): just the rates, the
    /// opacity slider, and the IP underneath — no session totals, no
    /// graph, no tabs. The window itself is ~360x190 at this point
    /// (see `update`), so every line here has to earn its height.
    fn draw_minimal(&mut self, ui: &mut egui::Ui, snap: &StateJson) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("DOWNLOAD").small().color(Color32::GRAY));
                ui.label(RichText::new(&snap.top.down).size(24.0).color(DOWN_COLOR));
            });
            ui.add_space(24.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("UPLOAD").small().color(Color32::GRAY));
                ui.label(RichText::new(&snap.top.up).size(24.0).color(UP_COLOR));
            });
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            // Label + percent take ~90px; the slider gets the rest.
            let slider_w = (ui.available_width() - 90.0).max(60.0);
            self.opacity_slider(ui, slider_w);
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("IP: {}", snap.selected_ip)).strong());
            ui.label(format!("  {}", snap.selected_name));
        });
        if let Some(note) = &snap.top.note {
            ui.label(RichText::new(note).small().color(Color32::YELLOW));
        }
    }

    fn draw_graph(&self, ui: &mut egui::Ui, snap: &StateJson) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 120.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
        if snap.history_down.len() < 2 {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Waiting for samples…",
                egui::FontId::default(),
                ui.visuals().weak_text_color(),
            );
            return;
        }
        let max_v = graph_max(&snap.history_down, &snap.history_up);
        let n = snap.history_down.len();
        let plot = |values: &[f64], color: Color32| {
            let points: Vec<egui::Pos2> = values
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let x = rect.left() + rect.width() * (i as f32 / (n - 1).max(1) as f32);
                    let y = rect.bottom() - rect.height() * (*v / max_v) as f32;
                    egui::Pos2::new(x, y)
                })
                .collect();
            painter.add(egui::Shape::line(points, Stroke::new(1.5_f32, color)));
        };
        plot(&snap.history_down, DOWN_COLOR);
        plot(&snap.history_up, UP_COLOR);
        painter.text(
            rect.left_top() + Vec2::new(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            format!("peak in graph: {}", format_rate(max_v)),
            egui::FontId::proportional(11.0),
            ui.visuals().weak_text_color(),
        );
    }

    fn draw_overview(&self, ui: &mut egui::Ui, snap: &StateJson) {
        let source = &snap.capture_label;
        if snap.capture_active {
            ui.label(RichText::new(source).color(Color32::from_rgb(120, 220, 130)));
        } else {
            ui.label(RichText::new(source).color(Color32::YELLOW));
        }
        if let Some(note) = &snap.selection_note {
            ui.label(RichText::new(note).small().color(Color32::GRAY));
        }
        ui.add_space(6.0);
        self.draw_graph(ui, snap);
        ui.add_space(6.0);
        ui.label(format!(
            "Top source: {}  •  Packets seen: {}  •  Adapters found: {}",
            snap.diagnostics.top_source,
            snap.diagnostics.packets,
            snap.diagnostics.adapters_enumerated
        ));
        if snap.diagnostics.fallback_active {
            ui.label(
                RichText::new(
                    "Showing all interfaces: the selected adapter carries no traffic right now.",
                )
                .small()
                .color(Color32::YELLOW),
            );
        }
        ui.add_space(4.0);
        // What the totals mean depends on the adapter mode (v0.7):
        // Auto follows one carrying adapter, All / Named sum
        // adapters — and a sum can double-count traffic that crosses
        // two of them (VPN over PdaNet).
        let totals_note = match snap.adapter_mode {
            AdapterSelection::Auto => {
                "Totals are for the carrying adapter (or all interfaces while it is idle) — \
                 never the sum of all adapters, so VPN + PDaNet carrying the same bytes are \
                 not counted twice."
            }
            AdapterSelection::All => {
                "Totals are the sum of ALL adapters — traffic that crosses two adapters \
                 (e.g. VPN over PdaNet) is counted twice."
            }
            AdapterSelection::Named(_) => {
                "Totals are the sum of the ticked adapters — traffic that crosses two of \
                 them (e.g. VPN over PdaNet) is counted twice."
            }
        };
        ui.label(RichText::new(totals_note).small().color(Color32::GRAY));
    }

    fn draw_apps(&mut self, ui: &mut egui::Ui, snap: &StateJson) {
        // Rate / Total view toggle (v0.4): re-sorts the list via the
        // bridge; both rate and total columns are shown either way.
        ui.horizontal(|ui| {
            ui.label("Show:");
            let rate = snap.app_view == AppView::Rate;
            if ui.selectable_label(rate, "Rate").clicked() {
                if let Ok(mut req) = self.shared.view_request.lock() {
                    *req = Some(AppView::Rate);
                }
            }
            if ui.selectable_label(!rate, "Total").clicked() {
                if let Ok(mut req) = self.shared.view_request.lock() {
                    *req = Some(AppView::Total);
                }
            }
            ui.label(
                RichText::new("(click a column header to sort)")
                    .small()
                    .color(Color32::GRAY),
            );
        });
        if snap.apps.is_empty() && snap.capture_active {
            ui.label(
                RichText::new(
                    "Per-app attribution is starting — apps appear here as they connect. \
                     (Flows that started before this app launched appear after they reconnect.)",
                )
                .small()
                .color(Color32::GRAY),
            );
        }
        if !snap.can_block && snap.capture_active {
            ui.label(
                RichText::new(
                    "Blocking is unavailable in sniff mode — restart the app so WinDivert \
                     can open in intercept mode.",
                )
                .small()
                .color(Color32::YELLOW),
            );
        }
        if let Some(note) = &snap.block_note {
            ui.label(RichText::new(note).color(Color32::YELLOW));
        }
        // Fixed columns: every numeric cell gets the same fixed
        // width and a monospace font, so nothing shifts as numbers
        // change. The name column takes the remaining width and
        // truncates long paths.
        let spacing = ui.spacing().item_spacing.x;
        let name_w = (ui.available_width()
            - RATE_COL_W * 2.0
            - TOTAL_COL_W * 2.0
            - ACTION_COL_W
            - spacing * 5.0)
            .max(80.0);
        let num_cell = |text: &str| RichText::new(text).monospace().size(13.0);
        let mut to_sort: Option<SortMode> = None;
        let mut to_toggle: Option<(String, bool)> = None;
        egui::ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("apps").striped(true).show(ui, |ui| {
                    // Clickable headers: rate columns sort by Rate,
                    // total columns by Total, App by name. The active
                    // sort is bold.
                    let bold = |label: &str, active: bool| {
                        let t = RichText::new(label);
                        if active {
                            t.strong()
                        } else {
                            t
                        }
                    };
                    let sort = snap.sort_mode;
                    if ui
                        .add_sized(
                            Vec2::new(name_w, 20.0),
                            egui::Button::new(bold("App", sort == SortMode::Name)),
                        )
                        .clicked()
                    {
                        to_sort = Some(SortMode::Name);
                    }
                    if ui
                        .add_sized(
                            Vec2::new(RATE_COL_W, 20.0),
                            egui::Button::new(bold("Down", sort == SortMode::Rate)),
                        )
                        .clicked()
                    {
                        to_sort = Some(SortMode::Rate);
                    }
                    if ui
                        .add_sized(
                            Vec2::new(RATE_COL_W, 20.0),
                            egui::Button::new(bold("Up", sort == SortMode::Rate)),
                        )
                        .clicked()
                    {
                        to_sort = Some(SortMode::Rate);
                    }
                    if ui
                        .add_sized(
                            Vec2::new(TOTAL_COL_W, 20.0),
                            egui::Button::new(bold("Down total", sort == SortMode::Total)),
                        )
                        .clicked()
                    {
                        to_sort = Some(SortMode::Total);
                    }
                    if ui
                        .add_sized(
                            Vec2::new(TOTAL_COL_W, 20.0),
                            egui::Button::new(bold("Up total", sort == SortMode::Total)),
                        )
                        .clicked()
                    {
                        to_sort = Some(SortMode::Total);
                    }
                    ui.add_sized(Vec2::new(ACTION_COL_W, 20.0), egui::Label::new(""));
                    ui.end_row();
                    for row in &snap.apps {
                        let blocked = row.blocked;
                        let name = if blocked {
                            format!("⛔ {}", row.name)
                        } else {
                            row.name.clone()
                        };
                        let name_text = if blocked {
                            RichText::new(name)
                                .strong()
                                .color(Color32::from_rgb(255, 120, 120))
                        } else {
                            RichText::new(name)
                        };
                        ui.add_sized(
                            Vec2::new(name_w, 18.0),
                            egui::Label::new(name_text).truncate(),
                        );
                        ui.add_sized(
                            Vec2::new(RATE_COL_W, 18.0),
                            egui::Label::new(num_cell(&row.down)).halign(egui::Align::RIGHT),
                        );
                        ui.add_sized(
                            Vec2::new(RATE_COL_W, 18.0),
                            egui::Label::new(num_cell(&row.up)).halign(egui::Align::RIGHT),
                        );
                        ui.add_sized(
                            Vec2::new(TOTAL_COL_W, 18.0),
                            egui::Label::new(num_cell(&row.down_total_fmt))
                                .halign(egui::Align::RIGHT),
                        );
                        ui.add_sized(
                            Vec2::new(TOTAL_COL_W, 18.0),
                            egui::Label::new(num_cell(&row.up_total_fmt))
                                .halign(egui::Align::RIGHT),
                        );
                        ui.horizontal(|ui| {
                            let badge = block_badge(row.block_state);
                            if !badge.is_empty() {
                                ui.label(
                                    RichText::new(badge)
                                        .small()
                                        .color(Color32::from_rgb(255, 120, 120)),
                                );
                            }
                            let label = block_button_label(blocked);
                            let btn = egui::Button::new(label);
                            if ui.add_enabled(snap.can_block || blocked, btn).clicked() {
                                to_toggle = Some((row.exe_path.clone(), !blocked));
                            }
                        });
                        ui.end_row();
                    }
                });
            });
        if let Some(mode) = to_sort {
            if let Ok(mut req) = self.shared.sort_request.lock() {
                *req = Some(mode);
            }
        }
        if let Some((exe, block)) = to_toggle {
            let can_block = snap.can_block;
            self.set_block(&exe, block, can_block);
        }
        ui.add_space(4.0);
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

    fn draw_adapters(&mut self, ui: &mut egui::Ui, snap: &StateJson) {
        ui.heading("Adapters (all — including virtual / VPN / PDAnet)");
        // --- v0.7: which adapters feed the top numbers. ---
        // Auto (default): the app picks the adapter carrying traffic
        // and switches when it goes quiet. All: sum of everything.
        // Ticking boxes switches to Named: the sum of just those
        // adapters. Untick all to go back to Auto.
        ui.horizontal(|ui| {
            ui.label("Top numbers from:");
            if ui
                .selectable_label(
                    snap.adapter_mode == AdapterSelection::Auto,
                    "Auto (smart switch)",
                )
                .clicked()
            {
                self.set_adapter_mode(AdapterSelection::Auto);
            }
            if ui
                .selectable_label(snap.adapter_mode == AdapterSelection::All, "All")
                .clicked()
            {
                self.set_adapter_mode(AdapterSelection::All);
            }
            if let AdapterSelection::Named(names) = &snap.adapter_mode {
                ui.label(format!("Selected: {}", names.join(", ")));
            }
        });
        ui.label(
            RichText::new(
                "Auto picks the adapter carrying traffic and switches when it goes quiet. \
                 Tick boxes below to sum specific adapters instead — traffic that crosses \
                 two adapters (e.g. VPN over PdaNet) is counted twice. Untick all to go \
                 back to Auto.",
            )
            .small()
            .color(Color32::GRAY),
        );
        ui.add_space(4.0);
        let spacing = ui.spacing().item_spacing.x;
        // Fixed numeric columns, same idea as the apps table; the
        // name column takes the remaining width. The checkbox column
        // is 24px at the left.
        let name_w = (ui.available_width()
            - 24.0
            - 70.0
            - 56.0
            - 130.0
            - RATE_COL_W * 2.0
            - TOTAL_COL_W
            - 70.0
            - spacing * 8.0)
            .max(80.0);
        let mut to_click: Option<(String, bool)> = None;
        egui::ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("adapters").striped(true).show(ui, |ui| {
                    ui.add_sized(Vec2::new(24.0, 20.0), egui::Label::new(""));
                    ui.add_sized(
                        Vec2::new(name_w, 20.0),
                        egui::Label::new(RichText::new("Adapter").strong()),
                    );
                    ui.add_sized(
                        Vec2::new(70.0, 20.0),
                        egui::Label::new(RichText::new("Type").strong()),
                    );
                    ui.add_sized(
                        Vec2::new(56.0, 20.0),
                        egui::Label::new(RichText::new("Status").strong()),
                    );
                    ui.add_sized(
                        Vec2::new(130.0, 20.0),
                        egui::Label::new(RichText::new("IP").strong()),
                    );
                    ui.add_sized(
                        Vec2::new(RATE_COL_W, 20.0),
                        egui::Label::new(RichText::new("Down").strong()),
                    );
                    ui.add_sized(
                        Vec2::new(RATE_COL_W, 20.0),
                        egui::Label::new(RichText::new("Up").strong()),
                    );
                    ui.add_sized(
                        Vec2::new(TOTAL_COL_W, 20.0),
                        egui::Label::new(RichText::new("Total").strong()),
                    );
                    ui.add_sized(Vec2::new(70.0, 20.0), egui::Label::new(""));
                    ui.end_row();
                    for a in &snap.adapters {
                        let mut checked = a.checked;
                        if ui
                            .add_sized(Vec2::new(24.0, 18.0), egui::Checkbox::new(&mut checked, ""))
                            .changed()
                        {
                            to_click = Some((a.name.clone(), checked));
                        }
                        let name = if a.selected && snap.adapter_mode == AdapterSelection::Auto {
                            RichText::new(format!("▶ {}", a.name)).strong()
                        } else {
                            RichText::new(&a.name)
                        };
                        ui.add_sized(Vec2::new(name_w, 18.0), egui::Label::new(name).truncate());
                        ui.label(&a.type_name);
                        ui.label(&a.status);
                        ui.label(a.ip.clone().unwrap_or_else(|| "—".to_string()));
                        ui.add_sized(
                            Vec2::new(RATE_COL_W, 18.0),
                            egui::Label::new(RichText::new(&a.down).monospace())
                                .halign(egui::Align::RIGHT),
                        );
                        ui.add_sized(
                            Vec2::new(RATE_COL_W, 18.0),
                            egui::Label::new(RichText::new(&a.up).monospace())
                                .halign(egui::Align::RIGHT),
                        );
                        ui.add_sized(
                            Vec2::new(TOTAL_COL_W, 18.0),
                            egui::Label::new(RichText::new(&a.total).monospace())
                                .halign(egui::Align::RIGHT),
                        );
                        if a.selected && snap.adapter_mode == AdapterSelection::Auto {
                            ui.label("selected");
                        } else {
                            ui.label("");
                        }
                        ui.end_row();
                    }
                });
            });
        if let Some((name, now_checked)) = to_click {
            let all_names: Vec<String> = snap.adapters.iter().map(|a| a.name.clone()).collect();
            let mode = adapter_mode_after_click(&snap.adapter_mode, &all_names, &name, now_checked);
            self.set_adapter_mode(mode);
        }
        ui.add_space(6.0);
        ui.heading("Diagnostics");
        ui.label(format!(
            "Capture totals by interface index: {}",
            snap.diagnostics
                .capture_totals
                .iter()
                .map(|t| format!(
                    "#{} ↓ {} ↑ {}",
                    t.index,
                    format_bytes(t.down),
                    format_bytes(t.up)
                ))
                .collect::<Vec<_>>()
                .join("  •  ")
        ));
        ui.label(format!(
            "Top source: {}  •  Fallback active: {}  •  Capture: {}",
            snap.diagnostics.top_source,
            snap.diagnostics.fallback_active,
            snap.diagnostics.capture_label
        ));
    }
}

impl eframe::App for MeterApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // ~10 Hz repaint so the numbers and graph stay live.
        ctx.request_repaint_after(Duration::from_millis(100));
        let snap = self.snapshot();

        // --- Minimal window size (v0.7) ---
        // The window itself shrinks when Minimal is clicked: v0.6
        // only hid panels and left the window big. The minimum size
        // must drop first, or the 560x420 normal minimum would clamp
        // the small window. Leaving Minimal restores the size the
        // window had before (or the default full size).
        if snap.minimal != self.minimal {
            self.minimal = snap.minimal;
            if snap.minimal {
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(MINIMAL_MIN_SIZE.into()));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(MINIMAL_SIZE.into()));
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(NORMAL_MIN_SIZE.into()));
                let size = self.normal_size.unwrap_or_else(|| NORMAL_SIZE.into());
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            }
        }
        // Remember the full-view size so leaving Minimal restores it.
        // Only real full-view sizes count — a window the user dragged
        // small must not become the restore size.
        if !snap.minimal {
            if let Some(size) = ctx.input(|i| i.viewport().inner_rect.map(|r| r.size())) {
                if size.x >= NORMAL_MIN_SIZE[0] && size.y >= NORMAL_MIN_SIZE[1] {
                    self.normal_size = Some(size);
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // Top bar: title, opacity slider (normal view), theme
            // toggle, Minimal toggle, run-on-startup.
            ui.horizontal(|ui| {
                if snap.minimal {
                    ui.heading("Internet Control");
                } else {
                    ui.heading(format!("Internet Control — Usage Meter  v{VERSION}"));
                    self.opacity_slider(ui, 90.0);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(theme_button_label(self.dark)).clicked() {
                        let dark = !self.dark;
                        self.set_theme(ctx, dark);
                    }
                    let minimal_label = if snap.minimal { "Full view" } else { "Minimal" };
                    if ui.button(minimal_label).clicked() {
                        if let Ok(mut req) = self.shared.minimal_request.lock() {
                            *req = Some(!snap.minimal);
                        }
                    }
                    if !snap.minimal {
                        let mut startup = snap.run_on_startup;
                        if ui.checkbox(&mut startup, "Run on startup").changed() {
                            if let Ok(mut req) = self.shared.startup_request.lock() {
                                *req = Some(startup);
                            }
                        }
                    }
                });
            });
            ui.separator();

            if let Some(err) = &snap.error {
                ui.label(RichText::new(err).color(Color32::YELLOW));
            }

            // The whole panel scrolls vertically: nothing is cut off
            // when the window is short. (The tables scroll
            // horizontally on their own when the window is narrow.)
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if snap.minimal {
                        // Minimal view (v0.7): rates + opacity slider
                        // + IP. No session totals, no graph, no tabs.
                        self.draw_minimal(ui, &snap);
                        return;
                    }

                    // The big numbers + IP are always on top in the
                    // full view.
                    self.draw_top_strip(ui, &snap);

                    ui.add_space(6.0);
                    // Tabs.
                    ui.horizontal(|ui| {
                        for (tab, label) in [
                            (Tab::Overview, "Overview"),
                            (Tab::Apps, "Apps"),
                            (Tab::Adapters, "Adapters"),
                        ] {
                            if ui.selectable_label(self.tab == tab, label).clicked() {
                                self.tab = tab;
                            }
                        }
                    });
                    ui.separator();
                    ui.add_space(4.0);
                    match self.tab {
                        Tab::Overview => self.draw_overview(ui, &snap),
                        Tab::Apps => self.draw_apps(ui, &snap),
                        Tab::Adapters => self.draw_adapters(ui, &snap),
                    }
                });
        });

        // Apply the opacity if it changed (v0.7). The HWND comes
        // from the frame; on Linux there is none and this is a no-op.
        if self.applied_opacity != Some(self.opacity) {
            if let Some(hwnd) = crate::opacity::hwnd_from_frame(frame) {
                crate::opacity::apply_opacity(hwnd, self.opacity);
            }
            self.applied_opacity = Some(self.opacity);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Ok(store) = self.shared.store.lock() {
            store.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Config, Store};

    #[test]
    fn graph_max_is_the_largest_rate_with_a_floor_of_one() {
        assert_eq!(graph_max(&[], &[]), 1.0);
        assert_eq!(graph_max(&[0.0, 0.0], &[0.0]), 1.0);
        assert_eq!(graph_max(&[100.0, 5.0], &[50.0]), 100.0);
        assert_eq!(graph_max(&[10.0], &[250.0, 3.0]), 250.0);
    }

    #[test]
    fn theme_button_shows_the_current_theme() {
        assert_eq!(theme_button_label(true), "Theme: Dark");
        assert_eq!(theme_button_label(false), "Theme: Light");
    }

    #[test]
    fn block_button_and_badge_labels() {
        assert_eq!(block_button_label(true), "Unblock");
        assert_eq!(block_button_label(false), "Block");
        assert_eq!(block_badge(BlockState::Blocked), "BLOCKED");
        assert_eq!(
            block_badge(BlockState::BlockedNewConnectionsOnly),
            "NEW CONNECTIONS ONLY"
        );
        assert_eq!(block_badge(BlockState::NotBlocked), "");
    }

    // A "999.9 MB/s" rate string at 13px monospace is ~70px wide;
    // the columns must be wider than that or values clip. Checked
    // at compile time so a width change cannot silently break it.
    const _: () = assert!(RATE_COL_W >= 80.0);
    const _: () = assert!(TOTAL_COL_W >= 80.0);
    const _: () = assert!(ACTION_COL_W >= 100.0);

    #[test]
    fn old_config_without_theme_defaults_to_dark() {
        // Configs written before v0.6 have no dark_theme field: they
        // must load as dark (the previous look), not light.
        let store: Store = serde_json::from_str(
            r#"{"config":{},"today":{"date":"2026-11-26","down_bytes":0,"up_bytes":0}}"#,
        )
        .unwrap();
        assert!(store.config.dark_theme);
        assert!(Config::default().dark_theme);
    }

    #[test]
    fn theme_round_trips_through_the_store() {
        let mut store = Store::default();
        store.config.dark_theme = false;
        let text = serde_json::to_string(&store).unwrap();
        let back: Store = serde_json::from_str(&text).unwrap();
        assert!(!back.config.dark_theme);
    }

    #[test]
    fn opacity_round_trips_through_the_store() {
        let mut store = Store::default();
        assert_eq!(store.config.opacity, 1.0);
        store.config.opacity = 0.65;
        let text = serde_json::to_string(&store).unwrap();
        let back: Store = serde_json::from_str(&text).unwrap();
        assert!((back.config.opacity - 0.65).abs() < f32::EPSILON);
        // Old configs without the field load as fully opaque.
        let store: Store = serde_json::from_str(
            r#"{"config":{},"today":{"date":"2026-11-26","down_bytes":0,"up_bytes":0}}"#,
        )
        .unwrap();
        assert_eq!(store.config.opacity, 1.0);
    }

    // Minimal really is small (v0.7): the window must shrink well
    // below the normal minimum, and the drag minimum must be smaller
    // still, or the window cannot reach the target size at all.
    const _: () = assert!(MINIMAL_SIZE[0] < NORMAL_MIN_SIZE[0]);
    const _: () = assert!(MINIMAL_SIZE[1] < NORMAL_MIN_SIZE[1]);
    const _: () = assert!(MINIMAL_MIN_SIZE[0] < MINIMAL_SIZE[0]);
    const _: () = assert!(MINIMAL_MIN_SIZE[1] < MINIMAL_SIZE[1]);
    const _: () = assert!(MINIMAL_MIN_SIZE[0] >= 200.0);
    const _: () = assert!(MINIMAL_MIN_SIZE[1] >= 100.0);
    const _: () = assert!(NORMAL_SIZE[0] >= NORMAL_MIN_SIZE[0]);
    const _: () = assert!(NORMAL_SIZE[1] >= NORMAL_MIN_SIZE[1]);

    #[test]
    fn minimal_window_target_size() {
        // ~360x190: rates + slider + IP, nothing else.
        assert_eq!(MINIMAL_SIZE, [360.0, 190.0]);
        assert_eq!(MINIMAL_MIN_SIZE, [300.0, 150.0]);
        assert_eq!(NORMAL_SIZE, [980.0, 720.0]);
        assert_eq!(NORMAL_MIN_SIZE, [560.0, 420.0]);
    }
}
