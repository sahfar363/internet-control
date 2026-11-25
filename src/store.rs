//! Persistence: selected adapter + daily totals.
//!
//! Stored as JSON in `%APPDATA%\internet-control\config.json` on Windows
//! (or `$HOME/.config/internet-control/config.json` elsewhere).
//! Daily totals are keyed by calendar date and reset at midnight.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[cfg(not(windows))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(not(windows))]
use crate::meter::civil_from_days;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// LUID of the selected adapter, if the user picked one explicitly.
    pub selected_luid: Option<u64>,
    /// Friendly name of the selected adapter (shown / used as a fallback
    /// match if the LUID is gone).
    pub selected_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DailyTotals {
    /// `YYYY-MM-DD`.
    pub date: String,
    pub down_bytes: u64,
    pub up_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    pub config: Config,
    pub today: DailyTotals,
}

impl Store {
    pub fn data_dir() -> PathBuf {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.is_empty() {
                return PathBuf::from(appdata).join("internet-control");
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".config").join("internet-control");
        }
        PathBuf::from("internet-control")
    }

    pub fn path() -> PathBuf {
        Self::data_dir().join("config.json")
    }

    /// Today's date as `YYYY-MM-DD` in local time.
    #[cfg(windows)]
    pub fn today_date() -> String {
        // Local time via Win32, so "today" rolls over at local midnight.
        let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
    }

    /// Today's date as `YYYY-MM-DD` (UTC fallback on non-Windows builds,
    /// which are only used for `cargo test`).
    #[cfg(not(windows))]
    pub fn today_date() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let days = secs.div_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        format!("{y:04}-{m:02}-{d:02}")
    }

    pub fn load() -> Self {
        let path = Self::path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default_with_today();
        };
        let Ok(mut store) = serde_json::from_str::<Store>(&text) else {
            return Self::default_with_today();
        };
        // Rollover at midnight: a stored date that is not today resets.
        if store.today.date != Self::today_date() {
            store.today = DailyTotals {
                date: Self::today_date(),
                ..Default::default()
            };
        }
        store
    }

    fn default_with_today() -> Self {
        Self {
            config: Config::default(),
            today: DailyTotals {
                date: Self::today_date(),
                ..Default::default()
            },
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, text);
        }
    }
}
