//! X11-only window property tweaks.
//!
//! GTK4 removed `gtk_window_set_type_hint` and offers nothing in its place, so a
//! window that wants to be anything other than a plain toplevel has to set
//! `_NET_WM_WINDOW_TYPE` itself. That matters because window managers and
//! compositors key their rules off it - picom's `window_type = 'normal'` and
//! AwesomeWM's `type` rules both read this property - so it is the only way to
//! opt a GTK4 window out of a rule written for ordinary windows.

use anyhow::{anyhow, Context, Result};
use gtk::prelude::*;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
use x11rb::wrapper::ConnectionExt as _;

/// Sets `_NET_WM_WINDOW_TYPE` to `_NET_WM_WINDOW_TYPE_<KIND>` on a window's X11
/// surface.
///
/// Must run while the window is realized but **not yet mapped** - a window
/// manager reads this when it takes the window over, so setting it later leaves
/// the WM's own decision made on the old value. `connect_realize` is that point.
///
/// Own x11rb connection rather than GDK's: gdk4-x11 exposes the XID but no
/// property calls, and x11rb is already in the tree for the XEmbed tray.
pub fn set_window_type(window: &impl IsA<gtk::Window>, kind: &str) -> Result<()> {
    let surface = window
        .as_ref()
        .surface()
        .context("window is not realized yet, so it has no surface")?;
    let surface = surface
        .downcast::<gdk4_x11::X11Surface>()
        .map_err(|_| anyhow!("not an X11 surface"))?;
    let xid: u32 = surface
        .xid()
        .try_into()
        .context("X11 window id does not fit in 32 bits")?;

    let (conn, _screen) = x11rb::rust_connection::RustConnection::connect(None)
        .context("connecting to X11")?;

    let property = conn.intern_atom(false, b"_NET_WM_WINDOW_TYPE")?.reply()?.atom;
    // EWMH spells these `_NET_WM_WINDOW_TYPE_UTILITY` and so on, so the config
    // takes the short name and this builds the atom.
    let name = format!("_NET_WM_WINDOW_TYPE_{}", kind.to_uppercase());
    let value = conn.intern_atom(false, name.as_bytes())?.reply()?.atom;

    conn.change_property32(PropMode::REPLACE, xid, property, AtomEnum::ATOM, &[value])
        .with_context(|| format!("setting {name}"))?;
    conn.flush().context("flushing the window type change")?;

    tracing::info!(xid, window_type = %name, "set X11 window type");
    Ok(())
}
