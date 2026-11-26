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

/// Retry the window once with a fresh profile only for
/// ERROR_INVALID_STATE, and only once — any other failure, or a
/// second invalid-state failure, goes straight to the browser.
/// Pure: no COM, no I/O.
#[cfg(any(windows, test))]
fn should_retry_window(reason: &str, already_retried: bool) -> bool {
    !already_retried && is_invalid_state_error(reason)
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

#[cfg(windows)]
mod imp {
    use std::mem;
    use std::rc::Rc;

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

    /// One window attempt: create the window + control on
    /// `user_data_folder` and run the first navigation. The caller
    /// (`run`, below) decides whether a failure is worth one retry
    /// with a fresh profile.
    pub fn run_once(
        url: &str,
        user_data_folder: &std::path::Path,
    ) -> std::result::Result<(), String> {
        let view = WebView::create(url, user_data_folder)?;
        view.run()
    }

    pub struct WebView {
        controller: Rc<ICoreWebView2Controller>,
        webview: Rc<ICoreWebView2>,
        frame: FrameWindow,
        url: String,
    }

    #[derive(Clone)]
    struct FrameWindow {
        window: HWND,
    }

    impl WebView {
        pub fn create(
            url: &str,
            user_data_folder: &std::path::Path,
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
            let no_options: Option<&ICoreWebView2EnvironmentOptions> = None;
            let environment = {
                let (tx, rx) = std::sync::mpsc::channel();
                CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                    Box::new(move |handler| unsafe {
                        CreateCoreWebView2EnvironmentWithOptions(
                            PCWSTR::null(),
                            *folder_wide.as_ref().as_pcwstr(),
                            no_options,
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
            let frame = FrameWindow { window: hwnd };
            let view = WebView {
                controller: Rc::new(controller),
                webview: Rc::new(webview),
                frame,
                url: url.to_string(),
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
    let mut retried = false;
    loop {
        match imp::run_once(url, &dir) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if should_retry_window(&e, retried) {
                    retried = true;
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
                    continue;
                }
                return Err(failure_with_runtime_info(&e, runtime_version.as_deref()));
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
    fn navigate_invalid_state_falls_back_but_is_retried_once() {
        // v0.5.3: Sahand's exact banner — Navigate failed with
        // 0x8007139F. decide_first_nav still says fallback (the
        // window cannot show the page), but should_retry_window
        // says the caller gets one retry with a fresh profile.
        let reason = "Navigate failed: Error { code: HRESULT(0x8007139F), message: \"The group or resource is not in the correct state to perform the requested operation.\" }";
        assert_eq!(
            decide_first_nav(&FirstNav::NavigateFailed(reason.to_string())),
            FirstNavDecision::Fallback(reason.to_string())
        );
        assert!(should_retry_window(reason, false));
        assert!(!should_retry_window(reason, true));
    }

    #[test]
    fn retry_only_on_invalid_state_hresult() {
        // Any other HRESULT — including the neighbouring code — is
        // not a stale-profile case and must never trigger a retry.
        assert!(!should_retry_window(
            "Navigate failed: Error { code: HRESULT(0x80004005), message: \"Unspecified error\" }",
            false
        ));
        assert!(!should_retry_window(
            "WebView2 runtime not available: Error { code: HRESULT(0x80040154), message: \"Class not registered\" }",
            false
        ));
        assert!(!is_invalid_state_error(
            "Navigate failed: Error { code: HRESULT(0x8007139E), message: \"Something else\" }"
        ));
        assert!(!should_retry_window(
            "WebView2 navigation timed out after 15 s; see internet-control.log",
            false
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
}
