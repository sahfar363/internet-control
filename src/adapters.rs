//! Adapter enumeration + default-route detection + 100 ms sampler.
//!
//! Windows implementation uses IP Helper (`GetIfTable2` /
//! `GetAdaptersAddresses` / `GetIpForwardTable2`) via the `windows` crate
//! and reads the 64-bit `InOctets` / `OutOctets` counters. No adapter is
//! filtered out: virtual adapters (PDAnet, VPN/TAP) are listed alongside
//! WiFi and Ethernet, because on a tethered setup the virtual adapter is
//! the one carrying the internet.
//!
//! Non-Windows builds provide stubs so `cargo test` runs on Linux.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

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

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct SampleBatch {
    pub adapters: Vec<AdapterReading>,
    pub at: Option<Instant>,
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

/// Background sampler: polls every 100 ms, computes per-adapter rates,
/// and sends a batch to the UI. Runs until the receiver is dropped.
pub fn sample_loop(tx: Sender<SampleBatch>) {
    let mut calculators: HashMap<u64, RateCalculator> = HashMap::new();
    let mut last = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let now = Instant::now();
        let elapsed = now.duration_since(last).as_secs_f64();
        last = now;
        let adapters = enumerate_adapters();
        let mut readings = Vec::with_capacity(adapters.len());
        for info in adapters {
            let calc = calculators.entry(info.luid).or_default();
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
        let live: std::collections::HashSet<u64> = readings.iter().map(|r| r.info.luid).collect();
        calculators.retain(|luid, _| live.contains(luid));
        if tx
            .send(SampleBatch {
                adapters: readings,
                at: Some(now),
            })
            .is_err()
        {
            break; // UI gone.
        }
    }
}
