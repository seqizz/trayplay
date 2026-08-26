//! Native XEmbed tray icon for X11.
//!
//! A `gtk::Window`/`GdkSurface` cannot be used for this: GDK4's X11 backend
//! does not tolerate being reparented by an external window manager the way
//! docking into a systray requires - the tray host embeds the window fine
//! (`_NET_SYSTEM_TRAY_S<screen>` docking succeeds), but GDK then tears the
//! surface down as if it had been destroyed, so the icon flashes and
//! vanishes. Confirmed against AwesomeWM's `wibox.widget.systray`: the icon
//! registers, then immediately disappears, and awesome's own systray widget
//! logs a stale-object error moments later trying to keep tracking it - GTK4
//! side pulls the window out from under it, not the other way round.
//!
//! The `tray` crate sidesteps this entirely by never handing the icon window
//! to GDK in the first place: it owns a raw X11 window via its own `x11rb`
//! connection and draws into it directly, so an external reparent is a
//! complete non-event. GTK is only used here to resolve `icon_name`s to
//! pixels (`gtk::IconTheme`, rendered through GTK's own snapshot/render-node
//! pipeline) - the icon window itself never touches GTK/GDK.
//!
//! Rendering deliberately does not go through `gdk_pixbuf::Pixbuf::from_file`:
//! that path needs the separate gdk-pixbuf SVG *loader module* (`librsvg`'s
//! `libpixbufloader-svg.so`, registered via a loaders cache), which is not on
//! the search path in a plain `nix develop -c cargo run` shell the way it
//! would be after `wrapGAppsHook4` wires it up at install time - it failed
//! with "rasterizing icon theme file" under `cargo run` for exactly this
//! reason. GTK's own icon rendering (what every `gtk::Image::from_icon_name`
//! elsewhere in this app already uses successfully) goes through
//! `IconPaintable`/`SymbolicPaintable::snapshot_symbolic` instead, which draws
//! SVG icons via GTK's own linked-in librsvg, not the loader-module system -
//! so it works in both environments. Plain `Paintable::snapshot()` is *not*
//! equivalent here: for a symbolic icon it ignores the artwork's actual shape
//! entirely and paints a solid swatch over the whole bounding box (confirmed
//! by dumping the rendered alpha channel - 100% opaque). Recolouring a
//! symbolic icon is a different interface with its own method that takes the
//! colour(s) explicitly, because there is no widget/CSS context here to
//! resolve "theme foreground" from.
//!
//! No right-click menu: `tray::TrayIconEvent` has no concept of one on
//! Linux (X11 systray icons are just a window; the tray host does not host a
//! menu for you the way SNI does), and pulling in `tray-menu`'s GTK feature
//! would mean linking a second full copy of GTK3 into this GTK4 process just
//! for six menu items - not worth it. Quit is reachable elsewhere (bound to
//! Ctrl+Q on the popup window).
use anyhow::{Context, Result};
use gtk::gdk;
use gtk::prelude::*;
use tray::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent, TrayIconId};

use crate::player::{Command, Event as PlayerEvent, PlayerHandle, State};

use super::UiRequest;

/// Icon names for this backend, deliberately *not* the freedesktop standard
/// names `sni::Tray::icon_name` sends over SNI. Those are meant for a host's
/// own system icon theme to resolve, which XEmbed has no host for - trayplay
/// resolves them itself, in-process, so it can safely use its own bundled
/// `trayplay-*-symbolic` icons (`icons::install`) instead, which are
/// guaranteed present regardless of how complete the system theme is. That
/// matters here: this environment's icon theme does not carry the legacy
/// non-symbolic names SNI relies on (`media-playback-start` etc. resolve to
/// GTK's broken-image fallback), and bundled icons sidestep that entirely.
fn icon_name_for_state(state: State) -> &'static str {
    match state {
        State::Playing => "trayplay-play-symbolic",
        State::Paused => "trayplay-pause-symbolic",
        State::Stopped => "trayplay-music-symbolic",
    }
}

/// Rendered at a higher resolution than any tray is likely to actually show
/// the icon at - `tray`'s own X11 backend rescales to whatever size the host
/// grants (`linux.rs::draw_icon` scales on every `ConfigureNotify`), so this
/// only sets the ceiling on quality, not the on-screen size.
const ICON_RENDER_SIZE: i32 = 48;

/// Kept alive for the process lifetime; dropping it destroys the X11 window
/// (`tray::TrayIconImpl`'s own `Drop` tears down its event thread and the
/// window together).
pub struct Handle {
    _icon: TrayIcon,
}

/// Builds the icon and wires clicks. `rt` is only used to run the
/// state/tooltip updater on the tokio runtime, same as the SNI backend's
/// `spawn_tray_updater` in `main.rs` - this backend never touches the GTK
/// thread at all, since neither the icon window nor its event loop are GTK
/// objects.
pub fn spawn(
    display: &gdk::Display,
    rt: &tokio::runtime::Handle,
    ui: async_channel::Sender<UiRequest>,
    player: Option<PlayerHandle>,
) -> Result<Handle> {
    // Rendered up front, not inside the updater task: `gdk::Display` is a GTK
    // object (`!Send`), so it cannot be carried into a future that a
    // multi-threaded tokio runtime is free to run on any worker thread. Only
    // three states ever exist, so pre-rendering all of them costs nothing.
    let icons = IconSet {
        playing: render_icon(display, icon_name_for_state(State::Playing))?,
        paused: render_icon(display, icon_name_for_state(State::Paused))?,
        stopped: render_icon(display, icon_name_for_state(State::Stopped))?,
    };

    let icon = TrayIconBuilder::new()
        .with_icon(icons.get(State::Stopped).clone())
        .with_tooltip("trayplay")
        .build()?;

    wire_input(icon.id().clone(), ui, player.clone());

    if let Some(player) = player {
        spawn_updater(rt, icon.clone(), icons, player);
    }

    Ok(Handle { _icon: icon })
}

struct IconSet {
    playing: Icon,
    paused: Icon,
    stopped: Icon,
}

impl IconSet {
    fn get(&self, state: State) -> &Icon {
        match state {
            State::Playing => &self.playing,
            State::Paused => &self.paused,
            State::Stopped => &self.stopped,
        }
    }
}

/// Left click toggles the popup, middle click play/pauses, scroll steps
/// next/previous - the same actions `sni::Tray` gives under SNI. Right click
/// toggles too: there is no menu to put on it here (see module docs), so it
/// behaves as a second left click rather than doing nothing.
fn wire_input(id: TrayIconId, ui: async_channel::Sender<UiRequest>, player: Option<PlayerHandle>) {
    std::thread::spawn(move || {
        let receiver = TrayIconEvent::receiver();
        while let Ok(event) = receiver.recv() {
            // The channel is global across every `TrayIcon` in the process;
            // there is only ever one here, but this keeps that assumption
            // from silently breaking if that ever changes.
            if event.id() != &id {
                continue;
            }
            match event {
                // Click fires on both press and release (`button_state`) -
                // only acting on release matches how every other click in
                // this app (and SNI's own `activate`) behaves.
                TrayIconEvent::Click {
                    button,
                    button_state: MouseButtonState::Up,
                    ..
                } => match button {
                    MouseButton::Left => {
                        if let Err(err) = ui.send_blocking(UiRequest::TogglePopup) {
                            tracing::warn!(%err, "UI channel closed, dropping tray request");
                        }
                    }
                    MouseButton::Middle => match &player {
                        Some(player) => player.send(Command::PlayPause),
                        None => tracing::warn!("no player available, run `trayplay login`"),
                    },
                    // No menu to open here (see module docs), so the button is
                    // a second left click rather than nothing: `TogglePopup`,
                    // so a right click both raises and closes and the two
                    // buttons never disagree about what the popup should do.
                    MouseButton::Right => {
                        if let Err(err) = ui.send_blocking(UiRequest::TogglePopup) {
                            tracing::warn!(%err, "UI channel closed, dropping tray request");
                        }
                    }
                },
                // Vendored patch (vendor/tray/PATCH.md): upstream has no
                // Scroll event on X11 at all, this is trayplay's own
                // addition. Same sign convention as `sni::Tray::scroll`.
                TrayIconEvent::Scroll { delta, .. } => match &player {
                    Some(player) => {
                        if delta < 0 {
                            player.send(Command::Previous);
                        } else if delta > 0 {
                            player.send(Command::Next);
                        }
                    }
                    None => tracing::warn!("no player available, run `trayplay login`"),
                },
                _ => {}
            }
        }
    });
}

/// Mirrors player state onto the icon and tooltip - the XEmbed equivalent of
/// `main.rs`'s `spawn_tray_updater`, just driven from this module since it
/// owns the `TrayIcon` handle.
fn spawn_updater(rt: &tokio::runtime::Handle, icon: TrayIcon, icons: IconSet, player: PlayerHandle) {
    let mut events = player.subscribe();
    rt.spawn(async move {
        // A restored queue's TrackChanged is emitted before this backend exists
        // (the tray is built from `activate`, the restore is sent before it), so
        // the tooltip is seeded by asking instead of waiting for an event. The
        // player answers commands in order, so this cannot see the queue as it
        // was before the restore.
        if let Some(snapshot) = player.snapshot().await {
            if let Some(item) = snapshot.items.get(snapshot.cursor) {
                let label = format!("{} - {}", item.display_artist(), item.name);
                if let Err(err) = icon.set_tooltip(Some(&label)) {
                    tracing::warn!(%err, "seeding XEmbed tray tooltip failed");
                }
            }
        }

        loop {
            match events.recv().await {
                Ok(PlayerEvent::TrackChanged(item)) => {
                    let label = item
                        .map(|i| format!("{} - {}", i.display_artist(), i.name))
                        .unwrap_or_else(|| "Nothing playing".into());
                    if let Err(err) = icon.set_tooltip(Some(&label)) {
                        tracing::warn!(%err, "updating XEmbed tray tooltip failed");
                    }
                }
                Ok(PlayerEvent::StateChanged(state)) => {
                    if let Err(err) = icon.set_icon(Some(icons.get(state).clone())) {
                        tracing::warn!(%err, "updating XEmbed tray icon failed");
                    }
                }
                // Position fires four times a second; nothing here reads it.
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(skipped = n, "XEmbed tray updater fell behind");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Resolves a standard freedesktop icon name to RGBA pixels through GTK's own
/// icon theme, the same names `sni::Tray::icon_name` sends to an SNI host to
/// resolve itself - the difference is trayplay is the one resolving them
/// here, not a host.
fn render_icon(display: &gdk::Display, name: &str) -> Result<Icon> {
    let theme = gtk::IconTheme::for_display(display);
    let paintable = theme.lookup_icon(
        name,
        &[],
        ICON_RENDER_SIZE,
        1,
        gtk::TextDirection::None,
        gtk::IconLookupFlags::empty(),
    );

    // Not `Paintable::snapshot()`: for a symbolic `IconPaintable`, that
    // ignores the icon's actual mask/shape entirely and just paints a solid
    // swatch over the whole bounding box (confirmed by dumping the rendered
    // alpha channel - every pixel came back fully opaque). Symbolic
    // recolouring is a *different* interface (`SymbolicPaintable`) with its
    // own snapshot method that takes the colour(s) explicitly, because there
    // is no widget/CSS context here to resolve "theme foreground" from.
    // White, since the XEmbed window this feeds is always on a solid black
    // background (see the module docs) - there is no theme to match anyway.
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot_symbolic(
        &snapshot,
        ICON_RENDER_SIZE as f64,
        ICON_RENDER_SIZE as f64,
        &[gdk::RGBA::WHITE; 4],
    );
    let node = snapshot
        .to_node()
        .context("icon theme produced no drawable content")?;

    let mut surface = gtk::cairo::ImageSurface::create(
        gtk::cairo::Format::ARgb32,
        ICON_RENDER_SIZE,
        ICON_RENDER_SIZE,
    )
    .context("creating offscreen surface for icon rendering")?;
    {
        // Scoped so the context's reference to `surface` is gone before
        // `surface.data()` needs exclusive access below.
        let cr = gtk::cairo::Context::new(&surface).context("creating cairo context")?;
        node.draw(&cr);
    }
    surface.flush();

    let width = surface.width() as u32;
    let height = surface.height() as u32;
    let stride = surface.stride() as usize;
    let data = surface.data().context("reading rendered icon pixels")?;

    // Cairo's ARGB32 is host-endian premultiplied alpha, byte order BGRA on
    // the little-endian targets this project builds for - `tray::Icon`
    // (X11 `PutImage` underneath) wants straight, RGBA-ordered bytes instead.
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for row in 0..height as usize {
        let row_start = row * stride;
        for col in 0..width as usize {
            let px = row_start + col * 4;
            let (b, g, r, a) = (
                data[px] as u32,
                data[px + 1] as u32,
                data[px + 2] as u32,
                data[px + 3] as u32,
            );
            let unpremultiply = |c: u32| (c * 255).checked_div(a).unwrap_or(0).min(255) as u8;
            rgba.push(unpremultiply(r));
            rgba.push(unpremultiply(g));
            rgba.push(unpremultiply(b));
            rgba.push(a as u8);
        }
    }

    Icon::from_rgba(rgba, width, height).context("building tray icon from rendered pixels")
}
