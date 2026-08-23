use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

use rodio::source::SeekError;
use rodio::Source;
use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use super::cache::CacheReader;

/// Consecutive decode errors tolerated before a track is abandoned. Individual
/// corrupt packets are common and recoverable; a run of them is not.
const MAX_DECODE_ERRORS: usize = 3;

/// A symphonia-backed `rodio::Source`, used instead of `rodio::Decoder`.
///
/// rodio's own decoder wrapper causes three problems that cannot be worked
/// around through its API:
///
/// * it hardcodes `enable_gapless: true`, which reaches an arithmetic underflow
///   in symphonia's mp3 demuxer on the Xing/LAME header ffmpeg writes for a
///   streamed transcode;
/// * it converts a symphonia seek error during initialisation into
///   `unreachable!()`, panicking the audio thread;
/// * its `MediaSource` reports `byte_len()` as None, so symphonia cannot seek in
///   a stream that carries no seek table - exactly the case for a transcode -
///   even when the complete file is sitting on disk with a known length.
///
/// Owning this layer fixes all three and lets the format hint be set from the
/// container we already know about.
pub struct SymphoniaSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    spec: SignalSpec,
    buffer: SampleBuffer<i16>,
    /// Index of the next sample to hand out of `buffer`.
    position: usize,
    total_duration: Option<Duration>,
}

impl SymphoniaSource {
    /// `extension` is only a probe hint; symphonia still verifies by content, so
    /// a wrong guess costs nothing.
    pub fn new(reader: CacheReader, extension: Option<&str>) -> Result<Self, String> {
        let source = CacheSource::new(reader);
        let stream = MediaSourceStream::new(Box::new(source), Default::default());

        let mut hint = Hint::new();
        if let Some(extension) = extension {
            hint.with_extension(extension);
        }

        // Gapless off on purpose: it is what trips symphonia's mp3 demuxer, and
        // trayplay's gapless transition comes from rodio's queue, not from
        // trimming encoder delay.
        let format_options = FormatOptions {
            enable_gapless: false,
            ..Default::default()
        };

        let probed = symphonia::default::get_probe()
            .format(&hint, stream, &format_options, &MetadataOptions::default())
            .map_err(|err| format!("unrecognised format: {err}"))?;
        let format = probed.format;

        let track = format
            .tracks()
            .iter()
            .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| "no track with a supported codec".to_string())?;
        let track_id = track.id;
        let codec_params = track.codec_params.clone();

        let decoder = symphonia::default::get_codecs()
            .make(&codec_params, &DecoderOptions::default())
            .map_err(|err| format!("unsupported codec: {err}"))?;

        let total_duration = codec_params
            .time_base
            .zip(codec_params.n_frames)
            .map(|(time_base, frames)| to_duration(time_base.calc_time(frames)));

        let spec = SignalSpec::new(
            codec_params.sample_rate.unwrap_or(44_100),
            codec_params
                .channels
                .unwrap_or(symphonia::core::audio::Channels::FRONT_LEFT),
        );

        let mut source = Self {
            format,
            decoder,
            track_id,
            spec,
            buffer: SampleBuffer::new(0, spec),
            position: 0,
            total_duration,
        };

        // Decode one packet up front so channels() and sample_rate() report the
        // real values rather than the header's guess before playback starts.
        source.next_packet().map_err(|err| err.to_string())?;

        tracing::debug!(
            codec = %codec_params.codec,
            n_frames = ?codec_params.n_frames,
            duration = ?source.total_duration,
            seekable = source.is_seekable(),
            rate = source.spec.rate,
            channels = source.spec.channels.count(),
            "decoder ready"
        );
        Ok(source)
    }

    /// Whether this stream can be positioned.
    ///
    /// Requires a known, non-zero frame count: symphonia's coarse seek derives a
    /// byte position from it. A CBR mp3 with no Xing/Info tag has none, and
    /// neither does a streamed transcode, so both are unseekable.
    pub fn is_seekable(&self) -> bool {
        matches!(self.total_duration, Some(duration) if !duration.is_zero())
    }

    /// Decodes the next usable packet into `buffer`.
    fn next_packet(&mut self) -> Result<(), SymphoniaError> {
        let mut errors = 0;

        loop {
            let packet = self.format.next_packet()?;
            if packet.track_id() != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let frames = decoded.capacity() as u64;

                    // Specs can change mid-stream, and the buffer is sized per
                    // packet, so it is rebuilt rather than reused.
                    let mut buffer = SampleBuffer::<i16>::new(frames, spec);
                    buffer.copy_interleaved_ref(decoded);

                    self.spec = spec;
                    self.buffer = buffer;
                    self.position = 0;
                    return Ok(());
                }
                // A single bad packet is worth skipping; a run of them is fatal.
                Err(SymphoniaError::DecodeError(err)) => {
                    errors += 1;
                    if errors > MAX_DECODE_ERRORS {
                        return Err(SymphoniaError::DecodeError(err));
                    }
                    tracing::debug!(%err, "skipping undecodable packet");
                }
                Err(SymphoniaError::ResetRequired) => {
                    self.decoder.reset();
                }
                Err(err) => return Err(err),
            }
        }
    }
}

impl Iterator for SymphoniaSource {
    type Item = i16;

    fn next(&mut self) -> Option<i16> {
        loop {
            if let Some(sample) = self.buffer.samples().get(self.position) {
                self.position += 1;
                return Some(*sample);
            }
            // End of stream is the normal way a track finishes.
            self.next_packet().ok()?;
        }
    }
}

impl Source for SymphoniaSource {
    /// Samples left in the current packet. rodio re-reads the channel count and
    /// rate at each frame boundary, which is how a mid-stream spec change is
    /// handled.
    fn current_frame_len(&self) -> Option<usize> {
        Some(self.buffer.len().saturating_sub(self.position))
    }

    fn channels(&self) -> u16 {
        self.spec.channels.count() as u16
    }

    fn sample_rate(&self) -> u32 {
        self.spec.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.total_duration
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        // Refuse rather than let symphonia divide by zero.
        //
        // Its mp3 coarse-seek estimates a byte position as
        // `ts * audio_len / (n_frames + delay + padding)`. ffmpeg writes a Xing
        // header claiming zero frames for a streamed transcode of unknown
        // length, so n_frames is Some(0) - which symphonia's own "unseekable"
        // check, testing only for None, lets straight through.
        match self.total_duration {
            None | Some(Duration::ZERO) => {
                return Err(SeekError::NotSupported {
                    underlying_source: "stream has no usable duration",
                })
            }
            Some(_) => {}
        }

        let seconds = pos.as_secs();
        let frac = pos.subsec_nanos() as f64 / 1_000_000_000.0;

        self.format
            .seek(
                // Coarse is enough for a seek bar and works on containers with
                // no seek index, where accurate seeking would be refused.
                SeekMode::Coarse,
                SeekTo::Time {
                    time: Time::new(seconds, frac),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|err| SeekError::Other(Box::new(SeekFailed(err.to_string()))))?;

        // Decoder state refers to the old position.
        self.decoder.reset();
        self.buffer = SampleBuffer::new(0, self.spec);
        self.position = 0;
        Ok(())
    }
}

#[derive(Debug)]
struct SeekFailed(String);

impl std::fmt::Display for SeekFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SeekFailed {}

/// Adapts a cache entry to symphonia, reporting the real byte length.
///
/// This is the whole point of the exercise: with a length, symphonia can seek by
/// byte-offset search in streams that carry no seek table.
struct CacheSource {
    inner: CacheReader,
    len: Option<u64>,
}

impl CacheSource {
    fn new(inner: CacheReader) -> Self {
        // A still-downloading entry must report itself unseekable, or the probe
        // will seek past the download head. Reads would then block until the
        // bytes arrive, stalling playback for as long as the download takes -
        // exactly what progressive decoding is meant to avoid.
        let len = inner.is_complete().then(|| inner.byte_len()).flatten();
        Self { inner, len }
    }
}

impl Read for CacheSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for CacheSource {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(from)
    }
}

impl MediaSource for CacheSource {
    fn is_seekable(&self) -> bool {
        // Playback always decodes a complete local file, so seeking is only
        // limited by what the container supports.
        self.len.is_some()
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

fn to_duration(time: Time) -> Duration {
    Duration::from_secs_f64(time.seconds as f64 + time.frac)
}
