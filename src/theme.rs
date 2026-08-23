use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use gtk::gdk;
use gtk::glib;
use gtk::CssProvider;
use notify::{RecursiveMode, Watcher};

use crate::config;

/// Baseline theme is embedded so the binary never depends on a runtime data path.
/// The same file is also installed to share/trayplay/default.css as a reference
/// for theme authors.
const DEFAULT_CSS: &str = include_str!("../data/default.css");

/// Installs the baseline stylesheet plus the user override, and keeps the
/// override live-reloading.
pub fn install(display: &gdk::Display) -> Result<()> {
    let base = CssProvider::new();
    base.load_from_string(DEFAULT_CSS);
    gtk::style_context_add_provider_for_display(
        display,
        &base,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let user_path = config::user_theme_path()?;
    let user = CssProvider::new();
    gtk::style_context_add_provider_for_display(
        display,
        &user,
        gtk::STYLE_PROVIDER_PRIORITY_USER,
    );
    load_user(&user, &user_path);

    spawn_watcher(display.clone(), user, user_path);
    Ok(())
}

fn load_user(provider: &CssProvider, path: &PathBuf) {
    if path.exists() {
        provider.load_from_path(path);
        tracing::info!(path = %path.display(), "loaded user theme");
    } else {
        // Clearing keeps behaviour correct when the file is deleted while running.
        provider.load_from_string("");
    }
}

/// Watches the *config directory* rather than theme.css itself: editors that
/// write via rename (vim, most of them) would otherwise break the inotify watch
/// on first save.
fn spawn_watcher(display: gdk::Display, provider: CssProvider, path: PathBuf) {
    let Some(dir) = path.parent().map(PathBuf::from) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        tracing::warn!(dir = %dir.display(), "cannot create config dir, theme watching disabled");
        return;
    }

    let (tx, rx) = async_channel::unbounded::<()>();

    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if event.paths.iter().any(|p| p.file_name() == Some(std::ffi::OsStr::new("theme.css"))) {
                let _ = tx.send_blocking(());
            }
        }
    });

    let mut watcher = match watcher {
        Ok(w) => w,
        Err(err) => {
            tracing::warn!(%err, "theme watcher unavailable");
            return;
        }
    };

    if let Err(err) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        tracing::warn!(%err, dir = %dir.display(), "cannot watch config dir");
        return;
    }

    glib::spawn_future_local(async move {
        // Watcher must outlive this future or the inotify handle is dropped.
        let _watcher = watcher;
        let _ = &display;
        while rx.recv().await.is_ok() {
            // Editors emit several events per save; coalesce them.
            glib::timeout_future(Duration::from_millis(80)).await;
            while rx.try_recv().is_ok() {}
            load_user(&provider, &path);
        }
    });
}
