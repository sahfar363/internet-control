//! egui UI for the meter.

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use egui::{Color32, RichText, Stroke, Vec2};

use crate::adapters::{default_route_luid, AdapterReading, SampleBatch};
use crate::meter::{format_bytes, format_rate};
use crate::store::Store;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HISTORY: usize = 600; // 600 samples @ 10 Hz = 60 seconds.

pub struct MeterApp {
    rx: Receiver<SampleBatch>,
    store: Store,
    adapters: Vec<AdapterReading>,
    selected_luid: Option<u64>,
    /// (down_bps, up_bps) history for the selected adapter.
    history: VecDeque<(f64, f64)>,
    down_bps: f64,
    up_bps: f64,
    session_down: u64,
    session_up: u64,
    peak_down: f64,
    peak_up: f64,
    last_batch: Option<Instant>,
    last_save: Instant,
    error: Option<String>,
}

impl MeterApp {
    pub fn new(rx: Receiver<SampleBatch>) -> Self {
        let store = Store::load();
        // Restore the saved selection if that adapter still exists later;
        // otherwise the default-route adapter is picked on first batch.
        let selected_luid = store.config.selected_luid;
        Self {
            rx,
            store,
            adapters: Vec::new(),
            selected_luid,
            history: VecDeque::with_capacity(HISTORY),
            down_bps: 0.0,
            up_bps: 0.0,
            session_down: 0,
            session_up: 0,
            peak_down: 0.0,
            peak_up: 0.0,
            last_batch: None,
            last_save: Instant::now(),
            error: None,
        }
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
        if let Some(a) = self.adapters.iter().find(|a| a.info.luid == luid) {
            self.store.config.selected_luid = Some(luid);
            self.store.config.selected_name = Some(a.info.name.clone());
            self.store.save();
        }
    }

    fn drain(&mut self) {
        while let Ok(batch) = self.rx.try_recv() {
            self.last_batch = Some(Instant::now());
            self.adapters = batch.adapters;
            self.pick_default();
            // Copy the selected reading out first (borrow ends here) so
            // the fields below can be assigned.
            let sel = self
                .selected()
                .map(|s| (s.down_bps, s.up_bps, s.down_delta, s.up_delta));
            if let Some((down_bps, up_bps, down_delta, up_delta)) = sel {
                self.down_bps = down_bps;
                self.up_bps = up_bps;
                self.session_down += down_delta;
                self.session_up += up_delta;
                // Daily totals are for the selected adapter only.
                self.store.today.down_bytes += down_delta;
                self.store.today.up_bytes += up_delta;
                if down_bps > self.peak_down {
                    self.peak_down = down_bps;
                }
                if up_bps > self.peak_up {
                    self.peak_up = up_bps;
                }
                self.history.push_back((down_bps, up_bps));
                if self.history.len() > HISTORY {
                    self.history.pop_front();
                }
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
        plot(&|p| p.0, Color32::from_rgb(80, 180, 255)); // download
        plot(&|p| p.1, Color32::from_rgb(255, 170, 60)); // upload
        painter.text(
            rect.left_top() + Vec2::new(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            format!("peak in graph: {}", format_rate(max_v)),
            egui::FontId::proportional(11.0),
            Color32::GRAY,
        );
    }
}

impl eframe::App for MeterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        // ~10 Hz repaint so the numbers and graph stay live.
        ctx.request_repaint_after(Duration::from_millis(100));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(format!("Internet Control — Usage Meter  v{VERSION}"));
            ui.label(format!("Selected adapter: {}", self.selected_name()));
            ui.add_space(4.0);

            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("DOWNLOAD").small().color(Color32::GRAY));
                    ui.label(
                        RichText::new(format_rate(self.down_bps))
                            .size(32.0)
                            .color(Color32::from_rgb(80, 180, 255)),
                    );
                });
                ui.add_space(32.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("UPLOAD").small().color(Color32::GRAY));
                    ui.label(
                        RichText::new(format_rate(self.up_bps))
                            .size(32.0)
                            .color(Color32::from_rgb(255, 170, 60)),
                    );
                });
            });

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
            ui.heading("Adapters (all — including virtual / VPN / PDAnet)");
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                egui::Grid::new("adapters")
                    .striped(true)
                    .show(ui, |ui| {
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

            if let Some(err) = &self.error {
                ui.add_space(6.0);
                ui.label(RichText::new(err).color(Color32::YELLOW));
            }
            ui.add_space(4.0);
            ui.label(
                "No admin rights needed for the meter. \
                 Verification: download a known 100 MB file — the session total \
                 should read ~100 MB, not 0 and not ~200 MB.",
            );
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.store.save();
    }
}
