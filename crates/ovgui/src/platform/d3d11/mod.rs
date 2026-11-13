//! D3D11 WARP Platform for Slint
//!
//! This module provides a custom Slint platform that uses D3D11 with WARP
//! (Windows Advanced Rasterization Platform) for presentation instead of
//! the default GDI-based softbuffer.
//!
//! # Why WARP?
//!
//! On headless Windows servers (e.g., A30 GPU in TCC mode), the Microsoft
//! Basic Display Adapter only refreshes at ~1 Hz, causing blank screen issues
//! with the standard software renderer (which uses softbuffer → GDI).
//!
//! WARP provides:
//! - Software D3D11 rasterization designed for servers/VMs
//! - DXGI SwapChain presentation that bypasses GDI
//! - `Present(0, DXGI_PRESENT_ALLOW_TEARING)` for immediate presentation
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Slint Application                        │
//! ├─────────────────────────────────────────────────────────────┤
//! │           D3D11Platform (custom Platform impl)              │
//! │  - run_event_loop() → winit event loop                      │
//! │  - duration_since_start() → timing                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │           D3D11WindowAdapter (custom WindowAdapter)         │
//! │  - window() → slint::Window                                 │
//! │  - size() → PhysicalSize                                    │
//! │  - renderer() → SoftwareRenderer                            │
//! ├─────────────────────────────────────────────────────────────┤
//! │           SoftwareRenderer (Slint-provided)                 │
//! │  - render(&mut pixel_buffer, stride)                        │
//! ├─────────────────────────────────────────────────────────────┤
//! │           WarpPresenter (custom D3D11 presentation)         │
//! │  - WARP Device → SwapChain → Present(0, TEARING)            │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use mbfs_slint::{D3D11Platform, WindowConfig};
//!
//! // Option 1: Initialize with defaults
//! D3D11Platform::init()?;
//!
//! // Option 2: Initialize with custom config
//! let config = WindowConfig::builder()
//!     .window_title("My Application")
//!     .initial_size(1920, 1080)
//!     .build();
//! D3D11Platform::init_with_config(config)?;
//!
//! // Now use Slint normally
//! let ui = MainWindow::new()?;
//! ui.run()?;
//! ```

mod platform;
mod warp_presenter;
mod window_adapter;

pub use platform::D3D11Platform;
#[allow(unused_imports)]
pub use platform::{main_hwnd, main_hwnd_raw};
pub use warp_presenter::WarpPresenter;
pub use window_adapter::D3D11WindowAdapter;
