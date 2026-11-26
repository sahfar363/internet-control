//! Shared UI state + the pure top-totals / app-list logic (v0.4).
//!
//! v0.3 root-cause summary (why the top DOWNLOAD/UPLOAD read 0 on
//! Sahand's PC while the per-app list kept moving):
//!
//! 1. **Sticky single-adapter selection (primary).** The top numbers,
//!    session, today, peak and graph all followed ONE adapter, picked
//!    once by `pick_default` (saved LUID -> default route -> busiest)
//!    and then never re-evaluated: `pick_default` returned immediately
//!    whenever a selection existed, even if that adapter sat at 0 for
//!    minutes while other interfaces carried megabytes. PdaNet is a PPP
//!    adapter: it gets a NEW LUID and InterfaceIndex every time the
//!    phone re-tethers, so a saved LUID goes stale, and the default
//!    route can point at a zero-traffic adapter (VPN / Ethernet) —
//!    after which the top numbers were 0 forever. The per-app list is
//!    interface-agnostic, so it kept moving. That is exactly the
//!    reported symptom pair.
//! 2. **Fragile interface-index keying.** Per-interface capture totals
//!    are keyed by WinDivert `interface_id` and matched against
//!    `GetIfTable2` `InterfaceIndex`. WinDivert's own docs say the
//!    IfIdx/SubIfIdx values are the NDIS-reported interface numbers
//!    ("ignored for outbound packets" in the send path) — for PPP /
//!    virtual adapters the reported index can disagree with
//!    `GetIfTable2`, so bytes land under a key no adapter owns.
//! 3. **Enumeration used `GetIfTable2` only.** An adapter that IP
//!    Helper lists in `GetAdaptersAddresses` (PdaNet shows up in
//!    `Get-NetAdapter` / `ipconfig`) but not in `GetIfTable2` never
//!    appeared in the UI at all. v0.4 unions both sources by LUID
//!    (see `adapters::enumerate_adapters`).
//!
//! v0.4 fix (all in this module, unit-tested):
//! * [`resolve_top`] — the top numbers use the selected adapter when
//!   it carries traffic, and **fall back to the sum over all capture
//!   interfaces** (with a visible note) when the selected adapter
//!   shows nothing while packets are flowing. The top numbers can
//!   never read 0 while the per-app list shows traffic.
//! * The bridge auto-reselects: if the selected adapter is idle for
//!   ~1 s while other interfaces carry traffic, it switches to the
//!   busiest carrying interface and says so.
//! * [`merge_capture_readings`] — the "Unknown interface N" fallback
//!   is a pure function now, covered by tests (v0.3 had it inline in
//!   the sampler, untestable on Linux — and Sahand never saw the row).
//! * Session / today / peak accumulate from the RESOLVED top totals,
//!   so they stay exact even in fallback mode.
//! * [`AppListState`] — the app list keeps a stable order across
//!   polls; it is re-sorted only when the user picks a sort or flips
//!   the Rate/Total view. Blocked apps are pinned to the top.
//! * [`block_state`] — a blocked app whose rates are still > 0 is
//!   shown as "BLOCKED — new connections only": its pre-existing
//!   flows started before this app launched, so their packets are
//!   unattributed and cannot be dropped until they reconnect.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::adapters::{AdapterInfo, AdapterReading, AppReading, SampleBatch};
use crate::capture::{CaptureStatus, IfTotals};
use crate::meter::{format_bytes, format_rate, RateCalculator, RateSmoother};
use crate::store::Store;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HISTORY: usize = 600; // 600 samples @ 10 Hz = 60 seconds.
const IDLE_RESELECT_TICKS: u32 = 10; // ~1 s of idle-selected while traffic flows.

// ---------------------------------------------------------------------------
// Top totals: selected adapter, with fallback to all interfaces
// ---------------------------------------------------------------------------

/// Where the big DOWNLOAD/UPLOAD numbers come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TopSource {
    /// The selected adapter's own capture/counter totals.
    Selected,
    /// The selected adapter shows no traffic while packets flow:
    /// summed over all capture interfaces. Shown with a note.
    AllInterfacesFallback,
    /// WinDivert unavailable: the v0.1 adapter-counter fallback.
    CounterFallback,
}

impl TopSource {
    pub fn label(&self) -> &'static str {
        match self {
            TopSource::Selected => "selected adapter",
            TopSource::AllInterfacesFallback => "all interfaces (fallback)",
            TopSource::CounterFallback => "adapter counters (fallback)",
        }
    }
}

/// The resolved top numbers for one tick.
#[derive(Debug, Clone, PartialEq)]
pub struct TopTotals {
    pub down_bps: f64,
    pub up_bps: f64,
    pub down_total: u64,
    pub up_total: u64,
    pub source: TopSource,
    /// Human-readable note when the source is not the plain selection.
    pub note: Option<String>,
}

/// Input for the selected adapter, if one is selected.
#[derive(Debug, Clone, Copy)]
pub struct TopInput {
    pub totals: IfTotals,
    pub down_bps: f64,
    pub up_bps: f64,
}

/// Resolve the top numbers. Pure — unit-tested below.
///
/// * `selected` — the selected adapter's totals + rates, if any.
/// * `total_all` — capture totals summed over ALL interfaces.
/// * `all_rates` — rates for that all-interfaces sum (from a dedicated
///   `RateCalculator` over `total_all`, so VPN + PdaNet carrying the
///   same bytes are not double-counted in the rates either).
/// * `capture_active` — false means the counter fallback: no capture
///   totals exist, so there is nothing to fall back to.
pub fn resolve_top(
    selected: Option<TopInput>,
    selected_name: Option<&str>,
    total_all: IfTotals,
    all_rates: (f64, f64),
    capture_active: bool,
) -> TopTotals {
    if !capture_active {
        return match selected {
            Some(sel) => TopTotals {
                down_bps: sel.down_bps,
                up_bps: sel.up_bps,
                down_total: sel.totals.down_bytes,
                up_total: sel.totals.up_bytes,
                source: TopSource::CounterFallback,
                note: None,
            },
            None => TopTotals {
                down_bps: 0.0,
                up_bps: 0.0,
                down_total: 0,
                up_total: 0,
                source: TopSource::CounterFallback,
                note: Some("No adapter selected.".to_string()),
            },
        };
    }
    let all_bytes = total_all.down_bytes + total_all.up_bytes;
    match selected {
        Some(sel) => {
            let sel_bytes = sel.totals.down_bytes + sel.totals.up_bytes;
            // Selected adapter carries traffic, or nothing flows
            // anywhere: show the selection.
            if sel_bytes > 0 || all_bytes == 0 {
                TopTotals {
                    down_bps: sel.down_bps,
                    up_bps: sel.up_bps,
                    down_total: sel.totals.down_bytes,
                    up_total: sel.totals.up_bytes,
                    source: TopSource::Selected,
                    note: None,
                }
            } else {
                // Selected adapter is dry while packets flow elsewhere
                // (stale LUID / wrong index / wrong adapter): fall back
                // to all interfaces instead of showing 0.
                let name = selected_name.unwrap_or("Selected adapter");
                TopTotals {
                    down_bps: all_rates.0,
                    up_bps: all_rates.1,
                    down_total: total_all.down_bytes,
                    up_total: total_all.up_bytes,
                    source: TopSource::AllInterfacesFallback,
                    note: Some(format!(
                        "{name} shows no captured traffic — showing all interfaces instead."
                    )),
                }
            }
        }
        None => {
            if all_bytes > 0 {
                TopTotals {
                    down_bps: all_rates.0,
                    up_bps: all_rates.1,
                    down_total: total_all.down_bytes,
                    up_total: total_all.up_bytes,
                    source: TopSource::AllInterfacesFallback,
                    note: Some("No adapter selected — showing all interfaces instead.".to_string()),
                }
            } else {
                TopTotals {
                    down_bps: 0.0,
                    up_bps: 0.0,
                    down_total: 0,
                    up_total: 0,
                    source: TopSource::Selected,
                    note: None,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Adapter merge: capture totals + enumeration, incl. "Unknown interface"
// ---------------------------------------------------------------------------

/// One adapter row for the UI: the enumerated adapter (if any) plus its
/// capture totals. `unknown == true` means WinDivert sees traffic on
/// an interface index `GetIfTable2` / `GetAdaptersAddresses` never
/// listed — shown as "Unknown interface N" instead of dropping bytes.
#[derive(Debug, Clone)]
pub struct MergedAdapter {
    pub info: AdapterInfo,
    pub totals: IfTotals,
    /// True for synthetic "Unknown interface N" rows. (Read by tests;
    /// the sampler only needs `info`/`totals`.)
    #[allow(dead_code)]
    pub unknown: bool,
}

/// LUIDs for synthetic "Unknown interface N" rows. Real LUIDs never
/// carry this prefix (ASCII "WDIV" in the high bytes).
pub fn synthetic_luid(if_index: u32) -> u64 {
    0x5744_4956_0000_0000u64 | if_index as u64
}

/// Merge enumerated adapters with raw per-interface capture totals.
/// Pure — unit-tested below. Rules:
/// * every enumerated adapter gets a row (zero totals if WinDivert
///   saw nothing on it);
/// * every capture interface with traffic and no matching adapter gets
///   an "Unknown interface N" row;
/// * a capture interface with zero traffic and no adapter gets no row
///   (nothing to show, and it would be a stale row forever).
pub fn merge_capture_readings(
    adapters: &[AdapterInfo],
    totals: &HashMap<u32, IfTotals>,
) -> Vec<MergedAdapter> {
    let mut out: Vec<MergedAdapter> = adapters
        .iter()
        .map(|info| MergedAdapter {
            info: info.clone(),
            totals: totals.get(&info.index).copied().unwrap_or_default(),
            unknown: false,
        })
        .collect();
    let mut unknown_idx: Vec<u32> = totals
        .iter()
        .filter(|(idx, t)| {
            t.down_bytes + t.up_bytes > 0 && !adapters.iter().any(|a| a.index == **idx)
        })
        .map(|(idx, _)| *idx)
        .collect();
    unknown_idx.sort_unstable();
    for idx in unknown_idx {
        let t = totals.get(&idx).copied().unwrap_or_default();
        out.push(MergedAdapter {
            info: AdapterInfo {
                luid: synthetic_luid(idx),
                index: idx,
                name: format!("Unknown interface {idx}"),
                type_name: "Unknown".to_string(),
                oper_status: 1,
                status_text: "Up".to_string(),
                in_octets: t.down_bytes,
                out_octets: t.up_bytes,
                ..Default::default()
            },
            totals: t,
            unknown: true,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// App list: stable order, sort only on demand, Rate/Total view
// ---------------------------------------------------------------------------

/// How the per-app list is sorted. Clicking a rate column sorts by
/// Rate (down+up rate), clicking a total column sorts by Total
/// (cumulative down+up bytes), clicking App sorts by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SortMode {
    #[default]
    Rate,
    Total,
    Name,
}

/// Which metric the Apps list is primarily shown/sorted by. The toggle
/// button switches between them; both rate and total columns exist in
/// the data either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppView {
    #[default]
    Rate,
    Total,
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

/// What a blocked row means right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockState {
    /// Not blocked.
    #[default]
    NotBlocked,
    /// Blocked and its traffic has stopped (rates are 0).
    Blocked,
    /// Blocked, but its rates are still > 0: the flows it uses started
    /// before this app launched, so their packets are unattributed and
    /// cannot be dropped. New connections are blocked; the old ones
    /// stop when the app reconnects. Shown as a distinct badge so the
    /// UI never pretends a still-moving app is fully blocked.
    BlockedNewConnectionsOnly,
}

/// Pure decision: a blocked app still moving is only blocked for new
/// connections. Unblocked apps are never "blocked".
pub fn block_state(blocked: bool, down_bps: f64, up_bps: f64) -> BlockState {
    if !blocked {
        BlockState::NotBlocked
    } else if down_bps > 0.0 || up_bps > 0.0 {
        BlockState::BlockedNewConnectionsOnly
    } else {
        BlockState::Blocked
    }
}

/// The per-app list with a STABLE row order: rows are keyed by exe
/// path, updated in place on every tick, and re-sorted only when the
/// user asks (header click / view toggle). This is what stops the
/// v0.3 "rows jump every tick, Block is unclickable" problem.
#[derive(Debug, Default)]
pub struct AppListState {
    /// Exe paths in current display order.
    order: Vec<String>,
    /// Latest reading per exe path.
    last: HashMap<String, AppReading>,
    sort: SortMode,
    view: AppView,
}

impl AppListState {
    pub fn new(sort: SortMode, view: AppView) -> Self {
        Self {
            order: Vec::new(),
            last: HashMap::new(),
            sort,
            view,
        }
    }

    /// Feed the latest readings. New exes are appended at the end
    /// (never inserted mid-list); exes that disappeared are dropped.
    /// The order is NOT re-sorted here — that only happens in
    /// `set_sort` / `set_view`.
    pub fn update(&mut self, apps: &[AppReading]) {
        let live: HashSet<&str> = apps.iter().map(|a| a.exe_path.as_str()).collect();
        self.order.retain(|exe| live.contains(exe.as_str()));
        for app in apps {
            if !self.last.contains_key(&app.exe_path) {
                self.order.push(app.exe_path.clone());
            }
            self.last.insert(app.exe_path.clone(), app.clone());
        }
        self.last.retain(|exe, _| live.contains(exe.as_str()));
    }

    /// Re-sort now, by the given mode (user clicked a header).
    pub fn set_sort(&mut self, mode: SortMode) {
        self.sort = mode;
        let mut rows: Vec<AppReading> = self
            .order
            .iter()
            .filter_map(|e| self.last.get(e))
            .cloned()
            .collect();
        sort_apps(&mut rows, mode);
        self.order = rows.iter().map(|r| r.exe_path.clone()).collect();
    }

    /// Switch Rate/Total view: re-sorts by the view's primary metric.
    pub fn set_view(&mut self, view: AppView) {
        self.view = view;
        let mode = match view {
            AppView::Rate => SortMode::Rate,
            AppView::Total => SortMode::Total,
        };
        self.set_sort(mode);
    }

    pub fn view(&self) -> AppView {
        self.view
    }

    pub fn sort(&self) -> SortMode {
        self.sort
    }

    /// Rows in display order, blocked apps pinned to the top (in
    /// their relative order), then the rest.
    pub fn rows(&self, blocked: &HashSet<String>) -> Vec<AppReading> {
        let mut rows: Vec<AppReading> = self
            .order
            .iter()
            .filter_map(|e| self.last.get(e))
            .cloned()
            .collect();
        rows.sort_by_key(|r| {
            // Blocked first (false < true, so blocked sorts first).
            !blocked.contains(&r.exe_path.to_lowercase())
        });
        rows
    }
}

// ---------------------------------------------------------------------------
// JSON snapshot served to the WebView2 UI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct TotalsJson {
    pub down: u64,
    pub up: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RatesJson {
    pub down: f64,
    pub up: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TopJson {
    pub down_bps: f64,
    pub up_bps: f64,
    pub down_total: u64,
    pub up_total: u64,
    pub down: String,
    pub up: String,
    pub source: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AdapterJson {
    pub luid: u64,
    pub index: u32,
    pub name: String,
    pub type_name: String,
    pub status: String,
    pub ip: Option<String>,
    pub down_bps: f64,
    pub up_bps: f64,
    pub down: String,
    pub up: String,
    pub total: String,
    pub unknown: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AppJson {
    pub exe_path: String,
    pub name: String,
    pub down_bps: f64,
    pub up_bps: f64,
    pub down: String,
    pub up: String,
    pub down_total: u64,
    pub up_total: u64,
    pub down_total_fmt: String,
    pub up_total_fmt: String,
    pub blocked: bool,
    pub block_state: BlockState,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IfTotalJson {
    pub index: u32,
    pub down: u64,
    pub up: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DiagnosticsJson {
    /// Raw per-interface capture totals, keyed by interface index.
    pub capture_totals: Vec<IfTotalJson>,
    /// How many adapters enumeration found (GetIfTable2 +
    /// GetAdaptersAddresses union).
    pub adapters_enumerated: usize,
    /// Total packets WinDivert has seen (0 in counter mode).
    pub packets: u64,
    /// Which interface index the top numbers' bytes came from.
    pub top_source: String,
    /// True when the top numbers are the all-interfaces fallback.
    pub fallback_active: bool,
    /// Capture mode label, e.g. "WinDivert intercept".
    pub capture_label: String,
}

/// The whole UI state, serialized to JSON for `GET /api/state`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StateJson {
    pub version: String,
    pub capture_label: String,
    pub capture_active: bool,
    pub can_block: bool,
    pub top: TopJson,
    pub session: TotalsJson,
    pub today: TotalsJson,
    pub peak: RatesJson,
    pub peak_down_fmt: String,
    pub peak_up_fmt: String,
    pub history_down: Vec<f64>,
    pub history_up: Vec<f64>,
    pub adapters: Vec<AdapterJson>,
    pub apps: Vec<AppJson>,
    pub diagnostics: DiagnosticsJson,
    pub selected_luid: Option<u64>,
    pub selected_name: String,
    pub selected_ip: String,
    pub minimal: bool,
    pub run_on_startup: bool,
    pub app_view: AppView,
    pub sort_mode: SortMode,
    pub selection_note: Option<String>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared handles: snapshot + controls the HTTP server writes, the
// bridge thread reads
// ---------------------------------------------------------------------------

/// Handles shared between the sampler bridge thread and the HTTP
/// server thread. The server never touches capture state directly —
/// it writes requests here and the bridge applies them on the next
/// sample tick (~100 ms), which is also what resets smoothers and
/// history when the selection changes.
pub struct Shared {
    pub snapshot: RwLock<StateJson>,
    /// Blocked apps (lowercased exe paths), shared with the capture
    /// thread. Blocking only applies while this app is running.
    /// `Arc` so the sampler can hold the same set.
    pub blocked: Arc<RwLock<HashSet<String>>>,
    pub store: Mutex<Store>,
    /// Adapter selection requested from the UI (LUID).
    pub select_request: Mutex<Option<u64>>,
    pub minimal_request: Mutex<Option<bool>>,
    pub view_request: Mutex<Option<AppView>>,
    pub sort_request: Mutex<Option<SortMode>>,
    pub startup_request: Mutex<Option<bool>>,
}

impl Shared {
    pub fn new(store: Store) -> Self {
        let blocked: HashSet<String> = store
            .config
            .blocked_exes
            .iter()
            .map(|e| e.to_lowercase())
            .collect();
        let snap = StateJson {
            version: VERSION.to_string(),
            minimal: store.config.minimal,
            app_view: store.config.app_view,
            run_on_startup: store.config.run_on_startup,
            ..Default::default()
        };
        Self {
            snapshot: RwLock::new(snap),
            blocked: Arc::new(RwLock::new(blocked)),
            store: Mutex::new(store),
            select_request: Mutex::new(None),
            minimal_request: Mutex::new(None),
            view_request: Mutex::new(None),
            sort_request: Mutex::new(None),
            startup_request: Mutex::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge: SampleBatch -> StateJson
// ---------------------------------------------------------------------------

/// Consumes sampler batches and maintains the snapshot the UI polls.
/// Owns selection, smoothing, session/today/peak and the app list.
pub struct Bridge {
    shared: std::sync::Arc<Shared>,
    selected_luid: Option<u64>,
    down_smoother: RateSmoother,
    up_smoother: RateSmoother,
    app_smoothers: HashMap<String, (RateSmoother, RateSmoother)>,
    /// Rates for the all-interfaces fallback, from a dedicated
    /// calculator over total-all (no double-counting).
    fallback_calc: RateCalculator,
    session_down: u64,
    session_up: u64,
    peak_down: f64,
    peak_up: f64,
    history: VecDeque<(f64, f64)>,
    /// Totals the last top numbers were computed from, per source.
    last_top: IfTotals,
    last_source: TopSource,
    app_list: AppListState,
    /// Ticks the selected adapter has been idle while traffic flows.
    idle_ticks: u32,
    selection_note: Option<String>,
    last_save: Instant,
    error: Option<String>,
    /// "Run on startup" key refreshed once at startup (the exe may
    /// have moved since it was set) — not on every tick.
    /// (Windows only; the Linux stub build never reads it.)
    #[allow(dead_code)]
    startup_checked: bool,
}

impl Bridge {
    pub fn new(shared: std::sync::Arc<Shared>) -> Self {
        let (selected_luid, minimal, view) = {
            let store = shared.store.lock().unwrap_or_else(|e| e.into_inner());
            (
                store.config.selected_luid,
                store.config.minimal,
                store.config.app_view,
            )
        };
        let _ = minimal;
        Self {
            shared,
            selected_luid,
            down_smoother: RateSmoother::new(),
            up_smoother: RateSmoother::new(),
            app_smoothers: HashMap::new(),
            fallback_calc: RateCalculator::new(),
            session_down: 0,
            session_up: 0,
            peak_down: 0.0,
            peak_up: 0.0,
            history: VecDeque::with_capacity(HISTORY),
            last_top: IfTotals::default(),
            last_source: TopSource::Selected,
            app_list: AppListState::new(SortMode::Rate, view),
            idle_ticks: 0,
            selection_note: None,
            last_save: Instant::now(),
            error: None,
            startup_checked: false,
        }
    }

    fn selected_reading<'a>(&self, batch: &'a SampleBatch) -> Option<&'a AdapterReading> {
        self.selected_luid
            .and_then(|luid| batch.adapters.iter().find(|a| a.info.luid == luid))
    }

    fn reset_display(&mut self) {
        self.history.clear();
        self.down_smoother = RateSmoother::new();
        self.up_smoother = RateSmoother::new();
        self.fallback_calc = RateCalculator::new();
        // The session-delta baseline is set by select() right after
        // this, from the newly selected adapter's current totals.
        self.last_top = IfTotals::default();
        self.last_source = TopSource::Selected;
        self.idle_ticks = 0;
    }

    fn select(&mut self, luid: u64, batch: &SampleBatch) {
        self.selected_luid = Some(luid);
        self.selection_note = None;
        self.reset_display();
        let mut store = self.shared.store.lock().unwrap_or_else(|e| e.into_inner());
        store.config.selected_luid = Some(luid);
        if let Some(a) = batch.adapters.iter().find(|a| a.info.luid == luid) {
            store.config.selected_name = Some(a.info.name.clone());
            // Re-baseline the session delta on the newly selected
            // adapter's current totals, so the switch never adds a
            // bogus jump to the session total.
            self.last_top = IfTotals {
                down_bytes: a.info.in_octets,
                up_bytes: a.info.out_octets,
            };
            self.last_source = TopSource::Selected;
        }
        store.save();
    }

    /// Pick the initial / replacement selection. Order: saved LUID,
    /// default-route adapter, busiest interface by CAPTURED TOTALS
    /// (not instantaneous rate — rates are 0 on the first tick).
    fn pick_default(&mut self, batch: &SampleBatch) {
        // Read the saved LUID into a local first (dropping the
        // store lock) — select() below needs &mut self.
        let saved_luid = self
            .shared
            .store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .selected_luid;
        if let Some(saved) = saved_luid {
            if batch.adapters.iter().any(|a| a.info.luid == saved) {
                // select() (not a bare assignment) so the session
                // delta re-baselines on this adapter's totals.
                self.select(saved, batch);
                return;
            }
        }
        // Saved NAME as a second chance: PPP adapters get a new LUID
        // on every reconnect, but the friendly name usually survives.
        let saved_name = self
            .shared
            .store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .selected_name
            .clone();
        if let Some(name) = saved_name {
            if let Some(a) = batch
                .adapters
                .iter()
                .find(|a| a.info.name.eq_ignore_ascii_case(&name))
            {
                let luid = a.info.luid;
                self.select(luid, batch);
                return;
            }
        }
        #[cfg(windows)]
        if let Some(luid) = crate::adapters::default_route_luid() {
            if batch.adapters.iter().any(|a| a.info.luid == luid) {
                self.select(luid, batch);
                return;
            }
        }
        if let Some(best) = batch
            .adapters
            .iter()
            .filter(|a| a.info.oper_status == 1)
            .max_by_key(|a| a.info.in_octets + a.info.out_octets)
        {
            self.select(best.info.luid, batch);
        }
    }

    pub fn apply(&mut self, batch: SampleBatch) {
        // --- UI requests -----------------------------------------------
        if let Some(luid) = self
            .shared
            .select_request
            .lock()
            .ok()
            .and_then(|mut r| r.take())
        {
            if batch.adapters.iter().any(|a| a.info.luid == luid) {
                self.select(luid, &batch);
            }
        }
        if let Some(minimal) = self
            .shared
            .minimal_request
            .lock()
            .ok()
            .and_then(|mut r| r.take())
        {
            let mut store = self.shared.store.lock().unwrap_or_else(|e| e.into_inner());
            store.config.minimal = minimal;
            store.save();
        }
        if let Some(view) = self
            .shared
            .view_request
            .lock()
            .ok()
            .and_then(|mut r| r.take())
        {
            self.app_list.set_view(view);
            let mut store = self.shared.store.lock().unwrap_or_else(|e| e.into_inner());
            store.config.app_view = view;
            store.save();
        }
        if let Some(mode) = self
            .shared
            .sort_request
            .lock()
            .ok()
            .and_then(|mut r| r.take())
        {
            self.app_list.set_sort(mode);
        }
        if let Some(enabled) = self
            .shared
            .startup_request
            .lock()
            .ok()
            .and_then(|mut r| r.take())
        {
            #[cfg(windows)]
            match crate::startup::set_run_on_startup(enabled) {
                Ok(()) => {
                    let mut store = self.shared.store.lock().unwrap_or_else(|e| e.into_inner());
                    store.config.run_on_startup = enabled;
                    store.save();
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("Run on startup failed: {e}")),
            }
            #[cfg(not(windows))]
            {
                let _ = enabled;
                self.error = Some("Run on startup is only available on Windows.".to_string());
            }
        }

        // --- selection --------------------------------------------------
        // Refresh the Run key once at startup if it went missing
        // (the exe may have moved since it was set).
        #[cfg(windows)]
        if !self.startup_checked {
            self.startup_checked = true;
            let run_on_startup = self
                .shared
                .store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .config
                .run_on_startup;
            if run_on_startup && !crate::startup::is_run_on_startup() {
                if let Err(e) = crate::startup::set_run_on_startup(true) {
                    self.error = Some(format!("Run on startup could not be refreshed: {e}"));
                }
            }
        }
        if self.selected_luid.is_none() || self.selected_reading(&batch).is_none() {
            self.pick_default(&batch);
        }

        let capture_active = batch.capture_status.is_active();
        let total_all = batch.capture_total_all;
        // Fallback rates from a dedicated calculator over total-all.
        let elapsed = 0.1; // sampler ticks at 10 Hz.
        let fb = self
            .fallback_calc
            .update(total_all.down_bytes, total_all.up_bytes, elapsed);
        let all_rates = (fb.down_bps, fb.up_bps);

        // Auto-reselect (v0.4 fix for the sticky-selection bug): the
        // selected adapter has been idle for ~1 s while other
        // interfaces carry traffic — switch to the busiest carrying
        // interface instead of showing the fallback forever. This
        // runs BEFORE the top numbers are resolved, so the tick after
        // a switch already shows the new adapter.
        let mut sel = self.selected_reading(&batch).map(|s| TopInput {
            totals: IfTotals {
                down_bytes: s.info.in_octets,
                up_bytes: s.info.out_octets,
            },
            down_bps: s.down_bps,
            up_bps: s.up_bps,
        });
        let sel_idle = sel
            .map(|s| s.totals.down_bytes + s.totals.up_bytes == 0)
            .unwrap_or(true);
        let traffic_flows = total_all.down_bytes + total_all.up_bytes > 0;
        if capture_active && sel_idle && traffic_flows {
            self.idle_ticks += 1;
        } else {
            self.idle_ticks = 0;
        }
        if self.idle_ticks >= IDLE_RESELECT_TICKS {
            self.idle_ticks = 0;
            if let Some(best) = batch
                .adapters
                .iter()
                .filter(|a| a.info.in_octets + a.info.out_octets > 0)
                .max_by_key(|a| a.info.in_octets + a.info.out_octets)
            {
                let name = best.info.name.clone();
                let luid = best.info.luid;
                self.select(luid, &batch);
                self.selection_note = Some(format!(
                    "Auto-switched to {name} — the previous adapter showed no traffic."
                ));
                sel = self.selected_reading(&batch).map(|s| TopInput {
                    totals: IfTotals {
                        down_bytes: s.info.in_octets,
                        up_bytes: s.info.out_octets,
                    },
                    down_bps: s.down_bps,
                    up_bps: s.up_bps,
                });
            }
        }

        let sel_name = self.selected_reading(&batch).map(|s| s.info.name.clone());
        let top = resolve_top(
            sel,
            sel_name.as_deref(),
            total_all,
            all_rates,
            capture_active,
        );

        // --- session / today / peak from the RESOLVED top totals ------
        let now_totals = IfTotals {
            down_bytes: top.down_total,
            up_bytes: top.up_total,
        };
        // A source change re-baselines: the two totals are different
        // counters, so the delta between them is meaningless.
        let (down_delta, up_delta) = if top.source != self.last_source {
            self.last_source = top.source;
            self.last_top = now_totals;
            (0, 0)
        } else {
            let d = (
                now_totals
                    .down_bytes
                    .saturating_sub(self.last_top.down_bytes),
                now_totals.up_bytes.saturating_sub(self.last_top.up_bytes),
            );
            self.last_top = now_totals;
            d
        };
        self.session_down += down_delta;
        self.session_up += up_delta;
        {
            let mut store = self.shared.store.lock().unwrap_or_else(|e| e.into_inner());
            store.today.down_bytes += down_delta;
            store.today.up_bytes += up_delta;
        }
        let down_bps = self.down_smoother.update(top.down_bps);
        let up_bps = self.up_smoother.update(top.up_bps);
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

        // --- per-app list (stable order) --------------------------------
        let blocked: HashSet<String> = self
            .shared
            .blocked
            .read()
            .map(|s| s.clone())
            .unwrap_or_default();
        let mut app_readings: Vec<AppReading> = Vec::with_capacity(batch.apps.len());
        for app in &batch.apps {
            let (down_s, up_s) = self
                .app_smoothers
                .entry(app.exe_path.clone())
                .or_insert_with(|| (RateSmoother::new(), RateSmoother::new()));
            let d = down_s.update(app.down_bps);
            let u = up_s.update(app.up_bps);
            let mut a = app.clone();
            a.down_bps = d;
            a.up_bps = u;
            app_readings.push(a);
        }
        // Blocked apps with no traffic stay in the list (so they can
        // be unblocked) — as zero rows.
        {
            let store = self.shared.store.lock().unwrap_or_else(|e| e.into_inner());
            for exe in &store.config.blocked_exes {
                if !app_readings
                    .iter()
                    .any(|a| a.exe_path.eq_ignore_ascii_case(exe))
                {
                    app_readings.push(AppReading {
                        exe_path: exe.clone(),
                        display_name: crate::adapters::display_name_for(exe),
                        ..Default::default()
                    });
                }
            }
        }
        self.app_list.update(&app_readings);
        let app_rows = self.app_list.rows(&blocked);

        // --- build the snapshot -----------------------------------------
        let sel_reading = self.selected_reading(&batch);
        let (today_down, today_up, minimal, run_on_startup) = {
            let store = self.shared.store.lock().unwrap_or_else(|e| e.into_inner());
            (
                store.today.down_bytes,
                store.today.up_bytes,
                store.config.minimal,
                store.config.run_on_startup,
            )
        };
        let mut capture_totals: Vec<IfTotalJson> = batch
            .capture_totals
            .iter()
            .map(|(idx, t)| IfTotalJson {
                index: *idx,
                down: t.down_bytes,
                up: t.up_bytes,
            })
            .collect();
        capture_totals.sort_by_key(|t| t.index);
        let adapters_json: Vec<AdapterJson> = batch
            .adapters
            .iter()
            .map(|a| AdapterJson {
                luid: a.info.luid,
                index: a.info.index,
                name: a.info.name.clone(),
                type_name: a.info.type_name.clone(),
                status: a.info.status_text.clone(),
                ip: a.info.ipv4.clone(),
                down_bps: a.down_bps,
                up_bps: a.up_bps,
                down: format_rate(a.down_bps),
                up: format_rate(a.up_bps),
                total: format_bytes(a.info.in_octets + a.info.out_octets),
                unknown: a.info.name.starts_with("Unknown interface"),
                selected: Some(a.info.luid) == self.selected_luid,
            })
            .collect();
        let apps_json: Vec<AppJson> = app_rows
            .iter()
            .map(|a| {
                let is_blocked = blocked.contains(&a.exe_path.to_lowercase());
                AppJson {
                    exe_path: a.exe_path.clone(),
                    name: a.display_name.clone(),
                    down_bps: a.down_bps,
                    up_bps: a.up_bps,
                    down: format_rate(a.down_bps),
                    up: format_rate(a.up_bps),
                    down_total: a.down_total,
                    up_total: a.up_total,
                    down_total_fmt: format_bytes(a.down_total),
                    up_total_fmt: format_bytes(a.up_total),
                    blocked: is_blocked,
                    block_state: block_state(is_blocked, a.down_bps, a.up_bps),
                }
            })
            .collect();
        let snap = StateJson {
            version: VERSION.to_string(),
            capture_label: batch.capture_status.label(),
            capture_active,
            can_block: batch.capture_status.can_block(),
            top: TopJson {
                down_bps,
                up_bps,
                down_total: top.down_total,
                up_total: top.up_total,
                down: format_rate(down_bps),
                up: format_rate(up_bps),
                source: top.source.label().to_string(),
                note: top.note.clone().or_else(|| self.selection_note.clone()),
            },
            session: TotalsJson {
                down: self.session_down,
                up: self.session_up,
            },
            today: TotalsJson {
                down: today_down,
                up: today_up,
            },
            peak: RatesJson {
                down: self.peak_down,
                up: self.peak_up,
            },
            peak_down_fmt: format_rate(self.peak_down),
            peak_up_fmt: format_rate(self.peak_up),
            history_down: self.history.iter().map(|(d, _)| *d).collect(),
            history_up: self.history.iter().map(|(_, u)| *u).collect(),
            adapters: adapters_json,
            apps: apps_json,
            diagnostics: DiagnosticsJson {
                capture_totals,
                adapters_enumerated: batch.adapters.len(),
                packets: match batch.capture_status {
                    CaptureStatus::Active { packets, .. } => packets,
                    _ => 0,
                },
                top_source: top.source.label().to_string(),
                fallback_active: top.source == TopSource::AllInterfacesFallback,
                capture_label: batch.capture_status.label(),
            },
            selected_luid: self.selected_luid,
            selected_name: sel_reading
                .map(|s| s.info.name.clone())
                .unwrap_or_else(|| "— (waiting for adapters…)".to_string()),
            selected_ip: sel_reading
                .and_then(|s| s.info.ipv4.clone())
                .unwrap_or_else(|| "—".to_string()),
            minimal,
            run_on_startup,
            app_view: self.app_list.view(),
            sort_mode: self.app_list.sort(),
            selection_note: self.selection_note.clone(),
            error: self.error.clone(),
        };
        if let Ok(mut slot) = self.shared.snapshot.write() {
            *slot = snap;
        }
        if self.last_save.elapsed() > Duration::from_secs(5) {
            self.shared
                .store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .save();
            self.last_save = Instant::now();
        }
        #[cfg(not(windows))]
        {
            if self.error.is_none() {
                self.error = Some(
                    "Non-Windows build: adapter enumeration is a stub, so no live \
                     data is shown. Build for Windows (x86_64-pc-windows-msvc) \
                     for real measurements."
                        .to_string(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AppReading;

    fn totals(down: u64, up: u64) -> IfTotals {
        IfTotals {
            down_bytes: down,
            up_bytes: up,
        }
    }

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

    fn adapter_info(luid: u64, index: u32, name: &str) -> AdapterInfo {
        AdapterInfo {
            luid,
            index,
            name: name.to_string(),
            ..Default::default()
        }
    }

    // --- resolve_top: the v0.3 top-0 bug --------------------------------

    #[test]
    fn top_uses_selected_adapter_when_it_carries_traffic() {
        let top = resolve_top(
            Some(TopInput {
                totals: totals(1_000_000, 500_000),
                down_bps: 10_000_000.0,
                up_bps: 5_000_000.0,
            }),
            Some("PdaNet"),
            totals(1_000_000, 500_000),
            (10_000_000.0, 5_000_000.0),
            true,
        );
        assert_eq!(top.source, TopSource::Selected);
        assert_eq!(top.down_total, 1_000_000);
        assert!(top.note.is_none());
    }

    #[test]
    fn top_falls_back_to_all_interfaces_when_selected_is_dry() {
        // THE v0.3 bug, reproduced as a unit test: selected adapter
        // (stale LUID / wrong index) shows 0 while packets flow.
        let top = resolve_top(
            Some(TopInput {
                totals: totals(0, 0),
                down_bps: 0.0,
                up_bps: 0.0,
            }),
            Some("PdaNet Broadband Connection"),
            totals(100_000_000, 2_000_000),
            (9_500_000.0, 180_000.0),
            true,
        );
        assert_eq!(top.source, TopSource::AllInterfacesFallback);
        assert_eq!(top.down_total, 100_000_000);
        assert_eq!(top.up_total, 2_000_000);
        assert!((top.down_bps - 9_500_000.0).abs() < 1.0);
        let note = top.note.unwrap();
        assert!(note.contains("PdaNet Broadband Connection"), "{note}");
        assert!(note.contains("all interfaces"), "{note}");
    }

    #[test]
    fn top_falls_back_when_nothing_selected_but_traffic_flows() {
        let top = resolve_top(
            None,
            None,
            totals(50_000, 1_000),
            (500_000.0, 10_000.0),
            true,
        );
        assert_eq!(top.source, TopSource::AllInterfacesFallback);
        assert_eq!(top.down_total, 50_000);
    }

    #[test]
    fn top_stays_zero_only_when_nothing_flows_anywhere() {
        let top = resolve_top(
            Some(TopInput {
                totals: totals(0, 0),
                down_bps: 0.0,
                up_bps: 0.0,
            }),
            Some("Ethernet"),
            totals(0, 0),
            (0.0, 0.0),
            true,
        );
        assert_eq!(top.source, TopSource::Selected);
        assert_eq!(top.down_total, 0);
        assert!(top.note.is_none());
    }

    #[test]
    fn top_in_counter_mode_never_uses_the_fallback() {
        // No capture totals exist in counter mode; falling back to a
        // sum of frozen counters would be meaningless.
        let top = resolve_top(
            Some(TopInput {
                totals: totals(0, 0),
                down_bps: 0.0,
                up_bps: 0.0,
            }),
            Some("PdaNet"),
            totals(0, 0),
            (0.0, 0.0),
            false,
        );
        assert_eq!(top.source, TopSource::CounterFallback);
    }

    // --- merge_capture_readings: the "Unknown interface" row -----------

    #[test]
    fn unknown_interface_row_is_created_for_unmatched_traffic() {
        let adapters = vec![adapter_info(11, 11, "Ethernet")];
        let mut map = HashMap::new();
        map.insert(11u32, totals(1_000, 500)); // Ethernet: matched
        map.insert(25u32, totals(100_000, 2_000)); // PdaNet index, not enumerated
        let merged = merge_capture_readings(&adapters, &map);
        assert_eq!(merged.len(), 2);
        assert!(!merged[0].unknown);
        assert_eq!(merged[0].totals, totals(1_000, 500));
        assert!(merged[1].unknown);
        assert_eq!(merged[1].info.name, "Unknown interface 25");
        assert_eq!(merged[1].info.index, 25);
        assert_eq!(merged[1].totals, totals(100_000, 2_000));
        // Synthetic LUIDs are stable and distinct from real ones.
        assert_eq!(merged[1].info.luid, synthetic_luid(25));
        assert_ne!(merged[1].info.luid, 11);
    }

    #[test]
    fn no_unknown_row_for_zero_traffic_or_matched_interfaces() {
        let adapters = vec![adapter_info(11, 11, "Ethernet")];
        let mut map = HashMap::new();
        map.insert(11u32, totals(1_000, 500));
        map.insert(99u32, totals(0, 0)); // never seen a packet: no row
        let merged = merge_capture_readings(&adapters, &map);
        assert_eq!(merged.len(), 1);
        // Empty capture map: one row per adapter, zero totals.
        let merged = merge_capture_readings(&adapters, &HashMap::new());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].totals, totals(0, 0));
    }

    #[test]
    fn synthetic_luid_prefix_is_stable() {
        assert_eq!(synthetic_luid(25), 0x5744_4956_0000_0019u64);
        assert_ne!(synthetic_luid(25), synthetic_luid(26));
    }

    // --- sorting (moved from the v0.3 egui UI, behaviour unchanged) ----

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

    // --- AppListState: stable order, sort only on demand ---------------

    #[test]
    fn app_list_keeps_order_across_updates() {
        let mut list = AppListState::new(SortMode::Rate, AppView::Rate);
        list.update(&[
            app("a.exe", 100.0, 0.0, 1_000, 0),
            app("b.exe", 200.0, 0.0, 2_000, 0),
            app("c.exe", 50.0, 0.0, 500, 0),
        ]);
        // Rates change drastically — order must NOT move on update.
        list.update(&[
            app("a.exe", 999_999.0, 0.0, 1_001_000, 0),
            app("b.exe", 1.0, 0.0, 2_001, 0),
            app("c.exe", 5.0, 0.0, 505, 0),
        ]);
        let names: Vec<_> = list
            .rows(&HashSet::new())
            .iter()
            .map(|r| r.display_name.clone())
            .collect();
        assert_eq!(names, vec!["a.exe", "b.exe", "c.exe"]);
        // New app appends at the end; gone app drops out.
        list.update(&[
            app("a.exe", 1.0, 0.0, 1_001_001, 0),
            app("c.exe", 1.0, 0.0, 506, 0),
            app("d.exe", 1.0, 0.0, 10, 0),
        ]);
        let names: Vec<_> = list
            .rows(&HashSet::new())
            .iter()
            .map(|r| r.display_name.clone())
            .collect();
        assert_eq!(names, vec!["a.exe", "c.exe", "d.exe"]);
    }

    #[test]
    fn app_list_resorts_only_when_asked() {
        let mut list = AppListState::new(SortMode::Rate, AppView::Rate);
        list.update(&[
            app("slow.exe", 1_000.0, 0.0, 999_999, 0),
            app("fast.exe", 500_000.0, 500_000.0, 1, 0),
        ]);
        // Insertion order kept until a sort is requested.
        let names: Vec<_> = list
            .rows(&HashSet::new())
            .iter()
            .map(|r| r.display_name.clone())
            .collect();
        assert_eq!(names, vec!["slow.exe", "fast.exe"]);
        list.set_sort(SortMode::Rate);
        let names: Vec<_> = list
            .rows(&HashSet::new())
            .iter()
            .map(|r| r.display_name.clone())
            .collect();
        assert_eq!(names, vec!["fast.exe", "slow.exe"]);
    }

    #[test]
    fn app_view_toggle_resorts_by_the_new_metric() {
        let mut list = AppListState::new(SortMode::Rate, AppView::Rate);
        list.update(&[
            // a: fast now, tiny total. b: slow now, huge total.
            app("a.exe", 999_999.0, 0.0, 100, 0),
            app("b.exe", 1.0, 0.0, 50_000, 40_000),
        ]);
        list.set_sort(SortMode::Rate);
        let names: Vec<_> = list
            .rows(&HashSet::new())
            .iter()
            .map(|r| r.display_name.clone())
            .collect();
        assert_eq!(names, vec!["a.exe", "b.exe"]);
        // Flip to Total view: b (90_000 total) must come first now.
        list.set_view(AppView::Total);
        assert_eq!(list.view(), AppView::Total);
        let names: Vec<_> = list
            .rows(&HashSet::new())
            .iter()
            .map(|r| r.display_name.clone())
            .collect();
        assert_eq!(names, vec!["b.exe", "a.exe"]);
        // Flip back to Rate: a first again.
        list.set_view(AppView::Rate);
        let names: Vec<_> = list
            .rows(&HashSet::new())
            .iter()
            .map(|r| r.display_name.clone())
            .collect();
        assert_eq!(names, vec!["a.exe", "b.exe"]);
    }

    #[test]
    fn blocked_apps_are_pinned_to_the_top() {
        let mut list = AppListState::new(SortMode::Rate, AppView::Rate);
        list.update(&[
            app("chrome.exe", 500_000.0, 0.0, 1_000_000, 0),
            app("game.exe", 1.0, 0.0, 10, 0),
            app("mail.exe", 300_000.0, 0.0, 500_000, 0),
        ]);
        let mut blocked = HashSet::new();
        blocked.insert(r"c:\apps\game.exe".to_string());
        let names: Vec<_> = list
            .rows(&blocked)
            .iter()
            .map(|r| r.display_name.clone())
            .collect();
        assert_eq!(names[0], "game.exe");
        assert_eq!(names, vec!["game.exe", "chrome.exe", "mail.exe"]);
    }

    // --- block_state ------------------------------------------------------

    #[test]
    fn blocked_app_still_moving_is_new_connections_only() {
        assert_eq!(block_state(false, 1_000.0, 0.0), BlockState::NotBlocked);
        assert_eq!(block_state(true, 0.0, 0.0), BlockState::Blocked);
        // Still moving despite being blocked: pre-existing flow.
        assert_eq!(
            block_state(true, 1_000.0, 0.0),
            BlockState::BlockedNewConnectionsOnly
        );
        assert_eq!(
            block_state(true, 0.0, 5.0),
            BlockState::BlockedNewConnectionsOnly
        );
    }
}
