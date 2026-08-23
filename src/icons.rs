//! Bundled icons.
//!
//! Icons that no icon theme provides come from ionicons (MIT, see
//! `data/icons/ionicons/`). They are compiled into a GResource bundle by
//! build.rs and mounted on the display's icon theme, which is the only way to
//! get GTK's symbolic recolouring: the artwork is black on transparent and GTK
//! uses it as a mask, keyed off the `-symbolic` suffix in the name.

use anyhow::{Context, Result};
use gtk::gdk;
use gtk::gio;
use gtk::glib;

/// Must match the prefix in data/icons/trayplay.gresource.xml.
const RESOURCE_PATH: &str = "/dev/trayplay/Trayplay/icons";

const BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/trayplay.gresource"));

pub fn install(display: &gdk::Display) -> Result<()> {
    let resource = gio::Resource::from_data(&glib::Bytes::from_static(BUNDLE))
        .context("loading the icon bundle")?;
    gio::resources_register(&resource);
    gtk::IconTheme::for_display(display).add_resource_path(RESOURCE_PATH);
    Ok(())
}
