//! WARP Presenter - D3D11 WARP-based pixel presentation
//!
//! Uses D3D11 with WARP (Windows Advanced Rasterization Platform) software
//! rasterizer to present pixels via DXGI SwapChain, bypassing GDI entirely.

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use windows::{
    core::Interface,
    Win32::{
        Foundation::{HMODULE, HWND},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
                D3D11_MAP_WRITE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            },
            Dxgi::{
                Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT_ALLOW_TEARING,
                DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING,
                DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
            },
        },
    },
};

/// WARP Presenter for D3D11-based pixel presentation
///
/// This struct manages the D3D11 WARP device and DXGI SwapChain for
/// presenting software-rendered pixels without relying on GDI.
pub struct WarpPresenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    staging_texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl WarpPresenter {
    /// Create a new WARP presenter for the given window
    ///
    /// # Arguments
    ///
    /// * `hwnd` - Handle to the target window
    /// * `width` - Initial width in pixels
    /// * `height` - Initial height in pixels
    ///
    /// # Errors
    ///
    /// Returns an error if WARP device creation fails or SwapChain cannot be created.
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        info!(
            "Creating WARP presenter: {}x{} for hwnd {:?}",
            width, height, hwnd
        );

        // Create D3D11 device with WARP driver
        let (device, context) = Self::create_warp_device()?;

        // Create DXGI SwapChain
        let swap_chain = Self::create_swap_chain(&device, hwnd, width, height)?;

        // Create staging texture for CPU writes
        let staging_texture = Self::create_staging_texture(&device, width, height)?;

        info!("WARP presenter created successfully");

        Ok(Self {
            device,
            context,
            swap_chain,
            staging_texture,
            width,
            height,
        })
    }

    /// Create D3D11 device with WARP driver
    fn create_warp_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        let feature_levels = [D3D_FEATURE_LEVEL_11_0];

        // Create device with WARP driver and BGRA support
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;

        // SAFETY: all out-pointers are valid `Option`s owned by this frame; no adapter or
        // software module is passed for the WARP driver.
        unsafe {
            D3D11CreateDevice(
                None,                  // No adapter (use WARP)
                D3D_DRIVER_TYPE_WARP,  // WARP software driver
                HMODULE::default(),    // No software module
                flags,                 // Device flags
                Some(&feature_levels), // Feature levels
                7,                     // SDK version (D3D11_SDK_VERSION)
                Some(&mut device),     // Output device
                None,                  // Output feature level
                Some(&mut context),    // Output immediate context
            )
            .context("Failed to create D3D11 WARP device")?;
        }

        let device = device.context("D3D11 device was not created")?;
        let context = context.context("D3D11 context was not created")?;

        debug!("Created D3D11 WARP device");

        Ok((device, context))
    }

    /// Create DXGI SwapChain for the window
    fn create_swap_chain(
        device: &ID3D11Device,
        hwnd: HWND,
        width: u32,
        height: u32,
    ) -> Result<IDXGISwapChain1> {
        // Get DXGI device and factory
        let dxgi_device: IDXGIDevice = device.cast()?;
        // SAFETY: COM call on a live interface obtained above.
        let dxgi_adapter = unsafe { dxgi_device.GetAdapter()? };
        // SAFETY: COM call on a live interface obtained above.
        let dxgi_factory: IDXGIFactory2 = unsafe { dxgi_adapter.GetParent()? };

        // SwapChain description
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM, // BGRA format
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2, // Double buffering
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD, // Modern flip model
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32,
        };

        // SAFETY: `hwnd` is the live window this presenter renders into and
        // `desc` outlives the call.
        let swap_chain =
            unsafe { dxgi_factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None)? };

        debug!("Created DXGI SwapChain: {}x{}", width, height);

        Ok(swap_chain)
    }

    /// Create staging texture for CPU→GPU pixel copy
    fn create_staging_texture(
        device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0, // No binding for staging texture
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };

        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` and the out-slot outlive the call; no initial data is passed.
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .context("Failed to create staging texture")?;
        }

        let texture = texture.context("Staging texture was not created")?;

        debug!("Created staging texture: {}x{}", width, height);

        Ok(texture)
    }

    /// Present pixels to the window
    ///
    /// Takes RGBA pixels from Slint's software renderer, converts to BGRA,
    /// copies to staging texture, then copies to backbuffer and presents.
    ///
    /// # Arguments
    ///
    /// * `pixels` - RGBA pixel data from software renderer (row-major, width * height elements)
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success, or an error if presentation fails.
    pub fn present(&self, pixels: &[u32]) -> Result<()> {
        let expected_len = (self.width * self.height) as usize;
        if pixels.len() != expected_len {
            anyhow::bail!(
                "Pixel buffer size mismatch: expected {}, got {}",
                expected_len,
                pixels.len()
            );
        }

        // SAFETY: the mapped staging texture is `width` x `height` BGRA, so each row of
        // `RowPitch` bytes holds at least `width` u32s; `pixels` was checked above
        // to hold exactly `width * height` elements. The texture is unmapped before
        // any other use.
        unsafe {
            // Map staging texture
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(
                    &self.staging_texture,
                    0,
                    D3D11_MAP_WRITE,
                    0,
                    Some(&mut mapped),
                )
                .context("Failed to map staging texture")?;

            // Copy pixels (already in BGRA format from window_adapter)
            let dst_ptr = mapped.pData as *mut u32;
            let dst_pitch = mapped.RowPitch as usize / 4; // Elements per row

            for y in 0..self.height as usize {
                let src_row = &pixels[y * self.width as usize..(y + 1) * self.width as usize];
                let dst_row =
                    std::slice::from_raw_parts_mut(dst_ptr.add(y * dst_pitch), self.width as usize);

                // Direct copy - window_adapter already outputs BGRA format
                dst_row.copy_from_slice(src_row);
            }

            // Unmap staging texture
            self.context.Unmap(&self.staging_texture, 0);

            // Get backbuffer and copy staging texture to it
            let backbuffer: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;
            self.context
                .CopyResource(&backbuffer, &self.staging_texture);

            // Present with immediate mode (no vsync, allow tearing)
            // This bypasses display driver refresh rate limitations
            self.swap_chain
                .Present(0, DXGI_PRESENT_ALLOW_TEARING)
                .ok()
                .context("SwapChain Present failed")?;
        }

        Ok(())
    }

    /// Resize the presenter for a new window size
    ///
    /// Must be called when the window is resized.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        if width == self.width && height == self.height {
            return Ok(());
        }

        info!(
            "Resizing WARP presenter: {}x{} → {}x{}",
            self.width, self.height, width, height
        );

        // Release old staging texture
        drop(std::mem::replace(
            &mut self.staging_texture,
            Self::create_staging_texture(&self.device, width, height)?,
        ));

        // Resize swap chain buffers
        // SAFETY: no backbuffer reference is held across this call (each `present`
        // drops its `GetBuffer` result), as `ResizeBuffers` requires.
        unsafe {
            // Must release all references to backbuffer before resize
            self.swap_chain.ResizeBuffers(
                0, // Preserve buffer count
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING,
            )?;
        }

        self.width = width;
        self.height = height;

        info!("WARP presenter resized to {}x{}", width, height);

        Ok(())
    }

    /// Get current width
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Get current height
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Check if WARP is available on this system
    ///
    /// Returns true if WARP device can be created, false otherwise.
    pub fn is_available() -> bool {
        match Self::create_warp_device() {
            Ok(_) => {
                info!("WARP is available on this system");
                true
            }
            Err(e) => {
                warn!("WARP is not available: {}", e);
                false
            }
        }
    }
}
