//! WebView2 (Edge) window for the v0.4 UI — Windows only.
//!
//! A plain Win32 window hosting a WebView2 control (adapted from the
//! `webview2-com` crate's own `examples/sample.rs` pattern: async
//! environment/controller creation with `wait_for_async_operation`,
//! then a classic `GetMessageW` loop). The page itself is served by
//! the loopback HTTP server in `server.rs`; the UI talks to it with
//! `fetch()`, so no JS bindings are needed here.
//!
//! v0.4.1 navigation flow (the v0.4.0 white-page fix): the window is
//! shown FIRST and the control bounds re-applied, then navigation
//! starts. NavigationCompleted is checked for real success —
//! `IsSuccess` + `WebErrorStatus` — with a 15 s watchdog. A failed
//! or timed-out navigation closes this window and returns an error
//! so `main` falls back to the default browser, instead of leaving
//! a blank white page with no clue why. Every outcome is written
//! to `internet-control.log` (see `log.rs`).
//!
//! If the WebView2 runtime is missing (no Edge / no Evergreen
//! runtime), environment creation fails and `run()` returns an `Err`
//! — `main` then falls back to opening the same URL in the default
//! browser. `WebView2Loader.dll` is shipped next to the exe by the
//! release workflow; without it the loader cannot find the runtime.
//!
//! v0.5.1: the `Err` string `run()` returns is the failure reason
//! `main` records in the shared state, so the browser page can show
//! it as a banner instead of the user hunting for the log file.
//!
//! v0.5.2: registering the `NavigationCompleted` handler can fail
//! with ERROR_INVALID_STATE (0x8007139F) when the WebView2 browser
//! process is not up yet at registration time. That is the browser
//! process's state, not a wrong order on our side — the sequence
//! (add, Navigate, wait, remove) matches the `webview2-com` crate's
//! own sample, and `add_NavigationCompleted` is a thin COM call
//! straight into Edge's implementation. `Navigate` is the call that
//! (re)starts the browser process, so a failed registration is no
//! longer fatal: the code navigates anyway, retries the registration
//! once after navigating, and only falls back to the browser if
//! `Navigate` itself fails. If the handler never registers, the
//! window stays open without the page-loaded check instead of
//! dropping to the browser. The keep-vs-fallback decision is the
//! pure `decide_first_nav` below, unit tested without any COM.
//!
//! v0.5.3: on Sahand's PC `Navigate` itself then failed with the
//! SAME HRESULT (0x8007139F) — so the browser process is not just
//! late, it never starts at all, and the default (crate-chosen)
//! user-data folder is the prime suspect (a stale / locked profile
//! from a crashed first run). The environment is now created with
//! an EXPLICIT user-data folder,
//! `%LOCALAPPDATA%\InternetControl\WebView2`, and when the window
//! fails with 0x8007139F the app moves that folder aside and
//! retries once with a fresh profile. The available runtime version
//! (`GetAvailableCoreWebView2BrowserVersionString`) is logged at
//! startup and appended to the failure reason, so the banner shows
//! the Edge version too — and says explicitly when no runtime is
//! installed at all (then the fix is installing the Evergreen
//! WebView2 Runtime). Only our own explicit folder is ever touched,
//! never any shared Edge folder; the webview only ever shows our
//! own local page, so its user data is disposable.
//!
//! v0.5.4: the v0.5.3 retry itself was broken, and the banner proved
//! it — "WebView2 controller creation failed: TaskCanceled
//! [WebView2 runtime 158.0.4370.33]". The runtime is present and
//! current, so Edge is fine. `TaskCanceled` comes from
//! `wait_with_pump` (see the `webview2-com` crate): it returns it
//! when `GetMessage` retrieves WM_QUIT. The only WM_QUIT source is
//! our own `window_proc`: attempt 1's window is destroyed on a
//! retryable failure (`Fallback` -> `destroy_window`), WM_DESTROY
//! posts WM_QUIT, and nobody pumps messages between the destroy and
//! the retry — so the stale quit message is still queued when
//! attempt 2 starts pumping in `wait_for_async_operation`, and the
//! wait dies at once. (Checked, not assumed: each attempt creates a
//! FRESH window in `WebView::create` — the retry never reuses a
//! destroyed HWND; the poison is the queued WM_QUIT, which also
//! explains why the window flashes open for a split second and
//! then closes: that is attempt 1 being shown and destroyed.)
//! The fix drains stale WM_QUIT messages before every attempt
//! (`drain_quit_messages`, `PeekMessageW` with a WM_QUIT filter),
//! pauses ~500 ms before the retry so a crashed Edge process can
//! exit and release its locks, and records every attempt in the
//! log and in the banner reason, so the next banner shows what
//! happened on each try.
//!
//! v0.5.5: the v0.5.4 drain worked — the retry now actually runs
//! (no more TaskCanceled) — but BOTH attempts still fail at
//! Navigate with the same 0x8007139F, with the runtime present and
//! current (158) and a fresh profile. So Edge's browser process
//! itself will not stay alive on Sahand's PC: most likely it
//! crashes at startup (GPU process) or is blocked (sandbox / AV).
//! The window now escalates across up to three attempts: plain,
//! then a fresh profile with `--disable-gpu`, then a fresh profile
//! with `--disable-gpu --no-sandbox` (last resort — the page is
//! local-only). A best-effort ProcessFailed handler is registered
//! right after obtaining the CoreWebView2, so the log says why the
//! process died (kind / reason / exit code) instead of guessing.

/// Plain-data outcome of the first navigation attempt (v0.5.2).
///
/// Deliberately platform-independent — no COM involved — so the
/// keep-vs-fallback decision (`decide_first_nav`) is unit testable
/// on any platform. `imp::WebView::run` (Windows-only) fills it in
/// from the real calls.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum FirstNav {
    /// `Navigate` itself failed, e.g. the webview is unusable.
    NavigateFailed(String),
    /// `Navigate` started but no `NavigationCompleted` handler could
    /// be registered (registration failed twice), so the navigation
    /// was never observed. This is NOT a failure: the page still
    /// renders, so the window is kept.
    Unobserved,
    /// `NavigationCompleted` fired with `IsSuccess == true`.
    Succeeded { http: i32 },
    /// `NavigationCompleted` fired with `IsSuccess == false`.
    Failed { web_error_status: i32, http: i32 },
    /// `NavigationCompleted` never fired within the 15 s watchdog.
    TimedOut,
    /// The user closed the window while waiting for navigation
    /// (`TaskCanceled` from `wait_with_pump`).
    WaitCancelled,
    /// The wait itself failed for another reason.
    WaitFailed(String),
}

/// What to do after the first navigation attempt.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum FirstNavDecision {
    /// Keep the app window open and run its message loop.
    KeepWindow,
    /// The window is already closed (user closed it while waiting):
    /// exit cleanly. Must NOT enter the message loop — `wait_with_pump`
    /// already consumed the WM_QUIT, so GetMessage would block forever.
    ExitCleanly,
    /// Close the window; the caller falls back to the browser.
    /// Carries the reason string (banner / log).
    Fallback(String),
}

/// Decide keep-vs-fallback after the first navigation attempt.
/// Pure: no COM, no I/O.
#[cfg(any(windows, test))]
fn decide_first_nav(nav: &FirstNav) -> FirstNavDecision {
    match nav {
        FirstNav::NavigateFailed(e) => FirstNavDecision::Fallback(e.clone()),
        FirstNav::Unobserved => FirstNavDecision::KeepWindow,
        FirstNav::Succeeded { .. } => FirstNavDecision::KeepWindow,
        FirstNav::Failed {
            web_error_status,
            http,
        } => FirstNavDecision::Fallback(format!(
            "WebView2 navigation failed (WebErrorStatus {web_error_status}, HTTP {http}); see internet-control.log"
        )),
        FirstNav::TimedOut => FirstNavDecision::Fallback(
            "WebView2 navigation timed out after 15 s; see internet-control.log".to_string(),
        ),
        FirstNav::WaitCancelled => FirstNavDecision::ExitCleanly,
        FirstNav::WaitFailed(e) => FirstNavDecision::Fallback(e.clone()),
    }
}

/// ERROR_INVALID_STATE as printed by `windows::core::Error`'s Debug
/// impl: `Error { code: HRESULT(0x8007139F), message: "…" }`.
/// The WebView2 browser process is down / never started. Both
/// `add_NavigationCompleted` (v0.5.2) and `Navigate` (v0.5.3) fail
/// with this on Sahand's PC.
#[cfg(any(windows, test))]
const HRESULT_INVALID_STATE: &str = "0x8007139f";

/// True when `reason` carries ERROR_INVALID_STATE (0x8007139F).
/// Pure: no COM, no I/O. Case-insensitive — the HRESULT is printed
/// in upper-case hex, but never trust a format string.
#[cfg(any(windows, test))]
fn is_invalid_state_error(reason: &str) -> bool {
    reason.to_ascii_lowercase().contains(HRESULT_INVALID_STATE)
}

/// Maximum window attempts (v0.5.5).
#[cfg(any(windows, test))]
const MAX_WINDOW_ATTEMPTS: usize = 3;

/// Additional browser arguments for window attempt N (1-based,
/// v0.5.5). Attempt 1 is plain, exactly as before. Attempt 2 adds
/// `--disable-gpu` — the browser process may be crashing in the
/// GPU process. Attempt 3 adds `--no-sandbox` as well — a last
/// resort for a sandbox / AV that kills the process; the page is
/// local-only, so this is a diagnostic fallback for a window that
/// otherwise cannot open at all. Pure: no COM, no I/O.
#[cfg(any(windows, test))]
fn browser_args_for_attempt(attempt_no: usize) -> &'static str {
    match attempt_no {
        2 => "--disable-gpu",
        3 => "--disable-gpu --no-sandbox",
        _ => "",
    }
}

/// Retry the window with a fresh profile (and, from attempt 2,
/// escalating browser arguments) only for ERROR_INVALID_STATE,
/// and only up to MAX_WINDOW_ATTEMPTS attempts total — any other
/// failure goes straight to the browser. Pure: no COM, no I/O.
#[cfg(any(windows, test))]
fn should_retry_window(reason: &str, attempts_used: usize) -> bool {
    attempts_used < MAX_WINDOW_ATTEMPTS && is_invalid_state_error(reason)
}

/// One window attempt's outcome, for the banner history (v0.5.5).
/// `args` is the additional browser arguments that attempt used
/// ("" for the plain first attempt).
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttemptRecord {
    error: String,
    args: &'static str,
}

/// `" [args: ...]"` suffix for the banner history, empty when the
/// attempt used no additional browser arguments. Pure.
#[cfg(any(windows, test))]
fn args_suffix(args: &str) -> String {
    if args.is_empty() {
        String::new()
    } else {
        format!(" [args: {args}]")
    }
}

/// Human-readable name for a COREWEBVIEW2_PROCESS_FAILED_KIND
/// value (v0.5.5, for the ProcessFailed log lines). Pure.
#[cfg(any(windows, test))]
fn process_failed_kind_name(kind: i32) -> String {
    match kind {
        0 => "browser process exited".to_string(),
        1 => "render process exited".to_string(),
        2 => "render process unresponsive".to_string(),
        3 => "frame render process exited".to_string(),
        4 => "utility process exited".to_string(),
        5 => "sandbox helper process exited".to_string(),
        6 => "GPU process exited".to_string(),
        7 => "PPAPI plugin process exited".to_string(),
        8 => "PPAPI broker process exited".to_string(),
        9 => "unknown process exited".to_string(),
        other => format!("process kind {other}"),
    }
}

/// Human-readable name for a COREWEBVIEW2_PROCESS_FAILED_REASON
/// value (v0.5.5, for the ProcessFailed log lines). Pure.
#[cfg(any(windows, test))]
fn process_failed_reason_name(reason: i32) -> String {
    match reason {
        0 => "unexpected".to_string(),
        1 => "unresponsive".to_string(),
        2 => "terminated".to_string(),
        3 => "crashed".to_string(),
        4 => "launch failed".to_string(),
        5 => "out of memory".to_string(),
        6 => "profile deleted".to_string(),
        other => format!("reason {other}"),
    }
}

/// `%LOCALAPPDATA%\InternetControl\WebView2` — our own explicit
/// WebView2 user-data folder (v0.5.3). Pure: the base is passed in
/// so it is testable on any platform.
#[cfg(any(windows, test))]
fn user_data_dir_for(base: &std::path::Path) -> std::path::PathBuf {
    base.join("InternetControl").join("WebView2")
}

/// Backup name for a stale profile: `WebView2.bak-<unix secs>`,
/// next to the profile folder. Pure.
#[cfg(any(windows, test))]
fn backup_dir_for(dir: &std::path::Path, timestamp_secs: u64) -> std::path::PathBuf {
    let mut name = dir
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".bak-{timestamp_secs}"));
    dir.with_file_name(name)
}

/// Folder to retry with after a failed profile (v0.5.3). When the
/// stale folder could be moved aside, retry with the original path
/// — it is a clean slate now. When it could NOT be moved (locked by
/// a crashed Edge process, or missing), use a fresh suffixed folder
/// (`WebView2-<unix secs>`) and never touch the old one. Pure.
#[cfg(any(windows, test))]
fn retry_dir_for(
    dir: &std::path::Path,
    timestamp_secs: u64,
    rename_succeeded: bool,
) -> std::path::PathBuf {
    if rename_succeeded {
        dir.to_path_buf()
    } else {
        let mut name = dir
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(format!("-{timestamp_secs}"));
        dir.with_file_name(name)
    }
}

/// Append the runtime version to a window-failure reason, so the
/// v0.5.1 banner shows it. When no runtime was found, say so
/// explicitly — then the fix is installing the Evergreen WebView2
/// Runtime. Pure.
#[cfg(any(windows, test))]
fn failure_with_runtime_info(reason: &str, runtime_version: Option<&str>) -> String {
    match runtime_version {
        Some(v) => format!("{reason} [WebView2 runtime {v}]"),
        None => format!(
            "{reason} [WebView2 runtime not found — install the Evergreen WebView2 Runtime]"
        ),
    }
}

/// Combine every attempt's failure into the final banner reason
/// (v0.5.4), so the next banner shows what happened on each try,
/// including which additional browser arguments each attempt used
/// (v0.5.5). One attempt: its error unchanged. Several: the last
/// error first, then the earlier attempts numbered in parentheses.
/// Pure: no COM, no I/O.
#[cfg(any(windows, test))]
fn format_attempt_history(attempts: &[AttemptRecord]) -> String {
    match attempts.len() {
        0 => "WebView2 window failed".to_string(),
        1 => format!("{}{}", attempts[0].error, args_suffix(attempts[0].args)),
        _ => {
            let last = attempts.last().expect("non-empty");
            let earlier: Vec<String> = attempts[..attempts.len() - 1]
                .iter()
                .enumerate()
                .map(|(i, a)| format!("attempt {}: {}{}", i + 1, a.error, args_suffix(a.args)))
                .collect();
            format!(
                "{}{} (earlier attempts: {})",
                last.error,
                args_suffix(last.args),
                earlier.join("; ")
            )
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::mem;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use webview2_com::Microsoft::Web::WebView2::Win32::*;
    use webview2_com::*;
    use windows::core::*;
    use windows::Win32::Foundation::{E_POINTER, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::LibraryLoader;
    use windows::Win32::UI::HiDpi;
    use windows::Win32::UI::WindowsAndMessaging::{self, MSG, WINDOW_LONG_PTR_INDEX, WNDCLASSW};

    /// Result of waiting for the first navigation: Done(success,
    /// WebErrorStatus, HTTP status), or Timeout when
    /// NavigationCompleted never fired within 15 s.
    enum NavResult {
        Done(bool, i32, i32),
        Timeout,
    }

    /// Explicit user-data folder (v0.5.3):
    /// `%LOCALAPPDATA%\InternetControl\WebView2`, temp dir fallback
    /// when LOCALAPPDATA is unset. Our own folder only — never any
    /// shared Edge folder.
    pub fn webview2_user_data_dir() -> std::path::PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        super::user_data_dir_for(&base)
    }

    /// Available WebView2 runtime version, e.g. `Some("139.0.7258.66")`.
    /// `None` when no runtime is installed — the loader returns an
    /// error HRESULT in that case.
    pub fn browser_version_string() -> Option<String> {
        unsafe {
            let mut raw = PWSTR::null();
            GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut raw).ok()?;
            if raw.is_null() {
                return None;
            }
            // take_pwstr copies the CoTaskMemAlloc'd string and frees it.
            let version = take_pwstr(raw);
            if version.is_empty() {
                None
            } else {
                Some(version)
            }
        }
    }

    /// Drain any stale WM_QUIT left in this thread's message queue.
    ///
    /// v0.5.4: a previous attempt's window is destroyed on a
    /// retryable failure (Fallback -> `destroy_window`), and our
    /// `window_proc` posts WM_QUIT from WM_DESTROY. Nobody pumps
    /// messages between the destroy and the retry, so the quit
    /// message is still queued when the next attempt starts pumping
    /// in `wait_for_async_operation` — `GetMessage` returns 0 and
    /// the wait dies with TaskCanceled, even though the runtime is
    /// fine. Draining before every attempt makes each attempt start
    /// with a clean queue. Only WM_QUIT is removed; no other
    /// message is touched. Safe to call when the queue is already
    /// clean (a no-op then, e.g. on the first attempt).
    fn drain_quit_messages() {
        unsafe {
            let mut msg = MSG::default();
            while WindowsAndMessaging::PeekMessageW(
                &mut msg,
                None,
                WindowsAndMessaging::WM_QUIT,
                WindowsAndMessaging::WM_QUIT,
                WindowsAndMessaging::PM_REMOVE,
            )
            .as_bool()
            {
                // Discard: this quit belongs to a window we already
                // destroyed on purpose between attempts, not to the
                // user closing the app.
            }
        }
    }

    /// Describe a ProcessFailed event as one loggable line
    /// (v0.5.5): kind, reason and exit code, e.g. "browser process
    /// exited (crashed, exit code -1073741819)". Reason and exit
    /// code live on the Args2 interface and may be absent on older
    /// runtimes — then they are left out rather than failing.
    fn describe_process_failed(args: Option<ICoreWebView2ProcessFailedEventArgs>) -> String {
        let Some(args) = args else {
            return "process failed (no details)".to_string();
        };
        let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND(0);
        let kind = unsafe { args.ProcessFailedKind(&mut kind) }
            .map(|()| super::process_failed_kind_name(kind.0))
            .unwrap_or_else(|_| "unknown process".to_string());
        let mut reason = "unknown reason".to_string();
        let mut exit = String::new();
        if let Ok(args2) = args.cast::<ICoreWebView2ProcessFailedEventArgs2>() {
            let mut r = COREWEBVIEW2_PROCESS_FAILED_REASON(0);
            if unsafe { args2.Reason(&mut r) }.is_ok() {
                reason = super::process_failed_reason_name(r.0);
            }
            let mut code = 0i32;
            if unsafe { args2.ExitCode(&mut code) }.is_ok() {
                exit = format!(", exit code {code}");
            }
        }
        format!("{kind} ({reason}{exit})")
    }

    /// One window attempt: create the window + control on
    /// `user_data_folder` and run the first navigation. The caller
    /// (`run`, below) decides whether a failure is worth one retry
    /// with a fresh profile.
    pub fn run_once(
        url: &str,
        user_data_folder: &std::path::Path,
        browser_args: &str,
    ) -> std::result::Result<(), String> {
        // v0.5.4: drain the stale WM_QUIT a previous attempt's
        // destroyed window left behind, or this attempt's first
        // `wait_for_async_operation` dies with TaskCanceled at once.
        drain_quit_messages();
        let view = WebView::create(url, user_data_folder, browser_args)?;
        view.run()
    }

    pub struct WebView {
        controller: Rc<ICoreWebView2Controller>,
        webview: Rc<ICoreWebView2>,
        frame: FrameWindow,
        url: String,
        /// v0.5.5: short note left by the ProcessFailed handler
        /// when Edge's browser process dies ("browser process
        /// exited (crashed, exit code -1073741819)"), read by
        /// `run()` when Navigate fails so the failure reason says
        /// why the process died. `None` when no process failure
        /// fired (the event is asynchronous, so the note is often
        /// still empty when Navigate returns — the handler's log
        /// line is the reliable record either way).
        process_failed_note: Arc<Mutex<Option<String>>>,
    }

    #[derive(Clone)]
    struct FrameWindow {
        window: HWND,
    }

    impl WebView {
        pub fn create(
            url: &str,
            user_data_folder: &std::path::Path,
            browser_args: &str,
        ) -> std::result::Result<WebView, String> {
            unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                    .ok()
                    .map_err(|e| format!("CoInitializeEx failed: {e:?}"))?;
                let _ = HiDpi::SetProcessDpiAwareness(HiDpi::PROCESS_PER_MONITOR_DPI_AWARE);
            }

            let window_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                lpszClassName: w!("InternetControlWebView"),
                ..Default::default()
            };
            let hwnd = unsafe {
                WindowsAndMessaging::RegisterClassW(&window_class);
                WindowsAndMessaging::CreateWindowExW(
                    Default::default(),
                    w!("InternetControlWebView"),
                    w!("Internet Control — Usage Meter"),
                    WindowsAndMessaging::WS_OVERLAPPEDWINDOW,
                    WindowsAndMessaging::CW_USEDEFAULT,
                    WindowsAndMessaging::CW_USEDEFAULT,
                    1060,
                    760,
                    None,
                    None,
                    LibraryLoader::GetModuleHandleW(None)
                        .ok()
                        .map(|h| HINSTANCE(h.0)),
                    None,
                )
                .map_err(|e| format!("CreateWindowExW failed: {e:?}"))?
            };

            // Async environment creation, pumped on this thread.
            // v0.5.3: explicit user-data folder via
            // CreateCoreWebView2EnvironmentWithOptions — the crate
            // default left the profile wherever Edge put it, and a
            // stale / locked profile from a crashed first run is the
            // prime suspect for the browser process never starting
            // (0x8007139F on Sahand's PC). The starter closure must
            // be 'static (Box<dyn FnOnce>), so the folder string is
            // MOVED into it as an owned CoTaskMemPWSTR — the Create
            // call copies the path, and the buffer is freed when the
            // closure is dropped after the call.
            let folder_wide: CoTaskMemPWSTR<'static> =
                CoTaskMemPWSTR::from(user_data_folder.to_string_lossy().as_ref());
            // v0.5.5: additional browser arguments for the
            // escalating attempts (--disable-gpu, then also
            // --no-sandbox). Only when args are non-empty — the
            // plain first attempt passes no options at all, exactly
            // as before. The crate's options default pins the
            // target runtime version to 145.0.3800.47, so clear it
            // to empty (use whatever runtime is installed) instead
            // of risking a failure on an older runtime. The options
            // object is moved into the starter closure and lives
            // until the Create call runs.
            let options: Option<ICoreWebView2EnvironmentOptions> = if browser_args.is_empty() {
                None
            } else {
                let opts = CoreWebView2EnvironmentOptions::default();
                unsafe {
                    opts.set_additional_browser_arguments(browser_args.to_string());
                    opts.set_target_compatible_browser_version(String::new());
                }
                Some(opts.into())
            };
            let environment = {
                let (tx, rx) = std::sync::mpsc::channel();
                CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                    Box::new(move |handler| unsafe {
                        CreateCoreWebView2EnvironmentWithOptions(
                            PCWSTR::null(),
                            *folder_wide.as_ref().as_pcwstr(),
                            options.as_ref(),
                            &handler,
                        )
                        .map_err(webview2_com::Error::WindowsError)
                    }),
                    Box::new(move |error_code, environment| {
                        error_code?;
                        tx.send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                            .expect("send over mpsc channel");
                        Ok(())
                    }),
                )
                .map_err(|e| format!("WebView2 runtime not available: {e:?}"))?;
                rx.recv()
                    .map_err(|_| "WebView2 environment creation was cancelled".to_string())?
                    .map_err(|e| format!("WebView2 runtime not available: {e:?}"))?
            };

            let controller = {
                let (tx, rx) = std::sync::mpsc::channel();
                CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
                    Box::new(move |handler| unsafe {
                        environment
                            .CreateCoreWebView2Controller(hwnd, &handler)
                            .map_err(webview2_com::Error::WindowsError)
                    }),
                    Box::new(move |error_code, controller| {
                        error_code?;
                        tx.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)))
                            .expect("send over mpsc channel");
                        Ok(())
                    }),
                )
                .map_err(|e| format!("WebView2 controller creation failed: {e:?}"))?;
                rx.recv()
                    .map_err(|_| "WebView2 controller creation was cancelled".to_string())?
                    .map_err(|e| format!("WebView2 controller creation failed: {e:?}"))?
            };

            let mut client_rect = RECT::default();
            unsafe {
                let _ = WindowsAndMessaging::GetClientRect(hwnd, &mut client_rect);
                controller
                    .SetBounds(RECT {
                        left: 0,
                        top: 0,
                        right: client_rect.right - client_rect.left,
                        bottom: client_rect.bottom - client_rect.top,
                    })
                    .map_err(|e| format!("SetBounds failed: {e:?}"))?;
                controller
                    .SetIsVisible(true)
                    .map_err(|e| format!("SetIsVisible failed: {e:?}"))?;
            }
            let webview = unsafe {
                controller
                    .CoreWebView2()
                    .map_err(|e| format!("CoreWebView2 failed: {e:?}"))?
            };
            // No context menus / dev tools in release builds; keep
            // them in debug builds for troubleshooting.
            if !cfg!(debug_assertions) {
                unsafe {
                    if let Ok(settings) = webview.Settings() {
                        let _ = settings.SetAreDefaultContextMenusEnabled(false);
                        let _ = settings.SetAreDevToolsEnabled(false);
                    }
                }
            }
            // v0.5.5: best-effort ProcessFailed handler — when
            // Edge's browser process dies, this is the only place
            // that says why (kind / reason / exit code). Registered
            // right after obtaining the CoreWebView2 so a crash at
            // startup is caught too. Registration failure is
            // non-fatal: the window works without it, we just lose
            // the diagnosis. (The handler object is kept alive by
            // COM: add_ProcessFailed AddRefs it, so the local below
            // may drop at the end of create().)
            let process_failed_note: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            {
                let note = process_failed_note.clone();
                let handler = ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
                    let detail = describe_process_failed(args);
                    crate::log::log(&format!("WebView2 process failed: {detail}"));
                    if let Ok(mut slot) = note.lock() {
                        *slot = Some(detail);
                    }
                    Ok(())
                }));
                let mut token = 0;
                unsafe {
                    if let Err(e) = webview.add_ProcessFailed(&handler, &mut token) {
                        crate::log::log(&format!(
                            "ProcessFailed handler registration failed ({e:?}); continuing without process-failure diagnosis"
                        ));
                    }
                }
            }
            let frame = FrameWindow { window: hwnd };
            let view = WebView {
                controller: Rc::new(controller),
                webview: Rc::new(webview),
                frame,
                url: url.to_string(),
                process_failed_note,
            };
            set_window_webview(hwnd, Some(Box::new(view.clone_for_window())));
            Ok(view)
        }

        fn clone_for_window(&self) -> WebView {
            WebView {
                controller: self.controller.clone(),
                webview: self.webview.clone(),
                frame: self.frame.clone(),
                url: self.url.clone(),
                process_failed_note: self.process_failed_note.clone(),
            }
        }

        /// Resize the control to fill the frame window's client area.
        fn resize_to_window(&self) {
            let mut client_rect = RECT::default();
            unsafe {
                let _ = WindowsAndMessaging::GetClientRect(self.frame.window, &mut client_rect);
                let _ = self.controller.SetBounds(RECT {
                    left: 0,
                    top: 0,
                    right: client_rect.right - client_rect.left,
                    bottom: client_rect.bottom - client_rect.top,
                });
            }
        }

        /// Outcome of the first navigation: (success, WebErrorStatus,
        /// HTTP status). WebErrorStatus -1 means the event carried no
        /// args at all.
        fn nav_outcome(args: Option<ICoreWebView2NavigationCompletedEventArgs>) -> NavResult {
            let Some(a) = args else {
                return NavResult::Done(false, -1, 0);
            };
            let mut ok = windows::core::BOOL(0);
            let is_success = unsafe { a.IsSuccess(&mut ok) }.is_ok() && ok.as_bool();
            let mut status = COREWEBVIEW2_WEB_ERROR_STATUS(0);
            let web_error = unsafe { a.WebErrorStatus(&mut status) }
                .map(|()| status.0)
                .unwrap_or(-1);
            // HTTP status lives on the Args2 interface; it may not
            // be there (e.g. a failed navigation), so 0 means unknown.
            let mut http = 0i32;
            if let Ok(a2) = a.cast::<ICoreWebView2NavigationCompletedEventArgs2>() {
                let _ = unsafe { a2.HttpStatusCode(&mut http) };
            }
            NavResult::Done(is_success, web_error, http)
        }

        pub fn run(self) -> std::result::Result<(), String> {
            // Show the window FIRST, then navigate: a stuck or failed
            // navigation must never leave a hidden window, and the
            // page is visible the moment it loads. (v0.4.0 waited for
            // NavigationCompleted before showing the window and never
            // checked whether navigation had succeeded — a failed
            // navigation left a blank white page with no clue why.)
            unsafe {
                let _ = WindowsAndMessaging::ShowWindow(
                    self.frame.window,
                    WindowsAndMessaging::SW_SHOW,
                );
                let _ = Gdi::UpdateWindow(self.frame.window);
            }
            // Re-apply the control bounds now the window is shown, in
            // case the rect at creation time was empty (a zero-size
            // control renders as a white page even when navigation
            // succeeds).
            self.resize_to_window();
            if self.url.is_empty() {
                return self.message_loop();
            }

            let (tx, rx) = std::sync::mpsc::channel::<NavResult>();
            let watchdog_tx = tx.clone();
            let handler =
                NavigationCompletedEventHandler::create(Box::new(move |_sender, args| {
                    let _ = tx.send(Self::nav_outcome(args));
                    Ok(())
                }));
            let mut token = 0;

            // v0.5.2: registering the NavigationCompleted handler can
            // fail with ERROR_INVALID_STATE (0x8007139F) when the
            // WebView2 browser process is not up yet at registration
            // time — the control itself is fine, and Navigate is the
            // call that (re)starts the browser process. So a failed
            // registration is NOT a window failure: log it and
            // navigate anyway. (Our sequence matches the crate's own
            // sample — add, Navigate, wait, remove — so this is the
            // browser process's state, not a wrong order on our side.)
            let mut handler_ok = unsafe {
                match self.webview.add_NavigationCompleted(&handler, &mut token) {
                    Ok(()) => true,
                    Err(e) => {
                        crate::log::log(&format!(
                            "add_NavigationCompleted failed ({e:?}); navigating without the page-loaded check"
                        ));
                        false
                    }
                }
            };

            // Navigate anyway — this is what (re)starts the browser
            // process. Only a failed Navigate means the window cannot
            // show the page, and only then do we fall back.
            let navigate_result = unsafe {
                let target = CoTaskMemPWSTR::from(self.url.as_str());
                self.webview
                    .Navigate(*target.as_ref().as_pcwstr())
                    .map_err(|e| format!("Navigate failed: {e:?}"))
            };

            // If Navigate worked but the first registration failed,
            // retry once: with the browser process up now, the
            // handler usually registers on the second try and the
            // watchdog below still applies.
            if navigate_result.is_ok() && !handler_ok {
                handler_ok = unsafe {
                    match self.webview.add_NavigationCompleted(&handler, &mut token) {
                        Ok(()) => {
                            crate::log::log(
                                "add_NavigationCompleted succeeded on retry after Navigate",
                            );
                            true
                        }
                        Err(e) => {
                            crate::log::log(&format!(
                                "add_NavigationCompleted failed again after Navigate ({e:?}); continuing without the page-loaded check"
                            ));
                            false
                        }
                    }
                };
            }

            // Plain-data outcome, then the pure keep-vs-fallback
            // decision (see decide_first_nav above).
            let first_nav = if let Err(e) = navigate_result {
                // v0.5.5: if the ProcessFailed event already fired,
                // say why the process died — that is the real reason
                // Navigate failed. (The event is asynchronous, so the
                // note is often still empty here; the handler's log
                // line is the reliable record either way.)
                let mut e = e;
                if let Ok(slot) = self.process_failed_note.lock() {
                    if let Some(note) = slot.as_ref() {
                        e = format!("{e} (WebView2 process failed: {note})");
                    }
                }
                crate::log::log(&format!("WebView2 Navigate failed: {e}"));
                super::FirstNav::NavigateFailed(e)
            } else if !handler_ok {
                crate::log::log(
                    "WebView2 navigation started without NavigationCompleted; keeping the app window open",
                );
                super::FirstNav::Unobserved
            } else {
                // Watchdog: if NavigationCompleted never fires (the
                // control can stall navigation while hidden or during
                // a bad first run), stop waiting after 15 s instead
                // of hanging on a white page forever.
                {
                    let tx = watchdog_tx;
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(15));
                        let _ = tx.send(NavResult::Timeout);
                    });
                }
                let waited = webview2_com::wait_with_pump(rx);
                let _ = unsafe { self.webview.remove_NavigationCompleted(token) };
                match waited {
                    Ok(NavResult::Done(true, _, http)) => {
                        crate::log::log(&format!(
                            "WebView2 navigation succeeded (HTTP {http}) for {}",
                            self.url
                        ));
                        super::FirstNav::Succeeded { http }
                    }
                    Ok(NavResult::Done(false, status, http)) => {
                        crate::log::log(&format!(
                            "WebView2 navigation FAILED: WebErrorStatus {status}, HTTP {http} for {}",
                            self.url
                        ));
                        super::FirstNav::Failed {
                            web_error_status: status,
                            http,
                        }
                    }
                    Ok(NavResult::Timeout) => {
                        crate::log::log(&format!(
                            "WebView2 navigation timed out after 15 s for {}",
                            self.url
                        ));
                        super::FirstNav::TimedOut
                    }
                    Err(e) => {
                        // wait_with_pump reports TaskCanceled when the
                        // message loop got WM_QUIT — i.e. the user
                        // closed the window while navigation was
                        // still running. That is a clean exit, not a
                        // failure to fall back from.
                        let e = format!("waiting for navigation failed: {e:?}");
                        if e.contains("TaskCanceled") {
                            super::FirstNav::WaitCancelled
                        } else {
                            crate::log::log(&format!("WebView2 navigation wait failed: {e}"));
                            super::FirstNav::WaitFailed(e)
                        }
                    }
                }
            };
            match super::decide_first_nav(&first_nav) {
                super::FirstNavDecision::KeepWindow => self.message_loop(),
                super::FirstNavDecision::ExitCleanly => Ok(()),
                super::FirstNavDecision::Fallback(reason) => {
                    // A failed navigation leaves a blank white page.
                    // Close this window and let the caller fall back
                    // to the default browser on the same URL instead.
                    self.destroy_window();
                    Err(reason)
                }
            }
        }

        /// Close the frame window (used when navigation failed so no
        /// blank window is left behind).
        fn destroy_window(&self) {
            unsafe {
                let _ = WindowsAndMessaging::DestroyWindow(self.frame.window);
            }
        }

        fn message_loop(self) -> std::result::Result<(), String> {
            let mut msg = MSG::default();
            loop {
                let result = unsafe { WindowsAndMessaging::GetMessageW(&mut msg, None, 0, 0).0 };
                match result {
                    -1 => return Err("GetMessageW failed".to_string()),
                    0 => return Ok(()),
                    _ => unsafe {
                        let _ = WindowsAndMessaging::TranslateMessage(&msg);
                        WindowsAndMessaging::DispatchMessageW(&msg);
                    },
                }
            }
        }
    }

    impl Drop for WebView {
        fn drop(&mut self) {
            // The controller is reference-counted (Rc here, COM
            // underneath); closing the window ends the message loop
            // and the process exits, releasing the capture handle —
            // which is what un-blocks every blocked app.
            unsafe {
                let _ = self.controller.Close();
            }
        }
    }

    fn set_window_webview(hwnd: HWND, webview: Option<Box<WebView>>) -> Option<Box<WebView>> {
        unsafe {
            match set_window_long(
                hwnd,
                WindowsAndMessaging::GWLP_USERDATA,
                match webview {
                    Some(webview) => Box::into_raw(webview) as _,
                    None => 0_isize,
                },
            ) {
                0 => None,
                ptr => Some(Box::from_raw(ptr as *mut WebView)),
            }
        }
    }

    fn get_window_webview(hwnd: HWND) -> Option<Box<WebView>> {
        unsafe {
            let data = get_window_long(hwnd, WindowsAndMessaging::GWLP_USERDATA);
            match data {
                0 => None,
                _ => {
                    let raw = Box::from_raw(data as *mut WebView);
                    let webview = raw.clone_for_window();
                    mem::forget(raw);
                    Some(Box::new(webview))
                }
            }
        }
    }

    #[allow(non_snake_case)]
    #[cfg(target_pointer_width = "64")]
    unsafe fn set_window_long(window: HWND, index: WINDOW_LONG_PTR_INDEX, value: isize) -> isize {
        WindowsAndMessaging::SetWindowLongPtrW(window, index, value)
    }

    #[allow(non_snake_case)]
    #[cfg(target_pointer_width = "64")]
    unsafe fn get_window_long(window: HWND, index: WINDOW_LONG_PTR_INDEX) -> isize {
        WindowsAndMessaging::GetWindowLongPtrW(window, index)
    }

    extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        w_param: WPARAM,
        l_param: LPARAM,
    ) -> LRESULT {
        match msg {
            WindowsAndMessaging::WM_SIZE => {
                if let Some(webview) = get_window_webview(hwnd) {
                    let mut client_rect = RECT::default();
                    unsafe {
                        let _ = WindowsAndMessaging::GetClientRect(hwnd, &mut client_rect);
                        let _ = webview.controller.SetBounds(RECT {
                            left: 0,
                            top: 0,
                            right: client_rect.right - client_rect.left,
                            bottom: client_rect.bottom - client_rect.top,
                        });
                    }
                }
                LRESULT::default()
            }
            WindowsAndMessaging::WM_CLOSE => {
                unsafe {
                    let _ = WindowsAndMessaging::DestroyWindow(hwnd);
                }
                LRESULT::default()
            }
            WindowsAndMessaging::WM_DESTROY => {
                set_window_webview(hwnd, None);
                unsafe {
                    WindowsAndMessaging::PostQuitMessage(0);
                }
                LRESULT::default()
            }
            _ => unsafe { WindowsAndMessaging::DefWindowProcW(hwnd, msg, w_param, l_param) },
        }
    }
}

/// Open the WebView2 window on `url` and run its message loop until
/// the window closes. Returns Err when WebView2 is unavailable —
/// the caller falls back to the default browser.
///
/// v0.5.3: the environment uses an explicit user-data folder
/// (`%LOCALAPPDATA%\InternetControl\WebView2`), the runtime version
/// is logged at startup, and when the window fails with
/// ERROR_INVALID_STATE (0x8007139F — the browser process never
/// started) the stale profile is moved aside and the window is
/// retried exactly once with a fresh profile. Any other failure,
/// or a second invalid-state failure, falls back immediately. The
/// final failure reason carries the runtime version, so the v0.5.1
/// banner shows it (and says explicitly when no runtime is
/// installed at all).
///
/// v0.5.4: the v0.5.3 retry died at once with TaskCanceled — the
/// stale WM_QUIT from attempt 1's destroyed window (see
/// `imp::drain_quit_messages`, called at the start of every
/// attempt). Each attempt is logged, the retry pauses ~500 ms so
/// a crashed Edge process can exit and release its locks, and the
/// final reason lists what happened on every attempt.
///
/// v0.5.5: the drain worked (no more TaskCanceled), but both
/// attempts still fail at Navigate with the same 0x8007139F — the
/// browser process itself will not stay alive. The window now
/// escalates across up to three attempts with a fresh profile
/// each time: plain, then `--disable-gpu`, then
/// `--disable-gpu --no-sandbox`. Any non-0x8007139F failure still
/// falls back immediately with no further attempts.
#[cfg(windows)]
pub fn run(url: &str) -> Result<(), String> {
    let folder = imp::webview2_user_data_dir();
    crate::log::log(&format!("WebView2 user data folder: {}", folder.display()));
    let runtime_version = imp::browser_version_string();
    match &runtime_version {
        Some(v) => crate::log::log(&format!("WebView2 runtime version: {v}")),
        None => crate::log::log("WebView2 runtime version: not found (no runtime installed)"),
    }

    let mut dir = folder;
    let mut attempts: Vec<AttemptRecord> = Vec::new();
    loop {
        let attempt_no = attempts.len() + 1;
        // v0.5.5: escalating browser arguments — plain, then
        // --disable-gpu, then --disable-gpu --no-sandbox.
        let args = browser_args_for_attempt(attempt_no);
        let args_note = if args.is_empty() {
            String::new()
        } else {
            format!("; additional browser arguments: {args}")
        };
        crate::log::log(&format!(
            "WebView2 window attempt {attempt_no} (user data folder: {}{args_note})",
            dir.display()
        ));
        match imp::run_once(url, &dir, args) {
            Ok(()) => return Ok(()),
            Err(e) => {
                crate::log::log(&format!("WebView2 window attempt {attempt_no} failed: {e}"));
                attempts.push(AttemptRecord {
                    error: e.clone(),
                    args,
                });
                if should_retry_window(&e, attempts.len()) {
                    let ts = now_secs();
                    let backup = backup_dir_for(&dir, ts);
                    let renamed = std::fs::rename(&dir, &backup).is_ok();
                    if renamed {
                        crate::log::log(&format!(
                            "WebView2 profile moved aside to {} after 0x8007139F; retrying with a fresh profile",
                            backup.display()
                        ));
                    } else {
                        crate::log::log(
                            "WebView2 profile could not be moved aside (locked by a crashed Edge process or missing); retrying with a fresh profile folder",
                        );
                    }
                    dir = retry_dir_for(&dir, ts, renamed);
                    // v0.5.4: give a crashed Edge process a moment
                    // to exit and release its locks before the
                    // retry creates a new environment.
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    continue;
                }
                let with_history = format_attempt_history(&attempts);
                return Err(failure_with_runtime_info(
                    &with_history,
                    runtime_version.as_deref(),
                ));
            }
        }
    }
}

/// Seconds since the Unix epoch (for backup / fresh folder names).
#[cfg(windows)]
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Non-Windows stub: there is no WebView2 here; the caller prints
/// the URL instead (used by `cargo run` / curl checks on Linux).
#[cfg(not(windows))]
pub fn run(_url: &str) -> Result<(), String> {
    Err("WebView2 is only available on Windows.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_failed_but_navigate_ok_keeps_window() {
        // v0.5.2: the exact case from Sahand's banner —
        // add_NavigationCompleted failed with 0x8007139F, but
        // Navigate started. The window is kept, not dropped to the
        // browser.
        assert_eq!(
            decide_first_nav(&FirstNav::Unobserved),
            FirstNavDecision::KeepWindow
        );
    }

    #[test]
    fn navigate_failed_falls_back() {
        assert_eq!(
            decide_first_nav(&FirstNav::NavigateFailed(
                "Navigate failed: Error { code: HRESULT(0x8007139F), message: \"The group or resource is not in the correct state to perform the requested operation.\" }".to_string()
            )),
            FirstNavDecision::Fallback(
                "Navigate failed: Error { code: HRESULT(0x8007139F), message: \"The group or resource is not in the correct state to perform the requested operation.\" }".to_string()
            )
        );
    }

    #[test]
    fn navigation_outcomes_decide_as_before() {
        assert_eq!(
            decide_first_nav(&FirstNav::Succeeded { http: 200 }),
            FirstNavDecision::KeepWindow
        );
        assert_eq!(
            decide_first_nav(&FirstNav::Failed {
                web_error_status: 12,
                http: 0
            }),
            FirstNavDecision::Fallback(
                "WebView2 navigation failed (WebErrorStatus 12, HTTP 0); see internet-control.log"
                    .to_string()
            )
        );
        assert_eq!(
            decide_first_nav(&FirstNav::TimedOut),
            FirstNavDecision::Fallback(
                "WebView2 navigation timed out after 15 s; see internet-control.log".to_string()
            )
        );
    }

    #[test]
    fn closing_window_while_waiting_exits_cleanly() {
        // TaskCanceled must NOT enter the message loop (WM_QUIT was
        // already consumed by the wait) and must NOT fall back.
        assert_eq!(
            decide_first_nav(&FirstNav::WaitCancelled),
            FirstNavDecision::ExitCleanly
        );
        assert_eq!(
            decide_first_nav(&FirstNav::WaitFailed(
                "waiting for navigation failed: Error { code: HRESULT(0x80004005), message: \"Unspecified error\" }".to_string()
            )),
            FirstNavDecision::Fallback(
                "waiting for navigation failed: Error { code: HRESULT(0x80004005), message: \"Unspecified error\" }".to_string()
            )
        );
    }

    #[test]
    fn navigate_invalid_state_falls_back_but_is_retried() {
        // v0.5.3: Sahand's exact banner — Navigate failed with
        // 0x8007139F. decide_first_nav still says fallback (the
        // window cannot show the page), but should_retry_window
        // says the caller gets retries with a fresh profile —
        // v0.5.5: up to three attempts total, then it stops.
        let reason = "Navigate failed: Error { code: HRESULT(0x8007139F), message: \"The group or resource is not in the correct state to perform the requested operation.\" }";
        assert_eq!(
            decide_first_nav(&FirstNav::NavigateFailed(reason.to_string())),
            FirstNavDecision::Fallback(reason.to_string())
        );
        assert!(should_retry_window(reason, 1));
        assert!(should_retry_window(reason, 2));
        assert!(!should_retry_window(reason, 3));
    }

    #[test]
    fn retry_only_on_invalid_state_hresult() {
        // Any other HRESULT — including the neighbouring code — is
        // not a stale-profile case and must never trigger a retry.
        assert!(!should_retry_window(
            "Navigate failed: Error { code: HRESULT(0x80004005), message: \"Unspecified error\" }",
            1
        ));
        assert!(!should_retry_window(
            "WebView2 runtime not available: Error { code: HRESULT(0x80040154), message: \"Class not registered\" }",
            1
        ));
        assert!(!is_invalid_state_error(
            "Navigate failed: Error { code: HRESULT(0x8007139E), message: \"Something else\" }"
        ));
        assert!(!should_retry_window(
            "WebView2 navigation timed out after 15 s; see internet-control.log",
            1
        ));
        // Detection is case-insensitive: lower-case hex still matches.
        assert!(is_invalid_state_error("hresult(0x8007139f)"));
        assert!(is_invalid_state_error(
            "Error { code: HRESULT(0x8007139F), message: \"The group or resource is not in the correct state to perform the requested operation.\" }"
        ));
    }

    #[test]
    fn user_data_folder_layout() {
        // v0.5.3: %LOCALAPPDATA%\InternetControl\WebView2.
        let base = std::path::Path::new("/tmp/fake-local");
        assert_eq!(
            user_data_dir_for(base),
            std::path::PathBuf::from("/tmp/fake-local/InternetControl/WebView2")
        );
    }

    #[test]
    fn backup_and_retry_folder_names() {
        let dir = std::path::Path::new("/tmp/fake-local/InternetControl/WebView2");
        assert_eq!(
            backup_dir_for(dir, 1_795_651_200),
            std::path::PathBuf::from("/tmp/fake-local/InternetControl/WebView2.bak-1795651200")
        );
        // Rename succeeded: retry reuses the original (now clean) folder.
        assert_eq!(retry_dir_for(dir, 1_795_651_200, true), dir.to_path_buf());
        // Rename failed (locked by a crashed Edge process, or the
        // folder was never created): fresh suffixed folder, the old
        // one untouched.
        assert_eq!(
            retry_dir_for(dir, 1_795_651_200, false),
            std::path::PathBuf::from("/tmp/fake-local/InternetControl/WebView2-1795651200")
        );
    }

    #[test]
    fn failure_string_carries_runtime_version() {
        let reason = "Navigate failed: Error { code: HRESULT(0x8007139F) }";
        let with = failure_with_runtime_info(reason, Some("139.0.7258.66"));
        assert!(with.starts_with(reason), "{with}");
        assert!(with.contains("139.0.7258.66"), "{with}");
        // No runtime at all: say so explicitly, so the banner tells
        // Sahand the fix is installing the Evergreen runtime.
        let without = failure_with_runtime_info(reason, None);
        assert!(without.starts_with(reason), "{without}");
        assert!(without.contains("not found"), "{without}");
        assert!(without.contains("Evergreen"), "{without}");
    }

    #[test]
    fn task_canceled_is_not_a_profile_retry() {
        // v0.5.4: Sahand's exact banner — the v0.5.3 retry died at
        // once with TaskCanceled. That is a stale WM_QUIT from the
        // previous attempt's destroyed window, NOT a stale profile:
        // it must never trigger a profile retry, and it must be
        // distinguishable from the 0x8007139F case.
        let reason = "WebView2 controller creation failed: TaskCanceled";
        assert!(!is_invalid_state_error(reason));
        assert!(!should_retry_window(reason, 1));
        assert!(!should_retry_window(reason, 3));
        // The 0x8007139F case still retries, up to three attempts.
        let invalid_state = "Navigate failed: Error { code: HRESULT(0x8007139F), message: \"The group or resource is not in the correct state to perform the requested operation.\" }";
        assert!(should_retry_window(invalid_state, 1));
        assert!(should_retry_window(invalid_state, 2));
        assert!(!should_retry_window(invalid_state, 3));
    }

    #[test]
    fn attempt_history_formatting() {
        // v0.5.4: no attempts — placeholder, never an empty reason.
        assert_eq!(format_attempt_history(&[]), "WebView2 window failed");
        // One attempt: its error unchanged (banner text as before).
        let one = vec![AttemptRecord {
            error: "Navigate failed: Error { code: HRESULT(0x8007139F) }".to_string(),
            args: "",
        }];
        assert_eq!(
            format_attempt_history(&one),
            "Navigate failed: Error { code: HRESULT(0x8007139F) }"
        );
        // Two attempts (Sahand's v0.5.4 run): the last error first,
        // the earlier attempt numbered in parentheses.
        let two = vec![
            AttemptRecord {
                error: "Navigate failed: Error { code: HRESULT(0x8007139F) }".to_string(),
                args: "",
            },
            AttemptRecord {
                error: "WebView2 controller creation failed: TaskCanceled".to_string(),
                args: "",
            },
        ];
        let history = format_attempt_history(&two);
        assert!(
            history.starts_with("WebView2 controller creation failed: TaskCanceled"),
            "{history}"
        );
        assert!(
            history.contains("attempt 1: Navigate failed: Error { code: HRESULT(0x8007139F) }"),
            "{history}"
        );
        // Three attempts keep their order.
        let three = vec![
            AttemptRecord {
                error: "a".to_string(),
                args: "",
            },
            AttemptRecord {
                error: "b".to_string(),
                args: "",
            },
            AttemptRecord {
                error: "c".to_string(),
                args: "",
            },
        ];
        assert_eq!(
            format_attempt_history(&three),
            "c (earlier attempts: attempt 1: a; attempt 2: b)"
        );
    }

    #[test]
    fn attempt_history_keeps_runtime_suffix() {
        // The final reason is history first, runtime version last —
        // so the banner shows both what happened on each try and
        // which Edge runtime is installed.
        let attempts = vec![
            AttemptRecord {
                error: "Navigate failed: Error { code: HRESULT(0x8007139F) }".to_string(),
                args: "",
            },
            AttemptRecord {
                error: "WebView2 controller creation failed: TaskCanceled".to_string(),
                args: "",
            },
        ];
        let reason =
            failure_with_runtime_info(&format_attempt_history(&attempts), Some("158.0.4370.33"));
        assert!(reason.contains("attempt 1: Navigate failed"), "{reason}");
        assert!(reason.contains("TaskCanceled"), "{reason}");
        assert!(
            reason.ends_with("[WebView2 runtime 158.0.4370.33]"),
            "{reason}"
        );
    }

    #[test]
    fn browser_args_escalate_per_attempt() {
        // v0.5.5: plain, then --disable-gpu, then also --no-sandbox.
        assert_eq!(browser_args_for_attempt(1), "");
        assert_eq!(browser_args_for_attempt(2), "--disable-gpu");
        assert_eq!(browser_args_for_attempt(3), "--disable-gpu --no-sandbox");
        // No fourth attempt exists (the cap is three), so anything
        // beyond the escalations is plain by definition.
        assert_eq!(browser_args_for_attempt(4), "");
        assert_eq!(browser_args_for_attempt(0), "");
    }

    #[test]
    fn three_attempt_cap() {
        // v0.5.5: an invalid-state failure escalates twice (three
        // attempts total), then stops.
        let reason = "Navigate failed: Error { code: HRESULT(0x8007139F) }";
        assert!(should_retry_window(reason, 1));
        assert!(should_retry_window(reason, 2));
        assert!(!should_retry_window(reason, 3));
        assert!(!should_retry_window(reason, 4));
        // A non-invalid-state failure never escalates, at any count.
        assert!(!should_retry_window(
            "WebView2 controller creation failed: TaskCanceled",
            1
        ));
        assert!(!should_retry_window(
            "WebView2 navigation timed out after 15 s; see internet-control.log",
            2
        ));
    }

    #[test]
    fn attempt_history_includes_args() {
        // v0.5.5: the banner history shows which arguments each
        // attempt used, so Sahand's next banner says whether the
        // --disable-gpu / --no-sandbox tries helped.
        let attempts = vec![
            AttemptRecord {
                error: "Navigate failed: Error { code: HRESULT(0x8007139F) }".to_string(),
                args: "",
            },
            AttemptRecord {
                error: "Navigate failed: Error { code: HRESULT(0x8007139F) }".to_string(),
                args: "--disable-gpu",
            },
            AttemptRecord {
                error: "Navigate failed: Error { code: HRESULT(0x8007139F) }".to_string(),
                args: "--disable-gpu --no-sandbox",
            },
        ];
        let history = format_attempt_history(&attempts);
        // The last attempt (with --no-sandbox) comes first.
        assert!(
            history.starts_with(
                "Navigate failed: Error { code: HRESULT(0x8007139F) } [args: --disable-gpu --no-sandbox]"
            ),
            "{history}"
        );
        // The plain first attempt carries no args suffix.
        assert!(
            history.contains("attempt 1: Navigate failed: Error { code: HRESULT(0x8007139F) };"),
            "{history}"
        );
        // The --disable-gpu attempt is labelled with its args.
        assert!(
            history.contains(
                "attempt 2: Navigate failed: Error { code: HRESULT(0x8007139F) } [args: --disable-gpu]"
            ),
            "{history}"
        );
    }

    #[test]
    fn process_failed_names() {
        // v0.5.5: kind / reason numbers from the ProcessFailed
        // event become readable words in the log.
        assert_eq!(process_failed_kind_name(0), "browser process exited");
        assert_eq!(process_failed_kind_name(6), "GPU process exited");
        assert_eq!(process_failed_kind_name(5), "sandbox helper process exited");
        assert_eq!(process_failed_kind_name(99), "process kind 99");
        assert_eq!(process_failed_reason_name(3), "crashed");
        assert_eq!(process_failed_reason_name(4), "launch failed");
        assert_eq!(process_failed_reason_name(5), "out of memory");
        assert_eq!(process_failed_reason_name(99), "reason 99");
    }
}
