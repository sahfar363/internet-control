//! Identifying the "Unknown (unattributed)" traffic (v0.8).
//!
//! Sahand asked (2026-11-26): "I want to be able to see the exact
//! location of each file that is using the internet. Also what is
//! unauthorized? it should be possible to find the app."
//!
//! Two things follow from that:
//!
//! 1. The per-app list shows only the exe *name*; the exact on-disk
//!    location is the full exe path the engine already captures.
//!    The UI (app.rs) now shows it on hover / on row click, with a
//!    Copy button, and the block note names the full path.
//! 2. The "Unknown (unattributed)" row aggregates traffic the meter
//!    could not match to a program. This module records what that
//!    traffic actually is, so it can be identified:
//!
//! * A bounded store (last ~50, LRU) of unattributed flow samples:
//!   protocol, local addr:port, remote addr:port, bytes, packets,
//!   first/last seen. Only unattributed traffic is stored — nothing
//!   for attributed traffic. The capture thread records samples;
//!   the UI shows them when the Unknown row is clicked.
//! * "Identify": at click time, each TCP/UDP sample is looked up in
//!   the *current* connection tables (GetExtendedTcpTable /
//!   GetExtendedUdpTable -> PID -> exe path, reusing
//!   `crate::connections`). A connection that started after the
//!   sample, or one that has since closed, may still show "no owner
//!   now" — the label says so honestly instead of guessing.
//! * What cannot be owned is labelled, not hidden: ICMP samples are
//!   "ICMP (ping/traceroute — no app owns this, the remote IP is
//!   shown)", and traffic from PID 4 is "System (Windows kernel)"
//!   — in fact PID 4 flows are attributed to a "System" row with
//!   path `C:\Windows\System32\ntoskrnl.exe` in the capture thread
//!   itself, instead of landing under Unknown at all (where the PID
//!   is known). That row cannot be blocked: dropping the kernel's
//!   packets would break networking, so the UI offers no Block
//!   button for it.
//!
//! Everything here is pure (std only, no WinDivert / IP Helper) and
//! unit-tested on Linux. Only the callers touch Windows APIs.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

use crate::capture::FlowKey;

/// The Windows kernel process. Its flows (SMB, etc.) are reported
/// by the FLOW layer and the connection tables with this PID.
pub const SYSTEM_PID: u32 = 4;
/// The on-disk location shown for the System row.
pub const SYSTEM_EXE: &str = r"C:\Windows\System32\ntoskrnl.exe";

/// Is this exe path the System (kernel) row?
pub fn is_system_exe(path: &str) -> bool {
    path.eq_ignore_ascii_case(SYSTEM_EXE)
}

/// Protocol numbers the meter can name.
pub fn protocol_name(protocol: u8) -> &'static str {
    match protocol {
        1 | 58 => "ICMP",
        6 => "TCP",
        17 => "UDP",
        _ => "Other",
    }
}

/// Is this an ICMP protocol (v4 = 1, v6 = 58)? ICMP has no ports
/// and no owning app — ping / traceroute traffic.
pub fn is_icmp(protocol: u8) -> bool {
    protocol == 1 || protocol == 58
}

/// One unattributed traffic sample: a single flow (TCP/UDP) or a
/// single remote peer (ICMP, which has no ports). Bytes and packets
/// accumulate across every packet that matched it.
#[derive(Debug, Clone)]
pub struct UnknownSample {
    pub protocol: u8,
    /// Local address (the PC's side).
    pub local: IpAddr,
    /// Local port (TCP/UDP only; None for ICMP).
    pub local_port: Option<u16>,
    /// Remote address.
    pub remote: IpAddr,
    /// Remote port (TCP/UDP only; None for ICMP).
    pub remote_port: Option<u16>,
    /// Total bytes seen for this sample.
    pub bytes: u64,
    /// Packets seen for this sample.
    pub packets: u64,
    /// When this sample was first seen (recorded for the spec's
    /// "first/last seen"; only last_seen is displayed in the UI).
    #[allow(dead_code)]
    pub first_seen: Instant,
    pub last_seen: Instant,
}

impl UnknownSample {
    /// Stable identity used by the UI to attach Identify results
    /// across snapshots, e.g. "TCP 10.0.0.5:52341 -> 93.184.216.34:443".
    pub fn key(&self) -> String {
        let proto = protocol_name(self.protocol);
        match (self.local_port, self.remote_port) {
            (Some(lp), Some(rp)) => {
                format!("{proto} {}:{lp} -> {}:{rp}", self.local, self.remote)
            }
            _ => format!("{proto} {} -> {}", self.local, self.remote),
        }
    }

    /// The 5-tuple this sample would have in the connection tables
    /// (TCP/UDP only). ICMP has no flow key.
    pub fn flow_key(&self) -> Option<FlowKey> {
        match (self.local_port, self.remote_port) {
            (Some(lp), Some(rp)) => {
                Some(FlowKey::new(self.protocol, self.local, lp, self.remote, rp))
            }
            _ => None,
        }
    }

    /// Identity fields, for matching a recorded packet to a sample.
    fn key_fields(&self) -> UnknownKey {
        UnknownKey {
            protocol: self.protocol,
            local: self.local,
            local_port: self.local_port,
            remote: self.remote,
            remote_port: self.remote_port,
        }
    }
}

/// Identity of one sample: protocol + both endpoints. Used to
/// match a recorded packet to the sample it accumulates into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnknownKey {
    protocol: u8,
    local: IpAddr,
    local_port: Option<u16>,
    remote: IpAddr,
    remote_port: Option<u16>,
}

/// How many unattributed samples to keep. Old samples are evicted
/// least-recently-seen first — the list shows the most recent
/// unknown traffic, never an unbounded log.
#[allow(dead_code)] // only the Windows capture thread builds the store; tests use it too
pub const MAX_UNKNOWN_SAMPLES: usize = 50;

/// Bounded store of unattributed traffic samples, keyed by
/// (protocol, local, local_port, remote, remote_port). The capture
/// thread records one sample per unattributed packet; the UI reads
/// a point-in-time copy through the snapshot.
#[allow(dead_code)] // only constructed by the Windows capture thread + tests
#[derive(Debug, Default)]
pub struct UnknownSampleStore {
    samples: Vec<UnknownSample>,
}

#[allow(dead_code)] // only called from the Windows capture thread + tests
impl UnknownSampleStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Record one unattributed TCP/UDP packet. Same 5-tuple as an
    /// existing sample accumulates into it; a new one is added
    /// (evicting the least-recently-seen sample when full).
    pub fn record_flow(&mut self, key: &FlowKey, len: u64, now: Instant) {
        self.record(
            UnknownKey {
                protocol: key.protocol,
                local: key.local,
                local_port: Some(key.local_port),
                remote: key.remote,
                remote_port: Some(key.remote_port),
            },
            len,
            now,
        );
    }

    /// Record one unattributed packet with no flow (ICMP): grouped
    /// by protocol + local + remote peer, no ports.
    pub fn record_packet(
        &mut self,
        protocol: u8,
        local: IpAddr,
        remote: IpAddr,
        len: u64,
        now: Instant,
    ) {
        self.record(
            UnknownKey {
                protocol,
                local,
                local_port: None,
                remote,
                remote_port: None,
            },
            len,
            now,
        );
    }

    fn record(&mut self, key: UnknownKey, len: u64, now: Instant) {
        if let Some(s) = self.samples.iter_mut().find(|s| s.key_fields() == key) {
            s.bytes += len;
            s.packets += 1;
            s.last_seen = now;
            return;
        }
        if self.samples.len() >= MAX_UNKNOWN_SAMPLES {
            // Evict the least-recently-seen sample (LRU).
            if let Some((idx, _)) = self
                .samples
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.last_seen)
            {
                self.samples.remove(idx);
            }
        }
        self.samples.push(UnknownSample {
            protocol: key.protocol,
            local: key.local,
            local_port: key.local_port,
            remote: key.remote,
            remote_port: key.remote_port,
            bytes: len,
            packets: 1,
            first_seen: now,
            last_seen: now,
        });
    }

    /// Point-in-time copy for the snapshot, biggest first — the
    /// heaviest unknown traffic is what Sahand most wants to find.
    pub fn snapshot(&self) -> Vec<UnknownSample> {
        let mut out = self.samples.clone();
        out.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| b.last_seen.cmp(&a.last_seen))
        });
        out
    }
}

/// What "Identify" found for one sample, at click time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentifyResult {
    /// ICMP: no app owns this by definition.
    Icmp,
    /// PID 4, the Windows kernel.
    System,
    /// Owned by a program — the full exe path.
    App(String),
    /// No owner in the current tables (connection closed since the
    /// sample, or started after it — the tables are a snapshot of
    /// *now*, the sample is from *then*).
    NoOwner,
}

impl IdentifyResult {
    /// The label shown next to the sample in the UI.
    pub fn label(&self) -> String {
        match self {
            IdentifyResult::Icmp => {
                "ICMP (ping/traceroute — no app owns this, the remote IP is shown)".to_string()
            }
            IdentifyResult::System => "System (Windows kernel)".to_string(),
            IdentifyResult::App(path) => path.clone(),
            IdentifyResult::NoOwner => "no owner found right now".to_string(),
        }
    }
}

/// Identify one sample against the current connection tables.
/// Pure — unit-tested below. `pid_exe` resolves a PID to its full
/// exe path (the caller passes `crate::capture::exe_for_pid`).
#[allow(dead_code)] // only called from the UI thread + tests
pub fn identify_sample(
    sample: &UnknownSample,
    tcp: &HashMap<FlowKey, u32>,
    udp: &HashMap<(IpAddr, u16), u32>,
    pid_exe: &dyn Fn(u32) -> Option<String>,
) -> IdentifyResult {
    if is_icmp(sample.protocol) {
        return IdentifyResult::Icmp;
    }
    let pid = match sample.flow_key() {
        Some(key) if sample.protocol == 6 => tcp.get(&key).copied(),
        Some(_) => udp
            .get(&(sample.local, sample.local_port.unwrap_or(0)))
            .copied(),
        None => None,
    };
    let Some(pid) = pid else {
        return IdentifyResult::NoOwner;
    };
    if pid == SYSTEM_PID {
        return IdentifyResult::System;
    }
    match pid_exe(pid) {
        Some(path) => IdentifyResult::App(path),
        None => IdentifyResult::NoOwner,
    }
}

/// "Last seen 3s ago" for the UI. Pure — unit-tested below.
pub fn format_ago(secs: u64) -> String {
    if secs < 2 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn now() -> Instant {
        Instant::now()
    }

    fn tcp_sample(local_port: u16, remote: [u8; 4], remote_port: u16) -> UnknownSample {
        UnknownSample {
            protocol: 6,
            local: IpAddr::from([10, 0, 0, 5]),
            local_port: Some(local_port),
            remote: IpAddr::from(remote),
            remote_port: Some(remote_port),
            bytes: 0,
            packets: 0,
            first_seen: now(),
            last_seen: now(),
        }
    }

    #[test]
    fn protocol_names_and_icmp() {
        assert_eq!(protocol_name(6), "TCP");
        assert_eq!(protocol_name(17), "UDP");
        assert_eq!(protocol_name(1), "ICMP");
        assert_eq!(protocol_name(58), "ICMP");
        assert_eq!(protocol_name(47), "Other");
        assert!(is_icmp(1));
        assert!(is_icmp(58));
        assert!(!is_icmp(6));
        assert!(!is_icmp(17));
    }

    #[test]
    fn sample_key_and_flow_key() {
        let s = tcp_sample(52341, [93, 184, 216, 34], 443);
        assert_eq!(s.key(), "TCP 10.0.0.5:52341 -> 93.184.216.34:443");
        let k = s.flow_key().unwrap();
        assert_eq!(k.protocol, 6);
        assert_eq!(k.local_port, 52341);
        assert_eq!(k.remote_port, 443);
        // ICMP has no ports and no flow key.
        let icmp = UnknownSample {
            protocol: 1,
            local: IpAddr::from([10, 0, 0, 5]),
            local_port: None,
            remote: IpAddr::from([8, 8, 8, 8]),
            remote_port: None,
            bytes: 100,
            packets: 1,
            first_seen: now(),
            last_seen: now(),
        };
        assert_eq!(icmp.key(), "ICMP 10.0.0.5 -> 8.8.8.8");
        assert!(icmp.flow_key().is_none());
    }

    #[test]
    fn store_aggregates_same_flow_and_bounds_to_fifty() {
        let mut store = UnknownSampleStore::new();
        let t = now();
        let key = FlowKey::new(
            6,
            IpAddr::from([10, 0, 0, 5]),
            52341,
            IpAddr::from([93, 184, 216, 34]),
            443,
        );
        store.record_flow(&key, 1500, t);
        store.record_flow(&key, 500, t + Duration::from_millis(10));
        assert_eq!(store.len(), 1);
        let snap = store.snapshot();
        assert_eq!(snap[0].bytes, 2000);
        assert_eq!(snap[0].packets, 2);
        // ICMP packets group by peer, not by port.
        store.record_packet(
            1,
            IpAddr::from([10, 0, 0, 5]),
            IpAddr::from([8, 8, 8, 8]),
            64,
            t,
        );
        store.record_packet(
            1,
            IpAddr::from([10, 0, 0, 5]),
            IpAddr::from([8, 8, 8, 8]),
            64,
            t,
        );
        assert_eq!(store.len(), 2);
        let snap = store.snapshot();
        let icmp = snap.iter().find(|s| s.protocol == 1).unwrap();
        assert_eq!(icmp.bytes, 128);
        assert_eq!(icmp.packets, 2);
        // 60 distinct flows: only the 50 most recent survive.
        let mut store = UnknownSampleStore::new();
        for i in 0..60u16 {
            let k = FlowKey::new(
                6,
                IpAddr::from([10, 0, 0, 5]),
                52000 + i,
                IpAddr::from([93, 184, 216, 34]),
                443,
            );
            store.record_flow(&k, 100, t + Duration::from_millis(i as u64));
        }
        assert_eq!(store.len(), MAX_UNKNOWN_SAMPLES);
        let snap = store.snapshot();
        // The oldest flows (ports 52000..52009) were evicted; the
        // newest (52059) is still there.
        assert!(snap.iter().all(|s| s.local_port.unwrap() >= 52010));
        assert!(snap.iter().any(|s| s.local_port == Some(52059)));
    }

    #[test]
    fn store_snapshot_is_biggest_first() {
        let mut store = UnknownSampleStore::new();
        let t = now();
        let a = FlowKey::new(
            6,
            IpAddr::from([10, 0, 0, 5]),
            1,
            IpAddr::from([1, 1, 1, 1]),
            80,
        );
        let b = FlowKey::new(
            6,
            IpAddr::from([10, 0, 0, 5]),
            2,
            IpAddr::from([2, 2, 2, 2]),
            80,
        );
        store.record_flow(&a, 100, t);
        store.record_flow(&b, 9000, t);
        let snap = store.snapshot();
        assert_eq!(snap[0].remote, IpAddr::from([2, 2, 2, 2]));
        assert_eq!(snap[1].remote, IpAddr::from([1, 1, 1, 1]));
    }

    #[test]
    fn identify_tcp_udp_icmp_system_and_no_owner() {
        let pid_exe = |pid: u32| match pid {
            1234 => Some(r"C:\Games\Game.exe".to_string()),
            4 => Some(SYSTEM_EXE.to_string()),
            _ => None,
        };
        let mut tcp = HashMap::new();
        let mut udp = HashMap::new();
        // TCP sample owned by the game.
        let game = tcp_sample(52341, [93, 184, 216, 34], 443);
        tcp.insert(game.flow_key().unwrap(), 1234);
        assert_eq!(
            identify_sample(&game, &tcp, &udp, &pid_exe),
            IdentifyResult::App(r"C:\Games\Game.exe".to_string())
        );
        // Same 5-tuple owned by PID 4 -> System, not the exe path.
        tcp.insert(game.flow_key().unwrap(), 4);
        assert_eq!(
            identify_sample(&game, &tcp, &udp, &pid_exe),
            IdentifyResult::System
        );
        // TCP sample with no table row -> no owner now.
        let gone = tcp_sample(59999, [9, 9, 9, 9], 443);
        assert_eq!(
            identify_sample(&gone, &tcp, &udp, &pid_exe),
            IdentifyResult::NoOwner
        );
        // UDP sample: looked up by local socket, whatever remote.
        let dns = UnknownSample {
            protocol: 17,
            local: IpAddr::from([10, 0, 0, 5]),
            local_port: Some(53000),
            remote: IpAddr::from([8, 8, 8, 8]),
            remote_port: Some(53),
            bytes: 100,
            packets: 1,
            first_seen: now(),
            last_seen: now(),
        };
        udp.insert((IpAddr::from([10, 0, 0, 5]), 53000), 1234);
        assert_eq!(
            identify_sample(&dns, &tcp, &udp, &pid_exe),
            IdentifyResult::App(r"C:\Games\Game.exe".to_string())
        );
        // ICMP is never owned, even if a table row matched.
        let icmp = UnknownSample {
            protocol: 1,
            local: IpAddr::from([10, 0, 0, 5]),
            local_port: None,
            remote: IpAddr::from([8, 8, 8, 8]),
            remote_port: None,
            bytes: 64,
            packets: 1,
            first_seen: now(),
            last_seen: now(),
        };
        assert_eq!(
            identify_sample(&icmp, &tcp, &udp, &pid_exe),
            IdentifyResult::Icmp
        );
        // A PID the resolver cannot name -> no owner (process gone).
        tcp.insert(gone.flow_key().unwrap(), 99999);
        assert_eq!(
            identify_sample(&gone, &tcp, &udp, &pid_exe),
            IdentifyResult::NoOwner
        );
    }

    #[test]
    fn identify_result_labels() {
        assert!(IdentifyResult::Icmp.label().contains("no app owns this"));
        assert_eq!(IdentifyResult::System.label(), "System (Windows kernel)");
        assert_eq!(
            IdentifyResult::App(r"C:\Browser\chrome.exe".to_string()).label(),
            r"C:\Browser\chrome.exe"
        );
        assert_eq!(IdentifyResult::NoOwner.label(), "no owner found right now");
    }

    #[test]
    fn system_exe_recognition() {
        assert!(is_system_exe(SYSTEM_EXE));
        assert!(is_system_exe(r"c:\windows\system32\ntoskrnl.exe"));
        assert!(!is_system_exe(r"C:\Windows\System32\svchost.exe"));
        assert_eq!(SYSTEM_PID, 4);
    }

    #[test]
    fn format_ago_units() {
        assert_eq!(format_ago(0), "just now");
        assert_eq!(format_ago(1), "just now");
        assert_eq!(format_ago(3), "3s ago");
        assert_eq!(format_ago(59), "59s ago");
        assert_eq!(format_ago(90), "1m ago");
        assert_eq!(format_ago(3600), "1h ago");
        assert_eq!(format_ago(7200), "2h ago");
    }
}
