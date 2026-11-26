//! Adapter enumeration + default-route detection + 100 ms sampler.
//!
//! Windows implementation uses IP Helper (`GetIfTable2` /
//! `GetAdaptersAddresses` / `GetIpForwardTable2`) via the `windows` crate
//! and reads the 64-bit `InOctets` / `OutOctets` counters. No adapter is
//! filtered out: virtual adapters (PDAnet, VPN/TAP) are listed alongside
//! WiFi and Ethernet, because on a tethered setup the virtual adapter is
//! the one carrying the internet.
//!
//! v0.2 measurement: the counters are only a fallback. The sampler first
//! tries WinDivert packet capture (`crate::capture`); on Sahand's PC the
//! PDAnet PPP adapter's counters never move, so counting packets is the
//! only way to see the traffic. Capture totals are cumulative per
//! interface index, so the same `RateCalculator` math turns them into
//! rates — and the `Total` column shows captured bytes instead of the
//! frozen 0. If an interface index has no matching adapter, its traffic
//! is shown as an "Unknown interface N" row so the top numbers never
//! read 0 while packets are flowing.
//!
//! Non-Windows builds provide stubs so `cargo test` runs on Linux.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::capture::{CaptureSnapshot, CaptureStatus};
use crate::meter::RateCalculator;

#[allow(dead_code)] // fields are read by the UI on Windows; stubs on Linux
#[derive(Debug, Clone, Default)]
pub struct AdapterInfo {
    pub luid: u64,
    pub index: u32,
    pub name: String,
    pub description: String,
    pub if_type: u32,
    pub type_name: String,
    pub oper_status: u32,
    pub status_text: String,
    pub ipv4: Option<String>,
    /// Total bytes received by this adapter (64-bit counter).
    pub in_octets: u64,
    /// Total bytes sent by this adapter (64-bit counter).
    pub out_octets: u64,
}

#[derive(Debug, Clone, Default)]
pub struct AdapterReading {
    pub info: AdapterInfo,
    pub down_bps: f64,
    pub up_bps: f64,
    pub down_delta: u64,
    pub up_delta: u64,
}

/// One app's traffic in the current session (v0.3). Rates are raw
/// per-100 ms samples here; the UI smooths them for display. Totals
/// are cumulative and exact.
#[derive(Debug, Clone, Default)]
pub struct AppReading {
    /// Full exe path, e.g. `C:\Program Files\...\chrome.exe`. This is
    /// the identity used for blocking (and for grouping connections).
    pub exe_path: String,
    /// File name only, for the list (e.g. `chrome.exe`).
    pub display_name: String,
    pub down_bps: f64,
    pub up_bps: f64,
    /// Bytes in this 100 ms tick (kept for symmetry with
    /// AdapterReading; the UI shows rates + cumulative totals).
    #[allow(dead_code)]
    pub down_delta: u64,
    #[allow(dead_code)]
    pub up_delta: u64,
    pub down_total: u64,
    pub up_total: u64,
}

/// Display name for an exe path: the file name after the last `\` or
/// `/`; the full path if there is no separator. Pure, tested via the
/// sampler tests in spirit — see `app.rs` sort tests for coverage of
/// the list built from these readings.
pub fn display_name_for(exe_path: &str) -> String {
    let name = exe_path.rsplit(['\\', '/']).next().unwrap_or(exe_path);
    if name.is_empty() {
        exe_path.to_string()
    } else {
        name.to_string()
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct SampleBatch {
    pub adapters: Vec<AdapterReading>,
    pub at: Option<Instant>,
    /// How these numbers were measured (WinDivert capture, or the v0.1
    /// counter fallback with the reason capture is unavailable).
    pub capture_status: CaptureStatus,
    /// Per-app traffic this session (empty in counter-fallback mode,
    /// where attribution is unavailable).
    pub apps: Vec<AppReading>,
}

#[allow(dead_code)] // used by the Windows enumeration path
pub fn type_name(if_type: u32) -> String {
    match if_type {
        1 => "Other".to_string(),
        6 => "Ethernet".to_string(),
        9 => "Token Ring".to_string(),
        15 => "FDDI".to_string(),
        23 => "PPP".to_string(),
        24 => "Loopback".to_string(),
        28 => "SLIP".to_string(),
        37 => "ATM".to_string(),
        71 => "Wi-Fi".to_string(),
        131 => "Tunnel".to_string(),
        144 => "IEEE 1394".to_string(),
        _ => format!("Type {if_type}"),
    }
}

#[allow(dead_code)] // used by the Windows enumeration path
pub fn status_text(oper_status: u32) -> String {
    match oper_status {
        1 => "Up".to_string(),
        2 => "Down".to_string(),
        3 => "Testing".to_string(),
        4 => "Unknown".to_string(),
        5 => "Dormant".to_string(),
        6 => "Not present".to_string(),
        7 => "Lower layer down".to_string(),
        _ => format!("Status {oper_status}"),
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::*;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetAdaptersAddresses, GetIfTable2, GetIpForwardTable2,
        GET_ADAPTERS_ADDRESSES_FLAGS, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};

    fn u16_slice_to_string(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    fn pwstr_to_string(p: PWSTR) -> String {
        if p.is_null() {
            return String::new();
        }
        unsafe {
            let mut len = 0usize;
            while *p.0.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(p.0, len);
            String::from_utf16_lossy(slice)
        }
    }

    /// Metadata from GetAdaptersAddresses, keyed by LUID value.
    struct Meta {
        friendly: String,
        description: String,
        ipv4: Option<String>,
        if_type: u32,
        oper_status: u32,
    }

    fn adapters_addresses_meta() -> HashMap<u64, Meta> {
        let mut out = HashMap::new();
        unsafe {
            let mut size: u32 = 0;
            // First call returns the required size.
            let _ = GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                GET_ADAPTERS_ADDRESSES_FLAGS(0),
                None,
                None,
                &mut size as *mut u32,
            );
            if size == 0 {
                return out;
            }
            let mut buf = vec![0u8; size as usize];
            let addrs = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
            let rc = GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                GET_ADAPTERS_ADDRESSES_FLAGS(0),
                None,
                Some(addrs),
                &mut size as *mut u32,
            );
            if rc != ERROR_SUCCESS.0 {
                return out;
            }
            let mut cur = addrs;
            while !cur.is_null() {
                let a = &*cur;
                let luid = a.Luid.Value;
                // First IPv4 unicast address, if any.
                let mut ipv4: Option<String> = None;
                let mut ua = a.FirstUnicastAddress;
                while !ua.is_null() {
                    let u = &*ua;
                    let sa = u.Address.lpSockaddr;
                    if !sa.is_null() && (*sa).sa_family == AF_INET {
                        let sin = &*(sa as *const SOCKADDR_IN);
                        let b = sin.sin_addr.S_un.S_un_b;
                        ipv4 = Some(format!("{}.{}.{}.{}", b.s_b1, b.s_b2, b.s_b3, b.s_b4));
                        break;
                    }
                    ua = u.Next;
                }
                out.insert(
                    luid,
                    Meta {
                        friendly: pwstr_to_string(a.FriendlyName),
                        description: pwstr_to_string(a.Description),
                        ipv4,
                        if_type: a.IfType,
                        oper_status: a.OperStatus.0 as u32,
                    },
                );
                cur = a.Next;
            }
        }
        out
    }

    pub fn enumerate_adapters() -> Vec<AdapterInfo> {
        let meta = adapters_addresses_meta();
        let mut out: Vec<AdapterInfo> = Vec::new();
        unsafe {
            let mut table: *mut windows::Win32::NetworkManagement::IpHelper::MIB_IF_TABLE2 =
                std::ptr::null_mut();
            let rc = GetIfTable2(&mut table as *mut _);
            if rc != ERROR_SUCCESS || table.is_null() {
                // Fallback: metadata only, counters unknown (0).
                for (luid, m) in meta {
                    out.push(AdapterInfo {
                        luid,
                        name: m.friendly.clone(),
                        description: m.description,
                        if_type: m.if_type,
                        type_name: type_name(m.if_type),
                        oper_status: m.oper_status,
                        status_text: status_text(m.oper_status),
                        ipv4: m.ipv4,
                        ..Default::default()
                    });
                }
                return out;
            }
            let num = (*table).NumEntries as usize;
            let rows = (*table).Table.as_ptr();
            for i in 0..num {
                let row = &*rows.add(i);
                let luid = row.InterfaceLuid.Value;
                let m = meta.get(&luid);
                let name = m
                    .map(|m| m.friendly.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| u16_slice_to_string(&row.Alias));
                let description = m
                    .map(|m| m.description.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| u16_slice_to_string(&row.Description));
                let if_type = row.Type;
                let oper = row.OperStatus.0 as u32;
                out.push(AdapterInfo {
                    luid,
                    index: row.InterfaceIndex,
                    name,
                    description,
                    if_type,
                    type_name: type_name(if_type),
                    oper_status: oper,
                    status_text: status_text(oper),
                    ipv4: m.and_then(|m| m.ipv4.clone()),
                    in_octets: row.InOctets,
                    out_octets: row.OutOctets,
                });
            }
            FreeMibTable(table as *const std::ffi::c_void);
        }
        // Operational adapters first, then by name, so the live ones are
        // at the top of the list.
        out.sort_by(|a, b| {
            (b.oper_status == 1)
                .cmp(&(a.oper_status == 1))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        out
    }

    pub fn default_route_luid() -> Option<u64> {
        unsafe {
            let mut table: *mut windows::Win32::NetworkManagement::IpHelper::MIB_IPFORWARD_TABLE2 =
                std::ptr::null_mut();
            let rc = GetIpForwardTable2(AF_INET, &mut table as *mut _);
            if rc != ERROR_SUCCESS || table.is_null() {
                return None;
            }
            let num = (*table).NumEntries as usize;
            let rows = (*table).Table.as_ptr();
            let mut best: Option<(u32, u64)> = None; // (metric, luid)
            for i in 0..num {
                let row = &*rows.add(i);
                // Default route: prefix length 0.
                if row.DestinationPrefix.PrefixLength != 0 {
                    continue;
                }
                let metric = row.Metric;
                let luid = row.InterfaceLuid.Value;
                if best.map(|(m, _)| metric < m).unwrap_or(true) {
                    best = Some((metric, luid));
                }
            }
            FreeMibTable(table as *const std::ffi::c_void);
            best.map(|(_, luid)| luid)
        }
    }
}

#[cfg(windows)]
pub use win::{default_route_luid, enumerate_adapters};

#[cfg(not(windows))]
pub fn enumerate_adapters() -> Vec<AdapterInfo> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn default_route_luid() -> Option<u64> {
    None
}

/// LUIDs for synthetic "Unknown interface N" rows. Real LUIDs never
/// carry this prefix (ASCII "WDIV" in the high bytes), so a captured
/// interface with no matching adapter can still be selected and shown.
fn synthetic_luid(if_index: u32) -> u64 {
    0x5744_4956_0000_0000u64 | if_index as u64
}

/// Background sampler: polls every 100 ms, computes per-adapter rates,
/// and sends a batch to the UI. Runs until the receiver is dropped.
///
/// Measurement source, in order:
/// 1. WinDivert packet capture (v0.2 default) — per-interface cumulative
///    byte totals from the capture thread, turned into rates with the
///    same `RateCalculator` as the counters. Session totals from these
///    deltas are exact; live rates can jitter by one tick because a
///    snapshot may land between ticks.
/// 2. Adapter counters (v0.1 fallback) — used only when WinDivert could
///    not be opened, e.g. not run as administrator or driver files
///    missing. The reason is carried in every batch for the UI.
pub fn sample_loop(
    tx: Sender<SampleBatch>,
    blocked: Arc<RwLock<std::collections::HashSet<String>>>,
) {
    let capture = crate::capture::try_start(blocked);
    let mut capture_rx: Option<Receiver<CaptureSnapshot>> = capture.rx;
    let mut capture_status = capture.status;
    let mut last_snapshot: Option<CaptureSnapshot> = None;
    // Rate calculators keyed by interface index (capture mode).
    let mut capture_calcs: HashMap<u32, RateCalculator> = HashMap::new();
    // Rate calculators keyed by exe path (per-app, capture mode).
    let mut app_calcs: HashMap<String, RateCalculator> = HashMap::new();
    // Rate calculators keyed by LUID (counter fallback mode).
    let mut counter_calcs: HashMap<u64, RateCalculator> = HashMap::new();
    let mut last = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let now = Instant::now();
        let elapsed = now.duration_since(last).as_secs_f64();
        last = now;

        // Drain capture snapshots, keep the latest.
        if let Some(rx) = &capture_rx {
            while let Ok(snap) = rx.try_recv() {
                last_snapshot = Some(snap);
            }
            if let Err(std::sync::mpsc::TryRecvError::Disconnected) = rx.try_recv() {
                capture_status = CaptureStatus::Unavailable(
                    "WinDivert capture stopped unexpectedly. \
                     Showing adapter counters (fallback)."
                        .to_string(),
                );
                capture_rx = None;
            }
        }
        if let Some(snap) = &last_snapshot {
            if let CaptureStatus::Active { intercept, .. } = capture_status {
                capture_status = CaptureStatus::Active {
                    packets: snap.packets,
                    intercept,
                };
            }
        }

        let adapters = enumerate_adapters();
        let mut readings = Vec::with_capacity(adapters.len());
        // Per-app readings for this tick (capture mode only; the
        // counter fallback has no attribution, so the list is empty).
        let mut app_readings: Vec<AppReading> = Vec::new();
        if capture_status.is_active() {
            let snap = last_snapshot.clone().unwrap_or_default();
            // Per-app rates from the cumulative per-app totals, with
            // the same RateCalculator math as the adapters. Not
            // filtered to any one interface: WinDivert sees WiFi,
            // Ethernet, PDAnet and VPN traffic alike, so the list
            // covers every app using the internet, on any connection.
            for (exe, totals) in &snap.apps {
                let calc = app_calcs.entry(exe.clone()).or_default();
                let s = calc.update(totals.down_bytes, totals.up_bytes, elapsed);
                app_readings.push(AppReading {
                    exe_path: exe.clone(),
                    display_name: display_name_for(exe),
                    down_bps: s.down_bps,
                    up_bps: s.up_bps,
                    down_delta: s.down_delta,
                    up_delta: s.up_delta,
                    down_total: totals.down_bytes,
                    up_total: totals.up_bytes,
                });
            }
            let live_apps: std::collections::HashSet<String> = snap.apps.keys().cloned().collect();
            app_calcs.retain(|exe, _| live_apps.contains(exe));
            for info in &adapters {
                let totals = snap.totals.get(&info.index).copied().unwrap_or_default();
                let calc = capture_calcs.entry(info.index).or_default();
                let s = calc.update(totals.down_bytes, totals.up_bytes, elapsed);
                let mut info = info.clone();
                // Total column shows captured bytes, not the frozen
                // standard counters.
                info.in_octets = totals.down_bytes;
                info.out_octets = totals.up_bytes;
                readings.push(AdapterReading {
                    info,
                    down_bps: s.down_bps,
                    up_bps: s.up_bps,
                    down_delta: s.down_delta,
                    up_delta: s.up_delta,
                });
            }
            // Interfaces WinDivert sees but GetIfTable2 does not list:
            // show them as their own row instead of dropping the bytes.
            for (idx, totals) in &snap.totals {
                if totals.down_bytes == 0 && totals.up_bytes == 0 {
                    continue;
                }
                if adapters.iter().any(|a| a.index == *idx) {
                    continue;
                }
                let calc = capture_calcs.entry(*idx).or_default();
                let s = calc.update(totals.down_bytes, totals.up_bytes, elapsed);
                readings.push(AdapterReading {
                    info: AdapterInfo {
                        luid: synthetic_luid(*idx),
                        index: *idx,
                        name: format!("Unknown interface {idx}"),
                        type_name: "Unknown".to_string(),
                        oper_status: 1,
                        status_text: "Up".to_string(),
                        in_octets: totals.down_bytes,
                        out_octets: totals.up_bytes,
                        ..Default::default()
                    },
                    down_bps: s.down_bps,
                    up_bps: s.up_bps,
                    down_delta: s.down_delta,
                    up_delta: s.up_delta,
                });
            }
            // Drop calculators for interfaces that disappeared, so a
            // reappearing interface re-baselines instead of spiking.
            let live: std::collections::HashSet<u32> =
                readings.iter().map(|r| r.info.index).collect();
            capture_calcs.retain(|idx, _| live.contains(idx));
        } else {
            for info in adapters {
                let calc = counter_calcs.entry(info.luid).or_default();
                let s = calc.update(info.in_octets, info.out_octets, elapsed);
                readings.push(AdapterReading {
                    info,
                    down_bps: s.down_bps,
                    up_bps: s.up_bps,
                    down_delta: s.down_delta,
                    up_delta: s.up_delta,
                });
            }
            // Drop calculators for adapters that disappeared, so a reappearing
            // adapter re-baselines instead of producing a bogus delta.
            let live: std::collections::HashSet<u64> =
                readings.iter().map(|r| r.info.luid).collect();
            counter_calcs.retain(|luid, _| live.contains(luid));
        }
        if tx
            .send(SampleBatch {
                adapters: readings,
                at: Some(now),
                capture_status: capture_status.clone(),
                apps: app_readings,
            })
            .is_err()
        {
            break; // UI gone.
        }
    }
}
