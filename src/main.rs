mod cli;
mod config;
mod fonts;
mod icons;
mod jellyfin;
mod mpris;
mod player;
mod theme;
mod tray;
mod ui;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use anyhow::{Context, Result};
use clap::Parser;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use ksni::TrayMethods;

use crate::config::Config;
use crate::jellyfin::TokenStore;
use crate::tray::{sni, xembed, TrayBackend, UiRequest};
use crate::ui::Popup;

const APP_ID: &str = "dev.trayplay.Trayplay";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "trayplay=info".into()),
        )
        .init();

    let args = cli::Cli::parse();
    let cfg = Config::load()?;

    // Every subcommand needs async too, so the runtime is built before the split.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("trayplay-rt")
        .build()
        .context("building tokio runtime")?;

    if let Some(cmd) = args.command {
        return cli::run(cmd, &cfg, &rt);
    }

    // Before anything can initialise GTK: fontconfig reads its directories when
    // it builds its config, so a font written after that point is invisible until
    // the next launch. See `fonts` for why this has to go through the filesystem
    // at all.
    if let Err(err) = fonts::install() {
        tracing::warn!(%err, "cannot install bundled fonts");
    }

    // WM_CLASS is derived from prgname; pin it so window manager rules that
    // match on "trayplay" keep working regardless of how the binary is invoked.
    glib::set_prgname(Some("trayplay"));
    glib::set_application_name("trayplay");

    let app = adw::Application::builder().application_id(APP_ID).build();

    // GApplication is unique per application_id by default. Registering here,
    // before any of the heavy player/tray/mpris setup below, lets a second
    // `trayplay` invocation detect the already-running one and get out
    // immediately - `run_with_args` alone would still return quickly for a
    // remote instance, but only *after* all that setup below had already run
    // pointlessly first (a second player, a second tray icon trying to dock,
    // a second MPRIS name registration), which is what left a second process
    // sitting around instead of exiting.
    app.register(gio::Cancellable::NONE)
        .context("registering GApplication")?;
    if app.is_remote() {
        // Hands off to the running instance - its own `activate` handler
        // (`build()`'s early-return branch) does `window.present()`. Nothing
        // else in this process needs to run.
        app.activate();
        return Ok(());
    }

    // The tray and MPRIS both run off the GTK thread, so they cannot touch it
    // directly. Window requests are funnelled through this channel instead.
    let (tx, rx) = async_channel::unbounded::<UiRequest>();

    // Missing credentials or a broken audio device are not fatal: the tray still
    // comes up, and the popup will offer a login form once the UI milestone lands.
    let session = match start_player(&cfg, &rt) {
        Ok(Some((handle, client))) => {
            mpris::spawn(handle.clone(), tx.clone(), client.clone());
            Some(ui::Session {
                player: handle,
                browser: ui::Browser::new(rt.handle().clone(), client),
            })
        }
        Ok(None) => None,
        Err(err) => {
            tracing::error!(%err, "player unavailable");
            None
        }
    };
    let player = session.as_ref().map(|s| s.player.clone());

    // Player events reach GTK over a channel; the broadcast receiver itself
    // cannot be awaited from glib's executor.
    let ui_events = player
        .as_ref()
        .map(|p| ui::bridge_events(rt.handle(), p));

    // Only now: Restore emits TrackChanged, and the broadcast channel drops
    // events sent before a receiver exists, so MPRIS, the tray updater and the
    // UI bridge all have to be subscribed first. Nothing starts playing - the
    // restored track is shown, and Play picks it up.
    if let Some(player) = &player {
        player.send(player::Command::Restore);
    }

    // Which tray backend to use is a display-backend question (XEmbed needs a
    // real X11 surface, SNI doesn't care), and there is no `gdk::Display` this
    // early - GTK hasn't run its own startup yet. Filled in once `build()`
    // knows, and read back after the main loop returns so the right teardown
    // happens (SNI unregisters, XEmbed's window closes) before `rt` drops.
    let tray_backend: Rc<RefCell<Option<TrayBackend>>> = Rc::new(RefCell::new(None));

    let rt_handle = rt.handle().clone();
    let tray_backend_for_build = tray_backend.clone();
    app.connect_activate(move |app| {
        if let Err(err) = build(
            app,
            &cfg,
            &rx,
            &session,
            &ui_events,
            tx.clone(),
            rt_handle.clone(),
            &tray_backend_for_build,
        ) {
            tracing::error!(%err, "startup failed");
            app.quit();
        }
    });

    // No window is visible at startup, so without a hold the app would exit
    // as soon as the main loop finds nothing to do.
    let _hold = app.hold();

    app.run_with_args::<&str>(&[]);

    // Keeping these alive until after the main loop returns: dropping the
    // backend unregisters the tray item / closes the XEmbed window, dropping
    // the runtime kills its worker threads.
    drop(tray_backend.borrow_mut().take());
    drop(rt);
    Ok(())
}

/// Builds the Jellyfin client, track cache and audio sink, and starts the
/// player actor. Returns None when there are no stored credentials.
///
/// The client comes back alongside the handle because MPRIS needs it to build
/// cover art URLs.
#[allow(clippy::type_complexity)]
fn start_player(
    cfg: &Config,
    rt: &tokio::runtime::Runtime,
) -> Result<Option<(player::PlayerHandle, Arc<jellyfin::Client>)>> {
    let Some(creds) = jellyfin::FileStore::new()?.load()? else {
        tracing::warn!("no stored credentials, run `trayplay login`");
        return Ok(None);
    };
    tracing::info!(server = %creds.server, user = %creds.username, "credentials loaded");

    let client = Arc::new(jellyfin::Client::authenticated(creds)?);

    let cache = Arc::new(player::cache::Cache::new(
        config::cache_dir()?,
        // The settings page's value wins; config.toml is the fallback for
        // anyone who set it there before the page existed.
        config::Settings::load()
            .cache_max_mb
            .unwrap_or(cfg.cache_max_mb)
            * 1024
            * 1024,
        client.http(),
    )?);
    if let Err(err) = cache.prune() {
        tracing::warn!(%err, "initial cache prune failed");
    }

    let sink = Box::new(player::rodio_sink::RodioSink::new()?);

    // Player::spawn calls tokio::spawn, so it needs a runtime in context.
    let _guard = rt.enter();
    // Repeat is remembered across restarts, like the queue it applies to.
    let repeat = config::Settings::load().repeat;
    let handle = player::Player::spawn(
        client.clone(),
        cache,
        sink,
        cfg.random_batch,
        cfg.prefetch_next,
        repeat,
    );
    Ok(Some((handle, client)))
}

/// Mirrors player state onto the tray icon and tooltip. Only used for the SNI
/// backend - XEmbed updates itself directly on the GTK thread from the same
/// bridged event stream every other GTK-side listener uses (see
/// `tray::xembed::spawn`), since it never leaves that thread to begin with.
fn spawn_tray_updater(
    rt: &tokio::runtime::Handle,
    tray: ksni::Handle<sni::Tray>,
    player: &player::PlayerHandle,
) {
    let mut events = player.subscribe();
    let player = player.clone();
    rt.spawn(async move {
        // Same reason as in `xembed::spawn_updater`: the restored track's
        // TrackChanged predates this subscription, so the label is seeded by
        // asking the player rather than waiting for the next track change.
        if let Some(snapshot) = player.snapshot().await {
            if let Some(item) = snapshot.items.get(snapshot.cursor) {
                let label = format!("{} - {}", item.display_artist(), item.name);
                tray.update(move |t: &mut sni::Tray| t.now_playing = Some(label))
                    .await;
            }
        }

        loop {
            match events.recv().await {
                Ok(player::Event::TrackChanged(item)) => {
                    let label = item.map(|i| format!("{} - {}", i.display_artist(), i.name));
                    tray.update(move |t: &mut sni::Tray| t.now_playing = label).await;
                }
                Ok(player::Event::StateChanged(state)) => {
                    tray.update(move |t: &mut sni::Tray| t.state = state).await;
                }
                // Position fires four times a second; redrawing the tray for it
                // would be pointless traffic on the bus.
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(skipped = n, "tray updater fell behind");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn build(
    app: &adw::Application,
    cfg: &Config,
    rx: &async_channel::Receiver<UiRequest>,
    session: &Option<ui::Session>,
    ui_events: &Option<async_channel::Receiver<player::Event>>,
    tray_ui: async_channel::Sender<UiRequest>,
    rt: tokio::runtime::Handle,
    tray_backend: &Rc<RefCell<Option<TrayBackend>>>,
) -> Result<()> {
    // `GApplication` is unique per `application_id` by default (no
    // `ApplicationFlags::NON_UNIQUE` is set), so launching a second `trayplay`
    // hands off to the running one over D-Bus instead of starting its own
    // main loop - that second process's `activate` fires *here*, on the
    // already-running instance, which is what detects "already running" for
    // free. `windows()` is non-empty in exactly that case (the window still
    // exists even if hidden - `set_hide_on_close(true)` never destroys it),
    // so this is also where a relaunch has to actually show it, or running
    // the binary again would look like doing nothing.
    if let Some(window) = app.windows().first() {
        window.present();
        return Ok(());
    }

    let display = gdk::Display::default().context("no display")?;
    // Before any widget asks for an icon by name.
    icons::install(&display)?;
    theme::install(&display)?;

    let player = session.as_ref().map(|s| s.player.clone());

    // XEmbed needs a real X11 surface to dock, which is the same reason the
    // popup itself waits for `display` before deciding layer-shell vs
    // `awful.rules` placement - this is the earliest point either backend can
    // be built. Everywhere else in the world (Wayland/somewm, KDE, GNOME)
    // gets SNI, same as always.
    if display.backend().is_wayland() {
        // assume_sni_available means a missing watcher is reported to
        // sni::Tray::watcher_offline rather than failing startup outright.
        let updater_rt = rt.clone();
        let updater_player = player.clone();
        let tray_backend = tray_backend.clone();
        ui::on_runtime(
            &rt,
            async move {
                sni::Tray::new(tray_ui, player)
                    .assume_sni_available(true)
                    .spawn()
                    .await
            },
            move |result| match result {
                Ok(handle) => {
                    if let Some(player) = &updater_player {
                        spawn_tray_updater(&updater_rt, handle.clone(), player);
                    }
                    *tray_backend.borrow_mut() = Some(TrayBackend::Sni(handle));
                }
                Err(err) => tracing::error!(?err, "registering StatusNotifierItem failed"),
            },
        );
    } else {
        // Not SNI: no host to describe the icon/menu to, so this docks a raw
        // X11 window by hand instead (see tray::xembed's module docs for why
        // a gtk::Window can't be used). No right-click menu, and the icon
        // sits on a solid black square - `tray` 0.1.2 limitations, not
        // something fixable from here (see "Tray: two backends, one per
        // display" in CLAUDE.md). Scroll works (next/previous), unlike
        // upstream `tray` - see vendor/tray/PATCH.md.
        tracing::info!("no SNI host on X11, docking a plain XEmbed tray icon instead (no menu, no transparency)");
        match xembed::spawn(&display, &rt, tray_ui, player) {
            Ok(handle) => *tray_backend.borrow_mut() = Some(TrayBackend::XEmbed(handle)),
            Err(err) => tracing::error!(?err, "setting up XEmbed tray icon failed"),
        }
    }
    // StyleManager is only meaningful once adw is initialised, which activate
    // guarantees. Applied before the first window so nothing is built light and
    // then restyled.
    ui::settings::apply(&config::Settings::load());

    let popup = Rc::new(Popup::new(
        app,
        cfg,
        &display,
        session.clone(),
        ui_events.clone(),
    ));

    let app = app.clone();
    let rx = rx.clone();
    glib::spawn_future_local(async move {
        while let Ok(req) = rx.recv().await {
            match req {
                UiRequest::TogglePopup => popup.toggle(),
                UiRequest::ShowPopup => popup.show(),
                UiRequest::HidePopup => popup.hide(),
                UiRequest::Quit => {
                    app.quit();
                    break;
                }
            }
        }
    });

    tracing::info!("trayplay started, waiting on tray");
    Ok(())
}
