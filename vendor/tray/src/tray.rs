use std::{
    convert::Infallible,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
};

use crossbeam_channel::{unbounded, Receiver, Sender};
use once_cell::sync::{Lazy, OnceCell};

use crate::{error::Result, icon::Icon, TrayIconImpl};

static COUNTER: AtomicU32 = AtomicU32::new(1);

/// Unique identifier for a tray icon.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TrayIconId(pub String);

impl TrayIconId {
    /// Creates a new tray icon ID from a string.
    pub fn new<S: AsRef<str>>(id: S) -> Self {
        Self(id.as_ref().to_string())
    }

    pub(crate) fn new_unique() -> Self {
        Self(format!("{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)))
    }
}

impl AsRef<str> for TrayIconId {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl<T: ToString> From<T> for TrayIconId {
    fn from(value: T) -> Self {
        Self::new(value.to_string())
    }
}

impl FromStr for TrayIconId {
    type Err = Infallible;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

impl PartialEq<&str> for TrayIconId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<&str> for &TrayIconId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for TrayIconId {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for &TrayIconId {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl PartialEq<&String> for TrayIconId {
    fn eq(&self, other: &&String) -> bool {
        self.0 == **other
    }
}

impl PartialEq<&TrayIconId> for TrayIconId {
    fn eq(&self, other: &&TrayIconId) -> bool {
        other.0 == self.0
    }
}

/// Attributes for creating a tray icon.
#[derive(Default)]
pub struct TrayIconAttributes {
    /// Tooltip text shown when hovering over the icon.
    pub tooltip: Option<String>,
    /// The icon to display.
    pub icon: Option<Icon>,
    /// Custom directory for temporary icon files (Linux only).
    pub temp_dir_path: Option<PathBuf>,
    /// Whether the icon should be treated as a template (macOS only).
    pub icon_is_template: bool,
    /// Title text shown next to the icon (macOS only).
    pub title: Option<String>,
}

/// Builder for creating a [`TrayIcon`].
#[derive(Default)]
pub struct TrayIconBuilder {
    id: TrayIconId,
    attrs: TrayIconAttributes,
}

impl TrayIconBuilder {
    /// Creates a new builder with a unique auto-generated ID.
    pub fn new() -> Self {
        Self {
            id: TrayIconId::new_unique(),
            attrs: TrayIconAttributes::default(),
        }
    }

    /// Sets a custom ID for the tray icon.
    pub fn with_id<I: Into<TrayIconId>>(mut self, id: I) -> Self {
        self.id = id.into();
        self
    }

    /// Sets the icon image.
    pub fn with_icon(mut self, icon: Icon) -> Self {
        self.attrs.icon = Some(icon);
        self
    }

    /// Sets the tooltip text shown on hover.
    pub fn with_tooltip<S: AsRef<str>>(mut self, s: S) -> Self {
        self.attrs.tooltip = Some(s.as_ref().to_string());
        self
    }

    /// Sets the title text shown next to the icon. macOS only.
    pub fn with_title<S: AsRef<str>>(mut self, title: S) -> Self {
        self.attrs.title.replace(title.as_ref().to_string());
        self
    }

    /// Sets a custom directory for temporary icon files. Linux only.
    pub fn with_temp_dir_path<P: AsRef<Path>>(mut self, s: P) -> Self {
        self.attrs.temp_dir_path = Some(s.as_ref().to_path_buf());
        self
    }

    /// Sets whether the icon should be treated as a template image. macOS only.
    pub fn with_icon_as_template(mut self, is_template: bool) -> Self {
        self.attrs.icon_is_template = is_template;
        self
    }

    /// Returns the ID that will be assigned to the tray icon.
    pub fn id(&self) -> &TrayIconId {
        &self.id
    }

    /// Builds and displays the tray icon.
    pub fn build(self) -> Result<TrayIcon> {
        TrayIcon::with_id(self.id, self.attrs)
    }
}

/// A system tray icon.
///
/// This is the main type for interacting with the system tray. It is `Clone` and
/// thread-safe (`Send + Sync`), so it can be shared across threads.
#[derive(Clone)]
pub struct TrayIcon {
    id: TrayIconId,
    tray: Arc<Mutex<TrayIconImpl>>,
}

impl TrayIcon {
    /// Creates a new tray icon with an auto-generated ID.
    pub fn new(attrs: TrayIconAttributes) -> Result<Self> {
        let id = TrayIconId::new_unique();
        Ok(Self {
            tray: Arc::new(Mutex::new(TrayIconImpl::new(id.clone(), attrs)?)),
            id,
        })
    }

    /// Creates a new tray icon with a custom ID.
    pub fn with_id<I: Into<TrayIconId>>(id: I, attrs: TrayIconAttributes) -> Result<Self> {
        let id = id.into();
        Ok(Self {
            tray: Arc::new(Mutex::new(TrayIconImpl::new(id.clone(), attrs)?)),
            id,
        })
    }

    /// Returns the unique identifier for this tray icon.
    pub fn id(&self) -> &TrayIconId {
        &self.id
    }

    /// Sets or clears the icon image.
    pub fn set_icon(&self, icon: Option<Icon>) -> Result<()> {
        self.tray.lock().unwrap().set_icon(icon)
    }

    /// Sets or clears the tooltip text.
    pub fn set_tooltip<S: AsRef<str>>(&self, tooltip: Option<S>) -> Result<()> {
        self.tray.lock().unwrap().set_tooltip(tooltip)
    }

    /// Sets or clears the title text. macOS only.
    pub fn set_title<S: AsRef<str>>(&self, title: Option<S>) {
        self.tray.lock().unwrap().set_title(title)
    }

    /// Shows or hides the tray icon.
    pub fn set_visible(&self, visible: bool) -> Result<()> {
        self.tray.lock().unwrap().set_visible(visible)
    }

    /// Sets the directory for temporary icon files. Linux only.
    pub fn set_temp_dir_path<P: AsRef<Path>>(&self, path: Option<P>) {
        #[cfg(target_os = "linux")]
        self.tray.lock().unwrap().set_temp_dir_path(path);
        #[cfg(not(target_os = "linux"))]
        let _ = path;
    }

    /// Sets whether the icon is a template image. macOS only.
    pub fn set_icon_as_template(&self, is_template: bool) {
        #[cfg(target_os = "macos")]
        self.tray.lock().unwrap().set_icon_as_template(is_template);
        #[cfg(not(target_os = "macos"))]
        let _ = is_template;
    }

    /// Sets the icon and template flag together. macOS only.
    pub fn set_icon_with_as_template(&self, icon: Option<Icon>, is_template: bool) -> Result<()> {
        #[cfg(target_os = "macos")]
        return self
            .tray
            .lock()
            .unwrap()
            .set_icon_with_as_template(icon, is_template);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = icon;
            let _ = is_template;
            Ok(())
        }
    }

    /// Returns the screen rectangle occupied by the tray icon, if available.
    pub fn rect(&self) -> Option<Rect> {
        self.tray.lock().unwrap().rect()
    }
}

/// Events emitted by a tray icon.
///
/// Use [`TrayIconEvent::receiver()`] to get a channel receiver for these events,
/// or [`TrayIconEvent::set_event_handler()`] to set a callback.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type"))]
#[non_exhaustive]
pub enum TrayIconEvent {
    #[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
    Click {
        id: TrayIconId,
        position: dpi::PhysicalPosition<f64>,
        rect: Rect,
        button: MouseButton,
        button_state: MouseButtonState,
    },
    DoubleClick {
        id: TrayIconId,
        position: dpi::PhysicalPosition<f64>,
        rect: Rect,
        button: MouseButton,
    },
    Enter {
        id: TrayIconId,
        position: dpi::PhysicalPosition<f64>,
        rect: Rect,
    },
    Move {
        id: TrayIconId,
        position: dpi::PhysicalPosition<f64>,
        rect: Rect,
    },
    Leave {
        id: TrayIconId,
        position: dpi::PhysicalPosition<f64>,
        rect: Rect,
    },
    // PATCH (trayplay): X11 only for now - a scroll wheel notch arrives as a
    // ButtonPress/Release with detail 4 (up) or 5 (down), which upstream
    // dropped into an ordinary `Click`. `delta` is `-1` for one notch up,
    // `+1` for one notch down (matches the sign convention already used by
    // `sni::Tray::scroll` on the SNI side of trayplay). See PATCH.md.
    Scroll { id: TrayIconId, delta: i32 },
}

/// State of a mouse button (pressed or released).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MouseButtonState {
    #[default]
    Up,
    Down,
}

/// Which mouse button was pressed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

/// Rectangle representing the tray icon's position and size.
#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Rect {
    pub size: dpi::PhysicalSize<u32>,
    pub position: dpi::PhysicalPosition<f64>,
}

impl Default for Rect {
    fn default() -> Self {
        Self {
            size: dpi::PhysicalSize::new(0, 0),
            position: dpi::PhysicalPosition::new(0., 0.),
        }
    }
}

/// Channel receiver for [`TrayIconEvent`]s.
pub type TrayIconEventReceiver = Receiver<TrayIconEvent>;
type TrayIconEventHandler = Box<dyn Fn(TrayIconEvent) + Send + Sync + 'static>;

static TRAY_CHANNEL: Lazy<(Sender<TrayIconEvent>, TrayIconEventReceiver)> = Lazy::new(unbounded);
static TRAY_EVENT_HANDLER: OnceCell<Option<TrayIconEventHandler>> = OnceCell::new();

impl TrayIconEvent {
    /// Returns the ID of the tray icon that emitted this event.
    pub fn id(&self) -> &TrayIconId {
        match self {
            TrayIconEvent::Click { id, .. } => id,
            TrayIconEvent::DoubleClick { id, .. } => id,
            TrayIconEvent::Enter { id, .. } => id,
            TrayIconEvent::Move { id, .. } => id,
            TrayIconEvent::Leave { id, .. } => id,
            TrayIconEvent::Scroll { id, .. } => id,
        }
    }

    /// Returns a global receiver for tray icon events.
    ///
    /// All tray icons send events to this single channel.
    pub fn receiver<'a>() -> &'a TrayIconEventReceiver {
        &TRAY_CHANNEL.1
    }

    /// Sets a global event handler callback.
    ///
    /// If set, events are dispatched to this handler instead of the channel.
    /// Can only be set once; subsequent calls are ignored.
    pub fn set_event_handler<F: Fn(TrayIconEvent) + Send + Sync + 'static>(f: Option<F>) {
        if let Some(f) = f {
            let _ = TRAY_EVENT_HANDLER.set(Some(Box::new(f)));
        } else {
            let _ = TRAY_EVENT_HANDLER.set(None);
        }
    }

    pub(crate) fn send(event: TrayIconEvent) {
        if let Some(handler) = TRAY_EVENT_HANDLER.get_or_init(|| None) {
            handler(event);
        } else {
            let _ = TRAY_CHANNEL.0.send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_icon_id_eq() {
        assert_eq!(TrayIconId::new("t"), "t");
        assert_eq!(TrayIconId::new("t"), String::from("t"));
        assert_eq!(TrayIconId::new("t"), &String::from("t"));
        assert_eq!(TrayIconId::new("t"), TrayIconId::new("t"));
        assert_eq!(TrayIconId::new("t"), &TrayIconId::new("t"));
        assert_eq!(&TrayIconId::new("t"), &TrayIconId::new("t"));
        assert_eq!(TrayIconId::new("t").as_ref(), "t");
    }
}
