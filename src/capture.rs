//! WinDivert packet counting + per-app attribution + blocking (v0.3).
//!
//! Why packet counting exists: v0.1 read the standard adapter counters
//! (`GetIfTable2` `InOctets` / `OutOctets`). On Sahand's PC those never
//! move — `Get-NetAdapterStatistics` shows only "Ethernet" 0/0 while the
//! "PdaNet Broadband Connection" (PPP virtual adapter, 10.1.19.2) carries
//! all the traffic. Counting the actual packets is the only way.
//!
//! v0.5 blocking (Sahand, 2026-11-26): Block now also terminates
//! the app's EXISTING TCP connections at once (`SetTcpEntry`, see
//! `crate::connections`) and attributes pre-existing flows through
//! a 5-tuple map rebuilt from the connection tables — so a blocked
//! app's old flows are dropped too, not just its new ones. UDP has
//! no terminate API; its pre-existing flows are dropped via the
//! same fallback map, keyed by local socket.
//! WinDivert intercept only — NO firewall rules are ever created, and
//! nothing persists at system level. The network handle runs in
//! intercept (non-sniff) mode: every packet is reinjected with
//! `divert.send()` EXCEPT packets belonging to a blocked app, which are
//! dropped (never sent, never counted). When the app closes — or
//! crashes — the handle closes and all traffic flows normally again;
//! that is the guarantee. If intercept mode cannot be opened, the
//! handle falls back to v0.2 sniff mode (copies only, no blocking —
//! the UI says so) and then to the v0.1 counter fallback.
//!
//! Per-app attribution: network-layer packets carry no PID, so a second
//! WinDivert handle on the FLOW layer reports flow established/deleted
//! events with ProcessId + 5-tuple. The capture thread maps each
//! packet's 5-tuple (parsed with etherparse) to a PID through that
//! live map, then PID -> exe path via OpenProcess +
//! QueryFullProcessImageName. v0.5 adds a fallback for flows that
//! started before the app launched: a 5-tuple -> PID map rebuilt ~1/s
//! from the connection tables (`crate::connections`), consulted when
//! the FLOW layer has no entry — so pre-existing flows are attributed
//! (and dropped when blocked) instead of falling under "Unknown".
//! ICMP / non-TCP-UDP packets have no flow at all — those bytes are
//! still counted in the totals, under "Unknown (unattributed)", never
//! lost. v0.8 records what that traffic is (`crate::unknown`) so the
//! Unknown row can be opened and identified, and attributes PID 4
//! (Windows kernel) flows to a "System" row instead of Unknown.
//! Attribution is best-effort and never breaks the total meter:
//! if the flow handle fails, per-app rows are empty but the totals
//! keep working exactly as in v0.2.
//!
//! The pure aggregation (`PacketCounter`, `parse_flow_key`,
//! `should_drop`, `count_routed`) has no WinDivert dependency and is
//! unit-tested on Linux. Only the `#[cfg(windows)]` threads touch the
//! driver.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::mpsc::Receiver;
#[cfg(windows)]
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
#[cfg(windows)]
use std::time::{Duration, Instant};

/// Bytes for packets that could not be attributed to an app (flow
/// started before the app launched, ICMP, or flow handle unavailable).
/// Shown as its own row in the per-app list so no traffic is hidden.
/// (Only referenced from Windows code; the allow keeps Linux builds
/// warning-free — the tests do use it.)
#[allow(dead_code)]
pub const UNKNOWN_APP: &str = "Unknown (unattributed)";

/// Cumulative captured bytes for one interface index (or one app).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IfTotals {
    /// Bytes received (inbound packets).
    pub down_bytes: u64,
    /// Bytes sent (outbound packets).
    pub up_bytes: u64,
}

/// A connection 5-tuple, local side first. Used as the key of the
/// flow map: the flow layer reports local/remote addr + port +
/// protocol for every established flow, and the capture thread builds
/// the same key from each packet's headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)] // only constructed from Windows code + tests
pub struct FlowKey {
    pub protocol: u8,
    pub local: IpAddr,
    pub local_port: u16,
    pub remote: IpAddr,
    pub remote_port: u16,
}

impl FlowKey {
    pub fn new(
        protocol: u8,
        local: IpAddr,
        local_port: u16,
        remote: IpAddr,
        remote_port: u16,
    ) -> Self {
        Self {
            protocol,
            local,
            local_port,
            remote,
            remote_port,
        }
    }
}

use crate::unknown::UnknownSample;

/// Parse the IP layer of one packet: (source, destination,
/// protocol). Shared by `parse_flow_key` and `parse_packet_addrs`.
/// Pure — no WinDivert dependency, tested on Linux.
#[allow(dead_code)] // only called from Windows code + tests
fn parse_ip_addrs(data: &[u8]) -> Option<(IpAddr, IpAddr, u8)> {
    let headers = etherparse::PacketHeaders::from_ip_slice(data).ok()?;
    match headers.ip? {
        etherparse::IpHeader::Version4(h, _) => Some((
            IpAddr::V4(h.source.into()),
            IpAddr::V4(h.destination.into()),
            h.protocol,
        )),
        etherparse::IpHeader::Version6(h, _) => Some((
            IpAddr::V6(h.source.into()),
            IpAddr::V6(h.destination.into()),
            h.next_header,
        )),
    }
}

/// Parse the 5-tuple out of one IP packet. `outbound == true` means
/// the packet is leaving this PC, so its source is the local side;
/// for inbound packets the destination is the local side. Returns
/// None for packets with no TCP/UDP transport header (ICMP etc.).
/// Pure — no WinDivert dependency, tested on Linux.
#[allow(dead_code)] // only called from Windows code + tests
pub fn parse_flow_key(data: &[u8], outbound: bool) -> Option<FlowKey> {
    let headers = etherparse::PacketHeaders::from_ip_slice(data).ok()?;
    let (src, dst, protocol) = parse_ip_addrs(data)?;
    let (src_port, dst_port) = match headers.transport? {
        etherparse::TransportHeader::Tcp(h) => (h.source_port, h.destination_port),
        etherparse::TransportHeader::Udp(h) => (h.source_port, h.destination_port),
        _ => return None,
    };
    let (local, local_port, remote, remote_port) = if outbound {
        (src, src_port, dst, dst_port)
    } else {
        (dst, dst_port, src, src_port)
    };
    Some(FlowKey::new(
        protocol,
        local,
        local_port,
        remote,
        remote_port,
    ))
}

/// Parse the addresses out of one packet that has no flow (ICMP):
/// (protocol, local, remote), local side first like `parse_flow_key`.
/// Pure — no WinDivert dependency, tested on Linux.
#[allow(dead_code)] // only called from Windows code + tests
pub fn parse_packet_addrs(data: &[u8], outbound: bool) -> Option<(u8, IpAddr, IpAddr)> {
    let (src, dst, protocol) = parse_ip_addrs(data)?;
    let (local, remote) = if outbound { (src, dst) } else { (dst, src) };
    Some((protocol, local, remote))
}

/// Shared live 5-tuple -> PID map, written by the flow thread and
/// read by the capture thread.
#[allow(dead_code)] // only used from Windows code + tests
pub type FlowMap = Arc<RwLock<HashMap<FlowKey, u32>>>;

/// Should this packet be dropped? Pure decision rule, shared by the
/// capture thread and the unit tests: drop only when the packet was
/// attributed to an exe whose (lowercased) full path is in the
/// blocked set. Unattributed packets are never dropped.
#[allow(dead_code)] // only called from Windows code + tests
pub fn should_drop(exe: Option<&str>, blocked: &HashSet<String>) -> bool {
    match exe {
        Some(path) => blocked.contains(&path.to_lowercase()),
        None => false,
    }
}

/// Simulate the capture loop's routing over a mock packet stream:
/// each entry is (attributed exe path or None, byte length).
/// Returns (packets reinjected, packets dropped, bytes reinjected).
/// Blocked packets are neither reinjected nor counted — exactly like
/// the real intercept loop. Pure, tested on Linux.
#[allow(dead_code)] // only called from tests on Linux
pub fn count_routed(packets: &[(Option<&str>, u64)], blocked: &HashSet<String>) -> (u64, u64, u64) {
    let mut reinjected = 0u64;
    let mut dropped = 0u64;
    let mut bytes = 0u64;
    for (exe, len) in packets {
        if should_drop(*exe, blocked) {
            dropped += 1;
        } else {
            reinjected += 1;
            bytes += len;
        }
    }
    (reinjected, dropped, bytes)
}

/// Pure packet aggregator: one `add` call per captured packet, counted
/// exactly once, by IP length. No WinDivert dependency — testable on Linux.
#[derive(Debug, Default)]
pub struct PacketCounter {
    totals: HashMap<u32, IfTotals>,
    apps: HashMap<String, IfTotals>,
    packets: u64,
}

// The read API is exercised by the unit tests; production code on
// Windows only needs add()/add_app() + snapshot().
#[allow(dead_code)]
impl PacketCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one captured packet. `outbound == true` means the packet
    /// is leaving this PC (upload); `false` means arriving (download).
    /// `len` is the IP packet length in bytes. Loopback packets must
    /// never reach here (the capture thread filters them out).
    pub fn add(&mut self, if_index: u32, outbound: bool, len: u64) {
        let entry = self.totals.entry(if_index).or_default();
        if outbound {
            entry.up_bytes += len;
        } else {
            entry.down_bytes += len;
        }
        self.packets += 1;
    }

    /// Record one captured packet against an app (full exe path, or
    /// [`UNKNOWN_APP`]). Totals stay exact; this is the per-app split
    /// of the same bytes `add()` counted for the interface.
    pub fn add_app(&mut self, exe: &str, outbound: bool, len: u64) {
        let entry = self.apps.entry(exe.to_string()).or_default();
        if outbound {
            entry.up_bytes += len;
        } else {
            entry.down_bytes += len;
        }
    }

    /// Cumulative totals for one interface (0/0 if never seen).
    pub fn totals(&self, if_index: u32) -> IfTotals {
        self.totals.get(&if_index).copied().unwrap_or_default()
    }

    /// Cumulative totals for one app (0/0 if never seen).
    pub fn app_totals(&self, exe: &str) -> IfTotals {
        self.apps.get(exe).copied().unwrap_or_default()
    }

    /// Total packets counted so far, across all interfaces.
    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// Interfaces that have seen at least one packet.
    pub fn known_interfaces(&self) -> Vec<u32> {
        self.totals.keys().copied().collect()
    }

    /// Sum over all interfaces. Useful for the "never show 0 while
    /// packets flow" check, and for tests.
    pub fn total_all(&self) -> IfTotals {
        self.totals
            .values()
            .fold(IfTotals::default(), |acc, t| IfTotals {
                down_bytes: acc.down_bytes + t.down_bytes,
                up_bytes: acc.up_bytes + t.up_bytes,
            })
    }

    /// Point-in-time copy for the sampler thread.
    pub fn snapshot(&self) -> CaptureSnapshot {
        CaptureSnapshot {
            totals: self.totals.clone(),
            apps: self.apps.clone(),
            packets: self.packets,
            // Unknown samples are attached by the capture thread
            // (v0.8); the counter itself does not record them.
            unknown: Vec::new(),
        }
    }
}

/// Snapshot sent from the capture thread to the sampler, ~10x/sec.
#[derive(Debug, Clone, Default)]
pub struct CaptureSnapshot {
    /// Cumulative per-interface totals (interface index -> bytes).
    pub totals: HashMap<u32, IfTotals>,
    /// Cumulative per-app totals (full exe path -> bytes).
    pub apps: HashMap<String, IfTotals>,
    /// Total packets counted so far, across all interfaces.
    pub packets: u64,
    /// Unattributed traffic samples (v0.8): the flows the meter
    /// could not match to a program, biggest first, at most 50.
    /// Empty in counter mode (no capture running).
    pub unknown: Vec<UnknownSample>,
}

/// What the UI should show as the measurement source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureStatus {
    /// WinDivert capture is running; `packets` seen so far.
    /// `intercept == true` means blocking works (packets are
    /// reinjected, blocked apps are dropped); `false` means sniff
    /// fallback — counting works, blocking does not.
    Active { packets: u64, intercept: bool },
    /// WinDivert could not be started; `reason` is the driver error.
    /// The meter falls back to the v0.1 adapter-counter mode.
    Unavailable(String),
}

impl Default for CaptureStatus {
    fn default() -> Self {
        CaptureStatus::Unavailable("WinDivert capture has not started yet.".to_string())
    }
}

impl CaptureStatus {
    /// One-line label for the UI.
    pub fn label(&self) -> String {
        match self {
            CaptureStatus::Active { packets, intercept } => {
                if *intercept {
                    format!(
                        "Source: WinDivert intercept — {packets} packets seen (blocking active)"
                    )
                } else {
                    format!("Source: WinDivert packet capture (sniff) — {packets} packets seen (blocking unavailable)")
                }
            }
            CaptureStatus::Unavailable(reason) => {
                format!("Source: adapter counters (fallback) — {reason}")
            }
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, CaptureStatus::Active { .. })
    }

    /// Can the Block buttons do anything right now?
    pub fn can_block(&self) -> bool {
        matches!(
            self,
            CaptureStatus::Active {
                intercept: true,
                ..
            }
        )
    }
}

/// Handle for the capture thread, owned by the sampler.
pub struct CaptureHandle {
    /// Snapshots from the capture thread, if it started.
    pub rx: Option<Receiver<CaptureSnapshot>>,
    /// Status at open time (Active, or Unavailable with the reason).
    pub status: CaptureStatus,
}

impl CaptureHandle {
    fn unavailable(reason: String) -> Self {
        Self {
            rx: None,
            status: CaptureStatus::Unavailable(reason),
        }
    }
}

/// Start WinDivert capture: flow thread for per-app attribution plus
/// a network thread in intercept mode (blocking), falling back to
/// sniff mode, then to no capture at all. Never panics; on failure
/// the caller falls back to counter mode with the returned reason.
/// `blocked` is the shared set of lowercased exe paths the user
/// blocked in the UI; the capture thread reads it on every packet.
#[cfg(windows)]
pub fn try_start(blocked: Arc<RwLock<HashSet<String>>>) -> CaptureHandle {
    use windivert::prelude::*;

    let flow_map: FlowMap = Arc::new(RwLock::new(HashMap::new()));
    // Flow handle is best-effort: if it fails, per-app rows stay
    // empty but the total meter keeps working.
    if let Ok(flow) = WinDivert::flow("true", 0, WinDivertFlags::default()) {
        let map = flow_map.clone();
        std::thread::spawn(move || flow_thread(flow, map));
    }

    // v0.5 fallback attribution: 5-tuple -> PID maps rebuilt ~1/s
    // from the connection tables, so flows that started before this
    // app launched are attributed (and dropped when blocked) even
    // though the FLOW layer never saw them. UDP has no FLOW entry
    // problem either way; the fallback covers it by local socket.
    let tcp_fb = crate::connections::TcpFallbackMap::default();
    let udp_fb = crate::connections::UdpFallbackMap::default();
    crate::connections::spawn_refresh(tcp_fb.clone(), udp_fb.clone());

    // 1. Intercept mode: packets are held until we reinject them, so
    //    blocked apps' packets can be dropped. This is the v0.3 path.
    match WinDivert::network("true", 0, WinDivertFlags::default()) {
        Ok(divert) => {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                capture_thread(divert, tx, flow_map, tcp_fb, udp_fb, blocked, true)
            });
            CaptureHandle {
                rx: Some(rx),
                status: CaptureStatus::Active {
                    packets: 0,
                    intercept: true,
                },
            }
        }
        Err(e) => {
            // 2. Sniff fallback (v0.2 behaviour): copies only, no
            //    blocking possible. Remember the reason for the UI.
            let flags = WinDivertFlags::default().set_sniff().set_recv_only();
            match WinDivert::network("true", 0, flags) {
                Ok(divert) => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        capture_thread(divert, tx, flow_map, tcp_fb, udp_fb, blocked, false)
                    });
                    CaptureHandle {
                        rx: Some(rx),
                        status: CaptureStatus::Active {
                            packets: 0,
                            intercept: false,
                        },
                    }
                }
                Err(_) => CaptureHandle::unavailable(format!(
                    "WinDivert failed: {e}. Run as administrator."
                )),
            }
        }
    }
}

/// WinDivert is Windows-only; on other platforms capture is simply
/// unavailable and the sampler uses counter mode.
#[cfg(not(windows))]
pub fn try_start(_blocked: Arc<RwLock<HashSet<String>>>) -> CaptureHandle {
    CaptureHandle::unavailable("WinDivert packet capture is only available on Windows.".to_string())
}

/// Flow thread: maintains the live 5-tuple -> PID map. Runs until
/// the handle errors or the process exits.
#[cfg(windows)]
fn flow_thread(divert: windivert::WinDivert<windivert::layer::FlowLayer>, flow_map: FlowMap) {
    use windivert::prelude::*;
    let mut endpoints: HashMap<u64, FlowKey> = HashMap::new();
    let mut buf = vec![0u8; 1024];
    while let Ok(packet) = divert.recv(Some(&mut buf[..])) {
        let addr = &packet.address;
        let key = FlowKey::new(
            addr.protocol(),
            addr.local_address(),
            addr.local_port(),
            addr.remote_address(),
            addr.remote_port(),
        );
        match addr.event() {
            WinDivertEvent::FlowStablished => {
                endpoints.insert(addr.endpoint_id(), key);
                if let Ok(mut map) = flow_map.write() {
                    map.insert(key, addr.process_id());
                }
            }
            WinDivertEvent::FlowDeleted => {
                if let Some(old) = endpoints.remove(&addr.endpoint_id()) {
                    if let Ok(mut map) = flow_map.write() {
                        map.remove(&old);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Full exe path for a PID, via OpenProcess +
/// QueryFullProcessImageName. Windows only; None if the process is
/// gone or cannot be queried (e.g. a protected system process).
/// `pub(crate)` so `crate::connections` can resolve table PIDs.
#[cfg(windows)]
pub(crate) fn exe_for_pid(pid: u32) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        if result.is_err() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

#[cfg(not(windows))]
#[allow(dead_code)] // Windows version is used on Windows; stub exists for tests
pub(crate) fn exe_for_pid(_pid: u32) -> Option<String> {
    None
}

/// Blocking recv loop. In intercept mode every non-blocked packet is
/// reinjected promptly with `send()`; blocked packets are dropped
/// (never sent, never counted). In sniff mode packets are copies and
/// `send()` is never called. Runs until the UI drops the receiver,
/// or a recv error ends the handle — either way the sampler notices
/// the channel closing and reports capture as stopped. If this thread
/// dies for any reason, the WinDivert handle closes with it and all
/// traffic flows normally again — blocking can never outlive the app.
#[cfg(windows)]
fn capture_thread(
    divert: windivert::WinDivert<windivert::layer::NetworkLayer>,
    tx: Sender<CaptureSnapshot>,
    flow_map: FlowMap,
    tcp_fb: crate::connections::TcpFallbackMap,
    udp_fb: crate::connections::UdpFallbackMap,
    blocked: Arc<RwLock<HashSet<String>>>,
    intercept: bool,
) {
    use crate::unknown::UnknownSampleStore;

    let mut counter = PacketCounter::new();
    // Unattributed traffic samples (v0.8): what the "Unknown"
    // row is made of, so the UI can show and identify it.
    let mut unknown = UnknownSampleStore::new();
    // PID -> exe path cache. A PID is only reused after its process
    // exits, and a relaunched app usually has the same path, so a
    // stale entry is harmless for a meter; failures (None) are cached
    // too so system PIDs do not cost an OpenProcess per packet.
    let mut exe_cache: HashMap<u32, Option<String>> = HashMap::new();
    // 64 KiB covers any IP packet (max 65535 bytes).
    let mut buf = vec![0u8; 65535];
    let mut last_send = Instant::now();
    while let Ok(packet) = divert.recv(Some(&mut buf[..])) {
        // Localhost traffic is not internet traffic. In intercept mode
        // it must still be reinjected immediately, or local apps
        // (and the loopback itself) would stall.
        if packet.address.loopback() {
            if intercept {
                let _ = divert.send(&packet);
            }
            continue;
        }
        let outbound = packet.address.outbound();
        let if_index = packet.address.interface_index();
        let len = packet.data.len() as u64;
        // The 5-tuple, parsed once: attribution uses it, and an
        // unattributed packet is recorded as an unknown sample
        // with it (v0.8). None for ICMP / non-TCP-UDP packets.
        let key = parse_flow_key(&packet.data, outbound);
        // Attribute the packet to an app, if its flow is known.
        // FLOW map first (live flows); on a miss, the v0.5 fallback
        // maps from the connection tables (flows that started before
        // this app launched, incl. UDP by local socket).
        let exe: Option<String> = (|| {
            let key = key?;
            let pid = flow_map
                .read()
                .ok()?
                .get(&key)
                .copied()
                .or_else(|| crate::connections::fallback_pid(&key, &tcp_fb, &udp_fb))?;
            // PID 4 is the Windows kernel (v0.8): attribute its
            // flows to a "System" row instead of Unknown, where the
            // PID is known. The UI offers no Block button for it —
            // dropping the kernel's packets would break networking.
            if pid == crate::unknown::SYSTEM_PID {
                return Some(crate::unknown::SYSTEM_EXE.to_string());
            }
            exe_cache
                .entry(pid)
                .or_insert_with(|| exe_for_pid(pid))
                .clone()
        })();
        let is_blocked = match &exe {
            Some(path) => blocked
                .read()
                .map(|set| should_drop(Some(path), &set))
                .unwrap_or(false),
            None => false,
        };
        if intercept {
            if is_blocked {
                // Drop: never reinjected, never counted. The app's
                // packets simply never reach the network (outbound)
                // or the app (inbound).
                continue;
            }
            // Reinject promptly; if it fails, do not count a packet
            // that never reached its destination.
            if divert.send(&packet).is_err() {
                continue;
            }
        }
        counter.add(if_index, outbound, len);
        match exe {
            Some(path) => counter.add_app(&path, outbound, len),
            None => {
                counter.add_app(UNKNOWN_APP, outbound, len);
                // Record what this unattributed packet was (v0.8),
                // so the Unknown row can be opened and identified:
                // TCP/UDP by 5-tuple, anything else (ICMP) by peer.
                let now = Instant::now();
                match key {
                    Some(k) => unknown.record_flow(&k, len, now),
                    None => {
                        if let Some((proto, local, remote)) =
                            parse_packet_addrs(&packet.data, outbound)
                        {
                            unknown.record_packet(proto, local, remote, len, now);
                        }
                    }
                }
            }
        }
        // Send a snapshot at most every 100 ms. Sparse traffic
        // still sends promptly: after a packet, the elapsed time
        // since the last send is usually already > 100 ms.
        if last_send.elapsed() >= Duration::from_millis(100) {
            last_send = Instant::now();
            let mut snap = counter.snapshot();
            snap.unknown = unknown.snapshot();
            if tx.send(snap).is_err() {
                break; // UI gone.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_counted_once() {
        let mut c = PacketCounter::new();
        c.add(11, false, 1500);
        assert_eq!(c.totals(11).down_bytes, 1500);
        assert_eq!(c.totals(11).up_bytes, 0);
        assert_eq!(c.packets(), 1);
        // A second, identical packet counts again (it is a new packet),
        // but one add() call never counts a packet twice.
        c.add(11, false, 1500);
        assert_eq!(c.totals(11).down_bytes, 3000);
        assert_eq!(c.packets(), 2);
    }

    #[test]
    fn outbound_is_upload_inbound_is_download() {
        let mut c = PacketCounter::new();
        c.add(11, true, 100); // outbound -> upload
        c.add(11, false, 900); // inbound -> download
        assert_eq!(c.totals(11).up_bytes, 100);
        assert_eq!(c.totals(11).down_bytes, 900);
    }

    #[test]
    fn per_interface_totals_are_independent() {
        let mut c = PacketCounter::new();
        c.add(11, false, 1_000); // PdaNet
        c.add(7, false, 2_000); // Ethernet
        c.add(11, true, 500);
        assert_eq!(
            c.totals(11),
            IfTotals {
                down_bytes: 1_000,
                up_bytes: 500
            }
        );
        assert_eq!(
            c.totals(7),
            IfTotals {
                down_bytes: 2_000,
                up_bytes: 0
            }
        );
        assert_eq!(c.totals(99), IfTotals::default());
        assert_eq!(
            c.total_all(),
            IfTotals {
                down_bytes: 3_000,
                up_bytes: 500
            }
        );
        let mut ifs = c.known_interfaces();
        ifs.sort_unstable();
        assert_eq!(ifs, vec![7, 11]);
    }

    #[test]
    fn snapshot_carries_cumulative_totals() {
        let mut c = PacketCounter::new();
        c.add(11, false, 10_000);
        c.add(11, false, 5_000);
        let snap = c.snapshot();
        assert_eq!(snap.totals[&11].down_bytes, 15_000);
        assert_eq!(snap.packets, 2);
        // The snapshot is a copy: later packets do not change it.
        c.add(11, false, 1);
        assert_eq!(snap.totals[&11].down_bytes, 15_000);
        assert_eq!(c.snapshot().totals[&11].down_bytes, 15_001);
    }

    #[test]
    fn per_app_aggregation_by_exe_path() {
        let mut c = PacketCounter::new();
        c.add_app(r"C:\Program Files\Chrome\chrome.exe", false, 10_000);
        c.add_app(r"C:\Program Files\Chrome\chrome.exe", true, 500);
        c.add_app(r"C:\Windows\System32\svchost.exe", false, 2_000);
        // Same exe, different connections: one row, totals summed.
        assert_eq!(
            c.app_totals(r"C:\Program Files\Chrome\chrome.exe"),
            IfTotals {
                down_bytes: 10_000,
                up_bytes: 500
            }
        );
        assert_eq!(
            c.app_totals(r"C:\Windows\System32\svchost.exe"),
            IfTotals {
                down_bytes: 2_000,
                up_bytes: 0
            }
        );
        assert_eq!(c.app_totals(r"C:\nope.exe"), IfTotals::default());
        let snap = c.snapshot();
        assert_eq!(snap.apps.len(), 2);
        assert_eq!(
            snap.apps[r"C:\Program Files\Chrome\chrome.exe"].down_bytes,
            10_000
        );
    }

    #[test]
    fn parse_flow_key_tcp_outbound_and_inbound() {
        use etherparse::PacketBuilder;
        let mut packet = Vec::new();
        PacketBuilder::ipv4([10, 0, 0, 5], [93, 184, 216, 34], 64)
            .tcp(52341, 443, 1, 65535)
            .write(&mut packet, &[1, 2, 3, 4])
            .unwrap();
        // Outbound: local = source.
        let key = parse_flow_key(&packet, true).unwrap();
        assert_eq!(key.protocol, 6);
        assert_eq!(key.local, IpAddr::from([10, 0, 0, 5]));
        assert_eq!(key.local_port, 52341);
        assert_eq!(key.remote, IpAddr::from([93, 184, 216, 34]));
        assert_eq!(key.remote_port, 443);
        // Same packet seen inbound: local/remote swap.
        let key_in = parse_flow_key(&packet, false).unwrap();
        assert_eq!(key_in.local, IpAddr::from([93, 184, 216, 34]));
        assert_eq!(key_in.remote, IpAddr::from([10, 0, 0, 5]));
        assert_eq!(key_in.local_port, 443);
    }

    #[test]
    fn parse_flow_key_udp_and_icmp() {
        use etherparse::PacketBuilder;
        let mut udp = Vec::new();
        PacketBuilder::ipv4([10, 0, 0, 5], [8, 8, 8, 8], 64)
            .udp(53000, 53)
            .write(&mut udp, &[1, 2])
            .unwrap();
        let key = parse_flow_key(&udp, true).unwrap();
        assert_eq!(key.protocol, 17);
        assert_eq!(key.local_port, 53000);
        assert_eq!(key.remote_port, 53);
        // ICMP has no transport ports: no flow key, but the bytes are
        // still counted by the caller under UNKNOWN_APP.
        let mut icmp = Vec::new();
        PacketBuilder::ipv4([10, 0, 0, 5], [8, 8, 8, 8], 64)
            .icmpv4_echo_request(1, 1)
            .write(&mut icmp, &[1, 2])
            .unwrap();
        assert!(parse_flow_key(&icmp, true).is_none());
        // Garbage is not a packet.
        assert!(parse_flow_key(&[1, 2, 3], true).is_none());
    }

    #[test]
    fn parse_packet_addrs_for_icmp_and_tcp() {
        use etherparse::PacketBuilder;
        // ICMP: no flow key, but the addresses + protocol are known
        // — that is what the v0.8 unknown sample records.
        let mut icmp = Vec::new();
        PacketBuilder::ipv4([10, 0, 0, 5], [8, 8, 8, 8], 64)
            .icmpv4_echo_request(1, 1)
            .write(&mut icmp, &[1, 2])
            .unwrap();
        let (proto, local, remote) = parse_packet_addrs(&icmp, true).unwrap();
        assert_eq!(proto, 1);
        assert_eq!(local, IpAddr::from([10, 0, 0, 5]));
        assert_eq!(remote, IpAddr::from([8, 8, 8, 8]));
        // Same packet seen inbound: local/remote swap.
        let (_, local_in, remote_in) = parse_packet_addrs(&icmp, false).unwrap();
        assert_eq!(local_in, IpAddr::from([8, 8, 8, 8]));
        assert_eq!(remote_in, IpAddr::from([10, 0, 0, 5]));
        // TCP packets have addresses too (used only when the flow
        // key is missing — normally the flow key is used instead).
        let mut tcp = Vec::new();
        PacketBuilder::ipv4([10, 0, 0, 5], [93, 184, 216, 34], 64)
            .tcp(52341, 443, 1, 65535)
            .write(&mut tcp, &[1, 2, 3, 4])
            .unwrap();
        let (proto, local, remote) = parse_packet_addrs(&tcp, true).unwrap();
        assert_eq!(proto, 6);
        assert_eq!(local, IpAddr::from([10, 0, 0, 5]));
        assert_eq!(remote, IpAddr::from([93, 184, 216, 34]));
        assert!(parse_packet_addrs(&[1, 2, 3], true).is_none());
    }

    #[test]
    fn flow_map_insert_lookup_delete() {
        let map: FlowMap = Arc::new(RwLock::new(HashMap::new()));
        let key = FlowKey::new(
            6,
            IpAddr::from([10, 0, 0, 5]),
            52341,
            IpAddr::from([93, 184, 216, 34]),
            443,
        );
        {
            let mut m = map.write().unwrap();
            m.insert(key, 1234);
        }
        assert_eq!(map.read().unwrap().get(&key), Some(&1234));
        {
            let mut m = map.write().unwrap();
            m.remove(&key);
        }
        assert!(map.read().unwrap().get(&key).is_none());
    }

    #[test]
    fn should_drop_only_blocked_exes() {
        let mut blocked = HashSet::new();
        blocked.insert(r"c:\games\game.exe".to_string());
        assert!(should_drop(Some(r"C:\Games\Game.exe"), &blocked));
        assert!(!should_drop(Some(r"C:\Games\Other.exe"), &blocked));
        // Unattributed packets are never dropped: dropping them could
        // blackhole traffic the user never chose to block.
        assert!(!should_drop(None, &blocked));
        assert!(!should_drop(Some(r"C:\Games\Game.exe"), &HashSet::new()));
    }

    #[test]
    fn mock_stream_reinject_vs_drop_counts() {
        // Simulates the intercept loop over a mock packet stream.
        let mut blocked = HashSet::new();
        blocked.insert(r"c:\games\game.exe".to_string());
        let stream: Vec<(Option<&str>, u64)> = vec![
            (Some(r"C:\Games\Game.exe"), 1500),     // blocked: dropped
            (Some(r"C:\Games\Game.exe"), 500),      // blocked: dropped
            (Some(r"C:\Browser\chrome.exe"), 9000), // reinjected
            (None, 200),                            // unattributed: reinjected
            (Some(r"C:\Browser\chrome.exe"), 300),
        ];
        let (reinjected, dropped, bytes) = count_routed(&stream, &blocked);
        assert_eq!(reinjected, 3);
        assert_eq!(dropped, 2);
        // Only reinjected bytes count: 9000 + 200 + 300.
        assert_eq!(bytes, 9500);
        // Every non-blocked packet is reinjected: nothing stalls.
        let (all_re, all_drop, _) = count_routed(&stream, &HashSet::new());
        assert_eq!(all_re, 5);
        assert_eq!(all_drop, 0);
    }

    #[test]
    fn capture_status_labels() {
        assert_eq!(
            CaptureStatus::Active {
                packets: 42,
                intercept: true
            }
            .label(),
            "Source: WinDivert intercept — 42 packets seen (blocking active)"
        );
        assert!(CaptureStatus::Active {
            packets: 0,
            intercept: true
        }
        .is_active());
        assert!(CaptureStatus::Active {
            packets: 0,
            intercept: true
        }
        .can_block());
        assert!(!CaptureStatus::Active {
            packets: 0,
            intercept: false
        }
        .can_block());
        assert!(CaptureStatus::Active {
            packets: 0,
            intercept: false
        }
        .label()
        .contains("sniff"));
        let s = CaptureStatus::Unavailable("WinDivert failed: x.".to_string());
        assert!(!s.is_active());
        assert!(!s.can_block());
        assert!(s.label().contains("adapter counters (fallback)"));
    }

    #[test]
    fn fallback_when_windivert_unavailable() {
        // The decision the sampler makes: no capture handle -> counter
        // mode, and the reason is carried to the UI instead of failing.
        let handle = try_start(Arc::new(RwLock::new(HashSet::new())));
        #[cfg(not(windows))]
        {
            assert!(handle.rx.is_none());
            assert!(!handle.status.is_active());
            assert!(handle.status.label().contains("only available on Windows"));
        }
        #[cfg(windows)]
        {
            // On Windows this test only runs in CI/dev machines; the
            // handle is either active or carries a reason — never a panic.
            let _ = handle;
        }
    }
}
