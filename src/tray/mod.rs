pub mod sni;
pub mod xembed;

use crate::player::State;

/// Requests the tray sends to the GTK main thread.
///
/// Both tray backends run off the GTK thread (SNI's `ksni::Tray` callbacks run
/// on its own zbus task, XEmbed's window is a GTK window but must not touch
/// other GTK windows directly from inside a click handler), so window work is
/// funnelled through an async_channel instead. Playback commands do not need
/// this detour, they go straight to the player.
// ShowPopup/HidePopup are unused until MPRIS Raise and the browse views land.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum UiRequest {
    TogglePopup,
    ShowPopup,
    HidePopup,
    Quit,
}

/// Standard freedesktop icon names, shared by both backends so the tray looks
/// the same regardless of which one is active. Under SNI the *host* resolves
/// these; under XEmbed trayplay resolves them itself through GTK's icon
/// theme - same names, same visual result either way.
pub fn icon_name_for_state(state: State) -> &'static str {
    match state {
        State::Playing => "media-playback-start",
        State::Paused => "media-playback-pause",
        State::Stopped => "multimedia-player",
    }
}

/// Whichever tray backend got started, kept alive for the process lifetime -
/// dropping the SNI handle unregisters the StatusNotifierItem, and dropping
/// the XEmbed handle tears down its window. Neither variant's payload is ever
/// read back out; holding it is the whole point.
#[allow(dead_code)]
pub enum TrayBackend {
    Sni(ksni::Handle<sni::Tray>),
    XEmbed(xembed::Handle),
}
