//! Embed the requireAdministrator manifest (v0.2: WinDivert needs admin).
//!
//! Only runs when the build host is Windows (e.g. GitHub Actions
//! windows-latest, where the MSVC resource compiler is available). When
//! cross-compiling from Linux the manifest step is skipped — the exe
//! then needs a manual "Run as administrator". A failure to embed is
//! non-fatal (warning only) so a missing resource compiler never breaks
//! the build; the UI still reports a WinDivert access-denied error
//! plainly if it is run without elevation.

#[cfg(target_os = "windows")]
fn main() {
    let mut res = winres::WindowsResource::new();
    res.set_manifest_file("internet-control.exe.manifest");
    if let Err(e) = res.compile() {
        println!("cargo:warning=winres failed ({e}); exe will not embed the admin manifest — run as administrator manually");
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {}
