//! Native YouTube resolution through the InnerTube API, in-process.
//!
//! The free-account engine resolves a song's title to a direct audio URL.
//! The primary path speaks YouTube's InnerTube API the way Google's own
//! YouTube app for visionOS does (the client yt-dlp itself prefers when no
//! JavaScript runtime is available): a search request to find the video,
//! then a player request for its stream, both plain HTTPS POSTs from this
//! process. Nothing is spawned and no JavaScript runs, so one resolve costs
//! a few milliseconds of CPU instead of a Python cold start; `yt-dlp`
//! stays as the fallback for the day YouTube rotates clients.
//!
//! The client identity below mirrors `INNERTUBE_CLIENTS['visionos']` in
//! yt-dlp's `yt_dlp/extractor/youtube/_base.py`; when resolves start being
//! refused, refresh the values there (or swap the client) and the engine
//! degrades to yt-dlp meanwhile.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

/// A direct audio URL and how long it is known to live.
pub(crate) struct Resolved {
    pub url: String,
    pub ttl: Duration,
}

/// The life given to URLs whose own lifetime is not known (the yt-dlp path
/// reports none).
pub(crate) const DEFAULT_URL_TTL: Duration = Duration::from_secs(30 * 60);

/// How long a URL without an `expire` parameter is assumed good for.
const ASSUMED_URL_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// Taken off the URL's own expiry, so a cached URL is never handed to mpv
/// while it is about to die.
const EXPIRE_MARGIN: Duration = Duration::from_secs(5 * 60);

/// A sanity cap on reported lifetimes (about as long as a URL ever lives).
const MAX_URL_TTL: Duration = Duration::from_secs(24 * 60 * 60);

// The VISIONOS client identity (see the module docs for where to refresh
// it). `CLIENT_ID` is InnerTube's own number for this client, sent as
// `X-YouTube-Client-Name`.
const CLIENT_NAME: &str = "VISIONOS";
const CLIENT_VERSION: &str = "1.02";
const CLIENT_ID: &str = "101";
const DEVICE_MAKE: &str = "Apple";
const DEVICE_MODEL: &str = "RealityDevice17,1";
const OS_NAME: &str = "visionOS";
const OS_VERSION: &str = "26.5.23O471";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 \
                          (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

const SEARCH_URL: &str = "https://www.youtube.com/youtubei/v1/search?prettyPrint=false";
const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";

/// How long one InnerTube request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// After this many resolves refused in a row, the session stays on the
/// fallback: a rotated client costs one refusal per listening session, not
/// one per song.
const FAILURES_BEFORE_FALLBACK: u32 = 2;

/// Streams from YouTube's InnerTube API without ever leaving this process.
pub(crate) struct NativeResolver {
    http: reqwest::Client,
    /// The visitor identity a response handed us, echoed back on the next
    /// requests (the player request needs one; a search response mints it).
    visitor: Mutex<Option<String>>,
    /// Consecutive failed resolves, to stop asking a refused client.
    failures: AtomicU32,
}

impl NativeResolver {
    pub(crate) fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("unable to build the YouTube HTTP client")?;
        Ok(Self {
            http,
            visitor: Mutex::new(None),
            failures: AtomicU32::new(0),
        })
    }

    /// One resolve: the title is searched, the best result's audio stream
    /// is asked for, and the direct URL comes back with its lifetime.
    pub(crate) async fn resolve(&self, query: &str) -> Result<Resolved> {
        if self.failures.load(Ordering::Relaxed) >= FAILURES_BEFORE_FALLBACK {
            return Err(anyhow!("the native client is disabled for this session"));
        }
        match self.try_resolve(query).await {
            Ok(stream) => {
                self.failures.store(0, Ordering::Relaxed);
                Ok(stream)
            }
            Err(error) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }

    async fn try_resolve(&self, query: &str) -> Result<Resolved> {
        let search = self
            .post(
                SEARCH_URL,
                &json!({ "context": { "client": self.client() }, "query": query }),
            )
            .await
            .context("the search was refused")?;
        self.remember_visitor(&search);
        let video = first_video_id(&search).ok_or_else(|| anyhow!("the search found no video"))?;
        let player = self
            .post(
                PLAYER_URL,
                &json!({
                    "context": { "client": self.client() },
                    "videoId": video,
                    "contentCheckOk": true,
                    "racyCheckOk": true,
                }),
            )
            .await
            .context("the player was refused")?;
        self.remember_visitor(&player);
        let status = player
            .get("playabilityStatus")
            .and_then(|status| status.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if status != "OK" {
            let reason = player
                .get("playabilityStatus")
                .and_then(|status| status.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(anyhow!("playability {status}: {reason}"));
        }
        pick_audio(&player).ok_or_else(|| anyhow!("no audio-only stream is offered"))
    }

    /// The client context of a request, with the visitor identity when one
    /// is known.
    fn client(&self) -> Value {
        let mut client = json!({
            "clientName": CLIENT_NAME,
            "clientVersion": CLIENT_VERSION,
            "deviceMake": DEVICE_MAKE,
            "deviceModel": DEVICE_MODEL,
            "userAgent": USER_AGENT,
            "osName": OS_NAME,
            "osVersion": OS_VERSION,
            "hl": "en",
            "gl": "US",
        });
        if let Some(visitor) = self.visitor() {
            client["visitorData"] = json!(visitor);
        }
        client
    }

    fn visitor(&self) -> Option<String> {
        self.visitor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Keeps the visitor identity a response minted, for the requests that
    /// follow.
    fn remember_visitor(&self, response: &Value) {
        let visitor = response
            .get("responseContext")
            .and_then(|context| context.get("visitorData"))
            .and_then(Value::as_str)
            .filter(|visitor| !visitor.is_empty());
        if let Some(visitor) = visitor {
            let mut held = self
                .visitor
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *held = Some(visitor.to_string());
        }
    }

    async fn post(&self, url: &str, body: &Value) -> Result<Value> {
        let mut request = self
            .http
            .post(url)
            .header("User-Agent", USER_AGENT)
            .header("X-YouTube-Client-Name", CLIENT_ID)
            .header("X-YouTube-Client-Version", CLIENT_VERSION)
            .json(body);
        if let Some(visitor) = self.visitor() {
            request = request.header("X-Goog-Visitor-Id", visitor);
        }
        let response = request.send().await.context("the request failed")?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .context("the response could not be read")?;
        if !status.is_success() {
            return Err(anyhow!("HTTP {status}"));
        }
        serde_json::from_slice(&body).context("the response was not JSON")
    }
}

/// The first video the search response names, walking it in document order.
/// Search results carry the video either as a `videoRenderer` or, in the
/// newer layout, as a lockup whose `contentId` is the video id.
fn first_video_id(response: &Value) -> Option<String> {
    match response {
        Value::Object(map) => {
            for key in ["videoId", "contentId"] {
                if let Some(id) = map.get(key).and_then(Value::as_str)
                    && id.len() == 11
                {
                    return Some(id.to_string());
                }
            }
            map.values().find_map(first_video_id)
        }
        Value::Array(items) => items.iter().find_map(first_video_id),
        _ => None,
    }
}

/// The best audio-only stream in a player response: m4a first, then the
/// other audio formats, and never a ciphered format without a direct URL.
fn pick_audio(response: &Value) -> Option<Resolved> {
    let formats = response
        .get("streamingData")?
        .get("adaptiveFormats")?
        .as_array()?;
    let audio = |itag: i64| {
        formats.iter().find(|format| {
            format.get("itag").and_then(Value::as_i64) == Some(itag)
                && format.get("url").and_then(Value::as_str).is_some()
                && format
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .is_some_and(|mime| mime.starts_with("audio/"))
        })
    };
    let format = [140_i64, 251, 139]
        .iter()
        .find_map(|itag| audio(*itag))
        .or_else(|| {
            formats.iter().find(|format| {
                format.get("url").and_then(Value::as_str).is_some()
                    && format
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .is_some_and(|mime| mime.starts_with("audio/"))
            })
        })?;
    let url = format.get("url")?.as_str()?.to_string();
    let ttl = url_ttl(&url);
    Some(Resolved { url, ttl })
}

/// How long a googlevideo URL lives, from its own signed `expire`
/// parameter, with a safety margin. A missing or unreadable parameter
/// falls back to the assumed life.
fn url_ttl(url: &str) -> Duration {
    let Some(query) = url.split_once('?').map(|(_, query)| query) else {
        return ASSUMED_URL_TTL;
    };
    let expire = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if key == "expire" {
            value.parse::<u64>().ok()
        } else {
            None
        }
    });
    let Some(expire) = expire else {
        return ASSUMED_URL_TTL;
    };
    let expires = UNIX_EPOCH + Duration::from_secs(expire);
    let remaining = expires
        .duration_since(SystemTime::now())
        .unwrap_or_default();
    remaining.saturating_sub(EXPIRE_MARGIN).min(MAX_URL_TTL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_result_names_the_video() {
        let response = json!({
            "contents": {
                "sectionListRenderer": {
                    "contents": [
                        { "itemSectionRenderer": { "contents": [
                            { "videoRenderer": { "videoId": "AAAAAAAAAAA" } },
                        ] } },
                        { "itemSectionRenderer": { "contents": [
                            { "videoRenderer": { "videoId": "BBBBBBBBBBB" } },
                        ] } },
                    ]
                }
            }
        });
        assert_eq!(first_video_id(&response).as_deref(), Some("AAAAAAAAAAA"));
    }

    #[test]
    fn a_lockup_result_names_the_video_too() {
        let response = json!({
            "items": [
                { "lockupViewModel": { "contentId": "CCCCCCCCCCC" } },
            ]
        });
        assert_eq!(first_video_id(&response).as_deref(), Some("CCCCCCCCCCC"));
    }

    #[test]
    fn the_m4a_audio_format_is_picked_first() {
        let response = json!({
            "streamingData": {
                "adaptiveFormats": [
                    { "itag": 137, "mimeType": "video/mp4", "url": "https://x/video" },
                    { "itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "url": "https://x/251" },
                    { "itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "url": "https://x/140" },
                    { "itag": 139, "mimeType": "audio/mp4; codecs=\"mp4a.40.5\"", "url": "https://x/139" },
                ]
            }
        });
        let stream = pick_audio(&response).expect("a stream");
        assert!(stream.url.ends_with("/140"), "{}", stream.url);
    }

    #[test]
    fn a_ciphered_format_is_skipped() {
        let response = json!({
            "streamingData": {
                "adaptiveFormats": [
                    { "itag": 140, "mimeType": "audio/mp4", "signatureCipher": "s=..." },
                    { "itag": 251, "mimeType": "audio/webm", "url": "https://x/251" },
                ]
            }
        });
        let stream = pick_audio(&response).expect("a stream");
        assert!(stream.url.ends_with("/251"), "{}", stream.url);
    }

    #[test]
    fn a_urls_life_is_read_from_its_expire_parameter() {
        let expire = (SystemTime::now() + Duration::from_secs(3600))
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs();
        let ttl = url_ttl(&format!("https://x/v?expire={expire}&foo=1"));
        assert!(
            ttl >= Duration::from_secs(3_200) && ttl <= Duration::from_secs(3_600),
            "an hour minus the margin: {ttl:?}"
        );
        assert_eq!(
            url_ttl("https://x/v?foo=1"),
            ASSUMED_URL_TTL,
            "no expire: the assumed life"
        );
        let almost = (SystemTime::now() + Duration::from_secs(60))
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs();
        assert_eq!(
            url_ttl(&format!("https://x/v?expire={almost}")),
            Duration::ZERO,
            "inside the margin is already stale"
        );
    }
}
