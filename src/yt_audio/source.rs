//! A resolved stream URL read over HTTP ranges as a symphonia source.
//!
//! Symphonia drives this like an ordinary seekable file. Reads pull the
//! body forward; seeks drop it and ask googlevideo for a later range (the
//! server serves ranged requests; mpv seeks the same way). A server that
//! ignores a range request is absorbed by discarding the bytes from the
//! start, so callers never see wrong data. Failures carry a [`LinkFailure`]
//! so the engine can tell an expired link (a fresh resolve helps) from a
//! bad connection (a retry might).

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, RANGE};
use symphonia::core::io::MediaSource;

/// How long the connection setup may take before a request gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Bytes discarded per read while absorbing a range-ignoring server.
const SKIP_CHUNK: usize = 32 * 1024;

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
/// One pooled client per source keeps seek connections cheap (they are the
/// dominant cost of a seek); the open body is dropped on a seek and the
/// next read opens a fresh ranged request at the target.
pub struct RangeSource {
    client: Client,
    url: String,
    inner: Mutex<Inner>,
}

struct Inner {
    /// Logical position in the stream: where the next read starts.
    pos: u64,
    /// The total length once known (resolver, Content-Range, or
    /// Content-Length).
    len: Option<u64>,
    /// The open response body, opened lazily at `pos`.
    body: Option<Response>,
    /// Bytes that a range-ignoring server sent from the start of the file
    /// and that still have to be discarded before the logical position is
    /// reached.
    pending_skip: u64,
}

impl RangeSource {
    /// Builds a source. `len` is the resolver's known length when it had
    /// one; the first response fills it in otherwise.
    pub fn new(url: impl Into<String>, len: Option<u64>) -> io::Result<Self> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|error| link(LinkFailure::Transport, format!("HTTP client: {error}")))?;
        Ok(Self {
            client,
            url: url.into(),
            inner: Mutex::new(Inner {
                pos: 0,
                len,
                body: None,
                pending_skip: 0,
            }),
        })
    }

    /// Opens the stream so that a dead link surfaces before decoding
    /// starts, rather than as a first-packet failure.
    pub fn open(&self) -> io::Result<()> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        if inner.body.is_none() {
            let pos = inner.pos;
            self.request(inner, pos)?;
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Issues one request at `start` and keeps its body.
    fn request(&self, inner: &mut Inner, start: u64) -> io::Result<()> {
        let mut request = self.client.get(&self.url);
        if start > 0 {
            request = request.header(RANGE, format!("bytes={start}-"));
        }
        let response = request
            .send()
            .map_err(|error| link(LinkFailure::Transport, format!("request failed: {error}")))?;
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
        inner.pos = start;
        inner.pending_skip = if whole_file { start } else { 0 };
        inner.body = Some(response);
        Ok(())
    }
}

impl Read for RangeSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let mut guard = self.lock();
            let inner = &mut *guard;
            if inner.body.is_none() {
                let pos = inner.pos;
                self.request(inner, pos)?;
                continue;
            }
            if inner.pending_skip > 0 {
                let mut scratch = [0u8; SKIP_CHUNK];
                let want = scratch.len().min(inner.pending_skip as usize);
                let Some(body) = inner.body.as_mut() else {
                    continue;
                };
                match body.read(&mut scratch[..want]) {
                    Ok(0) => return Ok(0),
                    Ok(read) => {
                        inner.pending_skip -= read as u64;
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        return Err(link(
                            LinkFailure::Transport,
                            format!("read failed: {error}"),
                        ));
                    }
                }
            }
            let Some(body) = inner.body.as_mut() else {
                continue;
            };
            match body.read(buf) {
                Ok(0) => return Ok(0),
                Ok(read) => {
                    inner.pos += read as u64;
                    return Ok(read);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(link(
                        LinkFailure::Transport,
                        format!("read failed: {error}"),
                    ));
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
            SeekFrom::Current(delta) => inner
                .pos
                .checked_add_signed(delta)
                .ok_or_else(seek_out_of_range)?,
            SeekFrom::End(delta) => inner
                .len
                .and_then(|len| len.checked_add_signed(delta))
                .ok_or_else(seek_out_of_range)?,
        };
        // The open body is dropped; the next read opens a ranged request at
        // the target.
        inner.body = None;
        inner.pending_skip = 0;
        inner.pos = target;
        Ok(target)
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
    use std::sync::Arc;
    use std::thread;

    /// How the stub server answers.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Stub {
        /// Serves the requested slice with 206.
        Honoring,
        /// Answers every request with 200 and the whole file.
        Ignoring,
        /// Answers every request with 403.
        Forbidden,
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
    fn seeking_reopens_with_a_range_request() {
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
        assert!(
            asked
                .iter()
                .flatten()
                .any(|value| value.as_str() == "bytes=120-"),
            "expected a range request at 120: {asked:?}"
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
}
