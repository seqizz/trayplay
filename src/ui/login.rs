//! In-app sign-in form, shown when trayplay starts with no usable session.
//!
//! Before the audio/player milestone this was a static "run `trayplay login`
//! in a terminal, then restart" label; the form replaces it so a fresh install
//! never needs the CLI. The server URL is probed (unauthenticated) before the
//! password is sent, so a typo'd host fails with a clear message instead of a
//! raw transport error, and the probe's reply is what the signed-in screen
//! shows for the server name and version.

use std::rc::Rc;

use adw::prelude::*;

use crate::config::Config;
use crate::jellyfin::auth::Credentials;
use crate::jellyfin::models::ServerInfo;
use crate::jellyfin::{self, Client, FileStore, TokenStore};

use super::on_runtime;

/// Invoked on the GTK thread once a login lands, to hot-start the session.
/// An `Err` keeps the form up with the reason inline (e.g. no audio device)
/// instead of installing a half-built session; `Ok` means the popup replaced
/// this page with the signed-in view.
pub type OnSignedIn = Rc<dyn Fn() -> Result<(), String>>;

/// The whole signed-out view: the form, and the success screen swapped in
/// after a login lands (phase 3 hot-starts instead, via `on_signed_in`).
/// `notice` is a one-line reason to show above the form, used when the session
/// was *lost* (server rejected the token) rather than simply missing.
pub fn page(
    cfg: &Config,
    rt: &tokio::runtime::Handle,
    notice: Option<&str>,
    on_signed_in: Option<OnSignedIn>,
) -> gtk::Widget {
    // Prefill from config.toml first, then from a stored login: a previous
    // `trayplay login` knows the server its token came from.
    let stored = FileStore::new()
        .ok()
        .and_then(|store| store.load().ok())
        .flatten();
    let default_server = cfg
        .server
        .clone()
        .or_else(|| stored.as_ref().map(|creds| creds.server.clone()));
    let default_username = cfg
        .username
        .clone()
        .or_else(|| stored.as_ref().map(|creds| creds.username.clone()));

    let title = gtk::Label::builder()
        .label("Sign in")
        .css_classes(["heading"])
        .build();
    let hint = gtk::Label::builder()
        .label("trayplay is not connected. Enter your Jellyfin server and account.")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label"])
        .build();

    let server = gtk::Entry::new();
    server.set_placeholder_text(Some("https://jellyfin.example.org"));
    if let Some(server_url) = default_server {
        server.set_text(&server_url);
    }

    let username = gtk::Entry::new();
    username.set_placeholder_text(Some("username"));
    if let Some(user) = default_username {
        username.set_text(&user);
    }

    let password = gtk::PasswordEntry::new();
    password.set_placeholder_text(Some("password"));

    // Errors and probe results belong inline next to the form, not in the
    // Toaster: the Toaster wraps the navigation view, which does not exist on
    // this page, and the fix for a failed login is on the form in front of
    // the user anyway.
    let error = gtk::Label::new(None);
    error.set_wrap(true);
    error.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    error.set_justify(gtk::Justification::Center);
    error.set_max_width_chars(36);
    error.set_visible(false);
    error.add_css_class("trayplay-login-error");
    if let Some(notice) = notice {
        set_error(&error, notice);
    }

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);

    let sign_in = gtk::Button::with_label("Sign in");
    sign_in.add_css_class("suggested-action");

    let form = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    form.append(&title);
    form.append(&hint);
    form.append(&server);
    form.append(&username);
    form.append(&password);

    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    actions.append(&spinner);
    actions.append(&sign_in);
    form.append(&actions);
    form.append(&error);

    // Enter in the password field submits, like any login form; in the other
    // two it moves to the next field.
    let next = username.clone();
    server.connect_activate(move |_| {
        next.grab_focus();
    });
    let next = password.clone();
    username.connect_activate(move |_| {
        next.grab_focus();
    });
    let submit = sign_in.clone();
    password.connect_activate(move |_| submit.emit_clicked());

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .margin_start(24)
        .margin_end(24)
        .build();
    // Stable selector root for themes.
    body.set_widget_name("trayplay-login");
    body.add_css_class("trayplay-body");
    body.append(&form);

    // Swapped in after a successful login. Kept hidden rather than built on
    // demand so `succeed` only fills it in; phase 2 replaces this with the
    // hot-started navigation view entirely.
    let success = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    success.set_visible(false);
    body.append(&success);

    let rt = rt.clone();
    sign_in.connect_clicked(move |button| {
        let server = server.text().trim().to_string();
        let username = username.text().trim().to_string();
        let password = password.text().to_string();
        if server.is_empty() || username.is_empty() || password.is_empty() {
            set_error(&error, "Enter the server URL, username and password.");
            return;
        }

        // One flight for probe + login: the button stays disabled, so there is
        // no re-entrancy window.
        button.set_sensitive(false);
        spinner.start();
        spinner.set_visible(true);
        error.set_visible(false);

        // The password only exists in this entry and in the request body; it is
        // never logged and never written to disk (the token store holds the
        // session, not the password).
        let url = jellyfin::normalize_base(&server);
        let url_for_client = url.clone();
        // The result callback consumes what it captures, but a signal handler
        // must be re-callable (`Fn`), so everything it needs gets a fresh clone
        // per invocation instead of the closure captures.
        let error = error.clone();
        let spinner = spinner.clone();
        let form = form.clone();
        let success = success.clone();
        let on_signed_in = on_signed_in.clone();
        // Owned handle for the result callback: it outlives this handler,
        // which cannot hold the borrowed `button` parameter.
        let button = button.clone();
        on_runtime(
            &rt,
            async move {
                // Probe first: a typo'd host costs one cheap unauthenticated
                // round trip, not a doomed login attempt with a confusing
                // transport error at the end.
                let info = jellyfin::probe(&url).await?;
                let mut client = Client::new(&url_for_client)?;
                let creds = client.login(&username, &password).await?;
                // Verify the token actually works before storing it.
                client.validate().await?;
                FileStore::new()?.store(&creds)?;
                Ok::<_, anyhow::Error>((info, creds))
            },
            move |result| {
                spinner.stop();
                spinner.set_visible(false);
                match result {
                    Ok((info, creds)) => {
                        if let Some(on_signed_in) = &on_signed_in {
                            // Hot-start: the popup replaces this page with the
                            // signed-in view. A failure keeps the form up with
                            // the reason inline; nothing is half-installed.
                            if let Err(message) = on_signed_in() {
                                set_error(&error, message);
                                button.set_sensitive(true);
                            }
                        } else {
                            // No hot-start wiring: show what the login learned
                            // and ask for a restart.
                            form.set_visible(false);
                            success.set_visible(true);
                            succeed(&success, &info, &creds);
                        }
                    }
                    Err(err) => {
                        set_error(&error, format!("{err}"));
                        button.set_sensitive(true);
                    }
                }
            },
        );
    });

    body.upcast()
}

fn set_error(label: &gtk::Label, message: impl Into<String>) {
    label.set_label(&message.into());
    label.set_visible(true);
}

/// Fills the success screen with what the probe and login learned.
fn succeed(body: &gtk::Box, info: &ServerInfo, creds: &Credentials) {
    let title = gtk::Label::builder()
        .label("Signed in")
        .css_classes(["heading"])
        .build();
    let version = if info.version.is_empty() {
        String::new()
    } else {
        format!(" ({})", info.version)
    };
    let message = gtk::Label::builder()
        .label(format!(
            "Signed in as {} on {}{version}.\n\nRestart trayplay to start playback.",
            creds.username, info.server_name
        ))
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    body.append(&title);
    body.append(&message);
}