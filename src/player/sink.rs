use std::time::Duration;

use anyhow::Result;

use super::cache::CacheReader;

/// A decode or output failure reported by the sink.
#[derive(Debug, Clone)]
pub struct Fault {
    pub message: String,
    /// True when the track currently playing is the one that failed, so the
    /// caller should skip it. A failure loading the *queued* track must not
    /// interrupt what is playing now.
    pub affects_current: bool,
}

/// Audio output abstraction.
///
/// Implementations own a dedicated thread (rodio's `OutputStream` is not Send,
/// and `Sink::try_seek` blocks on a feedback channel), so every method here is
/// non-blocking and state is read back from published values. This trait is the
/// swap point if the pure-Rust decoder stack ever needs replacing with
/// GStreamer.
///
/// Decoding itself lives in `player::decoder`, not in rodio.
///
/// Deliberately no volume control: the system mixer owns volume.
pub trait AudioSink: Send {
    /// Replaces whatever is playing, discarding any queued follow-up track.
    fn play(&mut self, reader: CacheReader) -> Result<()>;

    /// Queues the following track so the sink can cross into it without a
    /// decode gap. The sink advances on its own; the caller learns about it
    /// through [`AudioSink::take_advances`].
    fn set_next(&mut self, reader: CacheReader) -> Result<()>;

    fn pause(&mut self);
    fn resume(&mut self);
    fn stop(&mut self);

    /// Seeks by building a fresh decoder from `reader` and positioning it before
    /// it reaches the output.
    ///
    /// The reader is a parameter because rodio's own `Sink::try_seek` runs the
    /// seek on cpal's audio callback thread, where a panic in a demuxer cannot
    /// be contained, poisons rodio's internal mutex, and takes the process down
    /// with it. Rebuilding the source keeps every fallible step on a thread the
    /// implementation owns.
    fn seek(&mut self, reader: CacheReader, pos: Duration) -> Result<()>;

    /// Playback position within the current track.
    fn position(&self) -> Duration;

    /// Whether the current track can be positioned by the demuxer. False for
    /// streams with no frame count (a CBR mp3 without a Xing tag, or a streamed
    /// transcode); the caller falls back to [`AudioSink::play_at`].
    fn is_seekable(&self) -> bool;

    /// Plays from wherever `reader` is already positioned, reporting positions
    /// offset by `position`.
    ///
    /// This is the fallback for streams the demuxer cannot seek: the caller
    /// computes a byte offset itself and pre-positions the reader. Only sound
    /// for self-synchronising formats, which in practice means mp3 - the same
    /// formats that lack a frame count.
    fn play_at(&mut self, reader: CacheReader, position: Duration) -> Result<()>;

    /// True once nothing is playing and nothing is queued.
    fn finished(&mut self) -> bool;

    /// Number of times the sink moved to a queued track by itself since the
    /// last call. A counter rather than a flag so a caller that polls slowly
    /// cannot miss a transition.
    fn take_advances(&mut self) -> usize;

    /// Last decode or output failure, if any. Consumed by reading.
    fn take_fault(&mut self) -> Option<Fault>;
}
