//! Rendering platform selection.
//!
//! Default on Windows: the **D3D11 WARP platform** (feature `d3d11`, on by
//! default) — Slint's software renderer presented through a DXGI swap chain,
//! so text is crisp AND it works on headless / RDP / VM hosts where GDI
//! presentation blanks the window. Falls back to the plain software renderer
//! if D3D11 init fails. Non-Windows, or `--no-default-features`: software.
//!
//! Env overrides (honored before the default):
//!   SLINT_BACKEND=…      use this Slint backend as-is (wins over everything)
//!   SLINT_DISABLE_D3D11  skip D3D11, use the software renderer (Windows)
//!   SLINT_USE_SOFTWARE   force the software renderer
//!
//! The ~900-line D3D11 module under `d3d11/` is vendored from a known-good
//! source (mbfs-face-core-rust / vd-gui) and kept lint-exempt. Local patches
//! are marked `ovgui patch` (an idle-CPU fix: the upstream loop re-requested
//! a frame after every redraw and never woke for Slint timers — fine for a
//! foreground tool, not for a tray app that runs all day).

#[cfg(all(windows, feature = "d3d11"))]
#[allow(
    clippy::all,
    clippy::pedantic,
    missing_debug_implementations,
    dead_code
)]
pub mod config;
#[cfg(all(windows, feature = "d3d11"))]
#[allow(
    clippy::all,
    clippy::pedantic,
    missing_debug_implementations,
    dead_code
)]
pub mod d3d11;

/// Window parameters the platform needs up front. The D3D11 platform OWNS the
/// native window, so the `.slint` `title` / `preferred-*` are ignored there —
/// these values are the source of truth for title and size. (Unused when the
/// D3D11 path is compiled out; the software renderer takes title from `.slint`.)
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WindowSpec {
    pub title: String,
    pub size: (u32, u32),
    pub min_size: (u32, u32),
}

/// Select and install the rendering platform. Call ONCE, before the first
/// window is created and before spawning any thread (it may set env vars,
/// which is not thread-safe).
pub fn init(spec: &WindowSpec) {
    // A user-chosen backend always wins.
    if let Some(backend) = std::env::var_os("SLINT_BACKEND") {
        if backend.to_string_lossy().contains("software") {
            enable_full_speed_rendering();
        }
        return;
    }

    if !flag("SLINT_USE_SOFTWARE") {
        #[cfg(all(windows, feature = "d3d11"))]
        if !flag("SLINT_DISABLE_D3D11") {
            // No SLINT_DEBUG_PERFORMANCE here: the D3D11 adapter renders with
            // RepaintBufferType::NewBuffer (no partial rendering), and the
            // variable would only add a warning (it lacks an output mode).
            let config = config::WindowConfig::builder()
                .window_title(spec.title.clone())
                .initial_size(spec.size.0, spec.size.1)
                .min_size(spec.min_size.0, spec.min_size.1)
                .center_on_screen()
                .build();
            match d3d11::D3D11Platform::init_with_config(config) {
                Ok(()) => return, // D3D11 installed
                Err(_e) => { /* fall through to software */ }
            }
        }
    }

    software();
    // `spec` is only consumed by the D3D11 path; silence the warning when that
    // path is compiled out (non-Windows / no d3d11 feature).
    let _ = spec;
}

/// winit + Slint's software renderer (the fallback and the non-Windows path).
fn software() {
    let _ = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select();
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// `SLINT_DEBUG_PERFORMANCE=refresh_full_speed` disables partial rendering —
/// needed so model-structure changes don't trip the software renderer.
fn enable_full_speed_rendering() {
    if std::env::var_os("SLINT_DEBUG_PERFORMANCE").is_none() {
        // SAFETY: called from `init`, documented to run before any thread is
        // spawned, so nothing reads the environment concurrently.
        unsafe { std::env::set_var("SLINT_DEBUG_PERFORMANCE", "refresh_full_speed") };
    }
}

/// The main window's HWND (as isize) when the D3D11 platform owns it; 0
/// otherwise (software fallback: no tray integration).
pub fn main_hwnd_raw() -> isize {
    #[cfg(all(windows, feature = "d3d11"))]
    {
        d3d11::main_hwnd_raw()
    }
    #[cfg(not(all(windows, feature = "d3d11")))]
    {
        0
    }
}
