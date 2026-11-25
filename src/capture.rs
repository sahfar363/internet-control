//! WinDivert packet counting (v0.2).
//!
//! Why this exists: v0.1 read the standard adapter counters
//! (`GetIfTable2` `InOctets` / `OutOctets`). On Sahand's PC those never
//! move — `Get-NetAdapterStatistics` shows only "Ethernet" 0/0 while the
//! "PdaNet Broadband Connection" (PPP virtual adapter, 10.1.19.2) carries
//! all the traffic. The PDAnet driver simply does not update the standard
//! counters, so every counter-based meter shows 0. Counting the actual
//! packets is the only way to see that traffic.
//!
//! How it works: a WinDivert handle on the network layer, filter `true`
//! (all IP traffic), priority 0, flags SNIFF + RECV_ONLY. In sniff mode
//! WinDivert delivers a *copy* of each packet and the original keeps
//! flowing untouched — nothing is blocked, dropped, or re-injected, so
//! no `send()` call is ever made. Each packet is counted exactly once,
//! by its IP length (`packet.data.len()`), attributed to the interface
//! index in its address. Outbound = upload, inbound = download.
//! Loopback packets are skipped: at the network layer a localhost packet
//! can appear twice (once outbound, once inbound), and it is not
//! internet traffic anyway.
//!
//! The pure aggregation (`PacketCounter`) has no WinDivert dependency and
//! is unit-tested on Linux. Only the `#[cfg(windows)]` thread below
//! touches the driver. If WinDivert cannot be opened (not admin, driver
//! files missing), the sampler falls back to the v0.1 counter mode and
//! the UI says so plainly.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
#[cfg(windows)]
use std::sync::mpsc::Sender;
#[cfg(windows)]
use std::time::{Duration, Instant};

/// Cumulative captured bytes for one interface index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IfTotals {
    /// Bytes received (inbound packets) on this interface.
    pub down_bytes: u64,
    /// Bytes sent (outbound packets) on this interface.
    pub up_bytes: u64,
}

/// Pure packet aggregator: one `add` call per captured packet, counted
/// exactly once, by IP length. No WinDivert dependency — testable on Linux.
#[derive(Debug, Default)]
pub struct PacketCounter {
    totals: HashMap<u32, IfTotals>,
    packets: u64,
}

// The read API (totals/packets/known_interfaces/total_all) is exercised
// by the unit tests; production code on Windows only needs add() +
// snapshot(), and on Linux the capture thread is compiled out entirely.
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

    /// Cumulative totals for one interface (0/0 if never seen).
    pub fn totals(&self, if_index: u32) -> IfTotals {
        self.totals.get(&if_index).copied().unwrap_or_default()
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
            packets: self.packets,
        }
    }
}

/// Snapshot sent from the capture thread to the sampler, ~10x/sec.
#[derive(Debug, Clone, Default)]
pub struct CaptureSnapshot {
    /// Cumulative per-interface totals (interface index -> bytes).
    pub totals: HashMap<u32, IfTotals>,
    /// Total packets counted so far, across all interfaces.
    pub packets: u64,
}

/// What the UI should show as the measurement source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureStatus {
    /// WinDivert capture is running; `packets` seen so far.
    Active { packets: u64 },
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
            CaptureStatus::Active { packets } => {
                format!("Source: WinDivert packet capture — {packets} packets seen")
            }
            CaptureStatus::Unavailable(reason) => {
                format!("Source: adapter counters (fallback) — {reason}")
            }
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, CaptureStatus::Active { .. })
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

/// Try to start WinDivert packet capture. Never panics; on failure the
/// caller falls back to counter mode with the returned reason.
#[cfg(windows)]
pub fn try_start() -> CaptureHandle {
    use windivert::prelude::*;

    // SNIFF: packets are copied, never diverted — the originals keep
    // flowing untouched and no send() is ever needed. RECV_ONLY: this
    // handle never injects anything.
    let flags = WinDivertFlags::default().set_sniff().set_recv_only();
    match WinDivert::network("true", 0, flags) {
        Ok(divert) => {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || capture_thread(divert, tx));
            CaptureHandle {
                rx: Some(rx),
                status: CaptureStatus::Active { packets: 0 },
            }
        }
        Err(e) => {
            CaptureHandle::unavailable(format!("WinDivert failed: {e}. Run as administrator."))
        }
    }
}

/// WinDivert is Windows-only; on other platforms capture is simply
/// unavailable and the sampler uses counter mode.
#[cfg(not(windows))]
pub fn try_start() -> CaptureHandle {
    CaptureHandle::unavailable("WinDivert packet capture is only available on Windows.".to_string())
}

/// Blocking recv loop. Runs until the UI drops the receiver, or a recv
/// error ends the handle — either way the sampler notices the channel
/// closing and reports capture as stopped.
#[cfg(windows)]
fn capture_thread(
    divert: windivert::WinDivert<windivert::layer::NetworkLayer>,
    tx: Sender<CaptureSnapshot>,
) {
    let mut counter = PacketCounter::new();
    // 64 KiB covers any IP packet (max 65535 bytes).
    let mut buf = vec![0u8; 65535];
    let mut last_send = Instant::now();
    while let Ok(packet) = divert.recv(Some(&mut buf[..])) {
        // Localhost traffic can appear twice at this layer and
        // is not internet traffic; skip it.
        if packet.address.loopback() {
            continue;
        }
        counter.add(
            packet.address.interface_index(),
            packet.address.outbound(),
            packet.data.len() as u64,
        );
        // Send a snapshot at most every 100 ms. Sparse traffic
        // still sends promptly: after a packet, the elapsed time
        // since the last send is usually already > 100 ms.
        if last_send.elapsed() >= Duration::from_millis(100) {
            last_send = Instant::now();
            if tx.send(counter.snapshot()).is_err() {
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
    fn capture_status_labels() {
        assert_eq!(
            CaptureStatus::Active { packets: 42 }.label(),
            "Source: WinDivert packet capture — 42 packets seen"
        );
        assert!(CaptureStatus::Active { packets: 0 }.is_active());
        let s = CaptureStatus::Unavailable("WinDivert failed: x.".to_string());
        assert!(!s.is_active());
        assert!(s.label().contains("adapter counters (fallback)"));
    }

    #[test]
    fn fallback_when_windivert_unavailable() {
        // The decision the sampler makes: no capture handle -> counter
        // mode, and the reason is carried to the UI instead of failing.
        let handle = try_start();
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
