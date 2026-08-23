use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rodio::{OutputStream, Sink};

use super::cache::CacheReader;
use super::decoder::SymphoniaSource;
use super::sink::{AudioSink, Fault};

/// How often the audio thread republishes position and queue depth.
///
/// This also bounds how quickly end-of-track is noticed, which is why it is far
/// shorter than the actor's own tick.
const POLL: Duration = Duration::from_millis(50);

/// Below this, `Sink::get_pos()` is taken to describe the source just appended
/// rather than the one it replaced.
const SETTLED_POS: Duration = Duration::from_millis(500);

/// Polls to wait for that before trusting `get_pos()` regardless. A backstop:
/// without it, a rodio that never reset would freeze the position at `offset`.
const SETTLE_POLLS: u8 = 20;

enum Cmd {
    Play(CacheReader),
    Queue(CacheReader),
    Pause,
    Resume,
    Stop,
    /// A seek carries its own reader: the source is rebuilt rather than seeked
    /// in place. See AudioSink::seek.
    Seek {
        reader: CacheReader,
        pos: Duration,
    },
    /// Play from wherever the reader already sits, reporting `pos` as the
    /// starting position.
    PlayAt {
        reader: CacheReader,
        pos: Duration,
    },
}

/// State published by the audio thread for the handle to read.
#[derive(Default)]
struct Shared {
    position_ms: AtomicU64,
    /// Sources still loaded in the rodio sink.
    queued: AtomicUsize,
    /// Monotonic count of self-driven track transitions.
    advances: AtomicUsize,
    /// Whether the source now playing can be positioned.
    seekable: AtomicBool,
    fault: Mutex<Option<Fault>>,
}

impl Shared {
    fn fail(&self, message: String, affects_current: bool) {
        tracing::warn!(message, affects_current, "audio failure");
        *self.fault.lock().unwrap() = Some(Fault {
            message,
            affects_current,
        });
    }
}

/// Handle to the audio thread. All rodio types stay on that thread.
pub struct RodioSink {
    tx: mpsc::Sender<Cmd>,
    shared: Arc<Shared>,
    seen_advances: usize,
    /// Set between issuing a Play and the thread reporting a loaded source, so
    /// the gap is not mistaken for end-of-track.
    pending_start: bool,
}

impl RodioSink {
    pub fn new() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let shared = Arc::new(Shared::default());

        let thread_shared = shared.clone();
        thread::Builder::new()
            .name("trayplay-audio".into())
            .spawn(move || {
                // OutputStream must be created on, and never leave, this thread.
                match OutputStream::try_default()
                    .map_err(|e| anyhow!("opening audio output: {e}"))
                    .and_then(|(stream, handle)| {
                        let sink =
                            Sink::try_new(&handle).map_err(|e| anyhow!("creating sink: {e}"))?;
                        Ok((stream, sink))
                    })
                {
                    Ok((stream, sink)) => {
                        let _ = ready_tx.send(Ok(()));
                        // Held for the thread's lifetime: dropping the stream
                        // silences output.
                        let _stream = stream;
                        run(sink, rx, thread_shared);
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err.to_string()));
                    }
                }
            })
            .context("spawning audio thread")?;

        // Surface a broken audio setup at construction rather than on first play.
        ready_rx
            .recv()
            .context("audio thread died during startup")?
            .map_err(|e| anyhow!(e))?;

        Ok(Self {
            tx,
            shared,
            seen_advances: 0,
            pending_start: false,
        })
    }

    fn send(&self, cmd: Cmd) -> Result<()> {
        self.tx
            .send(cmd)
            .map_err(|_| anyhow!("audio thread is gone"))
    }
}

impl AudioSink for RodioSink {
    fn play(&mut self, reader: CacheReader) -> Result<()> {
        self.pending_start = true;
        self.send(Cmd::Play(reader))
    }

    fn set_next(&mut self, reader: CacheReader) -> Result<()> {
        self.send(Cmd::Queue(reader))
    }

    fn pause(&mut self) {
        let _ = self.send(Cmd::Pause);
    }

    fn resume(&mut self) {
        let _ = self.send(Cmd::Resume);
    }

    fn stop(&mut self) {
        self.pending_start = false;
        let _ = self.send(Cmd::Stop);
    }

    fn seek(&mut self, reader: CacheReader, pos: Duration) -> Result<()> {
        self.send(Cmd::Seek { reader, pos })
    }

    fn play_at(&mut self, reader: CacheReader, position: Duration) -> Result<()> {
        self.pending_start = true;
        self.send(Cmd::PlayAt {
            reader,
            pos: position,
        })
    }

    fn position(&self) -> Duration {
        Duration::from_millis(self.shared.position_ms.load(Ordering::Relaxed))
    }

    fn is_seekable(&self) -> bool {
        self.shared.seekable.load(Ordering::Relaxed)
    }

    fn finished(&mut self) -> bool {
        if self.shared.queued.load(Ordering::Relaxed) > 0 {
            self.pending_start = false;
            return false;
        }
        // Empty, but a Play may simply not have been picked up yet.
        !self.pending_start
    }

    fn take_advances(&mut self) -> usize {
        let total = self.shared.advances.load(Ordering::Relaxed);
        let new = total.saturating_sub(self.seen_advances);
        self.seen_advances = total;
        new
    }

    fn take_fault(&mut self) -> Option<Fault> {
        self.shared.fault.lock().unwrap().take()
    }
}

impl Drop for RodioSink {
    fn drop(&mut self) {
        // Dropping the sender makes the thread's recv fail, which ends it.
        let _ = self.send(Cmd::Stop);
    }
}

/// Builds a decoder, containing panics.
///
/// The known panic paths in rodio's decoder are gone now that SymphoniaSource
/// replaces it, but symphonia's demuxers still contain reachable arithmetic
/// panics on malformed input. An unwinding audio thread would take all playback
/// down for the rest of the process lifetime, so this stays as cheap insurance:
/// a skipped track is a far better outcome.
fn decode(reader: CacheReader) -> Result<SymphoniaSource, String> {
    let attempt =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| SymphoniaSource::new(reader, None)));
    match attempt {
        Ok(Ok(decoder)) => Ok(decoder),
        Ok(Err(err)) => Err(err),
        Err(_) => Err("decoder panicked while opening the track".into()),
    }
}

/// Positions a freshly built source, containing panics.
///
/// symphonia's coarse seek does arithmetic on header-derived values and can
/// panic on malformed ones. Running it here, rather than through
/// `Sink::try_seek`, keeps it off cpal's callback thread where a panic would
/// poison rodio's mutex and abort the process during cleanup.
fn seek_source(source: &mut SymphoniaSource, pos: Duration) -> Result<(), String> {
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rodio::Source::try_seek(source, pos)
    }));
    match attempt {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err("demuxer panicked while seeking".into()),
    }
}

fn run(sink: Sink, rx: mpsc::Receiver<Cmd>, shared: Arc<Shared>) {
    let mut last_len = 0usize;
    // Added to the sink's own position, which restarts at zero after a seek
    // replaces the source.
    let mut offset = Duration::ZERO;
    // Seekability of the source queued behind the current one, applied when the
    // sink crosses into it.
    let mut queued_seekable: Option<bool> = None;
    // Polls left to ignore sink.get_pos() after a source swap. See below.
    let mut settling = 0u8;

    loop {
        // A command that changes the queue on purpose must not be counted as a
        // self-driven track transition.
        let mut resync = false;

        match rx.recv_timeout(POLL) {
            Ok(Cmd::Play(reader)) => {
                sink.clear();
                match decode(reader) {
                    Ok(decoder) => {
                        shared.seekable.store(decoder.is_seekable(), Ordering::Relaxed);
                        sink.append(decoder);
                        // clear() leaves the sink paused.
                        sink.play();
                    }
                    Err(err) => shared.fail(format!("cannot decode track: {err}"), true),
                }
                queued_seekable = None;
                offset = Duration::ZERO;
                resync = true;
            }
            Ok(Cmd::Queue(reader)) => match decode(reader) {
                Ok(decoder) => {
                    // Remembered rather than published: it only applies once the
                    // sink crosses into this source.
                    queued_seekable = Some(decoder.is_seekable());
                    sink.append(decoder);
                }
                // Only the queued track is affected; whatever is playing now
                // must keep playing.
                Err(err) => shared.fail(format!("cannot decode next track: {err}"), false),
            },
            Ok(Cmd::Pause) => sink.pause(),
            Ok(Cmd::Resume) => sink.play(),
            Ok(Cmd::Stop) => {
                sink.clear();
                offset = Duration::ZERO;
                resync = true;
            }
            Ok(Cmd::Seek { reader, pos }) => {
                // The replacement source is positioned before it is handed over,
                // so a failure at any step leaves current playback untouched.
                // A refused seek is not a reason to abandon the track.
                match decode(reader)
                    .and_then(|mut source| seek_source(&mut source, pos).map(|()| source))
                {
                    Ok(source) => {
                        sink.clear();
                        sink.append(source);
                        sink.play();
                        // get_pos restarts at zero for a freshly appended source.
                        offset = pos;
                        resync = true;
                    }
                    Err(err) => shared.fail(format!("seek failed: {err}"), false),
                }
            }
            Ok(Cmd::PlayAt { reader, pos }) => {
                // The reader is already positioned; the decoder resynchronises
                // to the next frame from there.
                match decode(reader) {
                    Ok(decoder) => {
                        // Deliberately does not publish this source's
                        // seekability. Started past the header, an mp3 with a
                        // zero-frame Xing tag looks seekable because symphonia
                        // estimates duration from the bitrate - but the next
                        // seek rebuilds from byte 0, where it is not. The value
                        // must describe the file from its start, so it stays as
                        // the track's initial decode set it.
                        sink.clear();
                        sink.append(decoder);
                        sink.play();
                        offset = pos;
                        queued_seekable = None;
                        resync = true;
                    }
                    Err(err) => shared.fail(format!("seek failed: {err}"), false),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let len = sink.len();
        if resync {
            last_len = len;
            // The queue was replaced on purpose, so the position published below
            // must not mix the new offset with the old source's progress.
            settling = SETTLE_POLLS;
        } else if len < last_len {
            // One source played out and the next became current, which also
            // means the seek offset belongs to the track that just ended.
            //
            // Reaching *zero* is end-of-track, not a transition: there was
            // nothing to cross into. Counting it would have the actor step the
            // queue cursor for it and then step again from `finished()`, so a
            // track that ended with nothing queued behind it skipped its
            // successor - and under repeat-one, where nothing is ever queued on
            // purpose, it played the next track instead of repeating.
            let crossed = last_len - len;
            let crossed = if len == 0 {
                crossed.saturating_sub(1)
            } else {
                crossed
            };
            if crossed > 0 {
                shared.advances.fetch_add(crossed, Ordering::Relaxed);
            }
            offset = Duration::ZERO;
            if let Some(seekable) = queued_seekable.take() {
                shared.seekable.store(seekable, Ordering::Relaxed);
            }
            last_len = len;
        } else {
            last_len = len;
        }

        shared.queued.store(len, Ordering::Relaxed);

        // rodio updates its position from the audio callback, so for a few polls
        // after clear()/append() get_pos() still describes the source that was
        // replaced. Adding the new offset to that reports a position ahead of
        // both the old and the new one - after a seek backwards it is the seek
        // target plus wherever playback had got to, which the UI renders as a
        // jump forward before settling back.
        let mut sink_pos = sink.get_pos();
        if settling > 0 {
            if sink_pos <= SETTLED_POS {
                settling = 0;
            } else {
                settling -= 1;
                sink_pos = Duration::ZERO;
            }
        }

        shared
            .position_ms
            .store((offset + sink_pos).as_millis() as u64, Ordering::Relaxed);
    }

    tracing::info!("audio thread stopped");
}
