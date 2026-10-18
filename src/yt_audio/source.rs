//! A resolved stream URL read over HTTP ranges as a symphonia source.
//!
//! Symphonia drives this like an ordinary seekable file, but the wire
//! behavior is the CDN's: googlevideo delivers a few-megabyte audio stream
//! in a fast opening burst and punishes consumers that read at playback
//! pace (measured: a stream read on demand stalls after a hundred-odd
//! milliseconds of sound; the same URL drained eagerly arrives whole).
//! So each request hands its body to a background fetcher that drains it
//! at full speed into a buffer, the reader's position lives in that
//! buffer, and seeks inside the fetched window are free; only a seek
//! before the window starts a fresh ranged burst. The demuxer's index
//! search, which seeks in tight little jumps, costs nothing on the wire.
//! Failures carry a [`LinkFailure`] so the engine can tell an expired
//! link (a fresh resolve helps) from a bad connection (a retry might).

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, RANGE};
use symphonia::core::io::MediaSource;

/// How long the connection setup may take before a request gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a read may wait for the fetcher before the stream counts as
/// stalled. The blocking client has no read timeout of its own, so the
/// buffer's wait is where a dead connection is caught.
#[cfg(not(test))]
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const STALL_TIMEOUT: Duration = Duration::from_millis(500);

/// The most sound the fetcher keeps ahead of the reader, so a stream
/// longer than memory allows still streams.
const BUFFER_AHEAD: usize = 16 * 1024 * 1024;

/// Bytes fetched per read.
const FETCH_BLOCK: usize = 64 * 1024;

/// What the request names itself as. googlevideo serves a stalled, empty
/// answer to ranged requests without one (verified against a fresh URL
/// with curl: headers, then nothing), and reqwest sends no user agent by
/// default; this is the identity yt-dlp uses, which googlevideo serves.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0.0.0 Safari/537.36";

/// Why a link failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFailure {
    /// The signed link no longer works (401, 403, or 416): only a fresh
    /// resolve can fix it.
    Expired,
    /// Any other transport problem: retrying may help.
    Transport,
}

/// The source's own failure, carried inside `io::Error` so readers can
/// tell an expired link from a bad connection.
#[derive(Debug)]
pub struct LinkError {
    pub kind: LinkFailure,
    detail: String,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for LinkError {}

/// The link failure behind an `io::Error`, when the source named one.
pub fn link_failure(error: &io::Error) -> Option<LinkFailure> {
    error
        .get_ref()?
        .downcast_ref::<LinkError>()
        .map(|error| error.kind)
}

fn link(kind: LinkFailure, detail: String) -> io::Error {
    io::Error::other(LinkError { kind, detail })
}

/// A resolved stream URL read over HTTP ranges.
///
/// One pooled client per source; each request's body is drained by its own
/// fetcher thread into a buffer the reader seeks and reads through.
pub struct RangeSource {
    client: Client,
    url: String,
    inner: Mutex<Inner>,
}

struct Inner {
    /// The total length once known (resolver, Content-Range, or
    /// Content-Length).
    len: Option<u64>,
    /// The open body's buffered sound and its fetcher.
    feed: Option<Feed>,
    /// Where the next request opens when no feed is held.
    open_at: u64,
}

/// A body being drained in the background.
struct Feed {
    state: Arc<FeedState>,
    cancel: Arc<AtomicBool>,
}

impl Feed {
    /// Asks the fetcher to stop; it exits at its next block boundary (or
    /// when its connection finally errors).
    fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _data = self
            .state
            .data
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.state.ready.notify_all();
    }
}

struct FeedState {
    data: Mutex<FeedData>,
    ready: Condvar,
}

struct FeedData {
    /// The file offset of `buf`'s first byte (0 for a whole-file answer).
    start: u64,
    /// Bytes fetched and not yet discarded.
    buf: Vec<u8>,
    /// The reader's logical position in the file.
    pos: u64,
    /// The fetch finished; `failed` says why not.
    done: bool,
    failed: Option<(LinkFailure, String)>,
    /// Whether the body's first bytes were traced.
    head_logged: bool,
}

impl FeedData {
    /// How much of the fetched window the reader has already passed.
    fn consumed(&self) -> usize {
        let consumed = self.pos.saturating_sub(self.start);
        (consumed.min(self.buf.len() as u64)) as usize
    }
}

impl RangeSource {
    /// Builds a source. `len` is the resolver's known length when it had
    /// one; the first response fills it in otherwise.
    pub fn new(url: impl Into<String>, len: Option<u64>) -> io::Result<Self> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(USER_AGENT)
            // The shape mpv streams with; googlevideo's HTTP/2 handling
            // drops slow bodies where HTTP/1.1 streams steadily.
            .http1_only()
            .build()
            .map_err(|error| link(LinkFailure::Transport, format!("HTTP client: {error}")))?;
        Ok(Self {
            client,
            url: url.into(),
            inner: Mutex::new(Inner {
                len,
                feed: None,
                open_at: 0,
            }),
        })
    }

    /// Opens the stream so that a dead link surfaces before decoding
    /// starts, rather than as a first-packet failure.
    pub fn open(&self) -> io::Result<()> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        if inner.feed.is_none() {
            let at = inner.open_at;
            self.request(inner, at)?;
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Issues one ranged request at `start` and hands its body to a
    /// fetcher; the reader's position begins at `start` either way.
    fn request(&self, inner: &mut Inner, start: u64) -> io::Result<()> {
        // A ranged read, even from the start: a plain GET is the shape
        // googlevideo throttles and sometimes drops. The range travels as
        // the header alone; appending YouTube's `range=` query parameter
        // as well compounds the two (a request for byte 116 answered
        // `bytes 116-3704090/3704091`), which desynchronizes the stream
        // past the first request.
        let response = self
            .client
            .get(&self.url)
            .header(RANGE, format!("bytes={start}-"))
            .send()
            .map_err(|error| link(LinkFailure::Transport, format!("request failed: {error}")))?;
        log::debug!(
            "yt_audio: range {start}- answered {} {:?}",
            response.status().as_u16(),
            response.headers()
        );
        let status = response.status();
        if matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::RANGE_NOT_SATISFIABLE
        ) {
            return Err(link(
                LinkFailure::Expired,
                format!("the stream link answered {status}"),
            ));
        }
        if !status.is_success() {
            return Err(link(
                LinkFailure::Transport,
                format!("the stream answered {status}"),
            ));
        }
        let whole_file = status == StatusCode::OK;
        if let Some(total) = content_range_total(response.headers()) {
            inner.len = Some(total);
        } else if whole_file && let Some(length) = content_length(response.headers()) {
            inner.len = Some(length);
        }
        // A whole-file answer starts at byte zero however the request
        // asked; a ranged answer starts where it was asked from.
        let feed_start = if whole_file { 0 } else { start };
        if let Some(feed) = inner.feed.take() {
            feed.stop();
        }
        inner.open_at = start;
        inner.feed = Some(spawn_fetcher(response, feed_start, start));
        Ok(())
    }
}

impl Drop for RangeSource {
    fn drop(&mut self) {
        if let Some(feed) = self.lock().feed.take() {
            feed.stop();
        }
    }
}

/// Starts the body's background drain.
fn spawn_fetcher(mut response: Response, feed_start: u64, reader_at: u64) -> Feed {
    let state = Arc::new(FeedState {
        data: Mutex::new(FeedData {
            start: feed_start,
            buf: Vec::new(),
            pos: reader_at,
            done: false,
            failed: None,
            head_logged: false,
        }),
        ready: Condvar::new(),
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let fetch_state = Arc::clone(&state);
    let fetch_cancel = Arc::clone(&cancel);
    let spawned = thread::Builder::new()
        .name("yt-fetch".to_string())
        .spawn(move || {
            let mut block = vec![0u8; FETCH_BLOCK];
            loop {
                if fetch_cancel.load(Ordering::Relaxed) {
                    return;
                }
                // Keep no more than `BUFFER_AHEAD` ahead of the reader.
                {
                    let mut data = fetch_state
                        .data
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    while data.buf.len().saturating_sub(data.consumed()) > BUFFER_AHEAD
                        && !fetch_cancel.load(Ordering::Relaxed)
                    {
                        data = fetch_state
                            .ready
                            .wait(data)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                    if fetch_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                }
                match response.read(&mut block) {
                    Ok(0) => {
                        let mut data = fetch_state
                            .data
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        data.done = true;
                        fetch_state.ready.notify_all();
                        return;
                    }
                    Ok(read) => {
                        let mut data = fetch_state
                            .data
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        // Drop what the reader has passed before adding
                        // more, so a long stream does not grow forever.
                        let consumed = data.consumed();
                        if consumed == data.buf.len() {
                            data.buf.clear();
                            data.start += consumed as u64;
                        } else if consumed > BUFFER_AHEAD / 2 {
                            data.buf.drain(..consumed);
                            data.start += consumed as u64;
                        }
                        if !data.head_logged {
                            data.head_logged = true;
                            let head: Vec<String> = block[..read.min(16)]
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect();
                            log::debug!("yt_audio: the body's first bytes are {}", head.join(" "));
                        }
                        data.buf.extend_from_slice(&block[..read]);
                        drop(data);
                        fetch_state.ready.notify_all();
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let mut data = fetch_state
                            .data
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        data.done = true;
                        data.failed =
                            Some((LinkFailure::Transport, format!("read failed: {error}")));
                        fetch_state.ready.notify_all();
                        return;
                    }
                }
            }
        });
    if let Err(error) = &spawned {
        // A thread that cannot start leaves the feed to fail on first
        // read, which is where the caller is waiting anyway.
        log::warn!("yt_audio: fetcher did not start: {error}");
    }
    Feed { state, cancel }
}

impl Read for RangeSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let mut guard = self.lock();
            let inner = &mut *guard;
            if inner.feed.is_none() {
                let at = inner.open_at;
                self.request(inner, at)?;
                continue;
            }
            let Some(feed) = inner.feed.as_ref() else {
                continue;
            };
            let mut data = feed
                .state
                .data
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            loop {
                if data.pos < data.start {
                    // Seeks never leave the reader below the window; if
                    // this happens the source has a bug, not the stream.
                    return Err(link(
                        LinkFailure::Transport,
                        "the reader fell behind the fetched window".to_string(),
                    ));
                }
                let offset = (data.pos - data.start) as usize;
                if offset < data.buf.len() {
                    let take = (data.buf.len() - offset).min(buf.len());
                    buf[..take].copy_from_slice(&data.buf[offset..offset + take]);
                    data.pos += take as u64;
                    drop(data);
                    feed.state.ready.notify_all();
                    return Ok(take);
                }
                if let Some((kind, detail)) = data.failed.take() {
                    return Err(link(kind, detail));
                }
                if data.done {
                    return Ok(0);
                }
                let stalled = {
                    let (next, timeout) = feed
                        .state
                        .ready
                        .wait_timeout(data, STALL_TIMEOUT)
                        .unwrap_or_else(|poison| poison.into_inner());
                    data = next;
                    timeout.timed_out() && data.failed.is_none() && !data.done
                };
                if stalled && data.pos >= data.start {
                    let offset = (data.pos - data.start) as usize;
                    if offset >= data.buf.len() {
                        return Err(link(
                            LinkFailure::Transport,
                            "the stream stopped sending".to_string(),
                        ));
                    }
                }
            }
        }
    }
}

impl Seek for RangeSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let target = match from {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(delta) => {
                let current = match &inner.feed {
                    Some(feed) => {
                        feed.state
                            .data
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .pos
                    }
                    None => inner.open_at,
                };
                current
                    .checked_add_signed(delta)
                    .ok_or_else(seek_out_of_range)?
            }
            SeekFrom::End(delta) => inner
                .len
                .and_then(|len| len.checked_add_signed(delta))
                .ok_or_else(seek_out_of_range)?,
        };
        match &inner.feed {
            Some(feed)
                if target
                    >= feed
                        .state
                        .data
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .start =>
            {
                // Inside the fetched window: a buffer jump, nothing on the
                // wire.
                let mut data = feed
                    .state
                    .data
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                data.pos = target;
                drop(data);
                feed.state.ready.notify_all();
                Ok(target)
            }
            Some(_) => {
                // Before the window: a fresh ranged burst at the target.
                if let Some(feed) = inner.feed.take() {
                    feed.stop();
                }
                inner.open_at = target;
                Ok(target)
            }
            None => {
                inner.open_at = target;
                Ok(target)
            }
        }
    }
}

impl MediaSource for RangeSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.lock().len
    }
}

fn seek_out_of_range() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "seek out of range")
}

/// The total length in a `Content-Range` header (`bytes A-B/TOTAL`), unless
/// the server left it unknown (`*`).
fn content_range_total(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let total = value.rsplit('/').next()?.trim();
    if total == "*" {
        return None;
    }
    total.parse().ok()
}

/// The length in a `Content-Length` header.
fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};

    /// How the stub server answers.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Stub {
        /// Serves the requested slice with 206.
        Honoring,
        /// Answers every request with 200 and the whole file.
        Ignoring,
        /// Answers every request with 403.
        Forbidden,
        /// Sends headers, then holds the connection open without a body.
        Stalling,
    }

    /// A one-endpoint server: the URL, and the Range header of every
    /// request it received (`None` for requests without one).
    fn serve(mode: Stub, bytes: Vec<u8>) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let address = listener.local_addr().expect("local address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let bytes = Arc::new(bytes);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Some(range) = read_request(&mut stream) else {
                    continue;
                };
                log.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(range.clone());
                if mode == Stub::Forbidden {
                    let _ = stream.write_all(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                if mode == Stub::Stalling {
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{last}/{total}\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n",
                        last = bytes.len() - 1,
                        total = bytes.len(),
                    );
                    let _ = stream.write_all(head.as_bytes());
                    // Hold the connection open with no body.
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
                let start = range
                    .as_deref()
                    .and_then(|value| value.strip_prefix("bytes="))
                    .and_then(|value| value.split('-').next())
                    .and_then(|start| start.parse::<usize>().ok())
                    .unwrap_or(0)
                    .min(bytes.len());
                // A server that ignores ranges answers 200 with the whole
                // body, not with a slice, however the request asked.
                let slice: &[u8] = if mode == Stub::Ignoring {
                    &bytes[..]
                } else {
                    &bytes[start..]
                };
                let head = if mode == Stub::Honoring && start > 0 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{last}/{total}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
                        last = bytes.len() - 1,
                        total = bytes.len(),
                        length = slice.len(),
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",
                        length = slice.len(),
                    )
                };
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(slice);
            }
        });
        (format!("http://{address}/stream"), seen)
    }

    /// Reads one request's head; `None` means no request arrived.
    fn read_request(stream: &mut TcpStream) -> Option<Option<String>> {
        let mut buffer = [0u8; 16 * 1024];
        let mut filled = 0;
        loop {
            let read = stream.read(&mut buffer[filled..]).ok()?;
            if read == 0 {
                return None;
            }
            filled += read;
            let text = String::from_utf8_lossy(&buffer[..filled]);
            if let Some(end) = text.find("\r\n\r\n") {
                let head = &text[..end];
                return Some(head.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("range")
                        .then(|| value.trim().to_string())
                }));
            }
            if filled == buffer.len() {
                return Some(None);
            }
        }
    }

    const SIZE: usize = 4096;

    fn payload() -> Vec<u8> {
        (0..SIZE).map(|index| (index % 251) as u8).collect()
    }

    #[test]
    fn reads_return_the_bytes_in_order() {
        let bytes = payload();
        let (url, _seen) = serve(Stub::Honoring, bytes.clone());
        let mut source = RangeSource::new(url, None).expect("source");
        let mut collected = Vec::new();
        source.read_to_end(&mut collected).expect("read to end");
        assert_eq!(collected, bytes);
    }

    #[test]
    fn even_the_first_read_asks_for_a_range() {
        let bytes = payload();
        let (url, seen) = serve(Stub::Honoring, bytes.clone());
        let mut source = RangeSource::new(url, None).expect("source");
        source.open().expect("open");
        let mut byte = [0u8; 1];
        source.read_exact(&mut byte).expect("read");
        let asked = seen.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            asked
                .iter()
                .flatten()
                .any(|value| value.as_str() == "bytes=0-"),
            "the first request must be ranged: {asked:?}"
        );
    }

    #[test]
    fn a_forward_seek_is_served_from_the_buffer() {
        let bytes = payload();
        let (url, seen) = serve(Stub::Honoring, bytes.clone());
        let mut source = RangeSource::new(url, None).expect("source");
        source.open().expect("open");
        let target = source.seek(SeekFrom::Start(120)).expect("seek");
        assert_eq!(target, 120);
        let mut collected = Vec::new();
        source.read_to_end(&mut collected).expect("read");
        assert_eq!(collected, bytes[120..].to_vec());
        let asked = seen.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            asked.iter().flatten().count(),
            1,
            "a forward seek must not touch the wire: {asked:?}"
        );
    }

    #[test]
    fn a_relative_seek_lands_between_reads() {
        let bytes = payload();
        let (url, _seen) = serve(Stub::Honoring, bytes.clone());
        let mut source = RangeSource::new(url, None).expect("source");
        let mut head = [0u8; 10];
        source.read_exact(&mut head).expect("head");
        assert_eq!(head, bytes[..10]);
        let target = source.seek(SeekFrom::Current(90)).expect("seek");
        assert_eq!(target, 100);
        let mut collected = Vec::new();
        source.read_to_end(&mut collected).expect("read");
        assert_eq!(collected, bytes[100..].to_vec());
    }

    #[test]
    fn a_seek_before_the_fetched_window_asks_again() {
        let bytes = payload();
        let (url, seen) = serve(Stub::Honoring, bytes.clone());
        let mut source = RangeSource::new(url, None).expect("source");
        source.seek(SeekFrom::Start(2000)).expect("seek");
        let mut head = [0u8; 4];
        source.read_exact(&mut head).expect("head");
        assert_eq!(head, bytes[2000..2004]);
        let target = source.seek(SeekFrom::Start(100)).expect("seek");
        assert_eq!(target, 100);
        let mut collected = Vec::new();
        source.read_to_end(&mut collected).expect("read");
        assert_eq!(collected, bytes[100..].to_vec());
        let asked = seen.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            asked
                .iter()
                .flatten()
                .any(|value| value.as_str() == "bytes=2000-"),
            "{asked:?}"
        );
        assert!(
            asked
                .iter()
                .flatten()
                .any(|value| value.as_str() == "bytes=100-"),
            "{asked:?}"
        );
    }

    #[test]
    fn a_server_ignoring_ranges_is_absorbed() {
        let bytes = payload();
        let (url, _seen) = serve(Stub::Ignoring, bytes.clone());
        let mut source = RangeSource::new(url, None).expect("source");
        source.seek(SeekFrom::Start(1000)).expect("seek");
        let mut collected = Vec::new();
        source.read_to_end(&mut collected).expect("read");
        assert_eq!(collected, bytes[1000..].to_vec());
    }

    #[test]
    fn a_revoked_link_reports_expired() {
        let (url, _seen) = serve(Stub::Forbidden, payload());
        let source = RangeSource::new(url, None).expect("source");
        let error = source.open().expect_err("open must fail");
        assert_eq!(link_failure(&error), Some(LinkFailure::Expired));
    }

    #[test]
    fn byte_len_learns_the_total_length() {
        let bytes = payload();
        let (url, _seen) = serve(Stub::Honoring, bytes.clone());
        let source = RangeSource::new(url, None).expect("source");
        assert_eq!(source.byte_len(), None);
        source.open().expect("open");
        assert_eq!(source.byte_len(), Some(bytes.len() as u64));
    }

    #[test]
    fn seeking_from_the_end_uses_the_learned_length() {
        let bytes = payload();
        let (url, _seen) = serve(Stub::Honoring, bytes.clone());
        let mut source = RangeSource::new(url, None).expect("source");
        source.open().expect("open");
        let target = source.seek(SeekFrom::End(-10)).expect("seek");
        assert_eq!(target, bytes.len() as u64 - 10);
        let mut tail = Vec::new();
        source.read_to_end(&mut tail).expect("read");
        assert_eq!(tail, bytes[bytes.len() - 10..].to_vec());
    }

    #[test]
    fn seeking_from_the_end_without_a_length_is_refused() {
        let (url, _seen) = serve(Stub::Honoring, payload());
        let mut source = RangeSource::new(url, None).expect("source");
        let error = source.seek(SeekFrom::End(0)).expect_err("seek must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_body_that_stops_sending_fails_instead_of_hanging() {
        let (url, _seen) = serve(Stub::Stalling, payload());
        let mut source = RangeSource::new(url, None).expect("source");
        source.open().expect("open");
        let mut byte = [0u8; 4];
        let started = std::time::Instant::now();
        let error = source.read_exact(&mut byte).expect_err("must not hang");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the stall took {:?}",
            started.elapsed()
        );
        assert_eq!(link_failure(&error), Some(LinkFailure::Transport));
    }
}
