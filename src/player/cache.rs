use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

/// Progressive on-disk track cache.
///
/// Decoding reads from a local file rather than straight from HTTP: rodio's
/// decoder needs `Read + Seek`, and satisfying `Seek` with ranged requests is
/// both fragile and slow. A partially downloaded file is readable immediately,
/// with reads past the download head blocking until more bytes land, so playback
/// still starts without waiting for the whole track.
///
/// Files are named `<key>.<ext>` when complete and `<key>.<ext>.part` while in
/// flight, so the presence of the final name is itself the "cached" marker.
pub struct Cache {
    dir: PathBuf,
    http: reqwest::Client,
    max_bytes: u64,
    /// Downloads currently running, so a prefetch and a play of the same track
    /// share one HTTP request instead of racing each other.
    inflight: Mutex<HashMap<String, Inflight>>,
}

/// What a joining reader needs to know about a download already in progress.
#[derive(Clone)]
struct Inflight {
    progress: Arc<Progress>,
    /// Answers `SeekFrom::End`, which is how a decoder probe determines stream
    /// length. None until the response headers arrive, and for a response with
    /// no Content-Length at all.
    total: Option<u64>,
    /// None likewise. Carried because it decides whether the entry can be
    /// decoded while it downloads: without it every joining reader looks like an
    /// unknown format and waits for the whole file, which for a real-time
    /// transcode is seconds of silence.
    content_type: Option<String>,
}

#[derive(Debug, Default)]
struct ProgressState {
    written: u64,
    done: bool,
    failed: Option<String>,
}

/// Shared between the tokio writer task and the blocking decoder-side reader.
///
/// A Condvar rather than a tokio watch channel: the reader runs on rodio's
/// decode thread, which has no runtime to await on.
#[derive(Debug, Default)]
struct Progress {
    state: Mutex<ProgressState>,
    cv: Condvar,
}

impl Progress {
    fn publish(&self, written: u64) {
        let mut g = self.state.lock().unwrap();
        g.written = written;
        self.cv.notify_all();
    }

    fn finish(&self) {
        let mut g = self.state.lock().unwrap();
        g.done = true;
        self.cv.notify_all();
    }

    fn fail(&self, msg: String) {
        let mut g = self.state.lock().unwrap();
        g.failed = Some(msg);
        g.done = true;
        self.cv.notify_all();
    }

    /// Blocks until `want` bytes are available, the download finishes, or it
    /// fails. Returns the number of bytes now available.
    fn wait_for(&self, want: u64) -> io::Result<u64> {
        let mut g = self.state.lock().unwrap();
        loop {
            if let Some(err) = &g.failed {
                return Err(io::Error::other(err.clone()));
            }
            if g.written >= want || g.done {
                return Ok(g.written);
            }
            g = self.cv.wait(g).unwrap();
        }
    }

    fn is_done(&self) -> bool {
        self.state.lock().unwrap().done
    }
}

impl Cache {
    pub fn new(dir: PathBuf, max_bytes: u64, http: reqwest::Client) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            http,
            max_bytes,
            inflight: Mutex::new(HashMap::new()),
        })
    }

    /// True when a complete local copy exists, so a reader will never block.
    ///
    /// Decoders probe by seeking (an MP3 reader looks for a Xing header and the
    /// ID3v1 tag in the last 128 bytes), so a partially downloaded file can fail
    /// to open even when plenty of it has arrived. Callers that can afford to
    /// wait should check this first.
    pub fn is_cached(&self, key: &str, ext: &str) -> bool {
        self.dir.join(format!("{key}.{ext}")).exists()
    }

    /// Returns a reader over a *complete* local copy, downloading it first if
    /// necessary.
    ///
    /// This is what playback uses. Handing a partially downloaded file to a
    /// decoder does not work: rodio reports `byte_len()` as None, so symphonia
    /// never learns the stream length, and container probing (isomp4 in
    /// particular, whose moov atom may sit at the end of the file) seeks around
    /// freely. A seek past the download head then fails, and rodio 0.20 turns a
    /// symphonia seek error during init into `unreachable!()` - a panic on the
    /// audio thread.
    ///
    /// The cost is waiting for the download before the first note. In practice
    /// that is a fraction of a second, and every subsequent track is prefetched.
    pub async fn fetch(self: &Arc<Self>, key: &str, ext: &str, url: &str) -> Result<CacheReader> {
        let reader = self.open(key, ext, url).await?;

        // Already complete on disk.
        let Some(progress) = reader.progress.clone() else {
            return Ok(reader);
        };

        // Release the handle on the partial file before waiting.
        drop(reader);

        // wait_for blocks on a condvar, so it must not run on a runtime thread.
        tokio::task::spawn_blocking(move || progress.wait_for(u64::MAX))
            .await
            .context("waiting for download")?
            .with_context(|| format!("downloading {key}"))?;

        let final_path = self.dir.join(format!("{key}.{ext}"));
        let file = File::open(&final_path)
            .with_context(|| format!("opening {}", final_path.display()))?;
        let total = file.metadata()?.len();
        Ok(CacheReader {
            file,
            pos: 0,
            total: Some(total),
            progress: None,
            content_type: None,
        })
    }

    /// Reader for immediate playback.
    ///
    /// Starts decoding a live download when the format tolerates it (mp3), and
    /// otherwise waits for the file to complete. This is what keeps a transcoded
    /// track - which arrives in real time, so several seconds for a whole song -
    /// from delaying playback by its entire download.
    ///
    /// The trade-off is that a partial stream cannot be seeked by the demuxer;
    /// `Player::seek` falls back to a byte offset, which needs a complete file
    /// and so waits at that point instead.
    pub async fn open_for_playback(
        self: &Arc<Self>,
        key: &str,
        ext: &str,
        url: &str,
    ) -> Result<CacheReader> {
        let reader = self.open(key, ext, url).await?;
        if reader.is_complete() || reader.can_decode_while_downloading() {
            return Ok(reader);
        }

        tracing::debug!(
            key,
            content_type = ?reader.content_type,
            "format needs a complete file, waiting for download"
        );
        drop(reader);
        self.fetch(key, ext, url).await
    }

    /// Opens a reader for `key`, starting a download if no complete copy exists.
    /// The reader may block on reads while bytes are still arriving; prefer
    /// [`Cache::fetch`] or [`Cache::open_for_playback`] for anything decoded.
    ///
    /// Response headers are awaited before returning, so auth failures and 404s
    /// surface here instead of as a silent stall inside the decoder.
    pub async fn open(self: &Arc<Self>, key: &str, ext: &str, url: &str) -> Result<CacheReader> {
        let final_path = self.dir.join(format!("{key}.{ext}"));
        let part_path = self.dir.join(format!("{key}.{ext}.part"));

        if final_path.exists() {
            let file = File::open(&final_path)
                .with_context(|| format!("opening {}", final_path.display()))?;
            let total = file.metadata()?.len();
            // Touch so LRU pruning treats a replay as recent use.
            let _ = touch(&final_path);
            return Ok(CacheReader {
                file,
                pos: 0,
                total: Some(total),
                progress: None,
                content_type: None,
            });
        }

        // Claim the key before the request is sent, not after the response
        // arrives.
        //
        // Registering afterwards left a window - the whole round trip - in which
        // a second open of the same track also found the map empty. Both then
        // created the *same* .part file, truncating each other's bytes, and
        // whichever finished second tried to rename a file the first had already
        // renamed away: "cannot finalise cache entry", ENOENT. Readers joining
        // that entry saw the file shrink under them and stalled. Two rapid Next
        // presses, where the track being warmed is also the one being played, hit
        // this reliably.
        //
        // The part file is created while the map is still locked so a joiner
        // always finds something to open. The headers are not in yet, so length
        // and content type are filled in below - a joiner that arrives before
        // then has neither, exactly as for a response that sends no
        // Content-Length.
        let progress = {
            let mut inflight = self.inflight.lock().unwrap();
            match inflight.get(key).cloned() {
                Some(entry) => {
                    let file = File::open(&part_path)
                        .with_context(|| format!("opening {}", part_path.display()))?;
                    tracing::debug!(key, "joining in-flight download");
                    return Ok(CacheReader {
                        file,
                        pos: 0,
                        total: entry.total,
                        progress: Some(entry.progress),
                        content_type: entry.content_type,
                    });
                }
                None => {
                    File::create(&part_path)
                        .with_context(|| format!("creating {}", part_path.display()))?;
                    let progress = Arc::new(Progress::default());
                    inflight.insert(
                        key.to_string(),
                        Inflight {
                            progress: progress.clone(),
                            total: None,
                            content_type: None,
                        },
                    );
                    progress
                }
            }
        };

        // The key is reserved from here on, so every failure path has to release
        // it - otherwise later plays of this track would join a download that is
        // never going to start.
        let resp = match self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {key}"))
            .and_then(|resp| {
                resp.error_for_status()
                    .with_context(|| format!("streaming {key}"))
            })
        {
            Ok(resp) => resp,
            Err(err) => {
                self.inflight.lock().unwrap().remove(key);
                let _ = fs::remove_file(&part_path);
                // Anyone who joined between the reservation and this failure is
                // waiting on the condvar.
                progress.fail(format!("{err:#}"));
                return Err(err);
            }
        };
        let total = resp.content_length();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_string());
        tracing::debug!(
            key,
            status = %resp.status(),
            content_length = ?total,
            content_type = ?resp.headers().get(reqwest::header::CONTENT_TYPE),
            "streaming track"
        );

        // Now that the headers are in, publish them for readers that join from
        // here on; the file itself already exists from the reservation above.
        // Without the content type a joiner cannot tell that an mp3 is safe to
        // decode progressively, and waits out the whole download instead.
        if let Some(entry) = self.inflight.lock().unwrap().get_mut(key) {
            entry.total = total;
            entry.content_type = content_type.clone();
        }

        let writer = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&part_path)
            .with_context(|| format!("opening {} for writing", part_path.display()))?;
        let reader = File::open(&part_path)
            .with_context(|| format!("opening {}", part_path.display()))?;

        let this = self.clone();
        let key_owned = key.to_string();
        let progress_task = progress.clone();
        tokio::spawn(async move {
            let result = download(resp, writer, &progress_task).await;
            match result {
                Ok(()) => match fs::rename(&part_path, &final_path) {
                    Ok(()) => {
                        progress_task.finish();
                        if let Err(err) = this.prune() {
                            tracing::warn!(%err, "cache prune failed");
                        }
                    }
                    // Reported as a failure rather than a completion: a waiter
                    // woken by finish() goes on to open the final path, and
                    // failing there instead would surface as a mystery ENOENT
                    // far from the cause.
                    Err(err) => {
                        tracing::warn!(%err, key = %key_owned, "cannot finalise cache entry");
                        progress_task.fail(format!("cannot finalise cache entry: {err}"));
                    }
                },
                Err(err) => {
                    tracing::warn!(%err, key = %key_owned, "download failed");
                    // A truncated part file must not be mistaken for a cached track.
                    let _ = fs::remove_file(&part_path);
                    progress_task.fail(err.to_string());
                }
            }
            this.inflight.lock().unwrap().remove(&key_owned);
        });

        Ok(CacheReader {
            file: reader,
            pos: 0,
            total,
            progress: Some(progress),
            content_type,
        })
    }

    /// Drops least-recently-used entries until the cache fits in its budget.
    /// Partial downloads are never candidates.
    pub fn prune(&self) -> Result<()> {
        let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        let mut total = 0u64;

        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "part") {
                continue;
            }
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            total += meta.len();
            entries.push((path, meta.len(), meta.modified()?));
        }

        if total <= self.max_bytes {
            return Ok(());
        }

        entries.sort_by_key(|(_, _, used)| *used);
        for (path, len, _) in entries {
            if total <= self.max_bytes {
                break;
            }
            match fs::remove_file(&path) {
                Ok(()) => {
                    total = total.saturating_sub(len);
                    tracing::debug!(path = %path.display(), "pruned cache entry");
                }
                Err(err) => tracing::warn!(%err, path = %path.display(), "cannot prune"),
            }
        }
        Ok(())
    }
}

async fn download(resp: reqwest::Response, writer: File, progress: &Progress) -> Result<()> {
    use anyhow::bail;

    let mut file = tokio::fs::File::from_std(writer);
    let mut stream = resp.bytes_stream();
    let mut written = 0u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading response body")?;
        file.write_all(&chunk).await.context("writing cache file")?;

        // tokio's File buffers writes, so the bytes are not readable through a
        // second handle until they are flushed. Publishing a count the reader
        // cannot actually read yet makes the decoder see a premature EOF.
        file.flush().await.context("flushing cache file")?;

        written += chunk.len() as u64;
        progress.publish(written);
    }

    // A 200 with no body would otherwise be renamed into place and cached as a
    // permanently unplayable track.
    if written == 0 {
        bail!("server returned an empty body");
    }

    tracing::debug!(bytes = written, "download complete");
    Ok(())
}

/// Bumps mtime so pruning can treat it as a last-used timestamp. atime is not
/// usable for this: most of these file systems are mounted relatime or noatime.
fn touch(path: &PathBuf) -> io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_modified(std::time::SystemTime::now())
}

/// Blocking reader over a cache entry, complete or still downloading.
pub struct CacheReader {
    file: File,
    pos: u64,
    total: Option<u64>,
    /// None once the entry is fully cached, in which case reads never block.
    progress: Option<Arc<Progress>>,
    /// Content-Type of the response, when this reader came from a live download.
    content_type: Option<String>,
}

impl CacheReader {
    /// Length of the entry, when known. Complete entries always know it; a
    /// still-downloading one only does if the server sent a Content-Length.
    pub fn byte_len(&self) -> Option<u64> {
        self.total
    }

    /// True when the whole entry is on disk, so reads never block and the
    /// decoder may seek anywhere.
    pub fn is_complete(&self) -> bool {
        self.progress.is_none()
    }

    /// Whether this entry can be decoded before the download finishes.
    ///
    /// Only mp3 qualifies. Its demuxer reads the first frame and stops, so a
    /// forward-only probe succeeds, and the format is self-synchronising. Other
    /// containers probe by seeking - an MP4 `moov` atom may sit at the very end -
    /// and would fail or stall on a partial file.
    pub fn can_decode_while_downloading(&self) -> bool {
        matches!(
            self.content_type.as_deref(),
            Some("audio/mpeg" | "audio/mp3")
        )
    }
}

impl Read for CacheReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let limit = match &self.progress {
                Some(progress) => {
                    let available = progress.wait_for(self.pos + 1)?;
                    if available <= self.pos {
                        // Genuine EOF. Logged because a decoder reporting "end
                        // of stream" is otherwise indistinguishable from a
                        // corrupt file, and a short `total` points at the seek
                        // logic rather than the download.
                        tracing::debug!(
                            pos = self.pos,
                            available,
                            total = ?self.total,
                            "cache reader at end of stream"
                        );
                        return Ok(0);
                    }
                    (available - self.pos).min(buf.len() as u64) as usize
                }
                None => buf.len(),
            };

            // The file grows underneath us, so the offset is set explicitly per
            // read rather than relying on the handle's own cursor.
            self.file.seek(SeekFrom::Start(self.pos))?;
            let n = self.file.read(&mut buf[..limit])?;
            if n > 0 {
                self.pos += n as u64;
                return Ok(n);
            }

            // Zero bytes despite a non-zero limit means the writer's count is
            // ahead of what is visible in the file. Returning 0 here would look
            // like EOF to the decoder, so wait and retry instead.
            match &self.progress {
                Some(progress) if !progress.is_done() => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                _ => return Ok(0),
            }
        }
    }
}

impl Seek for CacheReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => {
                // Seeking from the end needs a known length. Content-Length
                // covers the streaming case; a complete file knows its size.
                let total = self
                    .total
                    .ok_or_else(|| io::Error::other("length unknown, cannot seek from end"))?;
                total as i64 + d
            }
        };

        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of file",
            ));
        }

        // Seeking past the download head is allowed; the next read blocks.
        self.pos = target as u64;
        Ok(self.pos)
    }
}
