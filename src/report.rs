//! Playback reporting to Jellyfin.
//!
//! Without this the server has no idea trayplay is playing anything: no play
//! counts, no "last played" dates, nothing in the dashboard's Now Playing, and
//! (since Jellyfin derives them from exactly that data) permanently empty
//! "Most played" and "Recently played" sections on the Library page.
//!
//! A tokio task subscribing to the player's broadcast, in the same shape as
//! `main::spawn_tray_updater`, rather than anything inside the actor: the actor
//! owns playback and must not be made to wait on an HTTP round trip for
//! telemetry. It has no commands to send either, so it never needs the handle
//! for anything but `subscribe`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::jellyfin::models::Item;
use crate::jellyfin::Client;
use crate::player::{Event, PlayerHandle, State};

/// How often a progress report goes out while a track plays.
///
/// The player's own tick is 250ms, which is the right rate for a seek bar and
/// absurd for the network. Ten seconds keeps the server's idea of the position
/// close enough for a dashboard that nobody watches second by second.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

pub fn spawn(rt: &tokio::runtime::Handle, player: &PlayerHandle, client: Arc<Client>) {
    let mut events = player.subscribe();
    rt.spawn(async move {
        let mut reporter = Reporter {
            client,
            playing: None,
            track: None,
            state: State::Stopped,
            position: Duration::ZERO,
            last_progress: None,
        };

        loop {
            match events.recv().await {
                Ok(event) => reporter.handle(event).await,
                // Only intermediate positions are lost, and the next one is
                // 250ms away. See `handle` for why this can happen at all.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(skipped = n, "playback reporter fell behind");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }

        // The channel closes when the player actor stops, which is on the way
        // out of the process. Best effort: `main` drops the runtime shortly
        // afterwards and may well cut this request off mid-flight.
        reporter.stop().await;
        tracing::debug!("playback reporter stopped");
    });
}

struct Reporter {
    client: Arc<Client>,
    /// Id of the track the server has been told about, and therefore the one a
    /// progress or stop report has to name. `None` means the server currently
    /// thinks nothing is playing.
    playing: Option<String>,
    /// The current track as far as the player is concerned, which is not the
    /// same thing: a restored session and an enqueue into an empty queue both
    /// emit `TrackChanged` with nothing playing, and reporting a start for
    /// those would put a track in the dashboard that is not being heard.
    track: Option<Item>,
    state: State,
    position: Duration,
    /// When the last progress report went out, for the throttle. Cleared with
    /// `playing`, so the first report of a track is never throttled.
    last_progress: Option<Instant>,
}

impl Reporter {
    /// Reports are awaited in order rather than spawned per event: a start has
    /// to reach the server before the progress reports that follow it, and a
    /// stop after them, or the server's idea of the session ends up describing
    /// the wrong track. The cost is that an unreachable server stalls this task
    /// for the request timeout and it falls behind the broadcast, which is
    /// exactly what the `Lagged` arm in `spawn` is for - playback itself is on
    /// another task and is unaffected either way.
    async fn handle(&mut self, event: Event) {
        match event {
            Event::TrackChanged(item) => {
                // Before the position is reset: this stop belongs to the track
                // that just ended, and how far it got is what decides whether
                // Jellyfin marks it played.
                self.stop().await;
                self.position = Duration::ZERO;
                self.track = item;
                if self.state == State::Playing {
                    self.start().await;
                }
            }
            Event::StateChanged(state) => {
                self.state = state;
                match state {
                    // A track can start playing without a `TrackChanged` of its
                    // own - Play on a restored session - so this is the other
                    // way a session begins.
                    State::Playing if self.playing.is_none() => self.start().await,
                    // Pause and resume are worth an immediate report rather
                    // than waiting out the throttle: `IsPaused` is the whole
                    // point of the dashboard entry.
                    State::Playing | State::Paused => self.progress().await,
                    State::Stopped => self.stop().await,
                }
            }
            Event::Position(pos) => {
                self.position = pos;
                if self
                    .last_progress
                    .is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL)
                {
                    self.progress().await;
                }
            }
            // A seek moves the position by more than the throttle would ever
            // notice, so it is reported as it happens.
            Event::Seeked(pos) => {
                self.position = pos;
                self.progress().await;
            }
            _ => {}
        }
    }

    async fn start(&mut self) {
        let Some(item) = self.track.clone() else { return };
        // Recorded even if the request below fails: Jellyfin treats a progress
        // report for an unknown session as opening one, so the reports that
        // follow can still recover a session whose start was lost to a blip.
        self.playing = Some(item.id.clone());
        self.last_progress = Some(Instant::now());

        tracing::debug!(id = %item.id, name = %item.name, "reporting playback start");
        if let Err(err) = self
            .client
            .report_playback_start(&item.id, self.position)
            .await
        {
            tracing::debug!(%err, "cannot report playback start");
        }
    }

    async fn progress(&mut self) {
        let Some(id) = self.playing.clone() else { return };
        self.last_progress = Some(Instant::now());

        let paused = self.state == State::Paused;
        if let Err(err) = self
            .client
            .report_playback_progress(&id, self.position, paused)
            .await
        {
            tracing::debug!(%err, "cannot report playback progress");
        }
    }

    async fn stop(&mut self) {
        let Some(id) = self.playing.take() else { return };
        self.last_progress = None;

        tracing::debug!(id = %id, position = ?self.position, "reporting playback stop");
        if let Err(err) = self.client.report_playback_stopped(&id, self.position).await {
            tracing::debug!(%err, "cannot report playback stop");
        }
    }
}
