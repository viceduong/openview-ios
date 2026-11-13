//! D3D11 Window Adapter
//!
//! Implements Slint's WindowAdapter trait using the software renderer
//! with WARP-based presentation.

use super::WarpPresenter;
use anyhow::Result;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Renderer, WindowAdapter};
use slint::{PhysicalSize, Window};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use tracing::info;
use windows::Win32::Foundation::HWND;

/// D3D11 Window Adapter for Slint
///
/// This adapter wraps Slint's MinimalSoftwareWindow and adds WARP-based
/// presentation. The software renderer draws to a pixel buffer, which is
/// then presented via D3D11 WARP SwapChain.
pub struct D3D11WindowAdapter {
    /// Win32 window handle
    hwnd: HWND,

    /// Slint's minimal software window for event handling and rendering
    window: Rc<MinimalSoftwareWindow>,

    /// WARP presenter for D3D11-based pixel presentation
    presenter: RefCell<WarpPresenter>,

    /// Pixel buffer for software rendering (RGBA format)
    buffer: RefCell<Vec<PremultipliedRgbaColor>>,

    /// Current window size (physical pixels)
    size: RefCell<PhysicalSize>,

    /// Current scale factor
    scale_factor: Cell<f32>,

    /// Winit window handle (kept alive for event handling)
    winit_window: RefCell<Option<winit::window::Window>>,
}

impl D3D11WindowAdapter {
    /// Create a new D3D11 window adapter
    ///
    /// # Arguments
    ///
    /// * `hwnd` - Handle to the Win32 window
    /// * `width` - Initial width in physical pixels
    /// * `height` - Initial height in physical pixels
    /// * `scale_factor` - Display scale factor
    pub fn new(hwnd: HWND, width: u32, height: u32, scale_factor: f32) -> Result<Rc<Self>> {
        info!(
            "Creating D3D11WindowAdapter: {}x{} physical (scale: {:.2})",
            width, height, scale_factor
        );

        // Create WARP presenter
        let presenter = WarpPresenter::new(hwnd, width, height)?;

        // Create minimal software window with new buffer each frame
        // This avoids issues with buffer age tracking on headless systems
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);

        // Allocate pixel buffer
        let buffer_size = (width * height) as usize;
        let buffer = vec![PremultipliedRgbaColor::default(); buffer_size];

        let size = PhysicalSize::new(width, height);

        let adapter = Rc::new(Self {
            hwnd,
            window,
            presenter: RefCell::new(presenter),
            buffer: RefCell::new(buffer),
            size: RefCell::new(size),
            scale_factor: Cell::new(scale_factor),
            winit_window: RefCell::new(None),
        });

        // Set window size using physical size
        // The scale factor will be set via ScaleFactorChanged event from platform
        adapter.window.set_size(slint::WindowSize::Physical(size));

        info!("D3D11WindowAdapter created successfully");

        Ok(adapter)
    }

    /// Get the Win32 HWND for this window
    ///
    /// Use this to set up modal dialogs or access native window features.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let hwnd = adapter.hwnd();
    /// let handle = mbfs_slint::dialog::WindowHandle::new(hwnd);
    /// let dialog = rfd::AsyncFileDialog::new().set_parent(&handle);
    /// ```
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Get the HWND as raw isize for storage
    ///
    /// Useful for storing in an AtomicIsize or passing to external code.
    pub fn hwnd_raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// Store the winit window handle
    pub fn set_winit_window(&self, window: winit::window::Window) {
        *self.winit_window.borrow_mut() = Some(window);
    }

    /// Get reference to winit window (if available)
    pub fn winit_window(&self) -> Option<std::cell::Ref<'_, winit::window::Window>> {
        let borrow = self.winit_window.borrow();
        if borrow.is_some() {
            Some(std::cell::Ref::map(borrow, |opt| opt.as_ref().unwrap()))
        } else {
            None
        }
    }

    /// Get current scale factor
    pub fn scale_factor(&self) -> f32 {
        self.scale_factor.get()
    }

    /// Update the scale factor
    pub fn set_scale_factor(&self, scale_factor: f32) {
        self.scale_factor.set(scale_factor);
    }

    /// Request redraw via winit window
    pub fn request_redraw_winit(&self) {
        if let Some(ref window) = *self.winit_window.borrow() {
            window.request_redraw();
        }
    }

    /// Render a frame and present it via WARP
    ///
    /// This is called from the event loop after processing events.
    pub fn render_frame(&self) -> Result<()> {
        let size = *self.size.borrow();
        let width = size.width;
        let height = size.height;

        if width == 0 || height == 0 {
            return Ok(());
        }

        // Ensure buffer is correct size
        {
            let mut buffer = self.buffer.borrow_mut();
            let expected_len = (width * height) as usize;
            if buffer.len() != expected_len {
                buffer.resize(expected_len, PremultipliedRgbaColor::default());
            }
        }

        // Render via software renderer
        {
            let mut buffer = self.buffer.borrow_mut();
            let drawn = self.window.draw_if_needed(|renderer| {
                renderer.render(&mut buffer, width as usize);
            });
            // ovgui patch: nothing changed -> keep the presented frame
            // (avoids converting + presenting a full frame per wake-up).
            if !drawn {
                return Ok(());
            }
        }

        // Convert RGBA buffer to u32 and present via WARP
        {
            let buffer = self.buffer.borrow();
            let pixels: Vec<u32> = buffer
                .iter()
                .map(|c| {
                    // PremultipliedRgbaColor fields are u8
                    // D3D11 expects BGRA format
                    let r = c.red;
                    let g = c.green;
                    let b = c.blue;
                    let a = c.alpha;
                    // BGRA format: B in low byte, A in high byte
                    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | (b as u32)
                })
                .collect();

            let presenter = self.presenter.borrow();
            presenter.present(&pixels)?;
        }

        Ok(())
    }

    /// Handle window resize (physical pixels)
    pub fn resize(&self, width: u32, height: u32) -> Result<()> {
        let current_size = *self.size.borrow();
        if current_size.width == width && current_size.height == height {
            return Ok(());
        }

        if width == 0 || height == 0 {
            return Ok(());
        }

        let sf = self.scale_factor.get();
        info!(
            "Resizing D3D11WindowAdapter: {}x{} physical ({:.0}x{:.0} logical)",
            width,
            height,
            width as f32 / sf,
            height as f32 / sf
        );

        // Update size
        let new_size = PhysicalSize::new(width, height);
        *self.size.borrow_mut() = new_size;

        // Resize presenter
        self.presenter.borrow_mut().resize(width, height)?;

        // Resize buffer
        let buffer_size = (width * height) as usize;
        self.buffer
            .borrow_mut()
            .resize(buffer_size, PremultipliedRgbaColor::default());

        // Update Slint window size (physical)
        self.window.set_size(slint::WindowSize::Physical(new_size));

        Ok(())
    }

    /// Get the Slint window for dispatching events
    pub fn slint_window(&self) -> &Window {
        self.window.window()
    }
}

impl WindowAdapter for D3D11WindowAdapter {
    fn window(&self) -> &Window {
        self.window.window()
    }

    fn size(&self) -> PhysicalSize {
        *self.size.borrow()
    }

    fn renderer(&self) -> &dyn Renderer {
        self.window.renderer()
    }

    fn request_redraw(&self) {
        self.window.request_redraw();
        self.request_redraw_winit();
    }

    // NOTE — Slint never dispatches to this impl beyond `window()`.
    //
    // `MinimalSoftwareWindow::new()` builds its `Window` with `Rc::new_cyclic`,
    // so that `Window`'s adapter weak-ref points at the MinimalSoftwareWindow
    // ITSELF. We hand Slint that same `Window` from `window()` above, and every
    // core call site resolves the adapter through
    // `WindowInner::window_adapter()` — which therefore lands on the
    // MinimalSoftwareWindow, not on us. `size`, `renderer` and `request_redraw`
    // here are satisfied trait obligations that the platform's own event loop
    // calls directly; they are not Slint's dispatch path.
    //
    // Consequence worth knowing before you try: implementing
    // `update_window_properties` here to forward `title:` from the .slint does
    // NOT work — it is never called. (MinimalSoftwareWindow leaves that method
    // at its no-op default, which is why the title never moves.) Making it work
    // means this adapter owning its own `Window` via `Rc::new_cyclic` and
    // delegating rendering to the SoftwareRenderer — a real rework of this
    // platform. Until then the native title is set at window creation from
    // `WindowConfig::window_title`; see `crate::platform`.
}
