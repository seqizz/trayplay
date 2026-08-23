//! Cross-platform system tray icon library.
//!
//! This crate provides a cross-platform API for creating and managing system tray icons
//! with native X11 support on Linux, Shell_NotifyIcon on Windows, and NSStatusItem on macOS.
//!
//! # Features
//!
//! - Native X11 system tray support on Linux
//! - Full mouse event support: click, double-click, enter, leave, move
//! - Thread-safe `TrayIcon` that can be used with async runtimes
//!
//! # Example
//!
//! ```no_run
//! use tray::{Icon, TrayIconBuilder, TrayIconEvent, MouseButton};
//!
//! // Create an icon from RGBA data
//! let icon = Icon::from_rgba(vec![0; 64 * 64 * 4], 64, 64).unwrap();
//!
//! // Build the tray icon
//! let tray = TrayIconBuilder::new()
//!     .with_icon(icon)
//!     .with_tooltip("My App")
//!     .build()
//!     .unwrap();
//!
//! // Handle events
//! let receiver = TrayIconEvent::receiver();
//! loop {
//!     if let Ok(event) = receiver.try_recv() {
//!         match event {
//!             TrayIconEvent::Click { button: MouseButton::Right, .. } => {
//!                 // Show context menu
//!             }
//!             _ => {}
//!         }
//!     }
//!     std::thread::sleep(std::time::Duration::from_millis(100));
//! }
//! ```

#![allow(clippy::uninlined_format_args)]

mod error;
mod icon;
mod tray;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
use linux::{PlatformIcon, TrayIconImpl};
#[cfg(target_os = "windows")]
use windows::{PlatformIcon, TrayIconImpl};
#[cfg(target_os = "macos")]
use macos::{PlatformIcon, TrayIconImpl};

pub use dpi;
pub use error::{Error, Result};
pub use icon::{BadIcon, Icon};
#[cfg(target_os = "macos")]
pub use icon::NativeIcon;
pub use tray::{
    MouseButton, MouseButtonState, Rect, TrayIcon, TrayIconAttributes, TrayIconBuilder,
    TrayIconEvent, TrayIconEventReceiver, TrayIconId,
};
