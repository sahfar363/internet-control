//! Native egui UI for the meter (v0.6: complete Rust, no WebView2/Edge).
//!
//! Reads the snapshot the bridge thread maintains in [`Shared`]
//! ([`StateJson`]) and writes user actions back through the same
//! request slots the old HTTP API used (select / minimal / view /
//! sort / startup) plus the block logic (shared blocked set +
//! store + SetTcpEntry termination, as the old `/api/block` did).
//!
//! Layout (clean and minimal):
//! * Top bar: title, Dark/Light theme toggle, Minimal toggle,
//!   run-on-startup checkbox.
//! * Big DOWNLOAD / UPLOAD rates + session totals + the carrying
//!   adapter's name and IP — shown in BOTH normal and minimal view.
//! * Tabs (full view): Overview (graph + totals + capture status),
//!   Apps (per-app list with Block/Unblock), Adapters (adapter list
//!   + diagnostics). Minimal view: top numbers + graph only.
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
use crate::state::{AppView, BlockState, Shared, SortMode, StateJson};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Download blue / upload orange — the v0.2 colours, kept ever since.
const DOWN_COLOR: Color32 = Color32::from_rgb(80, 180, 255);
const UP_COLOR: Color32 = Color32::from_rgb(255, 170, 60);

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
}

impl MeterApp {
    pub fn new(shared: Arc<Shared>, cc: &eframe::CreationContext<'_>) -> Self {
        let dark = shared
            .store
            .lock()
            .map(|s| s.config.dark_theme)
            .unwrap_or(true);
        cc.egui_ctx.set_visuals(if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        Self {
            shared,
            tab: Tab::Overview,
            dark,
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
        ui.label(
            RichText::new(
                "Totals are for the carrying adapter (or all interfaces while it is idle) — \
                 never the sum of all adapters, so VPN + PDAnet carrying the same bytes are \
                 not counted twice.",
            )
            .small()
            .color(Color32::GRAY),
        );
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
        let spacing = ui.spacing().item_spacing.x;
        // Fixed numeric columns, same idea as the apps table; the
        // name column takes the remaining width.
        let name_w = (ui.available_width()
            - 70.0
            - 56.0
            - 130.0
            - RATE_COL_W * 2.0
            - TOTAL_COL_W
            - 70.0
            - spacing * 7.0)
            .max(80.0);
        let mut to_select: Option<u64> = None;
        egui::ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("adapters").striped(true).show(ui, |ui| {
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
                        let name = if a.selected {
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
                        if a.selected {
                            ui.label("selected");
                        } else if ui.button("Select").clicked() {
                            to_select = Some(a.luid);
                        }
                        ui.end_row();
                    }
                });
            });
        if let Some(luid) = to_select {
            if let Ok(mut req) = self.shared.select_request.lock() {
                *req = Some(luid);
            }
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ~10 Hz repaint so the numbers and graph stay live.
        ctx.request_repaint_after(Duration::from_millis(100));
        let snap = self.snapshot();

        egui::CentralPanel::default().show(ctx, |ui| {
            // Top bar: title, theme toggle, Minimal toggle,
            // run-on-startup.
            ui.horizontal(|ui| {
                ui.heading(format!("Internet Control — Usage Meter  v{VERSION}"));
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
                    let mut startup = snap.run_on_startup;
                    if ui.checkbox(&mut startup, "Run on startup").changed() {
                        if let Ok(mut req) = self.shared.startup_request.lock() {
                            *req = Some(startup);
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
                    // The big numbers + IP are always on top, in both
                    // views.
                    self.draw_top_strip(ui, &snap);

                    if snap.minimal {
                        // Minimal view: numbers + IP + the live graph.
                        // No tabs, no app list, no adapters.
                        ui.add_space(6.0);
                        self.draw_graph(ui, &snap);
                        return;
                    }

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
}
