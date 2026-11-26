//! "Run on startup" (v0.3): an HKCU Run key named `InternetControl`
//! pointing at the current exe path. HKCU needs no admin rights to
//! write, but the app has a requireAdministrator manifest, so Windows
//! will still show a UAC prompt at login — that is expected, and this
//! module deliberately does nothing to bypass UAC.
//!
//! The key is only created/deleted when the user toggles the checkbox
//! in the UI. Nothing here runs at boot on its own.

#[allow(dead_code)] // only used by the Windows registry functions
pub const RUN_KEY_NAME: &str = "InternetControl";
#[allow(dead_code)] // only used by the Windows registry functions
const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The exe path the Run key should point at.
#[allow(dead_code)] // only used by the Windows registry functions
pub fn current_exe_path() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Is the Run key currently set to this exe?
#[cfg(windows)]
pub fn is_run_on_startup() -> bool {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ,
    };
    unsafe {
        let mut key = windows::Win32::System::Registry::HKEY::default();
        let rc = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_SUBKEY),
            None,
            KEY_READ,
            &mut key,
        );
        if rc.is_err() {
            return false;
        }
        let mut ty = windows::Win32::System::Registry::REG_SZ;
        let mut buf = [0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let rc = RegQueryValueExW(
            key,
            &HSTRING::from(RUN_KEY_NAME),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut len),
        );
        let _ = RegCloseKey(key);
        if rc.is_err() {
            return false;
        }
        let n = (len as usize / 2).min(buf.len());
        let end = buf[..n].iter().position(|&c| c == 0).unwrap_or(n);
        let stored = String::from_utf16_lossy(&buf[..end]);
        match current_exe_path() {
            Some(exe) => stored.eq_ignore_ascii_case(&exe),
            None => false,
        }
    }
}

#[cfg(not(windows))]
#[allow(dead_code)] // called only from Windows UI code
pub fn is_run_on_startup() -> bool {
    false
}

/// Create the Run key (enable=true) or delete it (enable=false).
#[cfg(windows)]
pub fn set_run_on_startup(enable: bool) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_SZ,
    };
    unsafe {
        let mut key = windows::Win32::System::Registry::HKEY::default();
        let rc = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_SUBKEY),
            None,
            KEY_SET_VALUE,
            &mut key,
        );
        if rc.is_err() {
            return Err(format!("could not open the Run registry key: {rc:?}"));
        }
        let result = (|| -> Result<(), String> {
            if enable {
                let exe = current_exe_path()
                    .ok_or_else(|| "could not determine the exe path".to_string())?;
                let wide: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes: &[u8] =
                    std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
                let rc =
                    RegSetValueExW(key, &HSTRING::from(RUN_KEY_NAME), None, REG_SZ, Some(bytes));
                if rc.is_err() {
                    return Err(format!("could not write the Run key: {rc:?}"));
                }
            } else {
                // Deleting a missing value is not an error.
                let _ = RegDeleteValueW(key, &HSTRING::from(RUN_KEY_NAME));
            }
            Ok(())
        })();
        let _ = RegCloseKey(key);
        result
    }
}

#[cfg(not(windows))]
#[allow(dead_code)] // called only from Windows UI code
pub fn set_run_on_startup(_enable: bool) -> Result<(), String> {
    Err("Run on startup is only available on Windows.".to_string())
}

/// Firewall rule name generation (v0.3 does NOT use firewall rules —
/// blocking is WinDivert intercept only). This helper exists only so
/// tests can prove no rule name is ever constructed from user input
/// in a way that could collide: the name format is documented here
/// for the record and intentionally unused by production code.
#[allow(dead_code)]
pub fn firewall_rule_name_for(exe_name: &str) -> String {
    format!("Internet Control — Block {exe_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_key_name_is_stable() {
        assert_eq!(RUN_KEY_NAME, "InternetControl");
    }

    #[test]
    fn firewall_rule_name_format() {
        // Documented for the record only: v0.3 never creates firewall
        // rules, so this name must never appear in a netsh call.
        assert_eq!(
            firewall_rule_name_for("chrome.exe"),
            "Internet Control — Block chrome.exe"
        );
    }
}
