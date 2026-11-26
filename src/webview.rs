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
        pub fn create(url: &str) -> std::result::Result<WebView, String> {
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
            let environment = {
                let (tx, rx) = std::sync::mpsc::channel();
                CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                    Box::new(|handler| unsafe {
                        CreateCoreWebView2Environment(&handler)
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
            // Watchdog: if NavigationCompleted never fires (the
            // control can stall navigation while hidden or during a
            // bad first run), stop waiting after 15 s instead of
            // hanging on a white page forever.
            {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(15));
                    let _ = tx.send(NavResult::Timeout);
                });
            }
            let handler =
                NavigationCompletedEventHandler::create(Box::new(move |_sender, args| {
                    let _ = tx.send(Self::nav_outcome(args));
                    Ok(())
                }));
            let mut token = 0;
            let nav = (|| unsafe {
                self.webview
                    .add_NavigationCompleted(&handler, &mut token)
                    .map_err(|e| format!("add_NavigationCompleted failed: {e:?}"))?;
                let target = CoTaskMemPWSTR::from(self.url.as_str());
                self.webview
                    .Navigate(*target.as_ref().as_pcwstr())
                    .map_err(|e| format!("Navigate failed: {e:?}"))?;
                let result = webview2_com::wait_with_pump(rx);
                let _ = self.webview.remove_NavigationCompleted(token);
                result.map_err(|e| format!("waiting for navigation failed: {e:?}"))
            })();
            match nav {
                Ok(NavResult::Done(true, _, http)) => {
                    crate::log::log(&format!(
                        "WebView2 navigation succeeded (HTTP {http}) for {}",
                        self.url
                    ));
                }
                Ok(NavResult::Done(false, status, http)) => {
                    crate::log::log(&format!(
                        "WebView2 navigation FAILED: WebErrorStatus {status}, HTTP {http} for {}",
                        self.url
                    ));
                    // A failed navigation leaves a blank white page.
                    // Close this window and let the caller fall back
                    // to the default browser on the same URL instead.
                    self.destroy_window();
                    return Err(format!(
                        "WebView2 navigation failed (WebErrorStatus {status}, HTTP {http}); see internet-control.log"
                    ));
                }
                Ok(NavResult::Timeout) => {
                    crate::log::log(&format!(
                        "WebView2 navigation timed out after 15 s for {}",
                        self.url
                    ));
                    self.destroy_window();
                    return Err(
                        "WebView2 navigation timed out after 15 s; see internet-control.log"
                            .to_string(),
                    );
                }
                Err(e) => {
                    // wait_with_pump reports TaskCanceled when the
                    // message loop got WM_QUIT — i.e. the user closed
                    // the window while navigation was still running.
                    // That is a clean exit, not a failure to fall
                    // back from.
                    if e.contains("TaskCanceled") {
                        return Ok(());
                    }
                    crate::log::log(&format!("WebView2 navigation wait failed: {e}"));
                    self.destroy_window();
                    return Err(e);
                }
            }
            self.message_loop()
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
#[cfg(windows)]
pub fn run(url: &str) -> Result<(), String> {
    let view = imp::WebView::create(url)?;
    view.run()
}

/// Non-Windows stub: there is no WebView2 here; the caller prints
/// the URL instead (used by `cargo run` / curl checks on Linux).
#[cfg(not(windows))]
pub fn run(_url: &str) -> Result<(), String> {
    Err("WebView2 is only available on Windows.".to_string())
}
