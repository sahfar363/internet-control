//! Public IP + country lookup (v0.9).
//!
//! Sahand asked for the IP of the country the system is connected
//! to — the public IP as the internet sees it (his VPN exit IP +
//! country) — instead of the local adapter IP (e.g. PDaNet's
//! internal 10.1.19.2), which is what the IP line showed until now.
//!
//! A background thread fetches it roughly every 5 minutes from a
//! what-is-my-ip service and stores it in [`Shared`]; the bridge
//! copies the formatted line into the snapshot, and the UI shows
//! "Public IP: 1.2.3.4 (Country)" in both normal and Minimal view.
//! While offline or when the fetch fails it shows "Public IP:
//! unavailable" and falls back to the local adapter IP — the meter
//! itself never needs the internet and never blocks on this.
//!
//! Privacy: the lookup service necessarily sees the public IP —
//! that is what the service is for. No other data is sent: the
//! request is a plain GET with no body, no identifier and no
//! account. Two services are tried in order (ipapi.co, then
//! ip-api.com) so one being rate-limited does not break the line.

use std::sync::Arc;
use std::time::Duration;

/// How often the public IP is re-checked.
pub const LOOKUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The public IP as the internet sees it, plus the country it
/// resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicIp {
    pub ip: String,
    /// Country name, empty when the service did not report one.
    pub country: String,
}

impl PublicIp {
    /// Display form used by the UI: "1.2.3.4 (Japan)", or just the
    /// IP when no country was reported. Pure — unit-tested.
    pub fn display(&self) -> String {
        if self.country.is_empty() {
            self.ip.clone()
        } else {
            format!("{} ({})", self.ip, self.country)
        }
    }
}

/// Parse the ipapi.co/json/ response:
/// `{"ip":"1.2.3.4","country_name":"Japan","country_code":"JP",...}`.
/// The IP is required; a missing country just leaves it empty.
/// Pure — unit-tested, no network in tests.
pub fn parse_ipapi(text: &str) -> Option<PublicIp> {
    #[derive(serde::Deserialize)]
    struct IpApi {
        ip: Option<String>,
        country_name: Option<String>,
    }
    let v: IpApi = serde_json::from_str(text).ok()?;
    let ip = v.ip.filter(|s| !s.is_empty())?;
    Some(PublicIp {
        ip,
        country: v.country_name.unwrap_or_default(),
    })
}

/// Parse the ip-api.com/json/ response:
/// `{"status":"success","query":"1.2.3.4","country":"Japan",
/// "countryCode":"JP",...}`. Only a "success" status counts.
/// Pure — unit-tested, no network in tests.
pub fn parse_ip_api(text: &str) -> Option<PublicIp> {
    #[derive(serde::Deserialize)]
    struct IpApiCom {
        status: Option<String>,
        query: Option<String>,
        country: Option<String>,
    }
    let v: IpApiCom = serde_json::from_str(text).ok()?;
    if v.status.as_deref() != Some("success") {
        return None;
    }
    let ip = v.query.filter(|s| !s.is_empty())?;
    Some(PublicIp {
        ip,
        country: v.country.unwrap_or_default(),
    })
}

/// One lookup attempt: ipapi.co first, ip-api.com as a fallback.
/// Short timeouts so a dead service never stalls the thread.
/// `None` on any failure — the caller keeps the last good value.
///
/// Note on ureq 2.x: the `native-tls` feature does NOT make
/// `ureq::get()` do TLS — it only adds the adapter, so an `Agent`
/// has to be built with an explicit `TlsConnector` (see the ureq
/// docs: "native-tls is never picked up as a default"). The
/// `proxy-from-env` feature makes the agent honour `HTTP_PROXY` /
/// `HTTPS_PROXY` when set (corporate networks); without proxy env
/// vars it connects directly.
///
/// The whole attempt also runs on a worker thread with a hard
/// 12-second wall-clock guard (`recv_timeout`): ureq's 5-second
/// timeouts did NOT bound a stalled TLS handshake through this
/// sandbox's egress proxy (one attempt took ~251 s before failing),
/// so the guard is what actually keeps the lookup thread on
/// schedule. A stuck worker is abandoned — it exits on its own when
/// its socket errors — and the loop just tries again in 5 minutes.
/// The UI never blocks on any of this.
pub fn fetch_once() -> Option<PublicIp> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(fetch_once_inner());
    });
    rx.recv_timeout(Duration::from_secs(12)).ok().flatten()
}

fn fetch_once_inner() -> Option<PublicIp> {
    let connector = match ureq::native_tls::TlsConnector::new() {
        Ok(c) => std::sync::Arc::new(c),
        Err(e) => {
            crate::log::log(&format!("public IP: TLS connector failed: {e}"));
            return None;
        }
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(5))
        .tls_connector(connector)
        .build();
    for (url, parse) in [
        (
            "https://ipapi.co/json/",
            parse_ipapi as fn(&str) -> Option<PublicIp>,
        ),
        ("http://ip-api.com/json/", parse_ip_api),
    ] {
        match agent.get(url).call() {
            Ok(resp) => match resp.into_string() {
                Ok(text) => {
                    if let Some(ip) = parse(&text) {
                        return Some(ip);
                    }
                }
                Err(e) => crate::log::log(&format!("public IP {url}: read failed: {e}")),
            },
            Err(e) => crate::log::log(&format!("public IP {url}: fetch failed: {e}")),
        }
    }
    None
}

/// Background thread body: fetch now, then every 5 minutes. The
/// last good value stays in `Shared` when a fetch fails, so the UI
/// keeps showing the previous IP instead of flickering to
/// "unavailable" on one bad attempt.
pub fn run_loop(shared: Arc<crate::state::Shared>) {
    loop {
        match fetch_once() {
            Some(ip) => {
                crate::log::log(&format!("public IP: {} ({})", ip.ip, ip.country));
                if let Ok(mut slot) = shared.public_ip.write() {
                    *slot = Some(ip);
                }
            }
            None => crate::log::log("public IP: lookup failed, keeping last value"),
        }
        std::thread::sleep(LOOKUP_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipapi_parses_ip_and_country() {
        let ip = parse_ipapi(
            r#"{"ip":"203.0.113.7","city":"Tokyo","country_name":"Japan","country_code":"JP"}"#,
        )
        .unwrap();
        assert_eq!(ip.ip, "203.0.113.7");
        assert_eq!(ip.country, "Japan");
        assert_eq!(ip.display(), "203.0.113.7 (Japan)");
    }

    #[test]
    fn ipapi_without_country_still_parses() {
        let ip = parse_ipapi(r#"{"ip":"203.0.113.7"}"#).unwrap();
        assert_eq!(ip.display(), "203.0.113.7");
    }

    #[test]
    fn ipapi_without_ip_is_nothing() {
        assert!(parse_ipapi(r#"{"error":true,"reason":"RateLimited"}"#).is_none());
        assert!(parse_ipapi("not json").is_none());
        assert!(parse_ipapi(r#"{"ip":""}"#).is_none());
    }

    #[test]
    fn ip_api_parses_on_success_only() {
        let ip = parse_ip_api(
            r#"{"status":"success","country":"United States","countryCode":"US","query":"198.51.100.23"}"#,
        )
        .unwrap();
        assert_eq!(ip.ip, "198.51.100.23");
        assert_eq!(ip.country, "United States");
        assert_eq!(ip.display(), "198.51.100.23 (United States)");
        assert!(parse_ip_api(r#"{"status":"fail","message":"reserved range"}"#).is_none());
        assert!(parse_ip_api(r#"{"status":"success","query":""}"#).is_none());
    }
}
