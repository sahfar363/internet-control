//! Pure, platform-independent rate / total math.
//!
//! Everything in this file is testable on any OS (including Linux, where
//! `cargo test` runs). All Windows-only code lives in `adapters.rs` behind
//! `#[cfg(windows)]`.
//!
//! Core rules:
//! * Rates come from 64-bit octet counters (`ifHCInOctets` /
//!   `ifHCOutOctets`), never from 32-bit counters and never from a
//!   per-second perf-counter sample.
//! * rate = (new_counter - old_counter) / elapsed_seconds, where elapsed is
//!   measured with a monotonic clock (`std::time::Instant` in the sampler).
//! * If a counter goes backwards (`new < old`) the adapter was reset /
//!   re-created: re-baseline and report 0 for that sample. Never report a
//!   huge spike from a reset.
//! * A 64-bit counter practically never wraps (2^64 bytes), so a backwards
//!   counter is treated as a reset, not a wrap.
//! * Totals are for the SELECTED adapter only. Summing all adapters
//!   double-counts when the same bytes cross two adapters (e.g. VPN
//!   adapter + PDAnet tethering adapter).

/// Rate (bytes/second) from a counter delta.
///
/// Returns 0.0 when `elapsed_secs` is zero, negative, non-finite, or when
/// the counter went backwards (reset).
pub fn rate_from_delta(new_counter: u64, old_counter: u64, elapsed_secs: f64) -> f64 {
    if !elapsed_secs.is_finite() || elapsed_secs <= 0.0 {
        return 0.0;
    }
    // Counter reset (adapter restarted / recreated) saturates to 0:
    // re-baseline, never a spike.
    new_counter.saturating_sub(old_counter) as f64 / elapsed_secs
}

/// Byte delta for a sample, or 0 when the counter went backwards.
pub fn delta_bytes(new_counter: u64, old_counter: u64) -> u64 {
    new_counter.saturating_sub(old_counter)
}

/// Per-counter rate calculator: keeps the previous reading and turns each
/// new reading into a rate + a byte delta.
#[derive(Debug, Default, Clone)]
pub struct RateCalculator {
    last_in: Option<u64>,
    last_out: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct RateSample {
    /// Download rate, bytes/second.
    pub down_bps: f64,
    /// Upload rate, bytes/second.
    pub up_bps: f64,
    /// Bytes downloaded since the previous sample.
    pub down_delta: u64,
    /// Bytes uploaded since the previous sample.
    pub up_delta: u64,
    /// True when this sample was the first reading (no rate yet) or a
    /// counter reset (re-baselined, rate forced to 0).
    pub rebaselined: bool,
}

impl RateCalculator {
    #[allow(dead_code)] // used in tests; production code uses Default
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a new counter reading. `elapsed_secs` is the wall/monotonic time
    /// since the previous call to `update` for this same adapter.
    pub fn update(&mut self, in_octets: u64, out_octets: u64, elapsed_secs: f64) -> RateSample {
        let (Some(last_in), Some(last_out)) = (self.last_in, self.last_out) else {
            self.last_in = Some(in_octets);
            self.last_out = Some(out_octets);
            return RateSample {
                rebaselined: true,
                ..Default::default()
            };
        };
        let reset = in_octets < last_in || out_octets < last_out;
        let sample = RateSample {
            down_bps: rate_from_delta(in_octets, last_in, elapsed_secs),
            up_bps: rate_from_delta(out_octets, last_out, elapsed_secs),
            down_delta: delta_bytes(in_octets, last_in),
            up_delta: delta_bytes(out_octets, last_out),
            rebaselined: reset,
        };
        self.last_in = Some(in_octets);
        self.last_out = Some(out_octets);
        sample
    }
}

/// Format a rate (bytes/second) as e.g. `0 B/s`, `12.3 KB/s`, `4.56 MB/s`.
pub fn format_rate(bytes_per_sec: f64) -> String {
    format_bytes_per_sec(bytes_per_sec)
}

fn format_bytes_per_sec(v: f64) -> String {
    const UNITS: [&str; 5] = ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"];
    format_scaled(v, &UNITS)
}

/// Format a byte total as e.g. `0 B`, `12.3 KB`, `4.56 MB`.
pub fn format_bytes(bytes: u64) -> String {
    format_scaled(bytes as f64, &["B", "KB", "MB", "GB", "TB"])
}

fn format_scaled(v: f64, units: &[&str]) -> String {
    let mut v = v;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", units[i])
    } else {
        format!("{v:.2} {}", units[i])
    }
}

/// Days since the Unix epoch (1970-01-01) -> civil date (year, month, day).
/// Howard Hinnant's algorithm; no chrono dependency needed for this.
#[allow(dead_code)] // used by the non-Windows (test) date path
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_from_a_counter_delta() {
        // 1_000_000 bytes in 0.1 s = 10_000_000 B/s.
        assert!((rate_from_delta(2_000_000, 1_000_000, 0.1) - 10_000_000.0).abs() < 1.0);
        // 1 KB in 1 s.
        assert!((rate_from_delta(1_024, 0, 1.0) - 1024.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rate_is_zero_when_no_time_passed() {
        assert_eq!(rate_from_delta(2_000, 1_000, 0.0), 0.0);
        assert_eq!(rate_from_delta(2_000, 1_000, -1.0), 0.0);
        assert_eq!(rate_from_delta(2_000, 1_000, f64::NAN), 0.0);
        assert_eq!(rate_from_delta(2_000, 1_000, f64::INFINITY), 0.0);
    }

    #[test]
    fn rate_is_zero_and_no_spike_on_counter_reset() {
        // Counter went backwards: adapter reset. Must NOT report a spike.
        assert_eq!(rate_from_delta(100, 1_000_000, 0.1), 0.0);
        assert_eq!(delta_bytes(100, 1_000_000), 0);
    }

    #[test]
    fn calculator_rebaselines_on_first_sample() {
        let mut c = RateCalculator::new();
        let s = c.update(5_000, 2_000, 0.1);
        assert_eq!(s.down_bps, 0.0);
        assert_eq!(s.up_bps, 0.0);
        assert!(s.rebaselined);
    }

    #[test]
    fn calculator_computes_rate_on_second_sample() {
        let mut c = RateCalculator::new();
        let _ = c.update(1_000_000, 500_000, 0.1);
        let s = c.update(1_100_000, 510_000, 0.1);
        assert!((s.down_bps - 1_000_000.0).abs() < 1.0);
        assert!((s.up_bps - 100_000.0).abs() < 1.0);
        assert_eq!(s.down_delta, 100_000);
        assert_eq!(s.up_delta, 10_000);
        assert!(!s.rebaselined);
    }

    #[test]
    fn calculator_no_spike_on_reset() {
        let mut c = RateCalculator::new();
        let _ = c.update(1_000_000, 500_000, 0.1);
        // Adapter recreated: counters start near zero again.
        let s = c.update(10, 5, 0.1);
        assert_eq!(s.down_bps, 0.0);
        assert_eq!(s.up_bps, 0.0);
        assert_eq!(s.down_delta, 0);
        assert!(s.rebaselined);
        // And the next sample after the reset measures normally again.
        let s2 = c.update(100_010, 10_005, 0.1);
        assert!((s2.down_bps - 1_000_000.0).abs() < 1.0);
        assert!(!s2.rebaselined);
    }

    #[test]
    fn format_rate_scales() {
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(512.0), "512 B/s");
        assert_eq!(format_rate(1536.0), "1.50 KB/s");
        assert_eq!(format_rate(2_500_000.0), "2.38 MB/s");
    }

    #[test]
    fn format_bytes_scales() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(104_857_600), "100.00 MB");
    }

    #[test]
    fn civil_date_known_values() {
        // 2026-11-25 = 20_782 days after 1970-01-01.
        assert_eq!(civil_from_days(20_782), (2026, 11, 25));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}
