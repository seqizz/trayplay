pub mod cache;
pub mod decoder;
pub mod persist;
pub mod queue;
pub mod rodio_sink;
pub mod sink;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::config::{Repeat, Settings};
use crate::jellyfin::models::Item;
use crate::jellyfin::Client;
use cache::Cache;
use queue::{Mode, Queue};
use sink::AudioSink;

/// How often the actor polls the sink for end-of-track and position.
const TICK: Duration = Duration::from_millis(250);

/// Refill a random queue once this few tracks remain.
const REFILL_SLACK: usize = 5;

/// How many missing tracks in a row `start_current` will skip past before giving
/// up and stopping.
///
/// A bound rather than "until something plays": with repeat-all and a queue whose
/// files have all moved, skipping would walk the same circle forever. Sixteen is
/// well past a few stale ids and well short of anything a user would sit through.
const MAX_MISSING_SKIPS: usize = 16;

/// How close to the end of the current track the next one is handed to the sink.
///
/// Late on purpose. rodio has no API for dropping a source already queued behind
/// the current one, so a committed track *will* be heard - and committing as
/// early as the download finished (which is what this used to do) meant almost
/// every "Play next" would land behind an already-committed track instead of
/// where the user asked. The download still starts as early as ever, so what
/// happens here is only a decoder build, and the gapless transition is
/// unaffected.
const PREQUEUE_LEAD: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug)]
pub enum Command {
    /// Start a fresh random queue.
    PlayRandom,
    /// Play an album from its first track. Unused by the UI, which sends
    /// `PlayItems` with a list it already has, but this is the id-only entry
    /// point anything outside the popup would want.
    #[allow(dead_code)]
    PlayAlbum(String),
    /// Play everything by an artist, album by album. Unused for the same reason.
    #[allow(dead_code)]
    PlayArtist(String),
    /// Play an explicit list, starting at an index. Used by the album track list
    /// so picking a track plays the rest of the album after it.
    PlayItems { items: Vec<Item>, start: usize },
    /// Play one track now and shuffle the rest of its scope behind it.
    ///
    /// What a track row in a browse page does: picking a song is a choice about
    /// that song, not a request to hear everything after it in listing order.
    /// The scope is whatever page it came from - an artist's whole catalogue, or
    /// one album.
    PlayShuffled { items: Vec<Item>, first: usize },
    /// Append to the end of the queue, playing nothing now.
    QueueLast { items: Vec<Item> },
    /// Insert directly after the track playing now.
    QueueNext { items: Vec<Item> },
    /// New track-cache ceiling, in bytes. Applied and pruned at once, so the
    /// settings page does not have to wait for the next download to see it.
    SetCacheLimit(u64),
    /// Step the repeat setting on: off → all → one → off. What the button does.
    CycleRepeat,
    /// Set it outright. What MPRIS `LoopStatus` does.
    SetRepeat(Repeat),
    /// Drop one track from the queue.
    ///
    /// Carries the id as well as the index because the caller is working from a
    /// snapshot: if the queue moved on in between, the id no longer matches and
    /// the removal is dropped rather than taking out an innocent neighbour.
    Remove { index: usize, id: String },
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    Stop,
    Seek(Duration),
    /// Load the queue left behind by the previous session, without playing it.
    ///
    /// A command rather than something `spawn` does, because it emits
    /// `TrackChanged` and the broadcast channel drops events sent before anyone
    /// subscribes - the tray, MPRIS and the UI bridge all have to be listening
    /// first. Ignored once anything else has filled the queue.
    Restore,
    /// Read the queue back. The player owns it outright, so the only way to show
    /// it is to ask; a request/response command keeps that ownership intact
    /// rather than sharing the queue behind a lock.
    Snapshot(oneshot::Sender<Snapshot>),
    Shutdown,
}

/// The queue as it stands, for display.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub items: Vec<Item>,
    /// Index of the track playing now. Meaningless when `items` is empty.
    pub cursor: usize,
    /// Carried along so a listener that starts late can seed itself - MPRIS
    /// registers on the bus well after the setting is loaded, and no
    /// `RepeatChanged` will be emitted for a value that never changed.
    pub repeat: Repeat,
}

/// `TrackChanged` carries a whole `Item` and so dwarfs every other variant, which
/// clippy objects to. Left alone deliberately: the broadcast ring is 64 slots, so
/// the "waste" is about 20 KB once, while boxing would add an allocation per track
/// change and force six match sites to dereference - and it would not make the
/// per-subscriber clone any cheaper, since that deep-copies the `Item` either way.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Event {
    TrackChanged(Option<Item>),
    /// The queue's contents changed without the track changing - an enqueue, a
    /// removal. Only the queue page cares; it reacts by asking for a fresh
    /// `Snapshot`, since this deliberately does not carry the queue with it.
    QueueChanged,
    /// Repeat changed, from the button or from an MPRIS client. Both listeners
    /// need it: the button paints the new state, MPRIS republishes LoopStatus.
    RepeatChanged(Repeat),
    StateChanged(State),
    /// A track is being opened: the request has gone out and nothing is playing
    /// yet.
    ///
    /// Emitted *before* the await that opens the stream, which is the whole
    /// point - the actor is single-threaded, so while a track is loading it
    /// cannot even answer a `Snapshot`, and a slow server or a real-time
    /// transcode is otherwise indistinguishable from a Next press that was
    /// dropped. `None` means something is being fetched whose track is not known
    /// yet (a random queue being filled).
    ///
    /// There is no explicit "done": `TrackChanged` and `Failed` are the two ways
    /// a load ends, and both are already emitted.
    Loading(Option<Item>),
    /// Download progress for one cache entry, throttled to a handful of events
    /// per track.
    ///
    /// Sent by the download task rather than by the actor, so it keeps arriving
    /// while the actor is blocked waiting for exactly this. `id` is the item id,
    /// because prefetches report too and only the track being waited for should
    /// be shown. `total` is None when the response carried no Content-Length -
    /// a transcode - in which case there is no fraction to draw.
    Buffering {
        id: String,
        got: u64,
        total: Option<u64>,
    },
    Position(Duration),
    /// A seek was performed. MPRIS clients need this to resync their own
    /// position estimate rather than waiting for the next poll.
    Seeked(Duration),
    /// Recoverable failure worth surfacing in the UI.
    Failed(String),
}

#[derive(Clone)]
pub struct PlayerHandle {
    tx: mpsc::Sender<Command>,
    events: broadcast::Sender<Event>,
}

impl PlayerHandle {
    pub fn send(&self, cmd: Command) {
        // A full queue means the actor is wedged; dropping is better than
        // blocking a UI callback.
        if let Err(err) = self.tx.try_send(cmd) {
            tracing::warn!(%err, "dropping player command");
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// None when the command could not be queued or the actor went away.
    ///
    /// The actor answers this in command order, so a snapshot requested while a
    /// track is still being loaded waits for that to finish - the queue page can
    /// take a moment to appear if the player is busy.
    pub async fn snapshot(&self) -> Option<Snapshot> {
        let (tx, rx) = oneshot::channel();
        if let Err(err) = self.tx.try_send(Command::Snapshot(tx)) {
            tracing::warn!(%err, "dropping queue snapshot request");
            return None;
        }
        rx.await.ok()
    }
}

pub struct Player {
    client: Arc<Client>,
    cache: Arc<Cache>,
    sink: Box<dyn AudioSink>,
    queue: Queue,
    state: State,
    repeat: Repeat,
    random_batch: u32,
    prefetch: bool,
    /// Id of the track already handed to the sink as the gapless follow-up, so
    /// the tick loop does not queue it repeatedly.
    queued_next: Option<String>,
    /// True while the queue holds a restored session nothing has played from
    /// yet. It is what makes Play resume that track instead of starting a fresh
    /// random queue, without changing what Play does after a queue has simply
    /// run out (also Stopped, also a non-empty queue, but not resumable).
    resume_pending: bool,
    /// Id of the track whose download has been started ahead of time. Separate
    /// from `queued_next` because warming and queueing happen on different
    /// ticks: the file has to finish downloading before the decoder sees it.
    warming: Option<String>,
    events: broadcast::Sender<Event>,
}

impl Player {
    /// Spawns the actor and returns a handle. Everything else in the process
    /// talks to the player through this; there is no shared mutable state.
    pub fn spawn(
        client: Arc<Client>,
        cache: Arc<Cache>,
        sink: Box<dyn AudioSink>,
        random_batch: u32,
        prefetch: bool,
        repeat: Repeat,
    ) -> PlayerHandle {
        let (tx, rx) = mpsc::channel(32);
        let (events, _) = broadcast::channel(64);

        let player = Self {
            client,
            cache,
            sink,
            queue: Queue::new(),
            state: State::Stopped,
            repeat,
            random_batch,
            prefetch,
            resume_pending: false,
            queued_next: None,
            warming: None,
            events: events.clone(),
        };

        // The cache reports download progress on the same channel. It is given
        // the sender here rather than at construction because the channel is
        // created with the actor, and main builds the cache before it.
        player.cache.set_events(events.clone());

        tokio::spawn(player.run(rx));
        PlayerHandle { tx, events }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        let mut ticker = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    if matches!(cmd, Command::Shutdown) {
                        self.sink.stop();
                        break;
                    }
                    if let Err(err) = self.handle(cmd).await {
                        tracing::warn!(%err, "command failed");
                        self.emit(Event::Failed(format!("{err:#}")));
                    }
                }
                _ = ticker.tick() => {
                    if let Err(err) = self.tick().await {
                        tracing::warn!(%err, "tick failed");
                        self.emit(Event::Failed(format!("{err:#}")));
                    }
                }
            }
        }
        tracing::info!("player actor stopped");
    }

    fn emit(&self, event: Event) {
        // No subscribers is normal at startup, so a send error is not a problem.
        let _ = self.events.send(event);
    }

    fn set_state(&mut self, state: State) {
        if self.state != state {
            self.state = state;
            self.emit(Event::StateChanged(state));
        }
    }

    async fn handle(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::PlayRandom => self.play_random().await?,
            Command::PlayAlbum(album_id) => {
                let tracks = self
                    .client
                    .album_tracks(&album_id)
                    .await
                    .context("fetching album tracks")?;
                self.queue.replace(tracks, 0, Mode::Explicit);
                self.start_current().await?;
            }
            Command::PlayArtist(artist_id) => {
                // Albums in release order, each expanded to its tracks, so the
                // artist plays through as a discography rather than shuffled.
                let albums = self
                    .client
                    .artist_albums(&artist_id)
                    .await
                    .context("fetching artist albums")?;
                let mut tracks = Vec::new();
                for album in albums {
                    tracks.extend(self.client.album_tracks(&album.id).await?);
                }
                self.queue.replace(tracks, 0, Mode::Explicit);
                self.start_current().await?;
            }
            Command::PlayItems { items, start } => {
                if items.is_empty() {
                    return Ok(());
                }
                self.queue.replace(items, start, Mode::Explicit);
                self.start_current().await?;
            }
            Command::PlayShuffled { mut items, first } => {
                if items.is_empty() || first >= items.len() {
                    return Ok(());
                }
                // Chosen track out first, the remainder shuffled behind it, so
                // the queue starts at index 0 and reads in play order.
                let chosen = items.remove(first);
                shuffle(&mut items);
                items.insert(0, chosen);

                self.queue.replace(items, 0, Mode::Explicit);
                self.start_current().await?;
            }
            Command::SetCacheLimit(bytes) => {
                self.cache.set_max_bytes(bytes);
                // Pruning is blocking filesystem work, but it is a directory
                // listing and a few unlinks, and it only happens when someone
                // moves the slider.
                if let Err(err) = self.cache.prune() {
                    tracing::warn!(%err, "cache prune failed");
                }
            }
            Command::CycleRepeat => self.set_repeat(self.repeat.next()),
            Command::SetRepeat(repeat) => self.set_repeat(repeat),
            Command::QueueLast { items } => self.enqueue(items, false),
            Command::QueueNext { items } => self.enqueue(items, true),
            Command::Remove { index, id } => {
                // The page that asked is working from a snapshot; if the queue
                // moved underneath it, the index now points at something else.
                if self.queue.items().get(index).map(|item| item.id.as_str()) != Some(id.as_str()) {
                    tracing::debug!(index, id, "ignoring a stale queue removal");
                    return Ok(());
                }
                if index == self.queue.cursor() {
                    self.emit(Event::Failed("that track is playing right now".into()));
                    return Ok(());
                }
                // Already handed to the sink for a gapless transition, and rodio
                // offers no way to take a queued source back - so it is going to
                // be heard whatever the queue says. Refusing is the only answer
                // that keeps the two in agreement. PREQUEUE_LEAD is what makes
                // this a narrow window rather than the usual case.
                if self.queued_next.as_deref() == Some(id.as_str()) {
                    self.emit(Event::Failed("that track is already cued up next".into()));
                    return Ok(());
                }
                if self.queue.remove(index) {
                    self.persist();
                    self.emit(Event::QueueChanged);
                }
            }
            Command::PlayPause => match self.state {
                State::Playing => {
                    self.sink.pause();
                    self.set_state(State::Paused);
                }
                State::Paused => {
                    self.sink.resume();
                    self.set_state(State::Playing);
                }
                // Nothing loaded yet, so treat it as "start something".
                State::Stopped => self.resume_or_random().await?,
            },
            Command::Play => {
                if self.state == State::Paused {
                    self.sink.resume();
                    self.set_state(State::Playing);
                } else if self.state == State::Stopped {
                    self.resume_or_random().await?;
                }
            }
            Command::Pause => {
                if self.state == State::Playing {
                    self.sink.pause();
                    self.set_state(State::Paused);
                }
            }
            Command::Next => self.advance().await?,
            Command::Previous => {
                if self.queue.back().is_some() {
                    self.start_current().await?;
                }
            }
            Command::Stop => {
                self.sink.stop();
                self.set_state(State::Stopped);
                self.emit(Event::Position(Duration::ZERO));
            }
            Command::Seek(pos) => self.seek(pos).await?,
            Command::Restore => {
                // Whatever the user has already asked for outranks the old
                // session: the restore command and a tray click can race.
                if !self.queue.is_empty() {
                    return Ok(());
                }
                if let Some((items, cursor, mode)) = persist::load() {
                    let count = items.len();
                    self.queue.replace(items, cursor, mode);
                    self.resume_pending = true;
                    tracing::info!(count, cursor, ?mode, "restored queue from previous session");
                    // Shows the track in now-playing, the tray tooltip and
                    // MPRIS metadata without touching the audio device.
                    self.emit(Event::TrackChanged(self.queue.current().cloned()));
                }
            }
            Command::Snapshot(reply) => {
                // A receiver that has gone away is normal: the popup may have
                // been closed while the request was queued.
                let _ = reply.send(Snapshot {
                    items: self.queue.items().to_vec(),
                    cursor: self.queue.cursor(),
                    repeat: self.repeat,
                });
            }
            // Handled in run() so the loop can break.
            Command::Shutdown => {}
        }
        Ok(())
    }

    async fn tick(&mut self) -> Result<()> {
        // Decode failures are reported even while paused or stopped.
        if let Some(fault) = self.sink.take_fault() {
            self.emit(Event::Failed(fault.message));
            // Only skip when the failure is the track being played; a queued
            // track that failed to load is retried when it comes up normally.
            if fault.affects_current {
                if self.state == State::Playing {
                    self.advance().await?;
                }
                return Ok(());
            }
        }

        if self.state != State::Playing {
            return Ok(());
        }

        // The sink crosses into a pre-queued track by itself, so the queue
        // cursor is caught up here rather than by starting playback again.
        let advances = self.sink.take_advances();
        let mut advanced = false;
        for _ in 0..advances {
            self.queued_next = None;
            self.warming = None;
            match self.queue.advance(self.repeat == Repeat::All) {
                Some(item) => {
                    let item = item.clone();
                    advanced = true;
                    self.emit(Event::TrackChanged(Some(item)));
                }
                // Sink moved on but the queue did not: nothing sensible to
                // report, and finished() below will settle it.
                None => break,
            }
        }
        if advanced {
            self.persist();
        }

        if self.sink.finished() {
            if self.repeat == Repeat::One {
                // Same track again. Not gapless - nothing was pre-queued, since
                // handing the sink a second copy of the current track would make
                // the tick loop's advance bookkeeping describe a track change
                // that did not happen. The file is cached by definition here, so
                // the join costs a decoder build.
                self.start_current().await?;
            } else {
                self.advance().await?;
            }
            return Ok(());
        }

        self.emit(Event::Position(self.sink.position()));

        if self.prefetch {
            self.queue_next().await;
        }
        Ok(())
    }

    /// Applies a repeat setting and remembers it for the next session.
    ///
    /// Persisted here rather than by the button, because MPRIS `LoopStatus` can
    /// change it too and this is the one place both paths pass through.
    fn set_repeat(&mut self, repeat: Repeat) {
        if self.repeat == repeat {
            return;
        }
        self.repeat = repeat;
        tracing::debug!(?repeat, "repeat changed");

        // Turning repeat-one on with the next track already committed to the sink
        // cannot be undone (rodio has no way to drop a queued source), so that
        // one transition still happens; from the track after it, repeat-one
        // holds. Clearing the marker means the tick loop reconsiders what to
        // queue as soon as the setting allows it.
        self.queued_next = None;

        if let Err(err) = Settings::update(|settings| settings.repeat = repeat) {
            tracing::warn!(%err, "cannot save the repeat setting");
        }
        self.emit(Event::RepeatChanged(repeat));
    }

    /// Adds tracks to the queue without disturbing what is playing.
    ///
    /// `next` distinguishes the two menu entries: "Play next" puts them directly
    /// after the current track, "Add to queue" at the end. Neither changes
    /// `Mode`, so a random queue carries on refilling behind the additions -
    /// deliberate: enqueueing an album should not silently end random play.
    fn enqueue(&mut self, items: Vec<Item>, next: bool) {
        if items.is_empty() {
            tracing::debug!("ignoring an empty enqueue");
            return;
        }
        tracing::debug!(
            count = items.len(),
            next,
            queue = self.queue.items().len(),
            cursor = self.queue.cursor(),
            "enqueue"
        );

        if self.queue.is_empty() {
            // Nothing to queue behind. These tracks become the queue, shown but
            // not started - the same state a restored session leaves behind, so
            // Play picks it up rather than starting a random queue.
            self.queue.replace(items, 0, Mode::Explicit);
            self.resume_pending = true;
            self.emit(Event::TrackChanged(self.queue.current().cloned()));
        } else if next {
            // If a track has already been committed to the sink it is going to
            // be heard next no matter what the queue says, so the insert goes
            // after it: the queue keeps describing what will actually play.
            let after = match self.queued_next {
                Some(_) => self.queue.cursor() + 1,
                None => self.queue.cursor(),
            };
            self.queue.insert_after(after, items);
        } else {
            self.queue.append(items);
        }

        self.persist();
        self.emit(Event::QueueChanged);
    }

    /// What Play means from a stopped player: pick up the restored session if
    /// there is one, otherwise start a fresh random queue - which is also what
    /// happens once a queue has played itself out.
    async fn resume_or_random(&mut self) -> Result<()> {
        if self.resume_pending && !self.queue.is_empty() {
            return self.start_current().await;
        }
        self.play_random().await
    }

    /// Records the queue for the next launch.
    ///
    /// Failures are logged and dropped: not being able to write the state file
    /// is no reason to interrupt playback.
    fn persist(&self) {
        if let Err(err) = persist::save(&self.queue) {
            tracing::warn!(%err, "cannot save queue state");
        }
    }

    /// Starts a fresh random queue.
    ///
    /// A separate method rather than recursing into handle(): a recursive async
    /// fn would need boxing, and this reads better anyway.
    async fn play_random(&mut self) -> Result<()> {
        // No track to name yet, but the query itself can be the slow part.
        self.emit(Event::Loading(None));
        let tracks = self
            .client
            .random_tracks(self.random_batch)
            .await
            .context("fetching random tracks")?;
        if tracks.is_empty() {
            self.emit(Event::Failed("server returned no tracks".into()));
            return Ok(());
        }
        self.queue.replace(tracks, 0, Mode::Random);
        self.start_current().await
    }

    /// Seeks the current track.
    ///
    /// Two strategies. If the demuxer knows the frame count it can position
    /// itself accurately. If it does not - a CBR mp3 with no Xing/Info tag, or a
    /// streamed transcode - the byte offset is estimated here instead, from
    /// Jellyfin's duration and the file size, and the decoder is started from
    /// there. That estimate is exact for constant bitrate, which is precisely
    /// the case that lacks a frame count.
    async fn seek(&mut self, pos: Duration) -> Result<()> {
        let Some(item) = self.queue.current().cloned() else {
            return Ok(());
        };

        // Seeking always needs a complete file: the demuxer path seeks, and the
        // byte-offset path needs a known length. Usually already cached, so this
        // returns at once; if the track is still arriving it waits for it.
        let mut reader = self.open_complete(&item).await?;

        if self.sink.is_seekable() {
            self.sink.seek(reader, pos).context("seeking")?;
            self.emit(Event::Seeked(pos));
            return Ok(());
        }

        let (Some(total), Some(len)) = (item.duration(), reader.byte_len()) else {
            self.emit(Event::Failed("this track cannot be seeked".into()));
            return Ok(());
        };
        if total.is_zero() {
            self.emit(Event::Failed("this track cannot be seeked".into()));
            return Ok(());
        }

        // Kept just inside the end so a seek to the very end still yields a
        // frame to resynchronise on rather than instant end-of-stream.
        let fraction = (pos.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 0.99);
        let byte = (len as f64 * fraction) as u64;

        use std::io::Seek as _;
        reader
            .seek(std::io::SeekFrom::Start(byte))
            .context("positioning reader")?;

        tracing::debug!(
            byte,
            of = len,
            ?pos,
            "seeking by byte offset (stream has no frame count)"
        );
        self.sink.play_at(reader, pos).context("seeking")?;
        self.emit(Event::Seeked(pos));
        Ok(())
    }

    /// Tops up a random queue before it runs dry.
    async fn maybe_refill(&mut self) {
        if self.queue.mode != Mode::Random || !self.queue.running_low(REFILL_SLACK) {
            return;
        }
        match self.client.random_tracks(self.random_batch).await {
            Ok(more) => {
                self.queue.append(more);
                self.persist();
            }
            // A refill failure should not stop what is already queued.
            Err(err) => tracing::warn!(%err, "random refill failed"),
        }
    }

    /// Moves to the next track, refilling a random queue if it is running out.
    async fn advance(&mut self) -> Result<()> {
        // Before the refill, not after: a random queue tops itself up over the
        // network here, so this is one of the places a Next press can sit for a
        // while with nothing to show for it. `start_current` announces the same
        // track again once the cursor has actually moved.
        self.emit(Event::Loading(
            self.queue.peek_next(self.repeat == Repeat::All).cloned(),
        ));
        self.maybe_refill().await;

        if self.queue.advance(self.repeat == Repeat::All).is_some() {
            self.start_current().await
        } else {
            self.sink.stop();
            self.set_state(State::Stopped);
            self.emit(Event::TrackChanged(None));
            Ok(())
        }
    }

    /// Starts whatever the cursor points at, skipping past tracks the server has
    /// lost.
    ///
    /// A loop rather than recursion into itself: an async fn calling itself needs
    /// boxing, and more importantly the bound is the thing that makes this safe.
    /// Repeat-all on a queue whose tracks have all gone would otherwise walk in a
    /// circle forever, dropping and re-trying.
    async fn start_current(&mut self) -> Result<()> {
        for _ in 0..MAX_MISSING_SKIPS {
            let Some(item) = self.queue.current().cloned() else {
                self.set_state(State::Stopped);
                return Ok(());
            };

            // Announced before the open, which is the await that can take
            // seconds: nothing else in the process can tell the UI that a track
            // is on its way, since this actor is busy for the whole of it.
            self.emit(Event::Loading(Some(item.clone())));

            let reader = match self.open(&item).await {
                Ok(reader) => reader,
                // A track the server cannot serve *at all* is dropped from the
                // queue and skipped, rather than reported and left in place to
                // fail again every time it comes round. Anything else - a network
                // blip, a transcode that fell over - is reported and left alone,
                // because it may well work next time.
                Err(err) if err.downcast_ref::<cache::Gone>().is_some() => {
                    if self.drop_missing(&item) {
                        continue;
                    }
                    return Ok(());
                }
                Err(err) => return Err(err),
            };
            self.sink.play(reader).context("starting playback")?;

            // play() discards anything previously queued behind it.
            self.queued_next = None;
            self.warming = None;
            // Whatever was restored has now been played from, so Play is back to
            // meaning "resume" rather than "resume the previous session".
            self.resume_pending = false;
            self.emit(Event::TrackChanged(Some(item)));
            self.set_state(State::Playing);
            // Every queue change worth remembering ends up here: a new queue, a
            // skip, a Previous, a track row. The sink's own gapless advance is the
            // one exception, persisted from the tick loop.
            self.persist();
            return Ok(());
        }

        self.sink.stop();
        self.set_state(State::Stopped);
        self.emit(Event::Failed(
            "too many tracks in a row are missing on the server".into(),
        ));
        Ok(())
    }

    /// Drops every copy of a track the server has no file for and steps to
    /// whatever follows it. Returns false when nothing does.
    ///
    /// Mostly a restored-queue problem: Jellyfin derives item ids from paths, so
    /// reorganising a library gives every moved file a new id while the old one
    /// still resolves to a path that is gone. A queue remembered from before the
    /// move is then full of ids the server will 404 forever - and the queue is
    /// exactly the thing this player restores on startup.
    fn drop_missing(&mut self, item: &Item) -> bool {
        tracing::warn!(id = %item.id, name = %item.name, "dropping a track the server no longer has");
        self.emit(Event::Failed(format!(
            "{} is no longer on the server, skipping it",
            item.name
        )));

        // Every copy, not just the one at the cursor: a queue can hold the same
        // track twice and both are equally dead. Reverse order so the indices
        // ahead of each removal stay valid, and `remove` keeps the cursor on its
        // own track as the list shifts under it.
        let cursor = self.queue.cursor();
        for index in (0..self.queue.items().len()).rev() {
            if index != cursor && self.queue.items()[index].id == item.id {
                self.queue.remove(index);
            }
        }

        // The one at the cursor last, since `remove` deliberately refuses it.
        let mut stepped = self.queue.remove_current();
        // It was the final track, but repeat-all means the queue does not end.
        if !stepped && self.repeat == Repeat::All && !self.queue.is_empty() {
            stepped = self.queue.advance(true).is_some();
        }

        self.persist();
        self.emit(Event::QueueChanged);

        if !stepped {
            self.sink.stop();
            self.set_state(State::Stopped);
            self.emit(Event::TrackChanged(None));
        }
        stepped
    }

    /// Reader for starting playback: may still be downloading if the format
    /// allows it.
    ///
    /// Takes &mut self rather than &self on purpose: a shared reference held
    /// across an await would require Player to be Sync, which the boxed
    /// AudioSink is not.
    async fn open(&mut self, item: &Item) -> Result<cache::CacheReader> {
        let url = self.client.stream_url(&item.id)?;
        self.cache
            .open_for_playback(&item.id, extension(item), &url)
            .await
            .with_context(|| format!("opening stream for {}", item.name))
    }

    /// Reader over a complete file, needed wherever the decoder must seek.
    async fn open_complete(&mut self, item: &Item) -> Result<cache::CacheReader> {
        let url = self.client.stream_url(&item.id)?;
        self.cache
            .fetch(&item.id, extension(item), &url)
            .await
            .with_context(|| format!("opening stream for {}", item.name))
    }

    /// Hands the following track to the sink so it can cross into it without a
    /// decode gap.
    ///
    /// Two steps, on separate ticks. First the download is started. Only once
    /// the file is complete is it handed to the sink: a decoder probes by
    /// seeking, so opening a still-arriving file can fail outright. There is no
    /// hurry, since a track lasts far longer than its own download.
    async fn queue_next(&mut self) {
        // Repeat-one has no next track to hand over: the current one is replayed
        // from `tick` when the sink runs dry. Nothing to warm either - it is
        // already on disk.
        if self.repeat == Repeat::One {
            return;
        }

        // Refill first, otherwise the last track of a random queue never gets a
        // gapless successor.
        self.maybe_refill().await;

        let Some(next) = self.queue.peek_next(self.repeat == Repeat::All).cloned() else {
            return;
        };
        if self.queued_next.as_deref() == Some(next.id.as_str()) {
            return;
        }

        let ext = extension(&next).to_string();
        if !self.cache.is_cached(&next.id, &ext) {
            self.warm(&next, ext);
            return;
        }

        // Downloaded, but not handed over until the current track is nearly
        // done: see PREQUEUE_LEAD.
        if !self.near_end() {
            return;
        }

        // Complete by the check above, so the queued source is fully seekable
        // once the sink crosses into it.
        match self.open_complete(&next).await {
            Ok(reader) => match self.sink.set_next(reader) {
                Ok(()) => self.queued_next = Some(next.id),
                Err(err) => tracing::warn!(%err, "cannot queue next track"),
            },
            // Not fatal: the track is retried the normal way when it comes up.
            Err(err) => tracing::debug!(%err, "prefetch failed"),
        }
    }

    /// Whether the current track is within `PREQUEUE_LEAD` of its end.
    ///
    /// A track whose duration the server did not give us cannot be measured, so
    /// it commits as soon as the file is there - the old behaviour, which is
    /// better than never handing anything over and losing the gapless join.
    fn near_end(&self) -> bool {
        let Some(total) = self.queue.current().and_then(|item| item.duration()) else {
            return true;
        };
        total.saturating_sub(self.sink.position()) <= PREQUEUE_LEAD
    }

    /// Starts the download for a track without reading from it. Repeated calls
    /// for the same track are ignored, and Cache::open itself de-duplicates
    /// concurrent requests for the same key.
    fn warm(&mut self, item: &Item, ext: String) {
        if self.warming.as_deref() == Some(item.id.as_str()) {
            return;
        }
        let Ok(url) = self.client.stream_url(&item.id) else {
            return;
        };
        self.warming = Some(item.id.clone());

        let cache = self.cache.clone();
        let id = item.id.clone();
        tokio::spawn(async move {
            if let Err(err) = cache.open(&id, &ext, &url).await {
                tracing::debug!(%err, id, "prefetch download failed");
            }
        });
    }
}

/// Shuffles in place.
///
/// Keyed on `Uuid::new_v4`, a getrandom-backed source already in the dependency
/// tree, rather than adding an RNG crate for one call - this machine has no
/// crates.io access anyway. The keys are cached, so every element is compared by
/// one fixed random value and the result is a proper permutation.
fn shuffle(items: &mut [Item]) {
    items.sort_by_cached_key(|_| uuid::Uuid::new_v4().as_u128());
}

/// File extension for the cache entry.
///
/// This is the *source* container, which is not necessarily what lands on disk:
/// a transcoded track keeps the source extension while holding mp3 bytes. That
/// is harmless because symphonia probes content and ignores the name, and it
/// keeps the cache key stable whether or not the server transcoded.
fn extension(item: &Item) -> &str {
    match item.container.as_deref() {
        Some(c) => c.split(',').next().unwrap_or("bin"),
        None => "bin",
    }
}
