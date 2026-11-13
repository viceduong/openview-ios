//! Windows resources (icon, manifest, VERSIONINFO) and delay-loaded Media
//! Foundation for the `ovview` exe.

include!("../../assets/winres.rs");

fn main() {
    embed_windows_resources(&WinRes {
        exe: "ovview.exe",
        description: "OpenView viewer",
        common_controls: false,
    });
    delay_load_media_foundation();
}

/// Windows N/KN editions ship without Media Foundation (until the user
/// installs the Media Feature Pack). Delay-loading `mfplat.dll` lets the exe
/// start anyway; `ovapp::mfcodec` probes for the DLL before the first MF call
/// and falls back to lossless tiles when it is missing.
fn delay_load_media_foundation() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows-msvc") {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:mfplat.dll");
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
