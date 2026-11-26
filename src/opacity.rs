//! Window opacity (v0.7): a slider makes the whole app window more
//! or less transparent.
//!
//! Windows implementation: the window is made a layered window
//! (`WS_EX_LAYERED`) and `SetLayeredWindowAttributes` applies a
//! constant alpha to the entire window. At full opacity the layered
//! style is removed again, so the window behaves exactly like a
//! normal window (no layered-window quirks). This is a compositor
//! alpha — it works with any rendering behind it, and it does not
//! touch the meter, capture or blocking at all.
//!
//! The pure parts (`clamp_opacity`, `opacity_alpha`) are platform
//! independent and unit-tested on Linux. The Win32 calls are
//! best-effort and only run on Windows; on other platforms
//! `apply_opacity` is a no-op so `cargo test` still runs.

/// Lowest opacity the slider allows: 0.2 (still readable, clearly
/// see-through). Below that the window would be hard to find.
pub const OPACITY_MIN: f32 = 0.2;
/// Fully opaque.
pub const OPACITY_MAX: f32 = 1.0;
/// Default opacity: fully opaque, as before.
pub const OPACITY_DEFAULT: f32 = 1.0;

/// Clamp a slider value to the allowed range. Non-finite input
/// (NaN / infinite, e.g. from a corrupt config) resets to fully
/// opaque instead of poisoning the window. Pure — unit-tested.
pub fn clamp_opacity(value: f32) -> f32 {
    if !value.is_finite() {
        return OPACITY_DEFAULT;
    }
    value.clamp(OPACITY_MIN, OPACITY_MAX)
}

/// The alpha byte `SetLayeredWindowAttributes` wants for an opacity:
/// 51 at 0.2, 255 at 1.0. Pure — unit-tested. (Only called by the
/// Windows `apply_opacity`, so Linux builds see it as unused.)
#[allow(dead_code)]
pub fn opacity_alpha(opacity: f32) -> u8 {
    (clamp_opacity(opacity) * 255.0).round() as u8
}

/// Extract the Win32 HWND from an eframe frame, if it has one.
/// `None` on non-Windows builds and when the handle is unavailable
/// (headless test runs).
#[cfg(windows)]
pub fn hwnd_from_frame(frame: &eframe::Frame) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle() {
        Ok(handle) => match handle.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        },
        Err(_) => None,
    }
}

/// No window handle on non-Windows builds.
#[cfg(not(windows))]
pub fn hwnd_from_frame(_frame: &eframe::Frame) -> Option<isize> {
    None
}

/// Apply an opacity to a window handle. At 1.0 the layered style is
/// removed so the window is an ordinary window again; below 1.0 the
/// layered style is added (if not already there) and the alpha is
/// set. Best-effort: every Win32 call's result is ignored, so a
/// failure leaves the window as-is instead of crashing the app.
#[cfg(windows)]
pub fn apply_opacity(hwnd: isize, opacity: f32) {
    use windows::Win32::Foundation::{COLORREF, HWND};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE, LWA_ALPHA,
    };
    // WS_EX_LAYERED = 0x00080000. Kept as a raw constant so the code
    // does not depend on the windows crate's style-newtype API.
    const WS_EX_LAYERED: isize = 0x0008_0000;
    // HWND wraps a raw pointer in windows 0.62, not an integer.
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    let opacity = clamp_opacity(opacity);
    unsafe {
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let is_layered = exstyle & WS_EX_LAYERED != 0;
        if opacity >= OPACITY_MAX {
            if is_layered {
                let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, exstyle & !WS_EX_LAYERED);
            }
            return;
        }
        if !is_layered {
            let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, exstyle | WS_EX_LAYERED);
        }
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), opacity_alpha(opacity), LWA_ALPHA);
    }
}

/// No-op on non-Windows builds (only used for `cargo test`).
#[cfg(not(windows))]
pub fn apply_opacity(_hwnd: isize, _opacity: f32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opacity_clamps_to_the_slider_range() {
        assert_eq!(clamp_opacity(1.0), 1.0);
        assert_eq!(clamp_opacity(0.2), 0.2);
        assert_eq!(clamp_opacity(0.5), 0.5);
        assert_eq!(clamp_opacity(0.0), OPACITY_MIN);
        assert_eq!(clamp_opacity(-3.0), OPACITY_MIN);
        assert_eq!(clamp_opacity(2.5), OPACITY_MAX);
        // A corrupt config must not make the window invisible.
        assert_eq!(clamp_opacity(f32::NAN), OPACITY_DEFAULT);
        assert_eq!(clamp_opacity(f32::INFINITY), OPACITY_DEFAULT);
        assert_eq!(clamp_opacity(f32::NEG_INFINITY), OPACITY_DEFAULT);
    }

    #[test]
    fn opacity_alpha_bytes() {
        assert_eq!(opacity_alpha(1.0), 255);
        assert_eq!(opacity_alpha(0.2), 51);
        assert_eq!(opacity_alpha(0.5), 128);
        // Out-of-range input is clamped first, never wraps.
        assert_eq!(opacity_alpha(0.0), 51);
        assert_eq!(opacity_alpha(2.0), 255);
    }

    // The range must be sane or the slider is useless: checked at
    // compile time, like the column-width asserts in app.rs.
    const _: () = assert!(OPACITY_MIN > 0.0 && OPACITY_MIN < OPACITY_MAX);
    const _: () = assert!(OPACITY_MAX == 1.0);
    const _: () = assert!(OPACITY_DEFAULT == OPACITY_MAX);
}
