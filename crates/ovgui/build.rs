include!("../../assets/winres.rs");

fn main() {
    // Rebuild codegen when any .slint / icon changes.
    println!("cargo::rerun-if-changed=ui");
    println!("cargo::rerun-if-changed=../../assets/openview.svg");

    let config = slint_build::CompilerConfiguration::new().with_style("native".into());
    // `.expect`, never `.ok()`: a .slint error must fail the build.
    slint_build::compile_with_config("ui/main.slint", config).expect("Slint compile failed");

    // Icon (resource id 1 for the exe, the window and the tray), manifest
    // (PerMonitorV2 DPI, Common Controls v6, asInvoker) and VERSIONINFO.
    embed_windows_resources(&WinRes {
        exe: "OpenView.exe",
        description: "OpenView",
        common_controls: true,
    });

    // Windows debug builds construct the window on the main stack; 1 MiB
    // overflows. Reserve 8 MiB (virtual until touched).
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows-msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:{}", 8 * 1024 * 1024);
    } else if target.contains("windows-gnu") {
        println!("cargo:rustc-link-arg-bins=-Wl,--stack,{}", 8 * 1024 * 1024);
    }
}
