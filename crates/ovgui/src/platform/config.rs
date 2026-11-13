//! Window configuration for Slint platform
//!
//! Provides configurable window settings for the D3D11 WARP platform.

use std::path::Path;
use tracing::warn;

/// Window position options
#[derive(Debug, Clone, Default)]
pub enum WindowPosition {
    /// Let the system decide window position
    #[default]
    Default,
    /// Center window on primary monitor
    Center,
    /// Specific position in logical pixels
    At(i32, i32),
}

/// Configuration for Slint window creation
///
/// # Example
///
/// ```rust,ignore
/// use mbfs_slint::WindowConfig;
///
/// let config = WindowConfig::builder()
///     .window_title("My Application")
///     .window_icon_bytes(include_bytes!("icon.png"))
///     .screen_fraction(0.75, 16, 9)
///     .min_size(800, 600)
///     .center_on_screen()
///     .always_on_top(false)
///     .enable_clipboard(true)
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct WindowConfig {
    /// Window title displayed in the title bar
    pub window_title: String,
    /// Window icon (RGBA pixels, width, height)
    pub window_icon: Option<WindowIcon>,
    /// Initial window width in logical pixels
    pub initial_width: u32,
    /// Initial window height in logical pixels
    pub initial_height: u32,
    /// Minimum window width in logical pixels (None = no constraint)
    pub min_width: Option<u32>,
    /// Minimum window height in logical pixels (None = no constraint)
    pub min_height: Option<u32>,
    /// Window position
    pub position: WindowPosition,
    /// Keep window always on top
    pub always_on_top: bool,
    /// Enable clipboard support (copy/paste)
    pub enable_clipboard: bool,
    /// Allow tearing for immediate presentation (ideal for headless servers)
    pub enable_tearing: bool,
    /// Size window as a fraction of screen size with a given aspect ratio.
    /// (fraction 0.0-1.0, aspect_width, aspect_height).
    /// When set, overrides initial_width/initial_height at window creation time.
    pub screen_fraction: Option<(f32, u32, u32)>,
}

/// Window icon data (RGBA format)
#[derive(Debug, Clone)]
pub struct WindowIcon {
    /// RGBA pixel data
    pub rgba: Vec<u8>,
    /// Icon width in pixels
    pub width: u32,
    /// Icon height in pixels
    pub height: u32,
}

impl WindowIcon {
    /// Create icon from RGBA pixel data
    pub fn from_rgba(rgba: Vec<u8>, width: u32, height: u32) -> Option<Self> {
        let expected_len = (width * height * 4) as usize;
        if rgba.len() != expected_len {
            warn!(
                "Icon RGBA data length mismatch: expected {}, got {}",
                expected_len,
                rgba.len()
            );
            return None;
        }
        Some(Self {
            rgba,
            width,
            height,
        })
    }

    // NOTE: the upstream `from_png_bytes` / `from_png_file` helpers decoded
    // PNGs with the `image` crate. Dropped here to keep the boilerplate lean;
    // use `from_rgba` with raw pixels, or add the `image` dep back and restore
    // these two methods. (See the upstream vd-gui/mbfs copy for the original.)
    #[allow(dead_code)]
    pub fn from_png_bytes(_bytes: &[u8]) -> Option<Self> {
        None
    }

    #[allow(dead_code)]
    pub fn from_png_file(_path: impl AsRef<Path>) -> Option<Self> {
        None
    }

    /// Convert to winit Icon
    #[cfg(windows)]
    pub(crate) fn to_winit_icon(&self) -> Option<winit::window::Icon> {
        winit::window::Icon::from_rgba(self.rgba.clone(), self.width, self.height).ok()
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            window_title: "MBFS Application".into(),
            window_icon: None,
            initial_width: 1280,
            initial_height: 720,
            min_width: None,
            min_height: None,
            position: WindowPosition::Default,
            always_on_top: false,
            enable_clipboard: true,
            enable_tearing: true,
            screen_fraction: None,
        }
    }
}

impl WindowConfig {
    /// Create a new WindowConfig builder
    pub fn builder() -> WindowConfigBuilder {
        WindowConfigBuilder::default()
    }
}

/// Builder for WindowConfig
///
/// # Example
///
/// ```rust,ignore
/// use mbfs_slint::WindowConfig;
///
/// let config = WindowConfig::builder()
///     .window_title("My App")
///     .window_icon_bytes(include_bytes!("icon.png"))
///     .initial_size(1600, 900)
///     .center_on_screen()
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct WindowConfigBuilder {
    config: WindowConfig,
}

impl Default for WindowConfigBuilder {
    fn default() -> Self {
        Self {
            config: WindowConfig::default(),
        }
    }
}

impl WindowConfigBuilder {
    /// Set the window title
    pub fn window_title(mut self, title: impl Into<String>) -> Self {
        self.config.window_title = title.into();
        self
    }

    /// Set window icon from RGBA pixel data
    pub fn window_icon_rgba(mut self, rgba: Vec<u8>, width: u32, height: u32) -> Self {
        self.config.window_icon = WindowIcon::from_rgba(rgba, width, height);
        self
    }

    /// Set window icon from PNG bytes (e.g., from include_bytes!)
    pub fn window_icon_bytes(mut self, png_bytes: &[u8]) -> Self {
        self.config.window_icon = WindowIcon::from_png_bytes(png_bytes);
        self
    }

    /// Set window icon from PNG file path
    pub fn window_icon_file(mut self, path: impl AsRef<Path>) -> Self {
        self.config.window_icon = WindowIcon::from_png_file(path);
        self
    }

    /// Set window icon directly
    pub fn window_icon(mut self, icon: Option<WindowIcon>) -> Self {
        self.config.window_icon = icon;
        self
    }

    /// Set the initial window size in logical pixels
    pub fn initial_size(mut self, width: u32, height: u32) -> Self {
        self.config.initial_width = width;
        self.config.initial_height = height;
        self
    }

    /// Set the initial window width in logical pixels
    pub fn initial_width(mut self, width: u32) -> Self {
        self.config.initial_width = width;
        self
    }

    /// Set the initial window height in logical pixels
    pub fn initial_height(mut self, height: u32) -> Self {
        self.config.initial_height = height;
        self
    }

    /// Set the minimum window size in logical pixels
    pub fn min_size(mut self, width: u32, height: u32) -> Self {
        self.config.min_width = Some(width);
        self.config.min_height = Some(height);
        self
    }

    /// Set the minimum window width in logical pixels
    pub fn min_width(mut self, width: u32) -> Self {
        self.config.min_width = Some(width);
        self
    }

    /// Set the minimum window height in logical pixels
    pub fn min_height(mut self, height: u32) -> Self {
        self.config.min_height = Some(height);
        self
    }

    /// Size window as a fraction of screen size with a given aspect ratio
    ///
    /// The window will be sized to `fraction` of the monitor's logical size,
    /// constrained to the given aspect ratio (width:height). Falls back to
    /// `initial_size` if no monitor is detected.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // 75% of screen, 16:9 aspect ratio
    /// .screen_fraction(0.75, 16, 9)
    /// ```
    pub fn screen_fraction(mut self, fraction: f32, aspect_width: u32, aspect_height: u32) -> Self {
        self.config.screen_fraction = Some((fraction.clamp(0.1, 1.0), aspect_width, aspect_height));
        self
    }

    /// Set window position to specific coordinates (logical pixels)
    pub fn position(mut self, x: i32, y: i32) -> Self {
        self.config.position = WindowPosition::At(x, y);
        self
    }

    /// Center window on primary monitor
    pub fn center_on_screen(mut self) -> Self {
        self.config.position = WindowPosition::Center;
        self
    }

    /// Use system default window position
    pub fn default_position(mut self) -> Self {
        self.config.position = WindowPosition::Default;
        self
    }

    /// Set always-on-top mode
    pub fn always_on_top(mut self, enabled: bool) -> Self {
        self.config.always_on_top = enabled;
        self
    }

    /// Enable or disable clipboard support
    pub fn enable_clipboard(mut self, enable: bool) -> Self {
        self.config.enable_clipboard = enable;
        self
    }

    /// Enable or disable tearing (immediate presentation)
    ///
    /// Tearing is recommended for headless servers where vsync
    /// would cause issues with slow display refresh rates.
    pub fn enable_tearing(mut self, enable: bool) -> Self {
        self.config.enable_tearing = enable;
        self
    }

    /// Build the WindowConfig
    pub fn build(self) -> WindowConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = WindowConfig::default();
        assert_eq!(config.window_title, "MBFS Application");
        assert_eq!(config.initial_width, 1280);
        assert_eq!(config.initial_height, 720);
        assert!(config.min_width.is_none());
        assert!(config.min_height.is_none());
        assert!(config.window_icon.is_none());
        assert!(matches!(config.position, WindowPosition::Default));
        assert!(!config.always_on_top);
        assert!(config.enable_clipboard);
        assert!(config.enable_tearing);
    }

    #[test]
    fn test_builder() {
        let config = WindowConfig::builder()
            .window_title("Test App")
            .initial_size(800, 600)
            .min_size(400, 300)
            .center_on_screen()
            .always_on_top(true)
            .enable_clipboard(false)
            .enable_tearing(false)
            .build();

        assert_eq!(config.window_title, "Test App");
        assert_eq!(config.initial_width, 800);
        assert_eq!(config.initial_height, 600);
        assert_eq!(config.min_width, Some(400));
        assert_eq!(config.min_height, Some(300));
        assert!(matches!(config.position, WindowPosition::Center));
        assert!(config.always_on_top);
        assert!(!config.enable_clipboard);
        assert!(!config.enable_tearing);
    }

    #[test]
    fn test_position_at() {
        let config = WindowConfig::builder().position(100, 200).build();

        match config.position {
            WindowPosition::At(x, y) => {
                assert_eq!(x, 100);
                assert_eq!(y, 200);
            }
            _ => panic!("Expected WindowPosition::At"),
        }
    }

    #[test]
    fn test_screen_fraction() {
        let config = WindowConfig::builder().screen_fraction(0.75, 16, 9).build();

        let (fraction, aw, ah) = config.screen_fraction.unwrap();
        assert!((fraction - 0.75).abs() < f32::EPSILON);
        assert_eq!(aw, 16);
        assert_eq!(ah, 9);
    }

    #[test]
    fn test_screen_fraction_clamp() {
        let config = WindowConfig::builder().screen_fraction(2.0, 16, 9).build();
        let (fraction, _, _) = config.screen_fraction.unwrap();
        assert!((fraction - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_window_icon_rgba() {
        // 2x2 RGBA image (16 bytes)
        let rgba = vec![0u8; 16];
        let icon = WindowIcon::from_rgba(rgba, 2, 2);
        assert!(icon.is_some());

        // Wrong size should fail
        let rgba = vec![0u8; 10];
        let icon = WindowIcon::from_rgba(rgba, 2, 2);
        assert!(icon.is_none());
    }
}
