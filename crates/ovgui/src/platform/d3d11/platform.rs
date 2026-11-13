//! D3D11 Platform for Slint
//!
//! Custom Slint platform that uses winit for window/event handling
//! and WARP for D3D11-based presentation, replacing GDI-based softbuffer.
//!
//! Scale factor handling follows Slint's winit backend:
//! - Scale factor change event must be dispatched BEFORE setting size (PR #8118)
//! - All coordinates from winit are physical and must be converted to logical
//! - Resize events dispatch logical size (physical / scale_factor)

use super::D3D11WindowAdapter;
use crate::platform::config::{WindowConfig, WindowPosition};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::platform::{Platform, WindowAdapter};
use slint::{LogicalPosition, LogicalSize as SlintLogicalSize, SharedString};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::HWND;
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, Position},
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy as WinitEventLoopProxy},
    keyboard::{Key, KeyCode, NamedKey, PhysicalKey},
    window::WindowId,
};

/// Queue for closures to be executed on the event loop
type ClosureQueue = Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>;

/// Global HWND storage for easy access to the main window handle
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

/// Get the HWND of the main window
///
/// Returns `None` if no window has been created yet.
///
/// # Example
///
/// ```rust,ignore
/// use mbfs_slint::{main_hwnd, dialog};
///
/// if let Some(hwnd) = main_hwnd() {
///     let _guard = dialog::ModalGuard::new(hwnd);
///     // Open modal dialog...
/// }
/// ```
pub fn main_hwnd() -> Option<HWND> {
    let value = MAIN_HWND.load(Ordering::SeqCst);
    if value == 0 {
        None
    } else {
        Some(HWND(value as *mut _))
    }
}

/// Get the main window HWND as raw isize
///
/// Returns 0 if no window has been created yet.
pub fn main_hwnd_raw() -> isize {
    MAIN_HWND.load(Ordering::SeqCst)
}

/// D3D11 WARP Platform for Slint
///
/// This platform provides D3D11 WARP-based rendering for headless Windows servers
/// where the standard GDI-based softbuffer has refresh rate issues.
///
/// # Usage
///
/// ```rust,ignore
/// use mbfs_slint::{D3D11Platform, WindowConfig};
///
/// // Option 1: Initialize with default settings
/// D3D11Platform::init()?;
///
/// // Option 2: Initialize with custom configuration
/// let config = WindowConfig::builder()
///     .window_title("My Application")
///     .initial_size(1920, 1080)
///     .build();
/// D3D11Platform::init_with_config(config)?;
///
/// // Now create and run your Slint UI normally
/// let ui = MainWindow::new()?;
/// ui.run()?;
/// ```
pub struct D3D11Platform {
    start_time: Instant,
    window: RefCell<Option<Rc<D3D11WindowAdapter>>>,
    event_loop: RefCell<Option<EventLoop<()>>>,
    /// Scale factor from winit, used for coordinate conversion
    scale_factor: Cell<f32>,
    /// Queue for closures from invoke_from_event_loop
    closure_queue: ClosureQueue,
    /// Winit event loop proxy for waking the event loop
    winit_proxy: RefCell<Option<WinitEventLoopProxy<()>>>,
    /// Window configuration
    config: WindowConfig,
}

/// EventLoopProxy implementation for Slint
struct D3D11EventLoopProxy {
    closure_queue: ClosureQueue,
    winit_proxy: WinitEventLoopProxy<()>,
}

impl slint::platform::EventLoopProxy for D3D11EventLoopProxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        // Send a wake-up to trigger event loop exit
        self.winit_proxy
            .send_event(())
            .map_err(|_| slint::EventLoopError::EventLoopTerminated)
    }

    fn invoke_from_event_loop(
        &self,
        event: Box<dyn FnOnce() + Send>,
    ) -> Result<(), slint::EventLoopError> {
        // Queue the closure
        if let Ok(mut queue) = self.closure_queue.lock() {
            queue.push(event);
        }
        // Wake the event loop to process the closure
        self.winit_proxy
            .send_event(())
            .map_err(|_| slint::EventLoopError::EventLoopTerminated)
    }
}

impl D3D11Platform {
    /// Initialize and set the D3D11 WARP platform with default configuration
    ///
    /// This must be called BEFORE creating any Slint windows.
    /// After calling this, all Slint windows will use D3D11 WARP for presentation.
    pub fn init() -> Result<(), slint::PlatformError> {
        Self::init_with_config(WindowConfig::default())
    }

    /// Initialize and set the D3D11 WARP platform with custom configuration
    ///
    /// This must be called BEFORE creating any Slint windows.
    /// After calling this, all Slint windows will use D3D11 WARP for presentation.
    ///
    /// # Arguments
    ///
    /// * `config` - Window configuration (title, size, etc.)
    pub fn init_with_config(config: WindowConfig) -> Result<(), slint::PlatformError> {
        info!("Initializing D3D11 WARP platform");
        info!("  Window title: {}", config.window_title);
        info!(
            "  Initial size: {}x{}",
            config.initial_width, config.initial_height
        );
        if let Some((fraction, aw, ah)) = config.screen_fraction {
            info!(
                "  Screen fraction: {:.0}% ({}:{})",
                fraction * 100.0,
                aw,
                ah
            );
        }
        if let (Some(min_w), Some(min_h)) = (config.min_width, config.min_height) {
            info!("  Minimum size: {}x{}", min_w, min_h);
        }
        info!("  Renderer: Software (Slint SoftwareRenderer)");
        info!("  Presentation: D3D11 WARP -> DXGI SwapChain");

        // Check if WARP is available
        if !super::WarpPresenter::is_available() {
            return Err(slint::PlatformError::Other(
                "D3D11 WARP is not available on this system".into(),
            ));
        }

        let event_loop = EventLoop::new().map_err(|e| {
            slint::PlatformError::Other(format!("Failed to create event loop: {}", e))
        })?;

        // Create proxy before moving event_loop
        let winit_proxy = event_loop.create_proxy();

        let platform = Box::new(Self {
            start_time: Instant::now(),
            window: RefCell::new(None),
            event_loop: RefCell::new(Some(event_loop)),
            scale_factor: Cell::new(1.0),
            closure_queue: Arc::new(Mutex::new(Vec::new())),
            winit_proxy: RefCell::new(Some(winit_proxy)),
            config,
        });

        slint::platform::set_platform(platform)
            .map_err(|e| slint::PlatformError::Other(format!("Failed to set platform: {:?}", e)))?;

        info!("D3D11 WARP platform initialized successfully");
        Ok(())
    }
}

impl Platform for D3D11Platform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        info!("Creating D3D11 window adapter");

        // Get or create the event loop
        let event_loop_ref = self.event_loop.borrow();
        let event_loop = event_loop_ref
            .as_ref()
            .ok_or_else(|| slint::PlatformError::Other("Event loop not available".into()))?;

        // Build window attributes (use initial_size as default; screen_fraction applied after creation)
        let mut attrs = winit::window::Window::default_attributes()
            .with_title(&self.config.window_title)
            .with_inner_size(LogicalSize::new(
                self.config.initial_width,
                self.config.initial_height,
            ));

        // Apply minimum window size if configured
        if let (Some(min_w), Some(min_h)) = (self.config.min_width, self.config.min_height) {
            attrs = attrs.with_min_inner_size(LogicalSize::new(min_w, min_h));
            info!("Minimum window size set: {}x{}", min_w, min_h);
        }

        // Apply window icon if configured
        if let Some(ref icon) = self.config.window_icon {
            if let Some(winit_icon) = icon.to_winit_icon() {
                attrs = attrs.with_window_icon(Some(winit_icon));
                info!("Window icon set: {}x{}", icon.width, icon.height);
            }
        }

        // Apply always-on-top if configured
        if self.config.always_on_top {
            attrs = attrs.with_window_level(winit::window::WindowLevel::AlwaysOnTop);
            info!("Window set to always-on-top");
        }

        // Create winit window with configured attributes
        #[allow(deprecated)]
        let window = event_loop
            .create_window(attrs)
            .map_err(|e| slint::PlatformError::Other(format!("Failed to create window: {}", e)))?;

        // Get scale factor from winit window
        let scale_factor = window.scale_factor() as f32;
        self.scale_factor.set(scale_factor);
        info!("Window scale factor: {:.2}", scale_factor);

        // Query monitor and constrain window to fit (like Slint's Qt backend does)
        // This is critical for headless servers where Windows may report incorrect metrics
        if let Some(monitor) = window.current_monitor() {
            let monitor_physical = monitor.size();
            let monitor_scale = monitor.scale_factor();

            // Calculate monitor's logical size (what we can actually use)
            let monitor_logical_width = (monitor_physical.width as f64 / monitor_scale) as u32;
            let monitor_logical_height = (monitor_physical.height as f64 / monitor_scale) as u32;

            info!(
                "Monitor: {}x{} physical, {}x{} logical (scale {:.2})",
                monitor_physical.width,
                monitor_physical.height,
                monitor_logical_width,
                monitor_logical_height,
                monitor_scale
            );

            // Apply screen_fraction sizing if configured
            if let Some((fraction, aspect_w, aspect_h)) = self.config.screen_fraction {
                let target_w = monitor_logical_width as f32 * fraction;
                let target_h = monitor_logical_height as f32 * fraction;
                let aspect = aspect_w as f32 / aspect_h as f32;

                let (w, h) = if target_w / target_h > aspect {
                    // Height is the constraint
                    (target_h * aspect, target_h)
                } else {
                    // Width is the constraint
                    (target_w, target_w / aspect)
                };

                info!(
                    "Screen fraction {:.0}% -> {}x{} logical ({}:{})",
                    fraction * 100.0,
                    w as u32,
                    h as u32,
                    aspect_w,
                    aspect_h
                );
                let _ = window.request_inner_size(LogicalSize::new(w as u32, h as u32));
            }

            // Get current window physical size
            let window_physical = window.inner_size();

            // Check if window exceeds monitor bounds (with 5% margin for taskbar/decorations)
            let max_physical_width = (monitor_physical.width as f32 * 0.95) as u32;
            let max_physical_height = (monitor_physical.height as f32 * 0.95) as u32;

            if window_physical.width > max_physical_width
                || window_physical.height > max_physical_height
            {
                // Calculate new logical size that will fit
                let new_logical_width = (max_physical_width as f64 / scale_factor as f64) as u32;
                let new_logical_height = (max_physical_height as f64 / scale_factor as f64) as u32;

                // Maintain aspect ratio of original request
                let aspect = self.config.initial_width as f32 / self.config.initial_height as f32;
                let (final_width, final_height) =
                    if new_logical_width as f32 / new_logical_height as f32 > aspect {
                        // Height is the constraint
                        (
                            (new_logical_height as f32 * aspect) as u32,
                            new_logical_height,
                        )
                    } else {
                        // Width is the constraint
                        (
                            new_logical_width,
                            (new_logical_width as f32 / aspect) as u32,
                        )
                    };

                info!(
                    "Window exceeds monitor bounds, resizing: {}x{} -> {}x{} logical",
                    self.config.initial_width,
                    self.config.initial_height,
                    final_width,
                    final_height
                );

                let _ = window.request_inner_size(LogicalSize::new(final_width, final_height));
            }
        } else {
            warn!("No monitor detected - cannot verify window fits screen");
        }

        // Apply window position if configured
        match &self.config.position {
            WindowPosition::Center => {
                if let Some(monitor) = window.current_monitor() {
                    let monitor_size = monitor.size();
                    let window_size = window.outer_size();
                    let x = (monitor_size.width.saturating_sub(window_size.width)) / 2;
                    let y = (monitor_size.height.saturating_sub(window_size.height)) / 2;
                    window.set_outer_position(Position::Physical(
                        winit::dpi::PhysicalPosition::new(x as i32, y as i32),
                    ));
                    info!("Window centered at ({}, {})", x, y);
                }
            }
            WindowPosition::At(x, y) => {
                window.set_outer_position(Position::Logical(winit::dpi::LogicalPosition::new(
                    *x as f64, *y as f64,
                )));
                info!("Window positioned at ({}, {})", x, y);
            }
            WindowPosition::Default => {
                // Let the system decide
            }
        }

        // Get HWND
        let hwnd = get_hwnd(&window)?;

        // Store HWND globally for modal dialog support
        MAIN_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        info!("Main window HWND stored: {:?}", hwnd);

        // Get final physical size for the render buffer (after potential resize)
        let physical_size = window.inner_size();
        info!(
            "Final window size: {}x{} physical, {}x{} logical",
            physical_size.width,
            physical_size.height,
            (physical_size.width as f32 / scale_factor) as u32,
            (physical_size.height as f32 / scale_factor) as u32
        );

        // Create D3D11 window adapter with physical size (for render buffer)
        let adapter = D3D11WindowAdapter::new(
            hwnd,
            physical_size.width.max(1),
            physical_size.height.max(1),
            scale_factor,
        )
        .map_err(|e| slint::PlatformError::Other(format!("Failed to create adapter: {}", e)))?;

        *self.window.borrow_mut() = Some(adapter.clone());

        // Store winit window in adapter
        adapter.set_winit_window(window);

        // CRITICAL: Dispatch ScaleFactorChanged BEFORE the window processes any size
        // This follows Slint PR #8118 - scale factor must be set first
        adapter
            .slint_window()
            .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor });

        Ok(adapter as Rc<dyn WindowAdapter>)
    }

    fn duration_since_start(&self) -> core::time::Duration {
        self.start_time.elapsed()
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        info!("Starting D3D11 WARP event loop");

        // Take ownership of the event loop
        let event_loop = self
            .event_loop
            .borrow_mut()
            .take()
            .ok_or_else(|| slint::PlatformError::Other("Event loop already consumed".into()))?;

        // Get the window adapter
        let adapter = self
            .window
            .borrow()
            .clone()
            .ok_or_else(|| slint::PlatformError::Other("No window adapter".into()))?;

        // Get initial scale factor
        let initial_scale_factor = self.scale_factor.get();

        // Track cursor position (in logical coordinates)
        let cursor_pos = Rc::new(RefCell::new(LogicalPosition::new(0.0f32, 0.0f32)));
        // Track scale factor for coordinate conversion
        let scale_factor = Rc::new(Cell::new(initial_scale_factor));
        // Get closure queue for processing invoke_from_event_loop calls
        let closure_queue = self.closure_queue.clone();
        // Get clipboard enabled flag
        let clipboard_enabled = self.config.enable_clipboard;

        struct AppHandler {
            adapter: Rc<D3D11WindowAdapter>,
            cursor_pos: Rc<RefCell<LogicalPosition>>,
            scale_factor: Rc<Cell<f32>>,
            closure_queue: ClosureQueue,
            #[allow(dead_code)]
            clipboard_enabled: bool,
        }

        impl ApplicationHandler for AppHandler {
            fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
                debug!("Event loop resumed");
                // Request initial redraw when event loop starts
                self.adapter.request_redraw_winit();
            }

            fn window_event(
                &mut self,
                event_loop: &ActiveEventLoop,
                _window_id: WindowId,
                event: WindowEvent,
            ) {
                let window = self.adapter.slint_window();
                let sf = self.scale_factor.get();
                // ovgui patch: don't re-request a frame after every
                // RedrawRequested (that spun a core forever).
                let is_redraw = matches!(event, WindowEvent::RedrawRequested);

                match event {
                    WindowEvent::CloseRequested => {
                        info!("Window close requested");
                        event_loop.exit();
                    }

                    WindowEvent::Destroyed => {
                        info!("Window destroyed");
                        event_loop.exit();
                    }

                    WindowEvent::Resized(physical_size) => {
                        if physical_size.width > 0 && physical_size.height > 0 {
                            // Convert physical to logical size
                            let logical_width = physical_size.width as f32 / sf;
                            let logical_height = physical_size.height as f32 / sf;

                            debug!(
                                "Window resized: {}x{} physical, {:.0}x{:.0} logical (scale: {:.2})",
                                physical_size.width,
                                physical_size.height,
                                logical_width,
                                logical_height,
                                sf
                            );

                            // Resize adapter with physical size (for render buffer)
                            if let Err(e) = self
                                .adapter
                                .resize(physical_size.width, physical_size.height)
                            {
                                error!("Failed to resize adapter: {}", e);
                            }

                            // Dispatch logical size to Slint
                            window.dispatch_event(slint::platform::WindowEvent::Resized {
                                size: SlintLogicalSize::new(logical_width, logical_height),
                            });
                        }
                    }

                    WindowEvent::RedrawRequested => {
                        if let Err(e) = self.adapter.render_frame() {
                            static LAST_ERROR: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(0);
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            let last = LAST_ERROR.load(Ordering::Relaxed);
                            if now > last + 5 {
                                LAST_ERROR.store(now, Ordering::Relaxed);
                                warn!("Render error: {}", e);
                            }
                        }
                    }

                    WindowEvent::CursorMoved { position, .. } => {
                        // winit gives physical coordinates, convert to logical
                        let logical_x = position.x as f32 / sf;
                        let logical_y = position.y as f32 / sf;
                        let pos = LogicalPosition::new(logical_x, logical_y);
                        *self.cursor_pos.borrow_mut() = pos;
                        window.dispatch_event(slint::platform::WindowEvent::PointerMoved {
                            position: pos,
                        });
                    }

                    WindowEvent::CursorLeft { .. } => {
                        window.dispatch_event(slint::platform::WindowEvent::PointerExited);
                    }

                    WindowEvent::MouseInput { state, button, .. } => {
                        let pos = *self.cursor_pos.borrow();
                        let slint_button = match button {
                            MouseButton::Left => slint::platform::PointerEventButton::Left,
                            MouseButton::Right => slint::platform::PointerEventButton::Right,
                            MouseButton::Middle => slint::platform::PointerEventButton::Middle,
                            _ => slint::platform::PointerEventButton::Other,
                        };

                        let event = match state {
                            ElementState::Pressed => slint::platform::WindowEvent::PointerPressed {
                                position: pos,
                                button: slint_button,
                            },
                            ElementState::Released => {
                                slint::platform::WindowEvent::PointerReleased {
                                    position: pos,
                                    button: slint_button,
                                }
                            }
                        };

                        window.dispatch_event(event);
                    }

                    WindowEvent::MouseWheel { delta, .. } => {
                        let pos = *self.cursor_pos.borrow();
                        let (dx, dy) = match delta {
                            MouseScrollDelta::LineDelta(x, y) => (x * 20.0, y * 20.0),
                            MouseScrollDelta::PixelDelta(p) => {
                                // PixelDelta is in physical pixels, convert to logical
                                (p.x as f32 / sf, p.y as f32 / sf)
                            }
                        };
                        window.dispatch_event(slint::platform::WindowEvent::PointerScrolled {
                            position: pos,
                            delta_x: dx,
                            delta_y: dy,
                        });
                    }

                    WindowEvent::KeyboardInput { event, .. } => {
                        // Convert winit key to Slint text representation
                        // Modifier keys must be dispatched so Slint can track modifier state
                        // for shortcuts like Ctrl+C, Ctrl+V, etc.
                        //
                        // IMPORTANT: When Ctrl is held, winit may send control characters
                        // (ASCII 1-26) or Key::Dead. We use physical_key as fallback to get
                        // the actual letter for Ctrl+C, Ctrl+V, etc.
                        let text: SharedString = match &event.logical_key {
                            // Character keys - check for control characters
                            Key::Character(c) => {
                                let s = c.as_str();
                                if let Some(ch) = s.chars().next() {
                                    let code = ch as u32;
                                    // Control characters for Ctrl+A through Ctrl+Z are 1-26
                                    if code >= 1 && code <= 26 {
                                        // Convert control character back to lowercase letter
                                        let letter =
                                            char::from_u32('a' as u32 + code - 1).unwrap_or(ch);
                                        SharedString::from(letter.to_string().as_str())
                                    } else {
                                        SharedString::from(s)
                                    }
                                } else {
                                    SharedString::from(s)
                                }
                            }

                            // Dead key or unidentified - try physical key for Ctrl+letter
                            Key::Dead(_) | Key::Unidentified(_) => {
                                physical_key_to_char(&event.physical_key)
                                    .map(|c| SharedString::from(c.to_string().as_str()))
                                    .unwrap_or_default()
                            }

                            // Modifier keys - MUST be dispatched for Ctrl+C/V to work
                            Key::Named(NamedKey::Control) => slint::platform::Key::Control.into(),
                            Key::Named(NamedKey::Shift) => slint::platform::Key::Shift.into(),
                            Key::Named(NamedKey::Alt) => slint::platform::Key::Alt.into(),
                            Key::Named(NamedKey::Super) => slint::platform::Key::Meta.into(),

                            // Navigation keys
                            Key::Named(NamedKey::Space) => SharedString::from(" "),
                            Key::Named(NamedKey::Enter) => slint::platform::Key::Return.into(),
                            Key::Named(NamedKey::Tab) => slint::platform::Key::Tab.into(),
                            Key::Named(NamedKey::Backspace) => {
                                slint::platform::Key::Backspace.into()
                            }
                            Key::Named(NamedKey::Delete) => slint::platform::Key::Delete.into(),
                            Key::Named(NamedKey::Escape) => slint::platform::Key::Escape.into(),
                            Key::Named(NamedKey::Home) => slint::platform::Key::Home.into(),
                            Key::Named(NamedKey::End) => slint::platform::Key::End.into(),
                            Key::Named(NamedKey::PageUp) => slint::platform::Key::PageUp.into(),
                            Key::Named(NamedKey::PageDown) => slint::platform::Key::PageDown.into(),

                            // Arrow keys
                            Key::Named(NamedKey::ArrowUp) => slint::platform::Key::UpArrow.into(),
                            Key::Named(NamedKey::ArrowDown) => {
                                slint::platform::Key::DownArrow.into()
                            }
                            Key::Named(NamedKey::ArrowLeft) => {
                                slint::platform::Key::LeftArrow.into()
                            }
                            Key::Named(NamedKey::ArrowRight) => {
                                slint::platform::Key::RightArrow.into()
                            }

                            // Function keys
                            Key::Named(NamedKey::F1) => slint::platform::Key::F1.into(),
                            Key::Named(NamedKey::F2) => slint::platform::Key::F2.into(),
                            Key::Named(NamedKey::F3) => slint::platform::Key::F3.into(),
                            Key::Named(NamedKey::F4) => slint::platform::Key::F4.into(),
                            Key::Named(NamedKey::F5) => slint::platform::Key::F5.into(),
                            Key::Named(NamedKey::F6) => slint::platform::Key::F6.into(),
                            Key::Named(NamedKey::F7) => slint::platform::Key::F7.into(),
                            Key::Named(NamedKey::F8) => slint::platform::Key::F8.into(),
                            Key::Named(NamedKey::F9) => slint::platform::Key::F9.into(),
                            Key::Named(NamedKey::F10) => slint::platform::Key::F10.into(),
                            Key::Named(NamedKey::F11) => slint::platform::Key::F11.into(),
                            Key::Named(NamedKey::F12) => slint::platform::Key::F12.into(),

                            // Insert key
                            Key::Named(NamedKey::Insert) => slint::platform::Key::Insert.into(),

                            // Other keys - not mapped
                            _ => SharedString::default(),
                        };

                        if !text.is_empty() {
                            let slint_event = match event.state {
                                ElementState::Pressed => {
                                    slint::platform::WindowEvent::KeyPressed { text }
                                }
                                ElementState::Released => {
                                    slint::platform::WindowEvent::KeyReleased { text }
                                }
                            };
                            window.dispatch_event(slint_event);
                        }
                    }

                    WindowEvent::Focused(focused) => {
                        window.dispatch_event(slint::platform::WindowEvent::WindowActiveChanged(
                            focused,
                        ));
                    }

                    WindowEvent::ScaleFactorChanged {
                        scale_factor: new_sf,
                        ..
                    } => {
                        info!("Scale factor changed: {:.2} -> {:.2}", sf, new_sf);
                        // Update our tracked scale factor
                        self.scale_factor.set(new_sf as f32);
                        // Update adapter's scale factor
                        self.adapter.set_scale_factor(new_sf as f32);
                        // Dispatch to Slint
                        window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                            scale_factor: new_sf as f32,
                        });
                    }

                    _ => {}
                }

                // Request redraw after input/state events; after a redraw
                // only while animations run (ovgui patch).
                if !is_redraw || self.adapter.slint_window().has_active_animations() {
                    self.adapter.request_redraw_winit();
                }
            }

            fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
                // ovgui patch: fire due Slint timers, redraw only when a timer
                // fired or an animation runs, and sleep until the next timer
                // (plain `Wait` never woke for Slint timers).
                let due = matches!(
                    slint::platform::duration_until_next_timer_update(),
                    Some(d) if d.is_zero()
                );
                slint::platform::update_timers_and_animations();

                if due || self.adapter.slint_window().has_active_animations() {
                    self.adapter.request_redraw_winit();
                }
                match slint::platform::duration_until_next_timer_update() {
                    Some(d) => event_loop
                        .set_control_flow(ControlFlow::WaitUntil(std::time::Instant::now() + d)),
                    None => event_loop.set_control_flow(ControlFlow::Wait),
                }
            }

            fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
                // Process closures from invoke_from_event_loop in user_event context
                // This runs in the same event loop phase as input events (window_event),
                // which avoids RefCell conflicts that occur when modifying layout-affecting
                // properties during timer processing
                if let Ok(mut queue) = self.closure_queue.lock() {
                    for closure in queue.drain(..) {
                        closure();
                    }
                }
                self.adapter.request_redraw_winit();
            }
        }

        let mut handler = AppHandler {
            adapter,
            cursor_pos,
            scale_factor,
            closure_queue,
            clipboard_enabled,
        };

        event_loop.set_control_flow(ControlFlow::Wait);

        event_loop
            .run_app(&mut handler)
            .map_err(|e| slint::PlatformError::Other(format!("Event loop error: {}", e)))?;

        Ok(())
    }

    fn clipboard_text(&self, clipboard: slint::platform::Clipboard) -> Option<String> {
        // Check if clipboard is enabled
        if !self.config.enable_clipboard {
            return None;
        }
        // Only support default clipboard
        if clipboard != slint::platform::Clipboard::DefaultClipboard {
            return None;
        }
        clipboard_get_text()
    }

    fn set_clipboard_text(&self, text: &str, clipboard: slint::platform::Clipboard) {
        // Check if clipboard is enabled
        if !self.config.enable_clipboard {
            return;
        }
        // Only support default clipboard
        if clipboard != slint::platform::Clipboard::DefaultClipboard {
            return;
        }
        clipboard_set_text(text);
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
        let winit_proxy = self.winit_proxy.borrow().clone()?;
        Some(Box::new(D3D11EventLoopProxy {
            closure_queue: self.closure_queue.clone(),
            winit_proxy,
        }))
    }
}

/// Extract HWND from a winit window
fn get_hwnd(window: &winit::window::Window) -> Result<HWND, slint::PlatformError> {
    let handle = window
        .window_handle()
        .map_err(|e| slint::PlatformError::Other(format!("Failed to get window handle: {}", e)))?;

    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => {
            let hwnd = HWND(win32.hwnd.get() as *mut _);
            Ok(hwnd)
        }
        _ => Err(slint::PlatformError::Other("Not a Win32 window".into())),
    }
}

/// Convert physical key to character (for Ctrl+letter shortcuts)
///
/// When Ctrl is held, winit may not provide the character in logical_key.
/// This function maps physical key codes to their corresponding characters.
fn physical_key_to_char(physical_key: &PhysicalKey) -> Option<char> {
    match physical_key {
        PhysicalKey::Code(code) => match code {
            KeyCode::KeyA => Some('a'),
            KeyCode::KeyB => Some('b'),
            KeyCode::KeyC => Some('c'),
            KeyCode::KeyD => Some('d'),
            KeyCode::KeyE => Some('e'),
            KeyCode::KeyF => Some('f'),
            KeyCode::KeyG => Some('g'),
            KeyCode::KeyH => Some('h'),
            KeyCode::KeyI => Some('i'),
            KeyCode::KeyJ => Some('j'),
            KeyCode::KeyK => Some('k'),
            KeyCode::KeyL => Some('l'),
            KeyCode::KeyM => Some('m'),
            KeyCode::KeyN => Some('n'),
            KeyCode::KeyO => Some('o'),
            KeyCode::KeyP => Some('p'),
            KeyCode::KeyQ => Some('q'),
            KeyCode::KeyR => Some('r'),
            KeyCode::KeyS => Some('s'),
            KeyCode::KeyT => Some('t'),
            KeyCode::KeyU => Some('u'),
            KeyCode::KeyV => Some('v'),
            KeyCode::KeyW => Some('w'),
            KeyCode::KeyX => Some('x'),
            KeyCode::KeyY => Some('y'),
            KeyCode::KeyZ => Some('z'),
            _ => None,
        },
        _ => None,
    }
}

/// Get text from Windows clipboard
fn clipboard_get_text() -> Option<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    // SAFETY: plain Win32 clipboard calls; the clipboard is closed on every path
    // after a successful open, and the locked buffer is read only up to its
    // NUL terminator (CF_UNICODETEXT is NUL-terminated) before being unlocked.
    unsafe {
        // Check if clipboard has text
        if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_err() {
            return None;
        }

        // Open clipboard
        if OpenClipboard(None).is_err() {
            warn!("Failed to open clipboard for reading");
            return None;
        }

        let result = (|| -> Option<String> {
            // Get clipboard data handle
            let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;

            // Lock the memory - handle is HANDLE, need to convert to HGLOBAL
            let hglobal = HGLOBAL(handle.0);
            let ptr = GlobalLock(hglobal) as *const u16;
            if ptr.is_null() {
                return None;
            }

            // Find string length (null-terminated UTF-16)
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }

            // Convert UTF-16 to String
            let slice = std::slice::from_raw_parts(ptr, len);
            let text = String::from_utf16_lossy(slice);

            // Unlock memory
            let _ = GlobalUnlock(hglobal);

            Some(text)
        })();

        // Always close clipboard
        let _ = CloseClipboard();

        result
    }
}

/// Set text to Windows clipboard
fn clipboard_set_text(text: &str) {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    // Convert to UTF-16 with null terminator
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let byte_size = utf16.len() * 2;

    // SAFETY: the clipboard is closed on every path after a successful open; the
    // `byte_size` allocation holds exactly `utf16.len()` u16s, and ownership of
    // `hmem` passes to the clipboard on a successful `SetClipboardData`.
    unsafe {
        // Open clipboard
        if OpenClipboard(None).is_err() {
            warn!("Failed to open clipboard for writing");
            return;
        }

        let success = (|| -> Option<()> {
            // Empty clipboard
            EmptyClipboard().ok()?;

            // Allocate global memory
            let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_size).ok()?;

            // Lock and copy
            let ptr = GlobalLock(hmem) as *mut u16;
            if ptr.is_null() {
                return None;
            }

            std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());

            let _ = GlobalUnlock(hmem);

            // Set clipboard data (clipboard takes ownership of hmem)
            // Convert HGLOBAL to HANDLE
            let handle = HANDLE(hmem.0);
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(handle)).ok()?;

            Some(())
        })();

        // Always close clipboard
        let _ = CloseClipboard();

        if success.is_none() {
            warn!("Failed to set clipboard text");
        }
    }
}
