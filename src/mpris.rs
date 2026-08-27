use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use mpris_server::{
    LoopStatus, Metadata, PlaybackStatus, Player as MprisPlayer, Time, TrackId,
};
use tokio::sync::broadcast::error::RecvError;

use crate::config::Repeat;
use crate::jellyfin::models::Item;
use crate::jellyfin::Client;
use crate::player::{Command, Event, PlayerHandle, State};
use crate::tray::UiRequest;

/// `Repeat::All` is Playlist rather than Track, and `One` the other way round -
/// the names line up once you read MPRIS's "playlist" as "the queue".
fn to_loop_status(repeat: Repeat) -> LoopStatus {
    match repeat {
        Repeat::Off => LoopStatus::None,
        Repeat::All => LoopStatus::Playlist,
        Repeat::One => LoopStatus::Track,
    }
}

fn from_loop_status(status: LoopStatus) -> Repeat {
    match status {
        LoopStatus::None => Repeat::Off,
        LoopStatus::Playlist => Repeat::All,
        LoopStatus::Track => Repeat::One,
    }
}

/// Track ids must be valid D-Bus object paths. Jellyfin item ids are hex, so
/// they need no escaping.
const TRACK_ID_PREFIX: &str = "/dev/trayplay/track/";

/// Cover art size handed to MPRIS clients.
const ART_HEIGHT: u32 = 512;

/// Starts the MPRIS server on its own thread.
///
/// mpris_server's `Player` is built on RefCell and is neither Send nor Sync, so
/// it cannot live on the shared runtime. A current-thread runtime with a
/// LocalSet keeps it pinned here; everything crossing the boundary (commands,
/// UI requests) already uses Send channels.
pub fn spawn(player: PlayerHandle, ui: async_channel::Sender<UiRequest>, client: Arc<Client>) {
    let spawned = std::thread::Builder::new()
        .name("trayplay-mpris".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::error!(%err, "cannot build MPRIS runtime");
                    return;
                }
            };

            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                if let Err(err) = run(player, ui, client).await {
                    tracing::error!(%err, "MPRIS server stopped");
                }
            });
        });

    if let Err(err) = spawned {
        tracing::error!(%err, "cannot spawn MPRIS thread");
    }
}

async fn run(
    player: PlayerHandle,
    ui: async_channel::Sender<UiRequest>,
    client: Arc<Client>,
) -> Result<()> {
    let mpris = MprisPlayer::builder("trayplay")
        .identity("trayplay")
        .desktop_entry("trayplay")
        .can_quit(true)
        .can_raise(true)
        .can_play(true)
        .can_pause(true)
        .can_go_next(true)
        .can_go_previous(true)
        .can_seek(true)
        .can_control(true)
        .build()
        .await
        .map_err(|e| anyhow!("registering MPRIS player: {e}"))?;

    // Needed to honour the spec for SetPosition, which must be ignored when the
    // supplied track id is not the one playing.
    let current: Rc<RefCell<Option<TrackId>>> = Rc::new(RefCell::new(None));

    {
        let player = player.clone();
        mpris.connect_play_pause(move |_| player.send(Command::PlayPause));
    }
    {
        let player = player.clone();
        mpris.connect_play(move |_| player.send(Command::Play));
    }
    {
        let player = player.clone();
        mpris.connect_pause(move |_| player.send(Command::Pause));
    }
    {
        let player = player.clone();
        mpris.connect_stop(move |_| player.send(Command::Stop));
    }
    {
        let player = player.clone();
        mpris.connect_next(move |_| player.send(Command::Next));
    }
    {
        let player = player.clone();
        mpris.connect_previous(move |_| player.send(Command::Previous));
    }
    {
        // Seek is relative to the current position, unlike SetPosition.
        let player = player.clone();
        mpris.connect_seek(move |mpris, offset| {
            let target = mpris.position() + offset;
            player.send(Command::Seek(to_duration(target)));
        });
    }
    {
        let player = player.clone();
        let current = current.clone();
        mpris.connect_set_position(move |_, track_id, position| {
            if current.borrow().as_ref() != Some(track_id) {
                tracing::debug!("ignoring SetPosition for a track that is not playing");
                return;
            }
            player.send(Command::Seek(to_duration(position)));
        });
    }
    {
        // The one optional property trayplay exposes: repeat exists internally,
        // so refusing to let a client read or set it would be arbitrary. Shuffle
        // still does not - random play is a way of *building* a queue here, not a
        // switch on an existing one.
        let player = player.clone();
        mpris.connect_set_loop_status(move |_, status| {
            player.send(Command::SetRepeat(from_loop_status(status)));
        });
    }
    {
        let ui = ui.clone();
        mpris.connect_raise(move |_| {
            if let Err(err) = ui.send_blocking(UiRequest::ShowPopup) {
                tracing::warn!(%err, "cannot raise window");
            }
        });
    }
    {
        let player = player.clone();
        let ui = ui.clone();
        mpris.connect_quit(move |_| {
            player.send(Command::Shutdown);
            if let Err(err) = ui.send_blocking(UiRequest::Quit) {
                tracing::warn!(%err, "cannot quit");
            }
        });
    }

    // Must be running before any property change is emitted.
    tokio::task::spawn_local(mpris.run());
    tracing::info!("MPRIS server registered as org.mpris.MediaPlayer2.trayplay");

    let mut events = player.subscribe();

    // A queue restored from the previous session announces itself before this
    // thread has finished registering on the bus, so its TrackChanged is gone by
    // the time the subscription above exists. Asking for a snapshot instead
    // cannot miss it: the player answers commands in order, and the restore was
    // queued before this. Playback status stays Stopped - nothing is playing.
    if let Some(snapshot) = player.snapshot().await {
        // Loaded from the state file before this thread existed, so there is no
        // RepeatChanged to wait for either.
        if let Err(err) = mpris.set_loop_status(to_loop_status(snapshot.repeat)).await {
            tracing::warn!(%err, "cannot publish the restored loop status");
        }
        if let Some(item) = snapshot.items.get(snapshot.cursor) {
            *current.borrow_mut() = track_id(&item.id);
            if let Err(err) = mpris.set_metadata(metadata_for(item, &client)).await {
                tracing::warn!(%err, "cannot publish restored metadata");
            }
        }
    }

    loop {
        match events.recv().await {
            Ok(Event::TrackChanged(item)) => {
                let metadata = match &item {
                    Some(item) => metadata_for(item, &client),
                    None => Metadata::new(),
                };
                *current.borrow_mut() = item.as_ref().and_then(|i| track_id(&i.id));
                if let Err(err) = mpris.set_metadata(metadata).await {
                    tracing::warn!(%err, "cannot publish metadata");
                }
            }
            Ok(Event::StateChanged(state)) => {
                let status = match state {
                    State::Playing => PlaybackStatus::Playing,
                    State::Paused => PlaybackStatus::Paused,
                    State::Stopped => PlaybackStatus::Stopped,
                };
                if let Err(err) = mpris.set_playback_status(status).await {
                    tracing::warn!(%err, "cannot publish playback status");
                }
            }
            // Position is a plain property with no change signal in MPRIS, so
            // this only refreshes the value clients read on demand.
            Ok(Event::Position(pos)) => mpris.set_position(to_time(pos)),
            Ok(Event::Seeked(pos)) => {
                mpris.set_position(to_time(pos));
                if let Err(err) = mpris.seeked(to_time(pos)).await {
                    tracing::warn!(%err, "cannot emit Seeked");
                }
            }
            // MPRIS exposes no queue, so nothing here reflects one changing.
            Ok(Event::QueueChanged) => {}
            Ok(Event::RepeatChanged(repeat)) => {
                if let Err(err) = mpris.set_loop_status(to_loop_status(repeat)).await {
                    tracing::warn!(%err, "cannot publish loop status");
                }
            }
            Ok(Event::Failed(_)) => {}
            // MPRIS has no notion of buffering: a client sees the old track
            // until the new one actually starts, which is when `TrackChanged`
            // republishes the metadata.
            Ok(Event::Loading(_)) | Ok(Event::Buffering { .. }) => {}
            Err(RecvError::Lagged(n)) => tracing::debug!(skipped = n, "MPRIS fell behind"),
            Err(RecvError::Closed) => break,
        }
    }

    Ok(())
}

fn metadata_for(item: &Item, client: &Client) -> Metadata {
    let mut md = Metadata::new();

    md.set_trackid(track_id(&item.id));
    md.set_title(Some(item.name.clone()));
    md.set_length(item.duration().map(to_time));
    md.set_album(item.album.clone());
    md.set_album_artist(item.album_artist.clone().map(|a| vec![a]));
    md.set_track_number(item.index_number);
    md.set_disc_number(item.parent_index_number);

    // Artists can be empty on sparsely tagged libraries; fall back to the same
    // value the UI shows so clients never display a blank artist.
    if item.artists.is_empty() {
        md.set_artist(Some(vec![item.display_artist().to_string()]));
    } else {
        md.set_artist(Some(item.artists.clone()));
    }

    // Jellyfin serves images unauthenticated, so the bare URL is usable by any
    // MPRIS client that wants to fetch the cover itself.
    if let Some((id, tag)) = item.cover_source() {
        md.set_art_url(Some(client.image_url(id, tag, ART_HEIGHT)));
    }

    md
}

fn track_id(item_id: &str) -> Option<TrackId> {
    match TrackId::try_from(format!("{TRACK_ID_PREFIX}{item_id}")) {
        Ok(id) => Some(id),
        Err(err) => {
            tracing::debug!(%err, item_id, "item id is not a valid object path");
            None
        }
    }
}

fn to_time(d: Duration) -> Time {
    Time::from_micros(d.as_micros() as i64)
}

/// Negative times are clamped: a client may seek backwards past the start.
fn to_duration(t: Time) -> Duration {
    Duration::from_micros(t.as_micros().max(0) as u64)
}
