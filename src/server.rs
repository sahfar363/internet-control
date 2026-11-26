//! Local HTTP server for the v0.4 WebView2 UI (and browser fallback).
//!
//! Serves the embedded HTML/CSS/JS on `http://127.0.0.1:<port>/` and a
//! small JSON API the page polls:
//!
//! * `GET  /api/state` — the whole [`crate::state::StateJson`].
//! * `POST /api/block`   `{ "exe": "...", "block": true|false }`
//! * `POST /api/select`  `{ "luid": 123 }`
//! * `POST /api/startup` `{ "enabled": true|false }`
//! * `POST /api/minimal` `{ "enabled": true|false }`
//! * `POST /api/view`    `{ "view": "rate"|"total" }`
//! * `POST /api/sort`    `{ "sort": "rate"|"total"|"name" }`
//!
//! Deliberately dependency-free (`std::net::TcpListener` only): the UI
//! must work offline on a plain Windows PC, and the server is also
//! what the Linux `cargo test` run exercises. Only loopback clients
//! are accepted (the listener binds 127.0.0.1). Blocking is WinDivert
//! intercept only — see `capture.rs` — and the Block buttons refuse
//! to act when intercept mode is unavailable, so a failed block is
//! never silent: the API answers `can_block: false` and the UI says so.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use crate::state::{AppView, Shared, SortMode};

const INDEX_HTML: &str = include_str!("../assets/index.html");
const STYLE_CSS: &str = include_str!("../assets/style.css");
const APP_JS: &str = include_str!("../assets/app.js");

/// Start the server on an ephemeral loopback port. Returns the port.
/// Runs until the process exits.
pub fn serve(shared: Arc<Shared>) -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let shared = shared.clone();
                    std::thread::spawn(move || handle(s, &shared));
                }
                Err(_) => continue,
            }
        }
    });
    Ok(port)
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    // Read until the header terminator, then the body (Content-Length).
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    break pos + 4;
                }
                if buf.len() > 65536 {
                    return None; // absurd header: drop it
                }
            }
            Err(_) => return None,
        }
    };
    let header = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = header.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    body.truncate(content_length);
    Some((method, path, body))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn json(stream: &mut TcpStream, status: u16, body: &str) {
    respond(stream, status, "application/json", body.as_bytes());
}

fn handle(mut stream: TcpStream, shared: &Arc<Shared>) {
    let Some((method, path, body)) = read_request(&mut stream) else {
        return;
    };
    // Static assets.
    if method == "GET" {
        match path.as_str() {
            "/" | "/index.html" => {
                respond(
                    &mut stream,
                    200,
                    "text/html; charset=utf-8",
                    INDEX_HTML.as_bytes(),
                );
                return;
            }
            "/style.css" => {
                respond(
                    &mut stream,
                    200,
                    "text/css; charset=utf-8",
                    STYLE_CSS.as_bytes(),
                );
                return;
            }
            "/app.js" => {
                respond(
                    &mut stream,
                    200,
                    "text/javascript; charset=utf-8",
                    APP_JS.as_bytes(),
                );
                return;
            }
            "/api/state" => {
                let snap = shared.snapshot.read().map(|s| s.clone());
                match snap {
                    Ok(s) => match serde_json::to_string(&s) {
                        Ok(text) => json(&mut stream, 200, &text),
                        Err(e) => json(&mut stream, 400, &format!("{{\"error\":\"{e}\"}}")),
                    },
                    Err(_) => json(&mut stream, 400, "{\"error\":\"state lock poisoned\"}"),
                }
                return;
            }
            _ => {
                respond(&mut stream, 404, "text/plain", b"not found");
                return;
            }
        }
    }
    if method != "POST" {
        respond(&mut stream, 405, "text/plain", b"method not allowed");
        return;
    }
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            json(&mut stream, 400, "{\"error\":\"invalid JSON body\"}");
            return;
        }
    };
    match path.as_str() {
        "/api/block" => {
            let Some(exe) = value.get("exe").and_then(|v| v.as_str()) else {
                json(&mut stream, 400, "{\"error\":\"missing exe\"}");
                return;
            };
            let block = value.get("block").and_then(|v| v.as_bool()).unwrap_or(true);
            // Never silently fail: when intercept mode is unavailable
            // the Block buttons must say so — the UI disables them
            // from can_block, and the API refuses as well.
            let can_block = shared.snapshot.read().map(|s| s.can_block).unwrap_or(false);
            if block && !can_block {
                json(
                    &mut stream,
                    400,
                    "{\"error\":\"blocking needs WinDivert intercept mode\"}",
                );
                return;
            }
            let lower = exe.to_lowercase();
            if let Ok(mut set) = shared.blocked.write() {
                if block {
                    set.insert(lower.clone());
                } else {
                    set.remove(&lower);
                }
            }
            // Persist the list (blocking itself only applies while
            // this app is running — see capture.rs).
            {
                let mut store = shared.store.lock().unwrap_or_else(|e| e.into_inner());
                if block {
                    if !store
                        .config
                        .blocked_exes
                        .iter()
                        .any(|e| e.eq_ignore_ascii_case(exe))
                    {
                        store.config.blocked_exes.push(exe.to_string());
                    }
                } else {
                    store
                        .config
                        .blocked_exes
                        .retain(|e| !e.eq_ignore_ascii_case(exe));
                }
                store.save();
            }
            // v0.5: a Block click also terminates the app's EXISTING
            // TCP connections at once (SetTcpEntry) instead of only
            // dropping new ones — and the fallback map already drops
            // its pre-existing flows. The result is shown as a note
            // in the UI (and in the log). Unblock clears the note.
            if block {
                let report = crate::connections::terminate_app_connections(exe);
                let note = report.summarize(&crate::adapters::display_name_for(exe));
                crate::log::log(&format!("Block {exe}: {note}"));
                if let Ok(mut n) = shared.block_note.lock() {
                    *n = Some(note);
                }
            } else if let Ok(mut n) = shared.block_note.lock() {
                *n = None;
            }
            json(&mut stream, 200, "{\"ok\":true}");
        }
        "/api/select" => {
            let Some(luid) = value.get("luid").and_then(|v| v.as_u64()) else {
                json(&mut stream, 400, "{\"error\":\"missing luid\"}");
                return;
            };
            if let Ok(mut req) = shared.select_request.lock() {
                *req = Some(luid);
            }
            json(&mut stream, 200, "{\"ok\":true}");
        }
        "/api/startup" => {
            let enabled = value
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Ok(mut req) = shared.startup_request.lock() {
                *req = Some(enabled);
            }
            json(&mut stream, 200, "{\"ok\":true}");
        }
        "/api/minimal" => {
            let enabled = value
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Ok(mut req) = shared.minimal_request.lock() {
                *req = Some(enabled);
            }
            json(&mut stream, 200, "{\"ok\":true}");
        }
        "/api/view" => {
            let view = match value.get("view").and_then(|v| v.as_str()) {
                Some("total") => AppView::Total,
                _ => AppView::Rate,
            };
            if let Ok(mut req) = shared.view_request.lock() {
                *req = Some(view);
            }
            json(&mut stream, 200, "{\"ok\":true}");
        }
        "/api/sort" => {
            let mode = match value.get("sort").and_then(|v| v.as_str()) {
                Some("total") => SortMode::Total,
                Some("name") => SortMode::Name,
                _ => SortMode::Rate,
            };
            if let Ok(mut req) = shared.sort_request.lock() {
                *req = Some(mode);
            }
            json(&mut stream, 200, "{\"ok\":true}");
        }
        _ => {
            respond(&mut stream, 404, "text/plain", b"not found");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    fn get(port: u16, path: &str) -> (String, Vec<u8>) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        let split = find_subslice(&out, b"\r\n\r\n").unwrap() + 4;
        (
            String::from_utf8_lossy(&out[..split]).into_owned(),
            out[split..].to_vec(),
        )
    }

    fn post(port: u16, path: &str, body: &str) -> (String, Vec<u8>) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        let split = find_subslice(&out, b"\r\n\r\n").unwrap() + 4;
        (
            String::from_utf8_lossy(&out[..split]).into_owned(),
            out[split..].to_vec(),
        )
    }

    fn test_shared() -> Arc<Shared> {
        Arc::new(Shared::new(Store::default()))
    }

    #[test]
    fn serves_index_and_state_json() {
        let shared = test_shared();
        let port = serve(shared.clone()).unwrap();
        let (header, body) = get(port, "/");
        assert!(header.contains("200 OK"), "{header}");
        assert!(header.contains("text/html"), "{header}");
        let html = String::from_utf8(body).unwrap();
        assert!(html.contains("Internet Control"), "index html");
        assert!(html.contains("app.js"), "index references app.js");
        let (header, body) = get(port, "/api/state");
        assert!(header.contains("200 OK"), "{header}");
        assert!(header.contains("application/json"), "{header}");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(v.get("top").is_some(), "state has top");
        assert!(v.get("adapters").is_some(), "state has adapters");
        assert!(v.get("apps").is_some(), "state has apps");
        assert!(v.get("diagnostics").is_some(), "state has diagnostics");
        assert!(v["diagnostics"].get("capture_totals").is_some());
    }

    #[test]
    fn serves_css_and_js() {
        let shared = test_shared();
        let port = serve(shared.clone()).unwrap();
        let (header, body) = get(port, "/style.css");
        assert!(header.contains("200 OK"), "{header}");
        assert!(!body.is_empty());
        let (header, body) = get(port, "/app.js");
        assert!(header.contains("200 OK"), "{header}");
        let js = String::from_utf8(body).unwrap();
        assert!(js.contains("api/state"), "js polls state");
    }

    #[test]
    fn block_requires_intercept_and_persists_the_list() {
        let shared = test_shared();
        let port = serve(shared.clone()).unwrap();
        // can_block is false in the fresh snapshot: a block must be refused.
        let (header, body) = post(
            port,
            "/api/block",
            r#"{"exe":"C:\\Apps\\game.exe","block":true}"#,
        );
        assert!(header.contains("400"), "{header}");
        assert!(
            String::from_utf8(body).unwrap().contains("intercept"),
            "refusal names intercept mode"
        );
        assert!(
            shared.blocked.read().unwrap().is_empty(),
            "refused block must not enter the set"
        );
        // Unblock of a non-blocked app is a harmless no-op.
        let (header, _) = post(
            port,
            "/api/block",
            r#"{"exe":"C:\\Apps\\game.exe","block":false}"#,
        );
        assert!(header.contains("200 OK"), "{header}");
    }

    #[test]
    fn select_view_sort_minimal_requests_land() {
        let shared = test_shared();
        let port = serve(shared.clone()).unwrap();
        let (header, _) = post(port, "/api/select", r#"{"luid":12345}"#);
        assert!(header.contains("200 OK"), "{header}");
        assert_eq!(*shared.select_request.lock().unwrap(), Some(12345));
        let (header, _) = post(port, "/api/view", r#"{"view":"total"}"#);
        assert!(header.contains("200 OK"), "{header}");
        assert_eq!(*shared.view_request.lock().unwrap(), Some(AppView::Total));
        let (header, _) = post(port, "/api/sort", r#"{"sort":"name"}"#);
        assert!(header.contains("200 OK"), "{header}");
        assert_eq!(*shared.sort_request.lock().unwrap(), Some(SortMode::Name));
        let (header, _) = post(port, "/api/minimal", r#"{"enabled":true}"#);
        assert!(header.contains("200 OK"), "{header}");
        assert_eq!(*shared.minimal_request.lock().unwrap(), Some(true));
    }

    #[test]
    fn unknown_paths_404() {
        let shared = test_shared();
        let port = serve(shared.clone()).unwrap();
        let (header, _) = get(port, "/nope");
        assert!(header.contains("404"), "{header}");
    }

    // v0.5.2: Minimal mode must keep the live download/upload graph
    // (and the v0.5.1 window-failure banner). The graph used to be
    // hidden by the Minimal hide-list in style.css, so pin the
    // wiring on the embedded assets: the canvas exists in the page,
    // the hide-list does not mention it or the banner, an explicit
    // minimal-mode rule gives the canvas its 120px height, and the
    // state JSON always carries the history the lines are drawn from.
    #[test]
    fn minimal_mode_keeps_graph_and_banner() {
        assert!(
            INDEX_HTML.contains("<canvas id=\"graph\""),
            "index.html must contain the graph canvas"
        );
        assert!(
            INDEX_HTML.contains("id=\"windowBanner\""),
            "index.html must contain the window-failure banner"
        );
        // The Minimal hide-list is the selector list starting at
        // `body.minimal .tabs,` and ending at the
        // `{ display: none !important; }` rule. Neither the graph
        // nor the banner may be among its selectors.
        let hide_start = STYLE_CSS
            .find("body.minimal .tabs,")
            .expect("minimal hide-list must exist");
        let hide_end = STYLE_CSS[hide_start..]
            .find("{ display: none !important; }")
            .expect("minimal hide rule must exist")
            + hide_start;
        let hide_list = &STYLE_CSS[hide_start..hide_end];
        assert!(
            !hide_list.contains("#graph"),
            "minimal must not hide #graph"
        );
        assert!(
            !hide_list.contains("windowBanner"),
            "minimal must not hide #windowBanner"
        );
        assert!(
            STYLE_CSS.contains("body.minimal #graph { display: block; height: 120px; }"),
            "minimal mode must give the graph an explicit height"
        );
        assert!(
            APP_JS.contains("drawGraph();"),
            "render() must keep drawing the graph every poll"
        );
        // The history the lines are drawn from is served regardless
        // of mode — minimal only changes what the page shows.
        let shared = test_shared();
        let port = serve(shared.clone()).unwrap();
        let (_, body) = get(port, "/api/state");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("history_down").is_some(), "state has history_down");
        assert!(v.get("history_up").is_some(), "state has history_up");
    }
}
