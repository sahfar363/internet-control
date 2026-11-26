//! Terminating an app's existing connections + fallback 5-tuple
//! attribution for flows that started before launch (v0.5).
//!
//! The v0.3/v0.4 problem: per-app attribution came ONLY from the
//! WinDivert FLOW layer, which reports flows established *after* the
//! app launches. Packets from flows that started earlier were
//! counted but unattributed, so a Block click only stopped NEW
//! connections — the UI showed "BLOCKED — NEW CONNECTIONS ONLY"
//! while the old connection kept moving. Sahand asked (2026-11-26)
//! whether open connections can be terminated at once. Yes — two
//! mechanisms, both one-time actions, both gone when the app closes:
//!
//! 1. **Terminate the app's open TCP connections** with `SetTcpEntry`
//!    (`MIB_TCP_STATE_DELETE_TCB`, the documented Windows way —
//!    what TCPView does). Enumerate the TCP table with
//!    `GetExtendedTcpTable` (`TCP_TABLE_OWNER_PID_ALL`), pick the
//!    rows owned by the blocked app's PIDs, and delete them. The
//!    connection dies immediately (the app sees it right away) and,
//!    if the app reconnects, the new connection is a NEW flow that
//!    the existing WinDivert FLOW drop catches. Needs admin — the
//!    app already requires it. IPv6 TCP has no `SetTcpEntry` in the
//!    Windows API, so those rows are skipped for termination and
//!    covered by mechanism 2 instead.
//! 2. **Fallback 5-tuple attribution** from the connection tables:
//!    a background thread rebuilds a 5-tuple -> PID map from
//!    `GetExtendedTcpTable` + `GetExtendedUdpTable` about once a
//!    second. The capture thread consults it when the FLOW layer has
//!    no entry for a packet — so pre-existing flows (TCP and UDP,
//!    which has no terminate API at all) are attributed and their
//!    packets dropped like any other blocked packet. UDP rows have
//!    no remote side, so they are keyed by (local addr, local port)
//!    instead: one local UDP socket belongs to one PID, whatever
//!    remote it talks to.
//!
//! Nothing here persists: no firewall rules, no registry keys, no
//! driver state. Terminations are one-time actions on live sockets;
//! closing Internet Control restores everything, same as before.
//!
//! The pure parts (row parsing, snapshot building, termination
//! selection, attribution order) have no Windows dependency and are
//! unit-tested on Linux. Only the `#[cfg(windows)]` functions touch
//! IP Helper.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, RwLock};

use crate::capture::FlowKey;

/// Shared fallback 5-tuple -> PID map, rebuilt ~1/s from the
/// connection tables and read by the capture thread on FLOW misses.
#[allow(dead_code)] // only used from Windows code + tests
pub type TcpFallbackMap = Arc<RwLock<HashMap<FlowKey, u32>>>;

/// Shared fallback (local addr, local port) -> PID map for UDP
/// sockets, rebuilt with the TCP map.
#[allow(dead_code)] // only used from Windows code + tests
pub type UdpFallbackMap = Arc<RwLock<HashMap<(IpAddr, u16), u32>>>;

/// The connection tables, reduced to what attribution and
/// termination need.
#[allow(dead_code)] // only used from Windows code + tests
#[derive(Debug, Clone, Default)]
pub struct TableSnapshot {
    /// Exact 5-tuple -> PID for TCP connections (v4 + v6).
    pub tcp: HashMap<FlowKey, u32>,
    /// (Local addr, local port) -> PID for UDP sockets (v4 + v6).
    pub udp: HashMap<(IpAddr, u16), u32>,
}

/// One TCP connection row, parsed from the MIB tables. The `raw_*`
/// DWORDs are the original MIB values, passed through unchanged to
/// `SetTcpEntry` (IPv4 only).
#[allow(dead_code)] // only used from Windows code + tests
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpRow {
    pub local: IpAddr,
    pub local_port: u16,
    pub remote: IpAddr,
    pub remote_port: u16,
    /// MIB_TCP_STATE value (5 = ESTABLISHED, 2 = LISTEN, ...).
    pub state: u32,
    pub pid: u32,
    /// Raw MIB DWORDs (network byte order), IPv4 rows only.
    pub raw_local_addr: u32,
    pub raw_local_port: u32,
    pub raw_remote_addr: u32,
    pub raw_remote_port: u32,
}

#[allow(dead_code)] // only used from Windows code + tests
impl TcpRow {
    /// Build an IPv4 row from raw MIB DWORDs (network byte order).
    pub fn v4(
        local_addr: u32,
        local_port: u32,
        remote_addr: u32,
        remote_port: u32,
        state: u32,
        pid: u32,
    ) -> Self {
        Self {
            local: ipv4_from_mib(local_addr),
            local_port: port_from_mib(local_port),
            remote: ipv4_from_mib(remote_addr),
            remote_port: port_from_mib(remote_port),
            state,
            pid,
            raw_local_addr: local_addr,
            raw_local_port: local_port,
            raw_remote_addr: remote_addr,
            raw_remote_port: remote_port,
        }
    }

    /// Build an IPv6 row from raw MIB fields.
    pub fn v6(
        local: [u8; 16],
        local_port: u32,
        remote: [u8; 16],
        remote_port: u32,
        state: u32,
        pid: u32,
    ) -> Self {
        Self {
            local: IpAddr::V6(Ipv6Addr::from(local)),
            local_port: port_from_mib(local_port),
            remote: IpAddr::V6(Ipv6Addr::from(remote)),
            remote_port: port_from_mib(remote_port),
            state,
            pid,
            raw_local_addr: 0,
            raw_local_port: 0,
            raw_remote_addr: 0,
            raw_remote_port: 0,
        }
    }

    pub fn is_ipv6(&self) -> bool {
        matches!(self.local, IpAddr::V6(_))
    }

    pub fn key(&self) -> FlowKey {
        FlowKey::new(
            6,
            self.local,
            self.local_port,
            self.remote,
            self.remote_port,
        )
    }
}

/// One UDP socket row: only a local side exists in the table.
#[allow(dead_code)] // only used from Windows code + tests
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpRow {
    pub local: IpAddr,
    pub local_port: u16,
    pub pid: u32,
}

#[allow(dead_code)] // only used from Windows code + tests
impl UdpRow {
    pub fn v4(local_addr: u32, local_port: u32, pid: u32) -> Self {
        Self {
            local: ipv4_from_mib(local_addr),
            local_port: port_from_mib(local_port),
            pid,
        }
    }

    pub fn v6(local: [u8; 16], local_port: u32, pid: u32) -> Self {
        Self {
            local: IpAddr::V6(Ipv6Addr::from(local)),
            local_port: port_from_mib(local_port),
            pid,
        }
    }
}

/// IPv4 address from a MIB DWORD: the DWORD is in network byte
/// order, so the bytes as read on a little-endian machine are
/// reversed — `to_be()` puts them back in octet order.
#[allow(dead_code)] // only used from Windows code + tests
pub fn ipv4_from_mib(raw: u32) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(raw.to_be()))
}

/// Port from a MIB DWORD: the port is in network byte order in the
/// low 16 bits.
#[allow(dead_code)] // only used from Windows code + tests
pub fn port_from_mib(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

/// Build a snapshot from parsed rows. Pure — unit-tested below.
#[allow(dead_code)] // only used from Windows code + tests
pub fn build_snapshot(tcp: &[TcpRow], udp: &[UdpRow]) -> TableSnapshot {
    TableSnapshot {
        tcp: tcp.iter().map(|r| (r.key(), r.pid)).collect(),
        udp: udp
            .iter()
            .map(|r| ((r.local, r.local_port), r.pid))
            .collect(),
    }
}

/// Is this TCP state a live connection worth terminating?
/// LISTEN sockets are not connections (and closing a listener would
/// stop the app accepting anything, which is beyond "terminate open
/// connections"); CLOSED / DELETE_TCB rows are already dying.
#[allow(dead_code)] // only used from Windows code + tests
pub fn is_terminable_state(state: u32) -> bool {
    // MIB_TCP_STATE: 1 CLOSED, 2 LISTEN, 3 SYN_SENT, 4 SYN_RCVD,
    // 5 ESTABLISHED, 6 FIN_WAIT_1, 7 FIN_WAIT_2, 8 CLOSE_WAIT,
    // 9 CLOSING, 10 LAST_ACK, 11 TIME_WAIT, 12 DELETE_TCB.
    (3..=11).contains(&state)
}

/// Rows selected for termination + IPv6 rows that match but cannot
/// be terminated (no IPv6 `SetTcpEntry` in the Windows API — their
/// packets are still dropped via the fallback map).
#[allow(dead_code)] // only used from Windows code + tests
#[derive(Debug, Default)]
pub struct TerminateSelection {
    /// IPv4 rows to delete with `SetTcpEntry`.
    pub terminate: Vec<TcpRow>,
    /// Matching IPv6 rows skipped (dropped instead, not terminated).
    pub ipv6_skipped: usize,
}

/// Pure selection: which of these TCP rows belong to the app (by exe
/// path, via the caller's PID -> exe resolver) and should be
/// terminated. The exe-path comparison is case-insensitive.
#[allow(dead_code)] // only used from Windows code + tests
pub fn select_to_terminate(
    exe_path: &str,
    rows: &[TcpRow],
    pid_exe: &dyn Fn(u32) -> Option<String>,
) -> TerminateSelection {
    let exe_lower = exe_path.to_lowercase();
    let mut out = TerminateSelection::default();
    for row in rows {
        if !is_terminable_state(row.state) {
            continue;
        }
        let owned = pid_exe(row.pid)
            .map(|exe| exe.to_lowercase() == exe_lower)
            .unwrap_or(false);
        if !owned {
            continue;
        }
        if row.is_ipv6() {
            out.ipv6_skipped += 1;
        } else {
            out.terminate.push(*row);
        }
    }
    out
}

/// Attribute one packet's 5-tuple to a PID. Order: live FLOW map
/// first, then the table-based TCP fallback, then (UDP packets only)
/// the UDP fallback by local socket. Pure — unit-tested below.
#[allow(dead_code)] // only used from Windows code + tests
pub fn attribute_pid(
    key: &FlowKey,
    flow: &HashMap<FlowKey, u32>,
    tcp_fb: &HashMap<FlowKey, u32>,
    udp_fb: &HashMap<(IpAddr, u16), u32>,
) -> Option<u32> {
    flow.get(key)
        .copied()
        .or_else(|| tcp_fb.get(key).copied())
        .or_else(|| {
            if key.protocol == 17 {
                udp_fb.get(&(key.local, key.local_port)).copied()
            } else {
                None
            }
        })
}

/// Fallback lookup for the capture thread: the FLOW map already
/// missed, so consult the shared table-based maps (locking them).
/// Pure over the shared maps; the locks live in the caller.
#[allow(dead_code)] // only used from Windows code + tests
pub fn fallback_pid(
    key: &FlowKey,
    tcp_fb: &TcpFallbackMap,
    udp_fb: &UdpFallbackMap,
) -> Option<u32> {
    let tcp = tcp_fb.read().ok()?;
    if let Some(pid) = tcp.get(key).copied() {
        return Some(pid);
    }
    if key.protocol == 17 {
        let udp = udp_fb.read().ok()?;
        udp.get(&(key.local, key.local_port)).copied()
    } else {
        None
    }
}

/// What one Block click did to the app's existing connections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminateReport {
    /// IPv4 TCP connections deleted with `SetTcpEntry`.
    pub tcp_terminated: usize,
    /// `SetTcpEntry` calls that failed.
    pub tcp_failed: usize,
    /// Matching IPv6 TCP rows skipped (dropped instead).
    pub tcp_ipv6_skipped: usize,
    /// UDP sockets owned by the app whose packets will be dropped.
    pub udp_flows: usize,
}

impl TerminateReport {
    /// One line for the UI note + log, e.g. "Terminated 3 open TCP
    /// connections for chrome.exe; 2 UDP flows will be dropped."
    pub fn summarize(&self, display_name: &str) -> String {
        if self.tcp_terminated == 0
            && self.udp_flows == 0
            && self.tcp_ipv6_skipped == 0
            && self.tcp_failed == 0
        {
            return format!(
                "Blocked {display_name} — no open connections found to terminate; new connections will be dropped."
            );
        }
        let mut parts: Vec<String> = Vec::new();
        if self.tcp_terminated > 0 {
            parts.push(format!(
                "Terminated {} open TCP connection{} for {display_name}",
                self.tcp_terminated,
                if self.tcp_terminated == 1 { "" } else { "s" }
            ));
        }
        if self.udp_flows > 0 {
            parts.push(format!(
                "{} UDP flow{} will be dropped",
                self.udp_flows,
                if self.udp_flows == 1 { "" } else { "s" }
            ));
        }
        if self.tcp_ipv6_skipped > 0 {
            parts.push(format!(
                "{} IPv6 connection{} could not be terminated directly and will be dropped instead",
                self.tcp_ipv6_skipped,
                if self.tcp_ipv6_skipped == 1 { "" } else { "s" }
            ));
        }
        if self.tcp_failed > 0 {
            parts.push(format!(
                "{} termination{} failed",
                self.tcp_failed,
                if self.tcp_failed == 1 { "" } else { "s" }
            ));
        }
        parts.join("; ") + "."
    }
}

// ---------------------------------------------------------------------------
// Windows implementation: IP Helper tables + SetTcpEntry
// ---------------------------------------------------------------------------

/// Read the current connection tables (TCP v4+v6, UDP v4+v6) and
/// reduce them to a snapshot. Windows only.
#[allow(dead_code)] // only used from Windows code + tests
#[cfg(windows)]
pub fn snapshot_tables() -> TableSnapshot {
    build_snapshot(&tcp_rows_win(), &udp_rows_win())
}

/// Non-Windows stub: no connection tables exist here.
#[allow(dead_code)] // only used from Windows code + tests
#[cfg(not(windows))]
pub fn snapshot_tables() -> TableSnapshot {
    TableSnapshot::default()
}

/// Terminate the app's open TCP connections (IPv4) and count its UDP
/// flows. One-time action; returns what happened. Windows only — the
/// stub below returns an empty report.
#[cfg(windows)]
pub fn terminate_app_connections(exe_path: &str) -> TerminateReport {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::NetworkManagement::IpHelper::{
        SetTcpEntry, MIB_TCPROW_LH, MIB_TCPROW_LH_0, MIB_TCP_STATE_DELETE_TCB,
    };

    let exe_lower = exe_path.to_lowercase();
    let tcp_rows = tcp_rows_win();
    let udp_rows = udp_rows_win();
    let sel = select_to_terminate(&exe_lower, &tcp_rows, &|pid| {
        crate::capture::exe_for_pid(pid)
    });
    let mut report = TerminateReport {
        tcp_ipv6_skipped: sel.ipv6_skipped,
        ..Default::default()
    };
    // Count UDP sockets whose owner is this app — their packets are
    // dropped via the fallback map (UDP has no terminate API).
    for u in &udp_rows {
        if crate::capture::exe_for_pid(u.pid)
            .map(|exe| exe.to_lowercase() == exe_lower)
            .unwrap_or(false)
        {
            report.udp_flows += 1;
        }
    }
    for row in sel.terminate {
        // Pass the raw MIB values through unchanged — SetTcpEntry
        // expects the same network-byte-order form the table gave us.
        let lh = MIB_TCPROW_LH {
            Anonymous: MIB_TCPROW_LH_0 {
                dwState: MIB_TCP_STATE_DELETE_TCB.0 as u32,
            },
            dwLocalAddr: row.raw_local_addr,
            dwLocalPort: row.raw_local_port,
            dwRemoteAddr: row.raw_remote_addr,
            dwRemotePort: row.raw_remote_port,
        };
        let rc = unsafe { SetTcpEntry(&lh) };
        if rc == ERROR_SUCCESS.0 {
            report.tcp_terminated += 1;
        } else {
            report.tcp_failed += 1;
        }
    }
    report
}

/// Non-Windows stub: nothing to terminate here.
#[cfg(not(windows))]
pub fn terminate_app_connections(_exe_path: &str) -> TerminateReport {
    TerminateReport::default()
}

/// Spawn the refresh thread: rebuilds the fallback maps from the
/// connection tables immediately, then about once a second. Windows
/// only — on other platforms the maps stay empty.
#[allow(dead_code)] // only used from Windows code + tests
#[cfg(windows)]
pub fn spawn_refresh(tcp_fb: TcpFallbackMap, udp_fb: UdpFallbackMap) {
    std::thread::spawn(move || loop {
        let snap = snapshot_tables();
        if let Ok(mut map) = tcp_fb.write() {
            *map = snap.tcp;
        }
        if let Ok(mut map) = udp_fb.write() {
            *map = snap.udp;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    });
}

/// Non-Windows stub: no refresh thread.
#[allow(dead_code)] // only used from Windows code + tests
#[cfg(not(windows))]
pub fn spawn_refresh(_tcp_fb: TcpFallbackMap, _udp_fb: UdpFallbackMap) {}

#[cfg(windows)]
mod win {
    use super::*;
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
        MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    /// Call GetExtendedTcpTable / GetExtendedUdpTable with a growing
    /// buffer until it fits; returns the raw bytes (first DWORD is
    /// the row count).
    fn get_table_bytes(tcp: bool, family: u32) -> Option<Vec<u32>> {
        let mut size: u32 = 0;
        unsafe {
            // First call reports the required size.
            let rc = if tcp {
                GetExtendedTcpTable(None, &mut size, false, family, TCP_TABLE_OWNER_PID_ALL, 0)
            } else {
                GetExtendedUdpTable(None, &mut size, false, family, UDP_TABLE_OWNER_PID, 0)
            };
            if rc != ERROR_INSUFFICIENT_BUFFER.0 && rc != ERROR_SUCCESS.0 {
                return None;
            }
            if size == 0 {
                return Some(Vec::new());
            }
            // u32 buffer: the tables are DWORD-aligned, and rows are
            // read with read_unaligned-free pointer casts below.
            let mut buf = vec![0u32; size as usize / 4 + 1];
            let rc = if tcp {
                GetExtendedTcpTable(
                    Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                    &mut size,
                    false,
                    family,
                    TCP_TABLE_OWNER_PID_ALL,
                    0,
                )
            } else {
                GetExtendedUdpTable(
                    Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                    &mut size,
                    false,
                    family,
                    UDP_TABLE_OWNER_PID,
                    0,
                )
            };
            if rc != ERROR_SUCCESS.0 {
                return None;
            }
            Some(buf)
        }
    }

    fn tcp_rows_for_family(family: u32) -> Vec<TcpRow> {
        let Some(buf) = get_table_bytes(true, family) else {
            return Vec::new();
        };
        if buf.is_empty() {
            return Vec::new();
        }
        let count = buf[0] as usize;
        let mut out = Vec::with_capacity(count);
        unsafe {
            if family == AF_INET.0 as u32 {
                let rows = buf.as_ptr().add(1) as *const MIB_TCPROW_OWNER_PID;
                for i in 0..count {
                    let r = &*rows.add(i);
                    out.push(TcpRow::v4(
                        r.dwLocalAddr,
                        r.dwLocalPort,
                        r.dwRemoteAddr,
                        r.dwRemotePort,
                        r.dwState,
                        r.dwOwningPid,
                    ));
                }
            } else {
                let rows = buf.as_ptr().add(1) as *const MIB_TCP6ROW_OWNER_PID;
                for i in 0..count {
                    let r = &*rows.add(i);
                    out.push(TcpRow::v6(
                        r.ucLocalAddr,
                        r.dwLocalPort,
                        r.ucRemoteAddr,
                        r.dwRemotePort,
                        r.dwState,
                        r.dwOwningPid,
                    ));
                }
            }
        }
        out
    }

    fn udp_rows_for_family(family: u32) -> Vec<UdpRow> {
        let Some(buf) = get_table_bytes(false, family) else {
            return Vec::new();
        };
        if buf.is_empty() {
            return Vec::new();
        }
        let count = buf[0] as usize;
        let mut out = Vec::with_capacity(count);
        unsafe {
            if family == AF_INET.0 as u32 {
                let rows = buf.as_ptr().add(1) as *const MIB_UDPROW_OWNER_PID;
                for i in 0..count {
                    let r = &*rows.add(i);
                    out.push(UdpRow::v4(r.dwLocalAddr, r.dwLocalPort, r.dwOwningPid));
                }
            } else {
                let rows = buf.as_ptr().add(1) as *const MIB_UDP6ROW_OWNER_PID;
                for i in 0..count {
                    let r = &*rows.add(i);
                    out.push(UdpRow::v6(r.ucLocalAddr, r.dwLocalPort, r.dwOwningPid));
                }
            }
        }
        out
    }

    pub fn tcp_rows_win() -> Vec<TcpRow> {
        let mut out = tcp_rows_for_family(AF_INET.0 as u32);
        out.extend(tcp_rows_for_family(AF_INET6.0 as u32));
        out
    }

    pub fn udp_rows_win() -> Vec<UdpRow> {
        let mut out = udp_rows_for_family(AF_INET.0 as u32);
        out.extend(udp_rows_for_family(AF_INET6.0 as u32));
        out
    }
}

#[cfg(windows)]
use win::{tcp_rows_win, udp_rows_win};

#[cfg(test)]
mod tests {
    use super::*;

    fn v4_row(local: [u8; 4], lport: u16, remote: [u8; 4], rport: u16, pid: u32) -> TcpRow {
        // Build from dotted quads the way the MIB stores them:
        // DWORD in network byte order.
        let la = u32::from_be_bytes(local);
        let ra = u32::from_be_bytes(remote);
        TcpRow::v4(
            la.to_be(),
            lport.to_be() as u32,
            ra.to_be(),
            rport.to_be() as u32,
            5, // ESTABLISHED
            pid,
        )
    }

    #[test]
    fn mib_byte_order_ipv4_and_ports() {
        // 10.0.0.5:52341 -> 93.184.216.34:443, as the MIB stores them.
        let row = v4_row([10, 0, 0, 5], 52341, [93, 184, 216, 34], 443, 1234);
        assert_eq!(row.local, IpAddr::from([10, 0, 0, 5]));
        assert_eq!(row.local_port, 52341);
        assert_eq!(row.remote, IpAddr::from([93, 184, 216, 34]));
        assert_eq!(row.remote_port, 443);
        assert_eq!(row.pid, 1234);
        assert!(!row.is_ipv6());
        // The raw DWORDs round-trip: SetTcpEntry gets exactly what
        // the table gave us.
        assert_eq!(ipv4_from_mib(row.raw_local_addr), row.local);
        assert_eq!(port_from_mib(row.raw_local_port), 52341);
        assert_eq!(row.key().protocol, 6);
        assert_eq!(row.key().local_port, 52341);
    }

    #[test]
    fn mib_byte_order_ipv6() {
        let local = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
        let remote = [
            0x26, 0x06, 0x47, 0x00, 0x47, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ];
        let row = TcpRow::v6(
            local,
            443u16.to_be() as u32,
            remote,
            8080u16.to_be() as u32,
            5,
            99,
        );
        assert_eq!(row.local, IpAddr::V6(Ipv6Addr::from(local)));
        assert_eq!(row.local_port, 443);
        assert_eq!(row.remote_port, 8080);
        assert!(row.is_ipv6());
    }

    #[test]
    fn build_snapshot_maps_tcp_exact_and_udp_by_local_socket() {
        let tcp = vec![
            v4_row([10, 0, 0, 5], 52341, [93, 184, 216, 34], 443, 1234),
            v4_row([10, 0, 0, 5], 52342, [1, 1, 1, 1], 80, 1234),
        ];
        let udp = vec![
            UdpRow::v4(
                u32::from_be_bytes([10, 0, 0, 5]).to_be(),
                53000u16.to_be() as u32,
                1234,
            ),
            UdpRow::v4(
                u32::from_be_bytes([10, 0, 0, 5]).to_be(),
                53001u16.to_be() as u32,
                777,
            ),
            UdpRow::v6(
                [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                53002u16.to_be() as u32,
                1234,
            ),
        ];
        let snap = build_snapshot(&tcp, &udp);
        assert_eq!(snap.tcp.len(), 2);
        let key = tcp[0].key();
        assert_eq!(snap.tcp.get(&key), Some(&1234));
        assert_eq!(
            snap.udp.get(&(IpAddr::from([10, 0, 0, 5]), 53000)),
            Some(&1234)
        );
        assert_eq!(
            snap.udp.get(&(IpAddr::from([10, 0, 0, 5]), 53001)),
            Some(&777)
        );
        assert_eq!(
            snap.udp.get(&(
                IpAddr::V6(Ipv6Addr::from([
                    0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1
                ])),
                53002
            )),
            Some(&1234)
        );
    }

    #[test]
    fn is_terminable_state_rules() {
        // 1 CLOSED, 2 LISTEN, 12 DELETE_TCB: not connections to kill.
        assert!(!is_terminable_state(1));
        assert!(!is_terminable_state(2));
        assert!(!is_terminable_state(12));
        assert!(!is_terminable_state(0));
        // Everything in between is a live connection.
        for s in 3..=11 {
            assert!(is_terminable_state(s), "state {s}");
        }
    }

    #[test]
    fn select_to_terminate_picks_only_the_apps_rows() {
        let rows = vec![
            v4_row([10, 0, 0, 5], 52341, [93, 184, 216, 34], 443, 1234), // game, ESTAB
            v4_row([10, 0, 0, 5], 52342, [1, 1, 1, 1], 80, 1234),        // game, ESTAB
            v4_row([10, 0, 0, 5], 52343, [8, 8, 8, 8], 53, 777),         // chrome: not the app
            TcpRow {
                state: 2, // LISTEN: same app, but not a connection
                ..v4_row([10, 0, 0, 5], 52344, [0, 0, 0, 0], 0, 1234)
            },
            TcpRow::v6(
                [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                443u16.to_be() as u32,
                [
                    0x26, 0x06, 0x47, 0x00, 0x47, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
                ],
                8080u16.to_be() as u32,
                5,
                1234,
            ), // game, IPv6: skipped, not terminated
        ];
        let pid_exe = |pid: u32| match pid {
            1234 => Some(r"C:\Games\Game.exe".to_string()),
            777 => Some(r"C:\Browser\chrome.exe".to_string()),
            _ => None,
        };
        let sel = select_to_terminate(r"c:\games\game.exe", &rows, &pid_exe);
        assert_eq!(sel.terminate.len(), 2);
        assert!(sel.terminate.iter().all(|r| !r.is_ipv6()));
        assert_eq!(sel.ipv6_skipped, 1);
        // Case-insensitive exe match; unknown PIDs never terminate.
        let sel2 = select_to_terminate(r"C:\GAMES\GAME.EXE", &rows, &pid_exe);
        assert_eq!(sel2.terminate.len(), 2);
        let sel3 = select_to_terminate(r"c:\nope.exe", &rows, &pid_exe);
        assert_eq!(sel3.terminate.len(), 0);
        assert_eq!(sel3.ipv6_skipped, 0);
    }

    #[test]
    fn attribution_order_flow_then_tcp_fallback_then_udp() {
        let flow_key = FlowKey::new(
            6,
            IpAddr::from([10, 0, 0, 5]),
            52341,
            IpAddr::from([93, 184, 216, 34]),
            443,
        );
        let udp_key = FlowKey::new(
            17,
            IpAddr::from([10, 0, 0, 5]),
            53000,
            IpAddr::from([8, 8, 8, 8]),
            53,
        );
        let mut flow = HashMap::new();
        flow.insert(flow_key, 1111); // FLOW layer knows this one
        let mut tcp_fb = HashMap::new();
        tcp_fb.insert(flow_key, 2222); // stale fallback entry loses
        let mut udp_fb = HashMap::new();
        udp_fb.insert((IpAddr::from([10, 0, 0, 5]), 53000), 3333);
        // FLOW wins over the fallback.
        assert_eq!(
            attribute_pid(&flow_key, &flow, &tcp_fb, &udp_fb),
            Some(1111)
        );
        // FLOW miss: TCP fallback answers (pre-existing flow).
        let pre_key = FlowKey::new(
            6,
            IpAddr::from([10, 0, 0, 5]),
            52342,
            IpAddr::from([1, 1, 1, 1]),
            80,
        );
        tcp_fb.insert(pre_key, 2222);
        assert_eq!(attribute_pid(&pre_key, &flow, &tcp_fb, &udp_fb), Some(2222));
        // UDP: no FLOW entry, no TCP fallback — local socket answers,
        // whatever remote the packet is going to.
        assert_eq!(attribute_pid(&udp_key, &flow, &tcp_fb, &udp_fb), Some(3333));
        let other_remote = FlowKey::new(
            17,
            IpAddr::from([10, 0, 0, 5]),
            53000,
            IpAddr::from([1, 1, 1, 1]),
            53,
        );
        assert_eq!(
            attribute_pid(&other_remote, &flow, &tcp_fb, &udp_fb),
            Some(3333)
        );
        // TCP packets never use the UDP map, even on a port match.
        let tcp_on_udp_port = FlowKey::new(
            6,
            IpAddr::from([10, 0, 0, 5]),
            53000,
            IpAddr::from([8, 8, 8, 8]),
            53,
        );
        assert_eq!(
            attribute_pid(&tcp_on_udp_port, &flow, &tcp_fb, &udp_fb),
            None
        );
        // Nothing anywhere: unattributed.
        let unknown = FlowKey::new(
            6,
            IpAddr::from([10, 0, 0, 5]),
            59999,
            IpAddr::from([9, 9, 9, 9]),
            443,
        );
        assert_eq!(attribute_pid(&unknown, &flow, &tcp_fb, &udp_fb), None);
    }

    #[test]
    fn fallback_pid_reads_the_shared_maps() {
        let tcp_fb: TcpFallbackMap = Arc::new(RwLock::new(HashMap::new()));
        let udp_fb: UdpFallbackMap = Arc::new(RwLock::new(HashMap::new()));
        let key = FlowKey::new(
            6,
            IpAddr::from([10, 0, 0, 5]),
            52341,
            IpAddr::from([93, 184, 216, 34]),
            443,
        );
        assert_eq!(fallback_pid(&key, &tcp_fb, &udp_fb), None);
        tcp_fb.write().unwrap().insert(key, 4242);
        assert_eq!(fallback_pid(&key, &tcp_fb, &udp_fb), Some(4242));
        let udp_key = FlowKey::new(
            17,
            IpAddr::from([10, 0, 0, 5]),
            53000,
            IpAddr::from([8, 8, 8, 8]),
            53,
        );
        udp_fb
            .write()
            .unwrap()
            .insert((IpAddr::from([10, 0, 0, 5]), 53000), 5555);
        assert_eq!(fallback_pid(&udp_key, &tcp_fb, &udp_fb), Some(5555));
    }

    #[test]
    fn terminate_report_summary_wording() {
        let name = "chrome.exe";
        assert_eq!(
            TerminateReport::default().summarize(name),
            "Blocked chrome.exe — no open connections found to terminate; new connections will be dropped."
        );
        let r = TerminateReport {
            tcp_terminated: 1,
            udp_flows: 0,
            tcp_ipv6_skipped: 0,
            tcp_failed: 0,
        };
        assert_eq!(
            r.summarize(name),
            "Terminated 1 open TCP connection for chrome.exe."
        );
        let r = TerminateReport {
            tcp_terminated: 3,
            udp_flows: 2,
            tcp_ipv6_skipped: 1,
            tcp_failed: 1,
        };
        assert_eq!(
            r.summarize(name),
            "Terminated 3 open TCP connections for chrome.exe; 2 UDP flows will be dropped; 1 IPv6 connection could not be terminated directly and will be dropped instead; 1 termination failed."
        );
    }
}
