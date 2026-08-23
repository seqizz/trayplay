use ksni::menu::{MenuItem, StandardItem};
use ksni::{OfflineReason, Orientation};

use crate::player::{Command, PlayerHandle, State};

use super::{icon_name_for_state, UiRequest};

pub struct Tray {
    ui: async_channel::Sender<UiRequest>,
    /// None until credentials exist or if audio output could not be opened.
    player: Option<PlayerHandle>,
    pub state: State,
    pub now_playing: Option<String>,
}

impl Tray {
    pub fn new(ui: async_channel::Sender<UiRequest>, player: Option<PlayerHandle>) -> Self {
        Self {
            ui,
            player,
            state: State::Stopped,
            now_playing: None,
        }
    }

    fn send(&self, req: UiRequest) {
        if let Err(err) = self.ui.send_blocking(req) {
            tracing::warn!(%err, "UI channel closed, dropping tray request");
        }
    }

    fn command(&self, cmd: Command) {
        match &self.player {
            Some(player) => player.send(cmd),
            None => tracing::warn!("no player available, run `trayplay login`"),
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "trayplay".into()
    }

    fn title(&self) -> String {
        "trayplay".into()
    }

    /// Standard freedesktop icon names are used on purpose: the SNI *host*
    /// resolves the icon, not us, so a private icon theme would only show up
    /// after installation. These names exist in every icon theme.
    fn icon_name(&self) -> String {
        icon_name_for_state(self.state).into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "trayplay".into(),
            description: self
                .now_playing
                .clone()
                .unwrap_or_else(|| "Nothing playing".into()),
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(UiRequest::TogglePopup);
    }

    /// Middle click toggles playback.
    ///
    /// Right click is deliberately left alone: in StatusNotifierItem that is
    /// ContextMenu, and rebinding it would leave the menu unreachable.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.command(Command::PlayPause);
    }

    /// Vertical scroll steps through the queue.
    ///
    /// The sign of `delta` is not fixed by the spec and depends on the host, so
    /// this follows the GTK convention of negative meaning "up". Swap the arms
    /// if your tray scrolls the other way.
    fn scroll(&mut self, delta: i32, orientation: Orientation) {
        if orientation != Orientation::Vertical || delta == 0 {
            return;
        }
        if delta < 0 {
            self.command(Command::Next);
        } else {
            self.command(Command::Previous);
        }
    }

    /// Returning true keeps the service running while no StatusNotifierWatcher
    /// is on the bus. This matters on X11, where trayplay may well start before
    /// snixembed does; ksni re-registers once the watcher shows up.
    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        tracing::warn!(
            ?reason,
            "no StatusNotifierWatcher on the bus - no tray icon until one appears"
        );
        true
    }

    fn watcher_online(&self) {
        tracing::info!("StatusNotifierWatcher online, tray item registered");
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let playing = self.state == State::Playing;
        vec![
            StandardItem {
                label: if playing { "Pause".into() } else { "Play".into() },
                icon_name: if playing {
                    "media-playback-pause".into()
                } else {
                    "media-playback-start".into()
                },
                activate: Box::new(|t: &mut Self| t.command(Command::PlayPause)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Next".into(),
                icon_name: "media-skip-forward".into(),
                activate: Box::new(|t: &mut Self| t.command(Command::Next)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Previous".into(),
                icon_name: "media-skip-backward".into(),
                activate: Box::new(|t: &mut Self| t.command(Command::Previous)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Random play".into(),
                icon_name: "media-playlist-shuffle".into(),
                activate: Box::new(|t: &mut Self| t.command(Command::PlayRandom)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Show/Hide".into(),
                icon_name: "view-reveal-symbolic".into(),
                activate: Box::new(|t: &mut Self| t.send(UiRequest::TogglePopup)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    t.command(Command::Shutdown);
                    t.send(UiRequest::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}
