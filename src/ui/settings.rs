//! Settings page.
//!
//! Only things that are a matter of taste and change more than once: the colour
//! scheme follows the time of day, auto-hide depends on how the window manager
//! treats focus, and reduce-motion is a preference about the app being alive on
//! screen. Everything else about trayplay is set once and belongs in config.toml.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;

use crate::config::{self, ColorScheme, Settings};
use crate::player::{cache::Cache, Command};

use super::Session;

/// How much of the cache is in use, for the limit row's subtitle.
///
/// Measured on the GTK thread rather than the runtime: it is one directory
/// listing and a `stat` per entry, and the page is being built anyway.
fn cache_subtitle() -> String {
    let Ok(dir) = config::cache_dir() else {
        return "Downloaded tracks, pruned oldest-first".to_string();
    };
    let megabytes = Cache::size_of(&dir) / (1024 * 1024);
    format!("{megabytes} MB in use; oldest downloads are pruned first")
}

/// Applies a stored choice, or hands control back to the desktop when there is
/// none.
///
/// `ColorScheme::Default` in libadwaita means "follow the system", which is what
/// a fresh install gets. Once the user has flipped the switch the choice is
/// forced, so it survives the desktop changing its mind - a night-light schedule
/// flipping the popup back is exactly the annoyance being avoided here.
pub fn apply(settings: &Settings) {
    let manager = adw::StyleManager::default();
    manager.set_color_scheme(match settings.color_scheme {
        Some(ColorScheme::Dark) => adw::ColorScheme::ForceDark,
        Some(ColorScheme::Light) => adw::ColorScheme::ForceLight,
        None => adw::ColorScheme::Default,
    });
}

/// Marks the window with the scheme in force, and keeps it marked.
///
/// GTK CSS has no equivalent of a prefers-color-scheme query, and the two
/// palettes do not want the same numbers: a scrim alpha that reads as depth over
/// album art in dark reads as a white sheet in light. The class is the only way
/// to say "this value, but only in light mode" in a stylesheet - including a
/// user's.
pub fn track_scheme(window: &adw::ApplicationWindow) {
    let manager = adw::StyleManager::default();
    apply_scheme_class(window, manager.is_dark());

    let window = window.clone();
    manager.connect_dark_notify(move |manager| {
        apply_scheme_class(&window, manager.is_dark());
    });
}

fn apply_scheme_class(window: &adw::ApplicationWindow, dark: bool) {
    let (add, remove) = if dark { ("dark", "light") } else { ("light", "dark") };
    window.remove_css_class(remove);
    window.add_css_class(add);
}

/// `hide_on_blur` is the flag the popup itself reads, and `on_reduce_motion`
/// reaches the now-playing view's backdrop, so both switches take effect without
/// a restart.
pub fn page(
    hide_on_blur: Rc<Cell<bool>>,
    on_reduce_motion: Rc<dyn Fn(bool)>,
    session: &Session,
    config_limit_mb: u64,
) -> adw::NavigationPage {
    let manager = adw::StyleManager::default();
    // Same fallback as the popup's auto-hide: the stored value wins, config.toml
    // answers until the row is touched.
    let limit = Settings::load().cache_max_mb.unwrap_or(config_limit_mb);

    let dark = adw::SwitchRow::builder()
        .title("Dark mode")
        // Nothing is stored until this is touched, so the initial state has to
        // come from what is actually being displayed rather than from Settings.
        .active(manager.is_dark())
        .build();
    dark.set_widget_name("trayplay-dark-switch");

    dark.connect_active_notify(|row| {
        let scheme = if row.is_active() {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        };
        apply(&Settings {
            color_scheme: Some(scheme),
            ..Settings::load()
        });
        // A failed write only costs the choice at the next start, so it is not
        // worth interrupting the user over.
        if let Err(err) = Settings::update(|settings| settings.color_scheme = Some(scheme)) {
            tracing::warn!(%err, "cannot persist the colour scheme");
        }
    });

    // Unlike the dark switch, this one has a stored value from the start: there
    // is nothing on screen to read the current state off, so Settings is the only
    // source for it.
    let motion = adw::SwitchRow::builder()
        .title("Reduce motion")
        .subtitle("Holds the pattern still behind tracks with no cover art")
        .active(Settings::load().reduce_motion)
        .build();
    motion.set_widget_name("trayplay-motion-switch");

    motion.connect_active_notify(move |row| {
        let reduce = row.is_active();
        on_reduce_motion(reduce);
        if let Err(err) = Settings::update(|settings| settings.reduce_motion = reduce) {
            tracing::warn!(%err, "cannot persist the reduce-motion setting");
        }
    });

    let appearance = adw::PreferencesGroup::builder()
        .title("Appearance")
        .description("Follows the desktop until changed here.")
        .build();
    appearance.add(&dark);
    appearance.add(&motion);

    let hide = adw::SwitchRow::builder()
        .title("Auto-hide when unfocused")
        .subtitle("With focus-follows-mouse this can close the popup constantly")
        .active(hide_on_blur.get())
        .build();
    hide.set_widget_name("trayplay-autohide-switch");

    hide.connect_active_notify(move |row| {
        let active = row.is_active();
        // The popup reads this flag on every focus change, so the switch applies
        // at once rather than at the next start.
        hide_on_blur.set(active);
        if let Err(err) = Settings::update(|settings| settings.hide_on_focus_loss = Some(active)) {
            tracing::warn!(%err, "cannot persist the auto-hide setting");
        }
    });

    let behaviour = adw::PreferencesGroup::builder().title("Behaviour").build();
    behaviour.add(&hide);

    // Megabytes, in steps of 100: the point of this control is "roughly how much
    // disk may this take", not a byte-exact figure.
    let cache = adw::SpinRow::with_range(100.0, 20_000.0, 100.0);
    cache.set_title("Cache limit (MB)");
    cache.set_subtitle(&cache_subtitle());
    cache.set_value(limit as f64);
    cache.set_widget_name("trayplay-cache-row");

    let player = session.player.clone();
    cache.connect_value_notify(move |row| {
        let megabytes = row.value().max(0.0) as u64;
        // Straight to the player, which owns the cache and prunes with it, so
        // lowering the limit takes effect now rather than at the next download.
        player.send(Command::SetCacheLimit(megabytes * 1024 * 1024));
        if let Err(err) = Settings::update(|settings| settings.cache_max_mb = Some(megabytes)) {
            tracing::warn!(%err, "cannot persist the cache limit");
        }
        // Pruning happens on the player thread, so the figure below is what was
        // on disk a moment ago rather than the result. Close enough to tell
        // whether the cache is anywhere near its ceiling, which is the question
        // this answers.
        row.set_subtitle(&cache_subtitle());
    });
    behaviour.add(&cache);

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    body.add_css_class("trayplay-body");
    body.set_widget_name("trayplay-settings-page");
    body.append(&appearance);
    body.append(&behaviour);

    // Header bar for the back button only; a tray popup has no use for window
    // controls.
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));

    adw::NavigationPage::new(&toolbar, "Settings")
}
