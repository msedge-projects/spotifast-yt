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
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A direct audio URL and everything known about that stream.
pub(crate) struct Resolved {
    pub url: String,
    pub ttl: Duration,
    /// The format's own mime type when the player response named one.
    pub mime: Option<String>,
    /// The stream's byte length when known.
    pub content_length: Option<u64>,
    /// The next-best candidates, best first, for a stream this engine
    /// cannot open (each carries no fallbacks of its own).
    pub fallbacks: Vec<Resolved>,
}

/// One YouTube Music search result: what a row needs to draw and play.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YtTrack {
    pub video_id: String,
    pub title: String,
    pub artists: String,
    pub album: String,
    pub duration_ms: u64,
    pub art_url: Option<String>,
    /// An official "art track": the song's own audio, not a user video.
    pub official_audio: bool,
}

impl YtTrack {
    /// The Spotify-shaped row this result draws as: a `yt:<video id>` URI
    /// with the title, artist, duration, and artwork a row draws and the
    /// engine plays by.
    pub fn to_row(&self) -> crate::api::models::Track {
        let mut images = Vec::new();
        if let Some(url) = &self.art_url {
            images.push(crate::api::models::Image {
                url: url.clone(),
                ..Default::default()
            });
        }
        let artists = if self.artists.is_empty() {
            Vec::new()
        } else {
            vec![crate::api::models::ArtistRef {
                name: self.artists.clone(),
                ..Default::default()
            }]
        };
        crate::api::models::Track {
            id: Some(self.video_id.clone()),
            name: self.title.clone(),
            uri: format!("yt:{}", self.video_id),
            duration_ms: self.duration_ms.min(u64::from(u32::MAX)) as u32,
            artists,
            album: Some(crate::api::models::Album {
                name: self.album.clone(),
                images,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

/// One YouTube Music artist result: enough to draw its card and open its
/// page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YtArtist {
    pub browse_id: String,
    pub name: String,
    pub art_url: Option<String>,
}

impl YtArtist {
    /// The Spotify-shaped artist the interface draws and opens.
    pub fn to_row(&self) -> crate::api::models::Artist {
        let mut images = Vec::new();
        if let Some(url) = &self.art_url {
            images.push(crate::api::models::Image {
                url: url.clone(),
                ..Default::default()
            });
        }
        crate::api::models::Artist {
            id: self.browse_id.clone(),
            name: self.name.clone(),
            uri: format!("spotify:artist:{}", self.browse_id),
            images,
            ..Default::default()
        }
    }
}

/// One YouTube Music album card or album page: enough to draw its card and
/// open its page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YtAlbum {
    pub browse_id: String,
    pub name: String,
    pub artist: String,
    pub year: Option<String>,
    pub art_url: Option<String>,
}

impl YtAlbum {
    /// The Spotify-shaped album the interface draws and opens.
    pub fn to_row(&self) -> crate::api::models::Album {
        let mut images = Vec::new();
        if let Some(url) = &self.art_url {
            images.push(crate::api::models::Image {
                url: url.clone(),
                ..Default::default()
            });
        }
        let artists = if self.artist.is_empty() {
            Vec::new()
        } else {
            vec![crate::api::models::ArtistRef {
                name: self.artist.clone(),
                ..Default::default()
            }]
        };
        crate::api::models::Album {
            id: self.browse_id.clone(),
            name: self.name.clone(),
            uri: format!("spotify:album:{}", self.browse_id),
            album_type: Some("album".to_string()),
            release_date: self.year.clone(),
            images,
            artists,
            ..Default::default()
        }
    }
}

/// One YouTube Music playlist: enough to draw its card and open its page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct YtPlaylist {
    pub browse_id: String,
    pub name: String,
    /// The line under the name: its owner or a description.
    pub owner: String,
    pub art_url: Option<String>,
}

impl YtPlaylist {
    /// The Spotify-shaped playlist the interface draws and opens.
    pub fn to_row(&self) -> crate::api::models::Playlist {
        let mut images = Vec::new();
        if let Some(url) = &self.art_url {
            images.push(crate::api::models::Image {
                url: url.clone(),
                ..Default::default()
            });
        }
        let owner = if self.owner.is_empty() {
            crate::api::models::Owner::default()
        } else {
            crate::api::models::Owner {
                display_name: Some(self.owner.clone()),
                ..Default::default()
            }
        };
        crate::api::models::Playlist {
            id: self.browse_id.clone(),
            name: self.name.clone(),
            uri: format!("spotify:playlist:{}", self.browse_id),
            images,
            owner,
            ..Default::default()
        }
    }
}

/// What one YouTube Music search answered.
#[derive(Clone, Debug, Default)]
pub struct MusicSearch {
    pub tracks: Vec<YtTrack>,
    pub artists: Vec<YtArtist>,
    pub albums: Vec<YtAlbum>,
    pub playlists: Vec<YtPlaylist>,
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

/// YouTube Music's own search: results carry artists, album, duration,
/// and thumbnails, which the plain video search does not.
const MUSIC_SEARCH_URL: &str = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
/// YouTube Music's browse: an album's own page, by its browse id.
const MUSIC_BROWSE_URL: &str = "https://music.youtube.com/youtubei/v1/browse?prettyPrint=false";
const MUSIC_CLIENT_NAME: &str = "WEB_REMIX";
const MUSIC_CLIENT_VERSION: &str = "1.20260707.12.00";
const MUSIC_CLIENT_ID: &str = "67";

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

    /// One resolve for a video named directly, by id (a locally stored
    /// track), with the same failure handling as a search resolve.
    pub(crate) async fn resolve_video(&self, video_id: &str) -> Result<Resolved> {
        if self.failures.load(Ordering::Relaxed) >= FAILURES_BEFORE_FALLBACK {
            return Err(anyhow!("the native client is disabled for this session"));
        }
        match self.player(video_id).await {
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

    /// YouTube Music search: music-typed tracks with their artists, album,
    /// duration, and thumbnail, and the artists and albums the response
    /// names. An answer with no rows at all is asked once more: YouTube's
    /// gates are per request, and the very next ask usually answers.
    pub async fn search_music(&self, query: &str) -> Result<MusicSearch> {
        let found = self.search_music_once(query).await?;
        if !found.tracks.is_empty()
            || !found.artists.is_empty()
            || !found.albums.is_empty()
            || !found.playlists.is_empty()
        {
            return Ok(found);
        }
        log::info!(
            "YouTube: the search for \u{201c}{query}\u{201d} answered no rows; asking once more"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        self.search_music_once(query).await
    }

    async fn search_music_once(&self, query: &str) -> Result<MusicSearch> {
        let response = self
            .post(
                MUSIC_SEARCH_URL,
                MUSIC_CLIENT_ID,
                MUSIC_CLIENT_VERSION,
                Some("https://music.youtube.com"),
                &json!({
                    "context": { "client": {
                        "clientName": MUSIC_CLIENT_NAME,
                        "clientVersion": MUSIC_CLIENT_VERSION,
                        "hl": "en",
                        "gl": "US",
                    } },
                    "query": query,
                }),
            )
            .await
            .context("the music search was refused")?;
        self.remember_visitor(&response);
        let found = MusicSearch {
            tracks: music_items(&response),
            artists: music_artists(&response),
            albums: music_albums(&response),
            playlists: music_playlists(&response),
        };
        if found.tracks.is_empty()
            && found.artists.is_empty()
            && found.albums.is_empty()
            && found.playlists.is_empty()
        {
            // A gate page parses to nothing; its own keys say which one it
            // was, should this ever become a habit.
            let keys: Vec<&str> = response
                .as_object()
                .map(|map| map.keys().map(String::as_str).take(12).collect())
                .unwrap_or_default();
            log::info!("YouTube: an empty search response held {keys:?}");
        }
        Ok(found)
    }

    /// One album page: its header and its songs, straight from the album's
    /// own browse id. An empty page is asked once more, like a search.
    pub async fn album_browse(&self, browse_id: &str) -> Result<(YtAlbum, Vec<YtTrack>)> {
        let page = self.album_browse_once(browse_id).await?;
        if !page.0.name.is_empty() || !page.1.is_empty() {
            return Ok(page);
        }
        log::info!("YouTube: the album page {browse_id} answered nothing; asking once more");
        tokio::time::sleep(Duration::from_millis(250)).await;
        self.album_browse_once(browse_id).await
    }

    async fn album_browse_once(&self, browse_id: &str) -> Result<(YtAlbum, Vec<YtTrack>)> {
        let response = self
            .post(
                MUSIC_BROWSE_URL,
                MUSIC_CLIENT_ID,
                MUSIC_CLIENT_VERSION,
                Some("https://music.youtube.com"),
                &json!({
                    "context": { "client": {
                        "clientName": MUSIC_CLIENT_NAME,
                        "clientVersion": MUSIC_CLIENT_VERSION,
                        "hl": "en",
                        "gl": "US",
                    } },
                    "browseId": browse_id,
                }),
            )
            .await
            .context("the album page was refused")?;
        self.remember_visitor(&response);
        Ok((album_header(browse_id, &response), music_items(&response)))
    }

    /// One playlist page: its header and its first songs (YouTube answers
    /// the first hundred rows in one browse). An empty page is asked once
    /// more, like a search.
    pub async fn playlist_browse(&self, browse_id: &str) -> Result<(YtPlaylist, Vec<YtTrack>)> {
        let page = self.playlist_browse_once(browse_id).await?;
        if !page.0.name.is_empty() || !page.1.is_empty() {
            return Ok(page);
        }
        log::info!("YouTube: the playlist page {browse_id} answered nothing; asking once more");
        tokio::time::sleep(Duration::from_millis(250)).await;
        self.playlist_browse_once(browse_id).await
    }

    async fn playlist_browse_once(&self, browse_id: &str) -> Result<(YtPlaylist, Vec<YtTrack>)> {
        let response = self
            .post(
                MUSIC_BROWSE_URL,
                MUSIC_CLIENT_ID,
                MUSIC_CLIENT_VERSION,
                Some("https://music.youtube.com"),
                &json!({
                    "context": { "client": {
                        "clientName": MUSIC_CLIENT_NAME,
                        "clientVersion": MUSIC_CLIENT_VERSION,
                        "hl": "en",
                        "gl": "US",
                    } },
                    "browseId": browse_id,
                }),
            )
            .await
            .context("the playlist page was refused")?;
        self.remember_visitor(&response);
        Ok((
            playlist_header(browse_id, &response),
            music_items(&response),
        ))
    }

    async fn try_resolve(&self, query: &str) -> Result<Resolved> {
        let search = self
            .post(
                SEARCH_URL,
                CLIENT_ID,
                CLIENT_VERSION,
                None,
                &json!({ "context": { "client": self.client() }, "query": query }),
            )
            .await
            .context("the search was refused")?;
        self.remember_visitor(&search);
        let video = first_video_id(&search).ok_or_else(|| anyhow!("the search found no video"))?;
        self.player(&video).await
    }

    /// One video's own length, from its player response's details. Used to
    /// fill lengths the search answer never carried.
    pub async fn video_duration_ms(&self, video_id: &str) -> Result<u64> {
        let player = self
            .post(
                PLAYER_URL,
                CLIENT_ID,
                CLIENT_VERSION,
                None,
                &json!({
                    "context": { "client": self.client() },
                    "videoId": video_id,
                    "contentCheckOk": true,
                    "racyCheckOk": true,
                }),
            )
            .await
            .context("the video was refused")?;
        self.remember_visitor(&player);
        let seconds = player
            .get("videoDetails")
            .and_then(|details| details.get("lengthSeconds"))
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<u64>().ok())
            .context("the video has no length")?;
        Ok(seconds.saturating_mul(1000))
    }

    /// One video's audio URL: the player request and the format pick, with
    /// one immediate retry. YouTube hands its "confirm you are not a bot"
    /// gate to the request, not to the client: the very next ask for the
    /// same video usually passes, and the retry keeps a gated request from
    /// costing the slow `yt-dlp` path.
    async fn player(&self, video: &str) -> Result<Resolved> {
        match self.player_once(video).await {
            Ok(stream) => Ok(stream),
            Err(first) => {
                log::info!("YouTube: the player request was refused ({first:#}); asking once more");
                tokio::time::sleep(Duration::from_millis(250)).await;
                self.player_once(video)
                    .await
                    .with_context(|| format!("the retry failed after: {first:#}"))
            }
        }
    }

    /// One player request and the format pick.
    async fn player_once(&self, video: &str) -> Result<Resolved> {
        let player = self
            .post(
                PLAYER_URL,
                CLIENT_ID,
                CLIENT_VERSION,
                None,
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

    async fn post(
        &self,
        url: &str,
        client_id: &str,
        client_version: &str,
        origin: Option<&str>,
        body: &Value,
    ) -> Result<Value> {
        let mut request = self
            .http
            .post(url)
            .header("User-Agent", USER_AGENT)
            .header("X-YouTube-Client-Name", client_id)
            .header("X-YouTube-Client-Version", client_version)
            .json(body);
        if let Some(origin) = origin {
            request = request
                .header("Origin", origin)
                .header("Referer", format!("{origin}/"));
        }
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

/// Every music result of a YouTube Music search response, in the order the
/// response lists them.
fn music_items(response: &Value) -> Vec<YtTrack> {
    let mut items = Vec::new();
    collect_music_items(response, &mut items);
    items
}

fn collect_music_items(value: &Value, items: &mut Vec<YtTrack>) {
    match value {
        Value::Object(map) => {
            if let Some(item) = map
                .get("musicResponsiveListItemRenderer")
                .and_then(music_item)
            {
                items.push(item);
            }
            for child in map.values() {
                collect_music_items(child, items);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_music_items(child, items);
            }
        }
        _ => {}
    }
}

/// One search result's fields, from the renderer YouTube Music answers
/// with: the title and the video id in the first flex column, the artists,
/// album, and duration in the second, and the thumbnail beside them.
fn music_item(renderer: &Value) -> Option<YtTrack> {
    let columns = renderer.get("flexColumns")?.as_array()?;
    let title_runs = music_column_texts(columns, 0);
    let title = title_runs.first()?.trim();
    let video_id = music_video_id(columns)?;
    let subtitle = music_column_texts(columns, 1);
    let subtitle = music_subtitle(&subtitle);
    let artists = subtitle.artists;
    let mut duration_ms = subtitle.duration_ms;
    let album = subtitle.album;
    if duration_ms == 0 {
        // YouTube sometimes keeps the length in the row's fixed column
        // (its right-aligned one) instead of the subtitle.
        duration_ms = music_fixed_text(renderer)
            .and_then(music_duration_ms)
            .unwrap_or(0);
    }
    let official_audio = renderer
        .get("flexColumns")
        .and_then(Value::as_array)
        .and_then(|columns| columns.first())
        .and_then(|column| column.get("musicResponsiveListItemFlexColumnRenderer"))
        .and_then(|column| column.get("text"))
        .and_then(|text| text.get("runs"))
        .and_then(Value::as_array)
        .and_then(|runs| runs.first())
        .and_then(|run| run.get("navigationEndpoint"))
        .and_then(|endpoint| endpoint.get("watchEndpoint"))
        .and_then(|player| player.get("watchEndpointMusicSupportedConfigs"))
        .and_then(|configs| configs.get("watchEndpointMusicConfig"))
        .and_then(|config| config.get("musicVideoType"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "MUSIC_VIDEO_TYPE_ATV");
    let art_url = renderer
        .get("thumbnail")
        .and_then(|thumbnail| thumbnail.get("musicThumbnailRenderer"))
        .and_then(|thumbnail| thumbnail.get("thumbnail"))
        .and_then(|thumbnail| thumbnail.get("thumbnails"))
        .and_then(Value::as_array)
        .and_then(|thumbnails| thumbnails.last())
        .and_then(|thumbnail| thumbnail.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(YtTrack {
        video_id: video_id.to_string(),
        title: title.to_string(),
        artists: artists.to_string(),
        album: album.to_string(),
        duration_ms,
        art_url,
        official_audio,
    })
}

/// Whether a subtitle run is the row's item kind rather than a name. The
/// kind run can end in a non-breaking space, so it is trimmed first.
fn is_music_kind(text: &str) -> bool {
    matches!(
        text.trim(),
        "Song" | "Video" | "Single" | "EP" | "Album" | "Playlist" | "Artist" | "Podcast"
    )
}

/// The first text of a row's fixed column (its right-aligned one), where
/// YouTube often keeps the length the subtitle left out.
fn music_fixed_text(renderer: &Value) -> Option<&str> {
    let text = renderer
        .get("fixedColumns")?
        .as_array()?
        .first()?
        .get("musicResponsiveListItemFixedColumnRenderer")?
        .get("text")?;
    text.get("runs")
        .and_then(Value::as_array)
        .and_then(|runs| runs.first())
        .and_then(|run| run.get("text"))
        .and_then(Value::as_str)
        .or_else(|| text.get("text").and_then(Value::as_str))
}

/// The texts of one flex column of a music row, with empty runs dropped;
/// the runs keep their own spacing, because YouTube splits artist names
/// into ", " and " & " separator runs the joined text needs.
fn music_column_texts(columns: &[Value], index: usize) -> Vec<&str> {
    columns
        .get(index)
        .and_then(|column| column.get("musicResponsiveListItemFlexColumnRenderer"))
        .and_then(|column| column.get("text"))
        .and_then(|text| text.get("runs"))
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|run| run.get("text").and_then(Value::as_str))
                .filter(|text| !text.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// A song row's subtitle taken apart: the kind dropped, the artists' runs
/// joined (their names arrive split by ", " and " & " runs), the album, the
/// length, and view counts separated by " • ".
struct MusicSubtitle {
    artists: String,
    album: String,
    duration_ms: u64,
}

fn music_subtitle(tokens: &[&str]) -> MusicSubtitle {
    let mut segments: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for token in tokens {
        if token.trim() == "•" {
            segments.push(std::mem::take(&mut current));
        } else {
            current.push(token);
        }
    }
    segments.push(current);
    // One pass over the segments: the kind and a year pass through, a
    // length is the length, the first name is the artists', the next is
    // the album. A row whose artist YouTube omitted leads with its length,
    // and a length must never squat in the artist slot nor lose its own.
    let mut artists = String::new();
    let mut album = String::new();
    let mut duration_ms = 0;
    for segment in &segments {
        let text = segment.concat();
        let text = text.trim();
        if text.is_empty() || text.ends_with("views") || is_music_kind(text) {
            continue;
        }
        if let Some(ms) = music_duration_ms(text) {
            duration_ms = ms;
            continue;
        }
        if is_release_year(text) {
            continue;
        }
        if artists.is_empty() {
            artists = text.to_string();
        } else if album.is_empty() {
            album = text.to_string();
        }
    }
    MusicSubtitle {
        artists,
        album,
        duration_ms,
    }
}

/// Whether a subtitle segment is a release year rather than a name.
fn is_release_year(text: &str) -> bool {
    text.len() == 4 && text.chars().all(|character| character.is_ascii_digit())
}

/// The video the first flex column names: the title's own link.
fn music_video_id(columns: &[Value]) -> Option<&str> {
    columns
        .first()?
        .get("musicResponsiveListItemFlexColumnRenderer")?
        .get("text")?
        .get("runs")?
        .as_array()?
        .first()?
        .get("navigationEndpoint")?
        .get("watchEndpoint")?
        .get("videoId")?
        .as_str()
}

/// A "3:53" or "1:02:03" text as milliseconds. A row may pack its whole
/// subtitle into one run ("Artist • Album • 3:53"), so only the text's
/// last token is read; anything else, like a view count, is none.
fn music_duration_ms(text: &str) -> Option<u64> {
    let last_token = text
        .rsplit([' ', '\u{a0}', '•'])
        .find(|token| !token.is_empty())?;
    let parts: Vec<&str> = last_token.split(':').collect();
    let seconds = match parts.as_slice() {
        [minutes, seconds] => {
            let minutes: u64 = minutes.trim().parse().ok()?;
            let seconds: u64 = seconds.trim().parse().ok()?;
            if seconds >= 60 {
                return None;
            }
            minutes.saturating_mul(60).saturating_add(seconds)
        }
        [hours, minutes, seconds] => {
            let hours: u64 = hours.trim().parse().ok()?;
            let minutes: u64 = minutes.trim().parse().ok()?;
            let seconds: u64 = seconds.trim().parse().ok()?;
            if minutes >= 60 || seconds >= 60 {
                return None;
            }
            hours
                .saturating_mul(60)
                .saturating_add(minutes)
                .saturating_mul(60)
                .saturating_add(seconds)
        }
        _ => return None,
    };
    (seconds > 0).then_some(seconds.saturating_mul(1000))
}

/// Every artist the search response names, in order, from the artist cards
/// (the top card and the two-row items) whose page is an artist page.
fn music_artists(response: &Value) -> Vec<YtArtist> {
    let mut artists = Vec::new();
    collect_music_artists(response, &mut artists);
    artists
}

fn collect_music_artists(value: &Value, artists: &mut Vec<YtArtist>) {
    match value {
        Value::Object(map) => {
            for key in ["musicCardShelfRenderer", "musicTwoRowItemRenderer"] {
                if let Some(renderer) = map.get(key)
                    && let Some(artist) = music_artist(renderer)
                    && !artists
                        .iter()
                        .any(|held| held.browse_id == artist.browse_id)
                {
                    artists.push(artist);
                }
            }
            for child in map.values() {
                collect_music_artists(child, artists);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_music_artists(child, artists);
            }
        }
        _ => {}
    }
}

/// One card's fields when its title links to an artist page: the name, the
/// channel the page reads, and the artwork beside them.
fn music_artist(renderer: &Value) -> Option<YtArtist> {
    let run = renderer.get("title")?.get("runs")?.as_array()?.first()?;
    let name = run.get("text")?.as_str()?;
    let browse = run.get("navigationEndpoint")?.get("browseEndpoint")?;
    let artist_page = browse
        .get("browseEndpointContextSupportedConfigs")
        .and_then(|configs| configs.get("browseEndpointContextMusicConfig"))
        .and_then(|config| config.get("pageType"))
        .and_then(Value::as_str)
        .is_some_and(|page| page == "MUSIC_PAGE_TYPE_ARTIST");
    if !artist_page {
        return None;
    }
    let browse_id = browse.get("browseId")?.as_str()?;
    let art_url = renderer
        .get("thumbnail")
        .and_then(|thumbnail| thumbnail.get("musicThumbnailRenderer"))
        .and_then(|thumbnail| thumbnail.get("thumbnail"))
        .and_then(|thumbnail| thumbnail.get("thumbnails"))
        .and_then(Value::as_array)
        .and_then(|thumbnails| thumbnails.last())
        .and_then(|thumbnail| thumbnail.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(YtArtist {
        browse_id: browse_id.to_string(),
        name: name.to_string(),
        art_url,
    })
}

/// Every album the search response names, from the album cards whose page
/// is an album page.
fn music_albums(response: &Value) -> Vec<YtAlbum> {
    let mut albums = Vec::new();
    collect_music_albums(response, &mut albums);
    albums
}

fn collect_music_albums(value: &Value, albums: &mut Vec<YtAlbum>) {
    match value {
        Value::Object(map) => {
            for key in ["musicCardShelfRenderer", "musicTwoRowItemRenderer"] {
                if let Some(renderer) = map.get(key)
                    && let Some(album) = music_album(renderer)
                    && !albums.iter().any(|held| held.browse_id == album.browse_id)
                {
                    albums.push(album);
                }
            }
            for child in map.values() {
                collect_music_albums(child, albums);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_music_albums(child, albums);
            }
        }
        _ => {}
    }
}

/// One card's fields when its title links to an album page: the name, the
/// artist and year under it, and the artwork beside them.
fn music_album(renderer: &Value) -> Option<YtAlbum> {
    let run = renderer.get("title")?.get("runs")?.as_array()?.first()?;
    let name = run.get("text")?.as_str()?;
    let browse = run.get("navigationEndpoint")?.get("browseEndpoint")?;
    let album_page = browse
        .get("browseEndpointContextSupportedConfigs")
        .and_then(|configs| configs.get("browseEndpointContextMusicConfig"))
        .and_then(|config| config.get("pageType"))
        .and_then(Value::as_str)
        .is_some_and(|page| page == "MUSIC_PAGE_TYPE_ALBUM");
    if !album_page {
        return None;
    }
    let browse_id = browse.get("browseId")?.as_str()?;
    let mut texts = card_texts(renderer);
    // "Album • Artist • 2024": the kind is not a name, and a bare year is
    // the release date.
    let year = texts
        .iter()
        .rev()
        .find(|text| text.len() == 4 && text.chars().all(|c| c.is_ascii_digit()))
        .map(|text| (*text).to_string());
    if let Some(year) = &year {
        texts.retain(|text| *text != year.as_str());
    }
    let artist = texts
        .first()
        .map(|text| (*text).to_string())
        .unwrap_or_default();
    Some(YtAlbum {
        browse_id: browse_id.to_string(),
        name: name.to_string(),
        artist,
        year,
        art_url: card_art(renderer),
    })
}

/// An album page's header: the title, artist, year, and artwork the browse
/// response draws with.
fn album_header(browse_id: &str, response: &Value) -> YtAlbum {
    let mut header = None;
    find_header(response, &mut header);
    let mut album = YtAlbum {
        browse_id: browse_id.to_string(),
        name: String::new(),
        artist: String::new(),
        year: None,
        art_url: None,
    };
    let Some(header) = header else {
        return album;
    };
    album.name = header
        .get("title")
        .and_then(|title| {
            title
                .get("runs")
                .and_then(Value::as_array)
                .and_then(|runs| runs.first())
                .and_then(|run| run.get("text"))
                .and_then(Value::as_str)
                .or_else(|| title.get("text").and_then(Value::as_str))
        })
        .unwrap_or_default()
        .to_string();
    let mut texts = header
        .get("subtitle")
        .and_then(|subtitle| subtitle.get("runs"))
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|run| run.get("text").and_then(Value::as_str))
                .map(str::trim)
                .filter(|text| !text.is_empty() && *text != "•" && !is_music_kind(text))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    album.year = texts
        .iter()
        .rev()
        .find(|text| text.len() == 4 && text.chars().all(|c| c.is_ascii_digit()))
        .map(|text| (*text).to_string());
    if let Some(year) = &album.year {
        texts.retain(|text| *text != year.as_str());
    }
    album.artist = texts
        .first()
        .map(|text| (*text).to_string())
        .unwrap_or_default();
    album.art_url = header
        .get("thumbnail")
        .and_then(|thumbnail| thumbnail.get("musicThumbnailRenderer"))
        .and_then(|thumbnail| thumbnail.get("thumbnail"))
        .and_then(|thumbnail| thumbnail.get("thumbnails"))
        .and_then(Value::as_array)
        .and_then(|thumbnails| thumbnails.last())
        .and_then(|thumbnail| thumbnail.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string);
    album
}

/// Finds the album page's header renderer anywhere in the response.
fn find_header<'a>(value: &'a Value, header: &mut Option<&'a Value>) {
    if header.is_some() {
        return;
    }
    match value {
        Value::Object(map) => {
            for key in ["musicResponsiveHeaderRenderer", "musicDetailHeaderRenderer"] {
                if let Some(found) = map.get(key) {
                    *header = Some(found);
                    return;
                }
            }
            for child in map.values() {
                find_header(child, header);
            }
        }
        Value::Array(values) => {
            for child in values {
                find_header(child, header);
            }
        }
        _ => {}
    }
}

/// A card's subtitle runs, trimmed with separators and the kind dropped.
fn card_texts(renderer: &Value) -> Vec<&str> {
    renderer
        .get("subtitle")
        .and_then(|subtitle| subtitle.get("runs"))
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|run| run.get("text").and_then(Value::as_str))
                .map(str::trim)
                .filter(|text| !text.is_empty() && *text != "•" && !is_music_kind(text))
                .collect()
        })
        .unwrap_or_default()
}

/// A card's artwork: the largest thumbnail beside it.
fn card_art(renderer: &Value) -> Option<String> {
    renderer
        .get("thumbnail")
        .and_then(|thumbnail| thumbnail.get("musicThumbnailRenderer"))
        .and_then(|thumbnail| thumbnail.get("thumbnail"))
        .and_then(|thumbnail| thumbnail.get("thumbnails"))
        .and_then(Value::as_array)
        .and_then(|thumbnails| thumbnails.last())
        .and_then(|thumbnail| thumbnail.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Every playlist the search response names, from the playlist cards.
fn music_playlists(response: &Value) -> Vec<YtPlaylist> {
    let mut playlists = Vec::new();
    collect_music_playlists(response, &mut playlists);
    playlists
}

fn collect_music_playlists(value: &Value, playlists: &mut Vec<YtPlaylist>) {
    match value {
        Value::Object(map) => {
            for key in ["musicCardShelfRenderer", "musicTwoRowItemRenderer"] {
                if let Some(renderer) = map.get(key)
                    && let Some(playlist) = music_playlist(renderer)
                    && !playlists
                        .iter()
                        .any(|held| held.browse_id == playlist.browse_id)
                {
                    playlists.push(playlist);
                }
            }
            for child in map.values() {
                collect_music_playlists(child, playlists);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_music_playlists(child, playlists);
            }
        }
        _ => {}
    }
}

/// One card's fields when its title links to a playlist page: the name,
/// the owner or description under it, and the artwork beside them.
fn music_playlist(renderer: &Value) -> Option<YtPlaylist> {
    let run = renderer.get("title")?.get("runs")?.as_array()?.first()?;
    let name = run.get("text")?.as_str()?;
    let browse = run.get("navigationEndpoint")?.get("browseEndpoint")?;
    let playlist_page = browse
        .get("browseEndpointContextSupportedConfigs")
        .and_then(|configs| configs.get("browseEndpointContextMusicConfig"))
        .and_then(|config| config.get("pageType"))
        .and_then(Value::as_str)
        .is_some_and(|page| page == "MUSIC_PAGE_TYPE_PLAYLIST");
    if !playlist_page {
        return None;
    }
    let browse_id = browse.get("browseId")?.as_str()?;
    let owner = card_texts(renderer)
        .first()
        .map(|text| (*text).to_string())
        .unwrap_or_default();
    Some(YtPlaylist {
        browse_id: browse_id.to_string(),
        name: name.to_string(),
        owner,
        art_url: card_art(renderer),
    })
}

/// A playlist page's header: the title, its owner or description, and the
/// artwork the browse response draws with.
fn playlist_header(browse_id: &str, response: &Value) -> YtPlaylist {
    let mut header = None;
    find_header(response, &mut header);
    let mut playlist = YtPlaylist {
        browse_id: browse_id.to_string(),
        name: String::new(),
        owner: String::new(),
        art_url: None,
    };
    let Some(header) = header else {
        return playlist;
    };
    playlist.name = header
        .get("title")
        .and_then(|title| {
            title
                .get("runs")
                .and_then(Value::as_array)
                .and_then(|runs| runs.first())
                .and_then(|run| run.get("text"))
                .and_then(Value::as_str)
                .or_else(|| title.get("text").and_then(Value::as_str))
        })
        .unwrap_or_default()
        .to_string();
    playlist.owner = header
        .get("subtitle")
        .and_then(|subtitle| subtitle.get("runs"))
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|run| run.get("text").and_then(Value::as_str))
                .map(str::trim)
                .find(|text| {
                    !text.is_empty()
                        && *text != "•"
                        && !is_music_kind(text)
                        && !text.ends_with("songs")
                })
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_default();
    playlist.art_url = header
        .get("thumbnail")
        .and_then(|thumbnail| thumbnail.get("musicThumbnailRenderer"))
        .and_then(|thumbnail| thumbnail.get("thumbnail"))
        .and_then(|thumbnail| thumbnail.get("thumbnails"))
        .and_then(Value::as_array)
        .and_then(|thumbnails| thumbnails.last())
        .and_then(|thumbnail| thumbnail.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string);
    playlist
}

/// The best audio-only streams in a player response, best first. Every
/// format this engine can decode (AAC-LC, Opus, Vorbis) is scored by its
/// effective bitrate and the highest wins; the next few travel along as
/// fallbacks for a stream that will not open.
fn pick_audio(response: &Value) -> Option<Resolved> {
    let formats = response
        .get("streamingData")?
        .get("adaptiveFormats")?
        .as_array()?;
    let duration_seconds = response
        .get("videoDetails")
        .and_then(|details| details.get("lengthSeconds"))
        .and_then(number)
        .unwrap_or(0);
    let mut candidates: Vec<(u64, &Value)> = formats
        .iter()
        .filter(|format| format.get("url").and_then(Value::as_str).is_some())
        .filter_map(|format| format_score(format, duration_seconds).map(|score| (score, format)))
        .collect();
    // Stable, so equally-scored formats keep the response's own order.
    candidates.sort_by(|left, right| right.0.cmp(&left.0));

    let build = |format: &Value| -> Option<Resolved> {
        let url = format.get("url").and_then(Value::as_str)?.to_string();
        let ttl = url_ttl(&url);
        Some(Resolved {
            url,
            ttl,
            mime: format
                .get("mimeType")
                .and_then(Value::as_str)
                .map(str::to_string),
            content_length: format.get("contentLength").and_then(number),
            fallbacks: Vec::new(),
        })
    };
    let mut best = build(candidates.first()?.1)?;
    best.fallbacks = candidates
        .iter()
        .skip(1)
        .take(3)
        .filter_map(|(_, format)| build(format))
        .collect();
    Some(best)
}

/// How good a format is, by effective bitrate; `None` for formats this
/// engine cannot decode. The known itag tiers are scored from their public
/// specs; anything else decodable scores by its declared bitrate, or by
/// length over duration when that is missing.
fn format_score(format: &Value, duration_seconds: u64) -> Option<u64> {
    let mime = format.get("mimeType").and_then(Value::as_str)?;
    if !mime.starts_with("audio/") {
        return None;
    }
    let lower = mime.to_ascii_lowercase();
    if !(lower.contains("mp4a") || lower.contains("opus") || lower.contains("vorbis")) {
        return None;
    }
    let derived = if duration_seconds > 0 {
        format
            .get("contentLength")
            .and_then(number)
            .map(|length| length.saturating_mul(8) / duration_seconds)
    } else {
        None
    };
    let known = match format.get("itag").and_then(Value::as_i64) {
        Some(141) => Some(256_000),
        Some(251) => Some(160_000),
        Some(140) => Some(128_000),
        Some(250) => Some(70_000),
        Some(249) => Some(50_000),
        Some(139) => Some(48_000),
        _ => None,
    };
    let mut score = known
        .or_else(|| format.get("bitrate").and_then(number))
        .or(derived)
        .unwrap_or(64_000);
    // High-efficiency AAC yields only its low-complexity core here, so it
    // never outranks a stream this engine decodes in full.
    if lower.contains("mp4a.40.5") || lower.contains("mp4a.40.29") {
        score = score.min(48_000);
    }
    Some(score)
}

/// A number that YouTube sends as a JSON number or a string.
fn number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

/// One decoded query parameter of a googlevideo URL.
fn url_query(url: &str, wanted: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == wanted).then(|| {
            percent_decode_str(value)
                .decode_utf8()
                .map(|value| value.into_owned())
                .unwrap_or_else(|_| value.to_string())
        })
    })
}

impl Resolved {
    /// A URL without a player response behind it (the yt-dlp fallback):
    /// what is known comes from googlevideo's own query.
    pub fn from_url(url: String) -> Self {
        let ttl = url_ttl(&url);
        Self {
            mime: url_query(&url, "mime"),
            content_length: url_query(&url, "clen").and_then(|value| value.parse().ok()),
            fallbacks: Vec::new(),
            url,
            ttl,
        }
    }
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
    fn the_best_sounding_format_is_picked_first() {
        let response = json!({
            "videoDetails": { "lengthSeconds": "200" },
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
        assert!(stream.url.ends_with("/251"), "{}", stream.url);
        assert_eq!(stream.mime.as_deref(), Some("audio/webm; codecs=\"opus\""));
        let fallbacks: Vec<&str> = stream
            .fallbacks
            .iter()
            .map(|stream| stream.url.as_str())
            .collect();
        assert_eq!(fallbacks.len(), 2, "{fallbacks:?}");
        assert!(fallbacks[0].ends_with("/140"), "{fallbacks:?}");
        assert!(fallbacks[1].ends_with("/139"), "{fallbacks:?}");
    }

    #[test]
    fn a_256k_stream_leads_even_over_opus() {
        let response = json!({
            "videoDetails": { "lengthSeconds": "200" },
            "streamingData": {
                "adaptiveFormats": [
                    { "itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "url": "https://x/251" },
                    { "itag": 141, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "url": "https://x/141" },
                ]
            }
        });
        let stream = pick_audio(&response).expect("a stream");
        assert!(stream.url.ends_with("/141"), "{}", stream.url);
    }

    #[test]
    fn an_unknown_opus_format_scores_by_its_declared_bitrate() {
        let response = json!({
            "videoDetails": { "lengthSeconds": "200" },
            "streamingData": {
                "adaptiveFormats": [
                    { "itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "url": "https://x/251" },
                    { "itag": 774, "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": "256000", "url": "https://x/774" },
                ]
            }
        });
        let stream = pick_audio(&response).expect("a stream");
        assert!(stream.url.ends_with("/774"), "{}", stream.url);
    }

    #[test]
    fn a_format_this_engine_cannot_decode_is_never_picked() {
        let response = json!({
            "videoDetails": { "lengthSeconds": "200" },
            "streamingData": {
                "adaptiveFormats": [
                    { "itag": 999, "mimeType": "audio/mp4; codecs=\"ec-3\"", "bitrate": "999000", "url": "https://x/999" },
                    { "itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "url": "https://x/140" },
                ]
            }
        });
        let stream = pick_audio(&response).expect("a stream");
        assert!(stream.url.ends_with("/140"), "{}", stream.url);
    }

    #[test]
    fn a_fallback_url_reports_its_own_mime_and_length() {
        let stream = Resolved::from_url(
            "https://x/v?mime=audio%2Fwebm&clen=9730538&expire=9999999999".to_string(),
        );
        assert_eq!(stream.mime.as_deref(), Some("audio/webm"));
        assert_eq!(stream.content_length, Some(9_730_538));
        assert!(stream.fallbacks.is_empty());
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

    #[test]
    fn a_music_search_answers_its_tracks_in_order() {
        let response = json!({
            "contents": { "musicShelfRenderer": { "contents": [
                { "musicResponsiveListItemRenderer": {
                    "flexColumns": [
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "Alright",
                              "navigationEndpoint": { "watchEndpoint": {
                                  "videoId": "67vr-3kpX3Q",
                                  "watchEndpointMusicSupportedConfigs": {
                                      "watchEndpointMusicConfig": { "musicVideoType": "MUSIC_VIDEO_TYPE_ATV" }
                                  }
                              } } }
                        ] } } },
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "Kendrick Lamar" },
                            { "text": " • " },
                            { "text": "To Pimp A Butterfly" },
                            { "text": " • " },
                            { "text": "3:53" }
                        ] } } }
                    ],
                    "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                        { "url": "https://lh3.googleusercontent.com/small" },
                        { "url": "https://lh3.googleusercontent.com/large" }
                    ] } } }
                } },
                { "musicResponsiveListItemRenderer": {
                    "flexColumns": [
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "alright (slowed + reverb)",
                              "navigationEndpoint": { "watchEndpoint": { "videoId": "nVev0Z1bQBU" } } }
                        ] } } },
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "some channel" },
                            { "text": " • " },
                            { "text": "1.2M views" }
                        ] } } }
                    ]
                } }
            ] } } }
        );
        let items = music_items(&response);
        assert_eq!(items.len(), 2, "one track per result");
        assert_eq!(items[0].video_id, "67vr-3kpX3Q");
        assert_eq!(items[0].title, "Alright");
        assert_eq!(items[0].artists, "Kendrick Lamar");
        assert_eq!(items[0].album, "To Pimp A Butterfly");
        assert_eq!(items[0].duration_ms, 233_000);
        assert_eq!(
            items[0].art_url.as_deref(),
            Some("https://lh3.googleusercontent.com/large")
        );
        assert!(
            items[0].official_audio,
            "an art track is the song's own audio"
        );
        assert_eq!(items[1].video_id, "nVev0Z1bQBU");
        assert_eq!(items[1].artists, "some channel");
        assert_eq!(items[1].album, "", "a view count is not an album");
        assert_eq!(items[1].duration_ms, 0, "a view count is not a duration");
        assert!(!items[1].official_audio);
    }

    #[test]
    fn a_row_whose_artist_is_omitted_still_keeps_its_length() {
        let response = json!({
            "contents": { "musicShelfRenderer": { "contents": [
                { "musicResponsiveListItemRenderer": {
                    "flexColumns": [
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "Creepin'",
                              "navigationEndpoint": { "watchEndpoint": { "videoId": "CCCCCCCCCCC" } } }
                        ] } } },
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "Song" },
                            { "text": " • " },
                            { "text": "3:42" }
                        ] } } }
                    ]
                } }
            ] } } }
        );
        let items = music_items(&response);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].artists, "", "a length is not an artist");
        assert_eq!(items[0].album, "");
        assert_eq!(
            items[0].duration_ms, 222_000,
            "the length is still the length"
        );
    }

    #[test]
    fn a_multiple_artist_row_joins_its_names() {
        let response = json!({
            "contents": { "musicShelfRenderer": { "contents": [
                { "musicResponsiveListItemRenderer": {
                    "flexColumns": [
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "They Wanna Have Fun",
                              "navigationEndpoint": { "watchEndpoint": { "videoId": "BBBBBBBBBBB" } } }
                        ] } } },
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "Song" },
                            { "text": " • " },
                            { "text": "Metro Boomin" },
                            { "text": ", " },
                            { "text": "Travis Porter" },
                            { "text": ", " },
                            { "text": "Young Dro" },
                            { "text": " & " },
                            { "text": "Gucci Mane" }
                        ] } } }
                    ]
                } }
            ] } } }
        );
        let items = music_items(&response);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].artists,
            "Metro Boomin, Travis Porter, Young Dro & Gucci Mane"
        );
        assert_eq!(items[0].album, "", "a separator is not an album");
        assert_eq!(items[0].duration_ms, 0);
    }

    #[test]
    fn a_kind_led_subtitle_keeps_its_names() {
        let response = json!({
            "contents": { "musicShelfRenderer": { "contents": [
                { "musicResponsiveListItemRenderer": {
                    "flexColumns": [
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "Too Many Nights",
                              "navigationEndpoint": { "watchEndpoint": { "videoId": "AAAAAAAAAAA" } } }
                        ] } } },
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "Song\u{a0}" },
                            { "text": " • " },
                            { "text": "Metro Boomin" },
                            { "text": " • " },
                            { "text": "HEROES & VILLAINS" },
                            { "text": " • " },
                            { "text": "3:20" }
                        ] } } }
                    ]
                } }
            ] } } }
        );
        let items = music_items(&response);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].artists, "Metro Boomin");
        assert_eq!(items[0].album, "HEROES & VILLAINS");
        assert_eq!(items[0].duration_ms, 200_000);
    }

    #[test]
    fn a_searches_artist_cards_are_read_with_their_pages() {
        let response = json!({
            "contents": { "musicCardShelfRenderer": {
                "title": { "runs": [ { "text": "Don Toliver",
                    "navigationEndpoint": { "browseEndpoint": {
                        "browseId": "UCSzWQmDsKG37iKN2vw1G-2Q",
                        "browseEndpointContextSupportedConfigs": {
                            "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ARTIST" }
                        } } } } ] },
                "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                    { "url": "https://lh3.googleusercontent.com/a" },
                    { "url": "https://lh3.googleusercontent.com/b" } ] } } }
            } },
            "musicTwoRowItemRenderer": {
                "title": { "runs": [ { "text": "Hardstone Psycho",
                    "navigationEndpoint": { "browseEndpoint": {
                        "browseId": "MPREb_x",
                        "browseEndpointContextSupportedConfigs": {
                            "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ALBUM" }
                        } } } } ] }
            }
        });
        let artists = music_artists(&response);
        assert_eq!(artists.len(), 1, "only artist pages are artists");
        assert_eq!(artists[0].browse_id, "UCSzWQmDsKG37iKN2vw1G-2Q");
        assert_eq!(artists[0].name, "Don Toliver");
        assert_eq!(
            artists[0].art_url.as_deref(),
            Some("https://lh3.googleusercontent.com/b")
        );
    }

    #[test]
    fn a_searches_playlist_cards_are_read_with_their_pages() {
        let response = json!({
            "contents": { "musicTwoRowItemRenderer": {
                "title": { "runs": [ { "text": "Radio Lofi",
                    "navigationEndpoint": { "browseEndpoint": {
                        "browseId": "VLRDATfcbG9maQ",
                        "browseEndpointContextSupportedConfigs": {
                            "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_PLAYLIST" }
                        } } } } ] },
                "subtitle": { "runs": [ { "text": "Saib, Sleepy Fish, Toonorth, and more" } ] },
                "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                    { "url": "https://lh3.googleusercontent.com/p" } ] } } }
            } }
        });
        let playlists = music_playlists(&response);
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].browse_id, "VLRDATfcbG9maQ");
        assert_eq!(playlists[0].name, "Radio Lofi");
        assert_eq!(playlists[0].owner, "Saib, Sleepy Fish, Toonorth, and more");
        assert_eq!(
            playlists[0].art_url.as_deref(),
            Some("https://lh3.googleusercontent.com/p")
        );
    }

    #[test]
    fn a_fixed_column_carries_the_length_the_subtitle_left_out() {
        let response = json!({
            "contents": { "musicShelfRenderer": { "contents": [
                { "musicResponsiveListItemRenderer": {
                    "flexColumns": [
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "Kalyani",
                              "navigationEndpoint": { "watchEndpoint": { "videoId": "DDDDDDDDDDD" } } }
                        ] } } },
                        { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                            { "text": "ARJN, KDS, FIFTY4 & Shreya Ghoshal" }
                        ] } } }
                    ],
                    "fixedColumns": [
                        { "musicResponsiveListItemFixedColumnRenderer": { "text": { "runs": [
                            { "text": "4:12" }
                        ] } } }
                    ]
                } }
            ] } } }
        );
        let items = music_items(&response);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].artists, "ARJN, KDS, FIFTY4 & Shreya Ghoshal");
        assert_eq!(items[0].duration_ms, 252_000);
    }

    #[test]
    fn a_duration_is_read_from_its_last_token() {
        assert_eq!(music_duration_ms("3:53"), Some(233_000));
        assert_eq!(music_duration_ms("1:02:03"), Some(3_723_000));
        assert_eq!(
            music_duration_ms("Artist • Album • 3:53"),
            Some(233_000),
            "a row that packs its subtitle into one run still answers"
        );
        assert_eq!(music_duration_ms("1.2M views"), None);
        assert_eq!(music_duration_ms("some channel"), None);
        assert_eq!(music_duration_ms(""), None);
    }
}
