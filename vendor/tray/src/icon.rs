use std::{error::Error, fmt, io};

use crate::PlatformIcon;

/// Errors that can occur when creating an [`Icon`].
#[derive(Debug)]
pub enum BadIcon {
    /// The RGBA byte array length is not divisible by 4.
    ByteCountNotDivisibleBy4 { byte_count: usize },
    /// The specified dimensions don't match the number of pixels in the RGBA data.
    DimensionsVsPixelCount {
        width: u32,
        height: u32,
        width_x_height: usize,
        pixel_count: usize,
    },
    /// An OS-level error occurred while creating the icon.
    OsError(io::Error),
}

impl fmt::Display for BadIcon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BadIcon::ByteCountNotDivisibleBy4 { byte_count } => write!(
                f,
                "The length of the `rgba` argument ({:?}) isn't divisible by 4, making it impossible to interpret as 32bpp RGBA pixels.",
                byte_count,
            ),
            BadIcon::DimensionsVsPixelCount {
                width,
                height,
                width_x_height,
                pixel_count,
            } => write!(
                f,
                "The specified dimensions ({:?}x{:?}) don't match the number of pixels supplied by the `rgba` argument ({:?}). For those dimensions, the expected pixel count is {:?}.",
                width, height, pixel_count, width_x_height,
            ),
            BadIcon::OsError(e) => write!(f, "OS error when instantiating the icon: {:?}", e),
        }
    }
}

impl Error for BadIcon {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            BadIcon::OsError(e) => Some(e),
            _ => None,
        }
    }
}

/// A tray icon image.
///
/// Create an icon from RGBA pixel data using [`Icon::from_rgba`], or on Windows
/// from a file path, resource, or handle.
#[derive(Clone)]
pub struct Icon {
    pub(crate) inner: PlatformIcon,
}

impl fmt::Debug for Icon {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        fmt::Debug::fmt(&self.inner, formatter)
    }
}

impl Icon {
    /// Creates an icon from 32-bit RGBA pixel data.
    ///
    /// The `rgba` vector must contain exactly `width * height * 4` bytes.
    pub fn from_rgba(rgba: Vec<u8>, width: u32, height: u32) -> Result<Self, BadIcon> {
        Ok(Icon {
            inner: PlatformIcon::from_rgba(rgba, width, height)?,
        })
    }

    /// Creates an icon from a file path. Windows only.
    ///
    /// Supports `.ico` files. If `size` is `None`, uses the system default size.
    #[cfg(windows)]
    pub fn from_path<P: AsRef<std::path::Path>>(
        path: P,
        size: Option<(u32, u32)>,
    ) -> Result<Self, BadIcon> {
        let win_icon = PlatformIcon::from_path(path, size)?;
        Ok(Icon { inner: win_icon })
    }

    /// Creates an icon from an embedded resource by ordinal. Windows only.
    #[cfg(windows)]
    pub fn from_resource(ordinal: u16, size: Option<(u32, u32)>) -> Result<Self, BadIcon> {
        let win_icon = PlatformIcon::from_resource(ordinal, size)?;
        Ok(Icon { inner: win_icon })
    }

    /// Creates an icon from an embedded resource by name. Windows only.
    #[cfg(windows)]
    pub fn from_resource_name(
        resource_name: &str,
        size: Option<(u32, u32)>,
    ) -> Result<Self, BadIcon> {
        let win_icon = PlatformIcon::from_resource_name(resource_name, size)?;
        Ok(Icon { inner: win_icon })
    }

    /// Creates an icon from an existing `HICON` handle. Windows only.
    ///
    /// The handle is not owned and will not be destroyed when the icon is dropped.
    #[cfg(windows)]
    pub fn from_handle(handle: isize) -> Self {
        let win_icon = PlatformIcon::from_handle(handle as _);
        Icon { inner: win_icon }
    }
}

/// Named system icons available on macOS.
///
/// These correspond to `NSImage` named images provided by AppKit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg(target_os = "macos")]
pub enum NativeIcon {
    Add,
    Advanced,
    Bluetooth,
    Bookmarks,
    Caution,
    ColorPanel,
    ColumnView,
    Computer,
    EnterFullScreen,
    Everyone,
    ExitFullScreen,
    FlowView,
    Folder,
    FolderBurnable,
    FolderSmart,
    FollowLinkFreestanding,
    FontPanel,
    GoLeft,
    GoRight,
    Home,
    IChatTheater,
    IconView,
    Info,
    InvalidDataFreestanding,
    LeftFacingTriangle,
    ListView,
    LockLocked,
    LockUnlocked,
    MenuMixedState,
    MenuOnState,
    MobileMe,
    MultipleDocuments,
    Network,
    Path,
    PreferencesGeneral,
    QuickLook,
    RefreshFreestanding,
    Refresh,
    Remove,
    RevealFreestanding,
    RightFacingTriangle,
    Share,
    Slideshow,
    SmartBadge,
    StatusAvailable,
    StatusNone,
    StatusPartiallyAvailable,
    StatusUnavailable,
    StopProgressFreestanding,
    StopProgress,
    TrashEmpty,
    TrashFull,
    User,
    UserAccounts,
    UserGroup,
    UserGuest,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_icon_from_rgba_valid() {
        let rgba = vec![0u8; 4 * 10 * 10];
        let icon = Icon::from_rgba(rgba, 10, 10);
        assert!(icon.is_ok());
    }

    #[test]
    fn test_icon_from_rgba_byte_count_not_divisible_by_4() {
        let rgba = vec![0u8; 5];
        let icon = Icon::from_rgba(rgba, 1, 1);
        assert!(matches!(icon, Err(BadIcon::ByteCountNotDivisibleBy4 { .. })));
    }

    #[test]
    fn test_icon_from_rgba_dimensions_mismatch() {
        let rgba = vec![0u8; 4 * 10];
        let icon = Icon::from_rgba(rgba, 5, 5);
        assert!(matches!(icon, Err(BadIcon::DimensionsVsPixelCount { .. })));
    }
}
