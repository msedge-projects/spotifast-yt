//! Free-account playback through YouTube's InnerTube API and mpv.
//!
//! Spotify does not hand a free account the audio keys librespot needs, so
//! this engine resolves each track's title to a direct audio URL: first
//! through the InnerTube API impersonating the VISIONOS client (two HTTPS
//! requests from this process, no JavaScript, no subprocess), and, when
//! that is refused, through `yt-dlp` (a flag-only `-g` call: no download,
//! no ffmpeg) as before. The URL plays through an `mpv` child process
//! driven over mpv's JSON IPC. It lives beside the librespot engine:
//! Premium accounts keep `player.rs`, free accounts get this one.
//!
//! Resolved URLs are kept for as long as their own lifetime allows, and
//! the next song is resolved while the current one plays, so starting a
//! song normally does not wait on a search at all.
//!
//! The engine owns the queue itself. librespot's Spirc advances tracks for
//! the Premium engine; here mpv reports the end of a track, and the engine
//! picks what plays next (the user's queued songs first, then the context,
//! honouring repeat and shuffle) and pushes the new track through the same
//! [`LocalState`] snapshots the interface already consumes.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::player::{
    EngineConfig, EngineEvent, Heard, Interrupted, LoadSpec, LocalState, LocalTrack, Notify,
    Playback, PlaybackResume, PlayerCommand, RepeatMode,
};
use crate::yt_resolver;

/// How long the yt-dlp fallback resolve may take before it is killed.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long mpv has to create its IPC pipe after the process starts.
const IPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// How often the played position is pushed to the interface. librespot
/// reports about once a second; half a second keeps the bar smooth without
/// repainting the UI for every mpv tick.
const POSITION_NOTIFY_MS: u32 = 500;
/// A Previous press within this of the start restarts the track instead of
/// moving to the one before it, like librespot.
const PREVIOUS_RESTART_MS: u32 = 3_000;
/// How long into a song the next one is resolved in the background. The wait
/// keeps a resolve clear of the transition between songs.
const PREFETCH_DELAY: Duration = Duration::from_secs(10);

/// The engine's actor messages: commands from the interface, mpv's events,
/// finished resolves, and the end of the pipe.
enum Message {
    Command(PlayerCommand),
    Mpv(Value),
    Resolved {
        id: u64,
        result: Result<yt_resolver::Resolved, String>,
    },
    /// A background resolve for the next song finished.
    Prefetched {
        uri: String,
        result: Result<yt_resolver::Resolved, String>,
    },
    Closed,
}

/// Either platform's IPC connection: one trait so the reader and writer
/// tasks do not care which one it is.
trait IpcStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> IpcStream for T {}

type BoxedIpc = Box<dyn IpcStream>;

pub struct YtEngine {
    device_id: String,
    state: Arc<Mutex<LocalState>>,
    messages: mpsc::UnboundedSender<Message>,
    child: Mutex<tokio::process::Child>,
    /// What was playing when mpv went away, for the engine that replaces
    /// this one.
    interrupted: Arc<Mutex<Option<Interrupted>>>,
    shutting_down: Arc<AtomicBool>,
}

impl YtEngine {
    /// Starts mpv and connects to its control pipe. Fails when yt-dlp or
    /// mpv is missing, so the interface can say what to install.
    pub async fn connect(config: &EngineConfig, notify: Notify) -> Result<Self> {
        let yt_dlp = find_tool("yt-dlp").ok_or_else(|| {
            anyhow!(
                "YouTube playback needs yt-dlp. Install it (for example `pip install yt-dlp`) \
                 and make sure it is on PATH."
            )
        })?;
        let mpv = find_tool("mpv").ok_or_else(|| {
            anyhow!(
                "YouTube playback needs mpv. Install mpv and make sure mpv.exe is on PATH or \
                 in %LOCALAPPDATA%\\Programs\\mpv."
            )
        })?;
        let resolver = Arc::new(yt_resolver::NativeResolver::new()?);

        let pipe = ipc_path();
        #[cfg(not(windows))]
        let _ = std::fs::remove_file(&pipe);
        let mut command = tokio::process::Command::new(&mpv);
        command
            .arg("--idle=yes")
            .arg("--no-video")
            .arg("--no-terminal")
            .arg("--really-quiet")
            .arg(format!("--input-ipc-server={}", pipe.display()))
            .arg("--ytdl=no")
            // A stalled stream fails in twenty seconds instead of a
            // minute, and short drops reconnect instead of ending the
            // track: both give the retry a healthy baseline to resume.
            .arg("--network-timeout=20")
            .arg("--stream-lavf-o=reconnect=1,reconnect_streamed=1,reconnect_delay_max=5")
            .arg(format!(
                "--volume={}",
                volume_percent(config.initial_volume)
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = command.spawn().context("unable to start mpv")?;
        let stream = connect_ipc(&pipe).await?;
        let (reader, writer) = tokio::io::split(stream);

        let state = Arc::new(Mutex::new(LocalState {
            volume: config.initial_volume,
            connected: true,
            ..LocalState::default()
        }));
        let (messages, receiver) = mpsc::unbounded_channel();
        let (mpv_lines, mpv_receiver) = mpsc::unbounded_channel();
        let interrupted: Arc<Mutex<Option<Interrupted>>> = Arc::default();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let actor = Actor {
            state: Arc::clone(&state),
            notify: Arc::clone(&notify),
            mpv: mpv_lines,
            messages: messages.clone(),
            interrupted: Arc::clone(&interrupted),
            shutting_down: Arc::clone(&shutting_down),
            yt_dlp,
            resolver,
            list: Vec::new(),
            manual: VecDeque::new(),
            cursor: None,
            current: None,
            from_list: false,
            repeat: RepeatMode::default(),
            shuffle: false,
            want_play: true,
            play_id: 0,
            loaded: false,
            awaiting: false,
            target_ms: 0,
            told_ms: 0,
            url_cache: HashMap::new(),
            prefetching: None,
            retried_uri: None,
            resolve_failures: 0,
        };
        tokio::spawn(run_actor(actor, receiver));
        tokio::spawn(run_writer(writer, mpv_receiver));
        tokio::spawn(run_reader(reader, messages.clone()));

        notify(EngineEvent::State(
            state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        ));
        Ok(Self {
            device_id: config.device_id(),
            state,
            messages,
            child: Mutex::new(child),
            interrupted,
            shutting_down,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The level this engine is heard at, for the engine that replaces it.
    pub(crate) fn heard(&self) -> Heard {
        Heard::from_state(Arc::clone(&self.state))
    }

    pub fn command(&self, command: PlayerCommand) -> Result<()> {
        self.messages
            .send(Message::Command(command))
            .map_err(|_| anyhow!("YouTube playback is not running"))
    }

    /// Takes the engine down. A replacement picks the playback up from
    /// [`Self::resume_point`].
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
    }

    pub fn interrupted(&self) -> Option<Interrupted> {
        let ended = self
            .interrupted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        ended.or_else(|| {
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .interrupted()
        })
    }

    pub fn resume_point(&self) -> Option<PlaybackResume> {
        let interrupted = self.interrupted()?;
        // The engine forgets its queue when it is replaced, but the
        // playing song's metadata is still in the state. Carrying it lets
        // the replacement start the song without another lookup (and
        // without the app having to answer for a cached row).
        let track = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .track
            .clone()
            .filter(|track| track.uri == interrupted.uri && !track.title.is_empty());
        Some(PlaybackResume::Track(LoadSpec {
            uris: vec![interrupted.uri],
            tracks: track.into_iter().collect(),
            position_ms: interrupted.position_ms,
            play: interrupted.playing,
            ..LoadSpec::default()
        }))
    }

    pub fn resume(&self, resume: PlaybackResume) -> Result<()> {
        match resume {
            PlaybackResume::Track(spec) => self.command(PlayerCommand::Load(spec)),
            // There is no Spotify Connect session here to take over.
            PlaybackResume::Session(_) => Ok(()),
        }
    }
}

/// One song the engine will play: the Spotify URI the interface keys
/// everything by, and the metadata yt-dlp is searched with.
#[derive(Clone)]
struct Entry {
    uri: String,
    track: LocalTrack,
    /// The metadata has not arrived yet. The interface fetches the track
    /// for a newly playing URI and answers with `SetTrackMetadata`; until
    /// then the item is parked as loading.
    waiting_for_metadata: bool,
}

impl Entry {
    fn new(uri: String, track: Option<LocalTrack>) -> Self {
        let track = track.unwrap_or_else(|| LocalTrack {
            uri: uri.clone(),
            ..LocalTrack::default()
        });
        let waiting_for_metadata = track.title.is_empty();
        Self {
            uri,
            track,
            waiting_for_metadata,
        }
    }

    /// Whether yt-dlp has something to search for.
    fn resolvable(&self) -> bool {
        !self.waiting_for_metadata && !self.uri.starts_with("spotify:episode:")
    }
}

/// A direct URL kept for a while, with the life YouTube gave it.
struct CachedStream {
    url: String,
    at: Instant,
    ttl: Duration,
}

/// The actor owns the queue and folds mpv's events into [`LocalState`].
struct Actor {
    state: Arc<Mutex<LocalState>>,
    notify: Notify,
    /// Newline-delimited JSON to mpv.
    mpv: mpsc::UnboundedSender<String>,
    /// Its own message channel, so a finished resolve can come back in.
    messages: mpsc::UnboundedSender<Message>,
    /// What was playing when mpv went away, for the engine that replaces
    /// this one.
    interrupted: Arc<Mutex<Option<Interrupted>>>,
    shutting_down: Arc<AtomicBool>,
    yt_dlp: PathBuf,
    resolver: Arc<yt_resolver::NativeResolver>,
    /// The context: the songs around the current one.
    list: Vec<Entry>,
    /// The user's queued songs, played before the context continues.
    manual: VecDeque<Entry>,
    /// The list position of the last song taken from the list.
    cursor: Option<usize>,
    current: Option<Entry>,
    from_list: bool,
    repeat: RepeatMode,
    shuffle: bool,
    /// Playing, as opposed to paused. A toggle during a load applies when
    /// the file arrives.
    want_play: bool,
    /// The play being set up. Results of older resolves are discarded.
    play_id: u64,
    /// mpv holds a playable file for the current song.
    loaded: bool,
    /// A resolve for the current song is in flight.
    awaiting: bool,
    /// Where the current song should start.
    target_ms: u32,
    /// The last position the interface was told, to throttle snapshots.
    told_ms: u32,
    /// Direct URLs resolved recently, by track URI: a song starting from
    /// here skips the resolver, and its CPU cost, entirely.
    url_cache: HashMap<String, CachedStream>,
    /// The track a background resolve is running for, so that only one runs
    /// at a time.
    prefetching: Option<String>,
    /// The song whose load failed once and was resolved again, so a repeated
    /// failure is not retried forever.
    retried_uri: Option<String>,
    /// Songs that failed to resolve in a row; a run of unavailable songs is
    /// skipped over, but not forever.
    resolve_failures: u32,
}

async fn run_actor(mut actor: Actor, mut messages: mpsc::UnboundedReceiver<Message>) {
    actor.observe();
    while let Some(message) = messages.recv().await {
        match message {
            Message::Command(command) => actor.apply(command),
            Message::Mpv(value) => actor.mpv_event(&value),
            Message::Resolved { id, result } => actor.resolved(id, result),
            Message::Prefetched { uri, result } => actor.prefetched(uri, result),
            Message::Closed => {
                actor.closed();
                return;
            }
        }
    }
}

async fn run_reader(
    reader: impl tokio::io::AsyncRead + Unpin + Send,
    messages: mpsc::UnboundedSender<Message>,
) {
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && messages.send(Message::Mpv(value)).is_err()
                {
                    return;
                }
            }
            Ok(None) | Err(_) => {
                let _ = messages.send(Message::Closed);
                return;
            }
        }
    }
}

async fn run_writer(
    mut writer: impl tokio::io::AsyncWrite + Unpin + Send,
    mut lines: mpsc::UnboundedReceiver<String>,
) {
    while let Some(line) = lines.recv().await {
        if writer.write_all(line.as_bytes()).await.is_err()
            || writer.write_all(b"\n").await.is_err()
        {
            return;
        }
        let _ = writer.flush().await;
    }
}

impl Actor {
    /// Watches the two properties the interface follows.
    fn observe(&self) {
        self.send(json!(["observe_property", 1, "time-pos"]));
        self.send(json!(["observe_property", 2, "duration"]));
    }

    fn send(&self, command: Value) {
        let _ = self.mpv.send(json!({ "command": command }).to_string());
    }

    fn publish(&self) {
        let snapshot = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        (self.notify)(EngineEvent::State(snapshot));
    }

    fn apply(&mut self, command: PlayerCommand) {
        match command {
            PlayerCommand::Toggle => self.toggle(),
            PlayerCommand::Next => self.advance(),
            PlayerCommand::Previous => self.previous(),
            PlayerCommand::ClearQueue => self.manual.clear(),
            PlayerCommand::AddToQueue { uri, track } => {
                self.manual.push_back(Entry::new(uri, track));
            }
            PlayerCommand::SetTrackMetadata { uri, track } => self.set_metadata(&uri, track),
            PlayerCommand::Seek(position_ms) => self.seek(position_ms),
            PlayerCommand::Volume(volume) | PlayerCommand::VolumePreview(volume) => {
                self.set_volume(volume)
            }
            PlayerCommand::Shuffle(enabled) => {
                self.shuffle = enabled;
                self.publish();
            }
            PlayerCommand::Repeat(mode) => {
                self.repeat = mode;
                self.publish();
            }
            PlayerCommand::Load(spec) => self.load(spec),
            // Free playback is not a Spotify Connect device, so there is no
            // session to take over. The interface's request is satisfied.
            PlayerCommand::Transfer => {}
        }
    }

    /// Starts the list a load names. The user's queued songs stay queued:
    /// starting a collection keeps them (the queue's rules).
    fn load(&mut self, spec: LoadSpec) {
        self.list = spec
            .uris
            .iter()
            .enumerate()
            .map(|(index, uri)| {
                let track = spec
                    .tracks
                    .get(index)
                    .filter(|track| !track.uri.is_empty())
                    .cloned();
                Entry::new(uri.clone(), track)
            })
            .collect();
        if self.list.is_empty()
            && let Some(uri) = spec.context_uri.clone()
        {
            // A single song arrives as a context of itself.
            self.list
                .push(Entry::new(uri, spec.tracks.first().cloned()));
        }
        if let Some(shuffle) = spec.shuffle {
            self.shuffle = shuffle;
        }
        if let Some(repeat) = spec.repeat {
            self.repeat = repeat;
        }
        if self.list.is_empty() {
            self.fail("This has no songs to play");
            return;
        }
        // A chosen row starts there; a shuffled load with no chosen row
        // starts anywhere in the list, the way librespot's shuffle does.
        let index = if let Some(uri) = spec.offset_uri.as_deref()
            && let Some(index) = self.list.iter().position(|entry| entry.uri == uri)
        {
            index
        } else if let Some(index) = spec.offset_index {
            (index as usize).min(self.list.len() - 1)
        } else if self.shuffle {
            rand::random_range(0..self.list.len())
        } else {
            0
        };
        self.cursor = Some(index);
        self.from_list = true;
        self.current = Some(self.list[index].clone());
        self.want_play = spec.play;
        self.start(spec.position_ms);
    }

    /// Starts the current song: state first, then the resolve.
    fn start(&mut self, position_ms: u32) {
        let Some(entry) = self.current.clone() else {
            return;
        };
        // Choosing a song is fresh business: a prefetch still in flight was
        // for an earlier pick, so let the next one schedule.
        self.prefetching = None;
        self.play_id = self.play_id.wrapping_add(1);
        let id = self.play_id;
        self.loaded = false;
        self.target_ms = position_ms;
        self.told_ms = position_ms;
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let replay = state
                .track
                .as_ref()
                .is_some_and(|track| track.uri == entry.uri);
            state.track = Some(entry.track.clone());
            // Every play of the same song is a new occurrence, so the
            // interface consumes no queue row twice; a replay gets a seek
            // for the media controls once the position arrives.
            state.track_sequence = state.track_sequence.wrapping_add(1);
            state.replay_pending = replay;
            state.error = None;
            state.loading = true;
            if state.playback == Playback::Stopped {
                state.playback = Playback::Loading;
            }
            state.position_ms = position_ms;
            state.position_at = None;
        }
        self.publish();
        if !entry.resolvable() {
            if entry.uri.starts_with("spotify:episode:") {
                self.fail("Episodes cannot play through YouTube yet");
                return;
            }
            // Metadata is missing: park the item as loading so the
            // interface fetches the track and answers with
            // SetTrackMetadata. Nothing keeps playing meanwhile.
            self.send(json!(["stop"]));
            return;
        }
        if let Some(url) = self.cached_url(&entry.uri) {
            // Resolved already, by the prefetch made while the last song
            // played, or by an earlier play: start without yt-dlp.
            log::info!("YouTube: reusing a cached URL for {}", entry.track.title);
            self.awaiting = false;
            self.send(json!(["loadfile", url, "replace"]));
            if position_ms > 0 {
                self.seek_command(position_ms);
            }
            return;
        }
        self.awaiting = true;
        let query = format!("{} {}", entry.track.title, entry.track.artist_names());
        let uri = entry.uri.clone();
        log::info!("YouTube: resolving {}", entry.track.title);
        let yt_dlp = self.yt_dlp.clone();
        let resolver = self.resolver.clone();
        let messages = self.messages.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let result = resolve(&resolver, &yt_dlp, &uri, query.trim())
                .await
                .map_err(|error| format!("{error:#}"));
            match &result {
                Ok(_) => log::info!("YouTube: resolved in {} ms", started.elapsed().as_millis()),
                Err(error) => log::warn!("YouTube: resolve failed: {error}"),
            }
            let _ = messages.send(Message::Resolved { id, result });
        });
    }

    fn resolved(&mut self, id: u64, result: Result<yt_resolver::Resolved, String>) {
        if id != self.play_id {
            // A newer song is already loading; this URL is stale.
            return;
        }
        match result {
            Ok(stream) => {
                self.resolve_failures = 0;
                let url = stream.url.clone();
                let uri = self.current.as_ref().map(|entry| entry.uri.clone());
                if let Some(uri) = uri {
                    self.store_url(uri, stream);
                }
                self.send(json!(["loadfile", url, "replace"]));
                if self.target_ms > 0 {
                    self.seek_command(self.target_ms);
                }
            }
            Err(message) => {
                self.awaiting = false;
                // A song YouTube cannot serve must not stop the music: skip
                // it, but not forever through a list of songs it cannot
                // serve.
                if self.resolve_failures < 3 && self.next_entry().is_some() {
                    self.resolve_failures += 1;
                    let title = self
                        .current
                        .as_ref()
                        .map(|entry| entry.track.title.as_str())
                        .unwrap_or("an unnamed song");
                    log::info!("YouTube: skipping {title} ({message})");
                    self.advance();
                } else {
                    self.fail(&message);
                }
            }
        }
    }

    /// A direct URL for `uri`, if it was resolved recently enough to reuse.
    fn cached_url(&self, uri: &str) -> Option<String> {
        let cached = self.url_cache.get(uri)?;
        if cached.at.elapsed() < cached.ttl {
            Some(cached.url.clone())
        } else {
            None
        }
    }

    fn store_url(&mut self, uri: String, stream: yt_resolver::Resolved) {
        self.url_cache
            .retain(|_, cached| cached.at.elapsed() < cached.ttl);
        self.url_cache.insert(
            uri,
            CachedStream {
                url: stream.url,
                at: Instant::now(),
                ttl: stream.ttl,
            },
        );
    }

    /// A background resolve finished: the URL waits in the cache for the
    /// song's turn.
    fn prefetched(&mut self, uri: String, result: Result<yt_resolver::Resolved, String>) {
        if self.prefetching.as_deref() == Some(uri.as_str()) {
            self.prefetching = None;
        }
        if let Ok(stream) = result {
            self.store_url(uri, stream);
        }
    }

    /// The entry that follows the current one: the user's queue first, then
    /// the context, as `advance` picks it. `None` when the pick is a random
    /// one (shuffle) or the list is over: nothing to prefetch.
    fn next_entry(&self) -> Option<Entry> {
        if let Some(entry) = self.manual.front() {
            return Some(entry.clone());
        }
        if self.shuffle || self.repeat == RepeatMode::Track {
            return None;
        }
        let index = next_list_index(self.list.len(), self.cursor, false, self.repeat, |_| 0)?;
        self.list.get(index).cloned()
    }

    /// Resolves the next song while the current one plays, so starting it is
    /// instant and free of the resolver's CPU burst: that burst lands
    /// mid-song instead of at the transition.
    fn prefetch_next(&mut self) {
        if self.prefetching.is_some() {
            return;
        }
        let Some(entry) = self.next_entry() else {
            return;
        };
        if !entry.resolvable() || self.cached_url(&entry.uri).is_some() {
            return;
        }
        let uri = entry.uri.clone();
        let query = format!("{} {}", entry.track.title, entry.track.artist_names());
        let yt_dlp = self.yt_dlp.clone();
        let resolver = self.resolver.clone();
        let messages = self.messages.clone();
        self.prefetching = Some(uri.clone());
        log::info!("YouTube: prefetching the next song {}", entry.track.title);
        tokio::spawn(async move {
            tokio::time::sleep(PREFETCH_DELAY).await;
            let started = Instant::now();
            let result = resolve(&resolver, &yt_dlp, &uri, query.trim())
                .await
                .map_err(|error| format!("{error:#}"));
            match &result {
                Ok(_) => log::info!(
                    "YouTube: prefetched the next song in {} ms",
                    started.elapsed().as_millis()
                ),
                Err(error) => {
                    log::info!("YouTube: prefetch failed ({error}); it will resolve on demand")
                }
            }
            let _ = messages.send(Message::Prefetched { uri, result });
        });
    }

    fn mpv_event(&mut self, value: &Value) {
        match value.get("event").and_then(Value::as_str) {
            Some("file-loaded") => self.file_loaded(),
            Some("end-file") => self.end_file(value),
            Some("property-change") => self.property_change(value),
            _ => {}
        }
    }

    fn file_loaded(&mut self) {
        // A file loaded for a song that is still parked (or was replaced
        // already) is not the current one; its events are not applied.
        if self
            .current
            .as_ref()
            .is_none_or(|entry| entry.waiting_for_metadata)
        {
            return;
        }
        self.loaded = true;
        self.awaiting = false;
        self.send(json!(["set_property", "pause", !self.want_play]));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.loading = false;
        state.position_ms = self.target_ms;
        if self.want_play {
            state.playback = Playback::Playing;
            state.position_at = Some(Instant::now());
            if std::mem::take(&mut state.replay_pending) {
                state.seek_sequence = state.seek_sequence.wrapping_add(1);
            }
        } else {
            state.playback = Playback::Paused;
            state.position_at = None;
        }
        drop(state);
        self.publish();
        self.retried_uri = None;
        self.prefetch_next();
    }

    fn end_file(&mut self, value: &Value) {
        match value.get("reason").and_then(Value::as_str) {
            Some("eof") => {
                // The track played out. One advance per file: `loaded`
                // guards against a seek to the end arriving twice.
                if self.loaded && self.current.is_some() {
                    self.loaded = false;
                    if self.repeat == RepeatMode::Track {
                        self.start(0);
                    } else {
                        self.advance();
                    }
                }
            }
            Some("error") => {
                self.loaded = false;
                let detail = value
                    .get("file_error")
                    .and_then(Value::as_str)
                    .unwrap_or("mpv could not play this stream");
                let position = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .position_now();
                // The URL that failed is never reused, whether or not a
                // retry is left: a bad URL in the cache must not poison
                // every later play of the song.
                if let Some(entry) = self.current.clone() {
                    self.url_cache.remove(&entry.uri);
                    // One fresh resolve, then the failure stands.
                    if self.retried_uri.as_deref() != Some(entry.uri.as_str()) {
                        log::warn!("YouTube: load failed ({detail}), resolving again");
                        self.retried_uri = Some(entry.uri.clone());
                        self.start(position);
                        return;
                    }
                }
                self.fail(&format!("Playback stopped: {detail}"));
            }
            // "stop" and "quit" follow our own loadfile and shutdown.
            _ => {}
        }
    }

    /// The next song: the user's queue first, then the context. Repeat and
    /// shuffle apply to the context only, as they do in Spotify.
    fn advance(&mut self) {
        if let Some(entry) = self.manual.pop_front() {
            self.from_list = false;
            self.current = Some(entry);
            self.start(0);
            return;
        }
        let next = next_list_index(
            self.list.len(),
            self.cursor,
            self.shuffle,
            self.repeat,
            |len| {
                // Shuffled pick that does not immediately repeat the song
                // that just played.
                if len <= 1 {
                    return 0;
                }
                let mut index = rand::random_range(0..len - 1);
                if Some(index) >= self.cursor {
                    index += 1;
                }
                index
            },
        );
        match next {
            Some(index) => {
                self.cursor = Some(index);
                self.from_list = true;
                self.current = Some(self.list[index].clone());
                self.start(0);
            }
            None => self.stop_at_end(),
        }
    }

    fn previous(&mut self) {
        let position = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .position_now();
        if position > PREVIOUS_RESTART_MS {
            self.seek(0);
            return;
        }
        if self.from_list
            && let Some(cursor) = self.cursor
            && cursor > 0
        {
            self.cursor = Some(cursor - 1);
            self.current = Some(self.list[cursor - 1].clone());
            self.start(0);
            return;
        }
        self.seek(0);
    }

    fn stop_at_end(&mut self) {
        self.current = None;
        self.loaded = false;
        // Anything still resolving for the song that just left is not to
        // be played any more.
        self.play_id = self.play_id.wrapping_add(1);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.playback = Playback::Stopped;
        state.loading = false;
        state.position_ms = 0;
        state.position_at = None;
        drop(state);
        self.publish();
    }

    fn toggle(&mut self) {
        // Nothing is loaded in mpv (a failed resolve stopped it, say):
        // pressing play starts the current song again, with a fresh
        // one-retry budget.
        if !self.loaded && !self.awaiting && self.current.is_some() {
            self.want_play = true;
            self.retried_uri = None;
            self.start(self.target_ms);
            return;
        }
        let playback = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .playback;
        match playback {
            Playback::Playing => {
                self.want_play = false;
                if self.loaded {
                    self.send(json!(["set_property", "pause", true]));
                }
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.playback = Playback::Paused;
                state.position_at = None;
            }
            Playback::Paused => {
                self.want_play = true;
                if self.loaded {
                    self.send(json!(["set_property", "pause", false]));
                }
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.playback = Playback::Playing;
                state.position_at = Some(Instant::now());
            }
            // A toggle during the load applies when the file arrives.
            Playback::Loading => self.want_play = !self.want_play,
            Playback::Stopped => {}
        }
        self.publish();
    }

    fn seek(&mut self, position_ms: u32) {
        let duration = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .track
            .as_ref()
            .map_or(0, |track| track.duration_ms);
        if duration == 0 {
            // A stream with no duration cannot be seeked.
            return;
        }
        let position = position_ms.min(duration.saturating_sub(1));
        self.target_ms = position;
        if self.loaded {
            self.seek_command(position);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.position_ms = position;
        state.seek_sequence = state.seek_sequence.wrapping_add(1);
        state.position_at = (state.playback == Playback::Playing).then(Instant::now);
        drop(state);
        self.publish();
    }

    fn seek_command(&self, position_ms: u32) {
        self.send(json!(["seek", f64::from(position_ms) / 1000.0, "absolute"]));
    }

    fn set_volume(&mut self, volume: u16) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .volume = volume;
        self.send(json!(["set_property", "volume", volume_percent(volume)]));
        self.publish();
    }

    fn set_metadata(&mut self, uri: &str, track: LocalTrack) {
        for entry in self.manual.iter_mut().chain(self.list.iter_mut()) {
            if entry.uri == uri {
                entry.track = track.clone();
                entry.waiting_for_metadata = entry.track.title.is_empty();
            }
        }
        let mut start_current = false;
        if let Some(current) = self.current.as_mut()
            && current.uri == uri
            && current.waiting_for_metadata
        {
            current.track = track;
            current.waiting_for_metadata = false;
            start_current = true;
        }
        if start_current {
            // The parked song can play now.
            self.start(0);
        }
    }

    fn property_change(&mut self, value: &Value) {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let data = value.get("data").cloned().unwrap_or(Value::Null);
        match name {
            "time-pos" => {
                let Some(seconds) = data.as_f64() else {
                    return;
                };
                let position = (seconds.max(0.0) * 1000.0) as u32;
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.playback != Playback::Playing {
                    // A paused or idle mpv still reports a position.
                    return;
                }
                state.position_ms = position;
                state.position_at = Some(Instant::now());
                drop(state);
                if position.abs_diff(self.told_ms) >= POSITION_NOTIFY_MS {
                    self.told_ms = position;
                    self.publish();
                }
            }
            "duration" => {
                let Some(seconds) = data.as_f64() else {
                    return;
                };
                let duration = (seconds.max(0.0) * 1000.0) as u32;
                if duration == 0 {
                    return;
                }
                // mpv reads the real duration off the stream; it also
                // covers songs whose cached metadata had none.
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(track) = state.track.as_mut() else {
                    return;
                };
                if track.duration_ms == duration {
                    return;
                }
                track.duration_ms = duration;
                drop(state);
                self.publish();
            }
            _ => {}
        }
    }

    fn fail(&mut self, message: &str) {
        // The song that failed is not what is heard any more: mpv must
        // not keep playing whatever it had while the state reports the
        // failure.
        self.loaded = false;
        self.awaiting = false;
        self.send(json!(["stop"]));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.error = Some(message.to_string());
        state.loading = false;
        if state.playback != Playback::Stopped {
            // The last song stays visible, but the transport is not left
            // pretending to play something that never loaded.
            state.playback = Playback::Paused;
        }
        state.position_at = None;
        drop(state);
        self.publish();
    }

    fn closed(&mut self) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Kept before the state is marked stopped, so a reconnect
            // knows what to pick up.
            *self
                .interrupted
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = state.interrupted();
            state.connected = false;
            state.playback = Playback::Stopped;
            state.loading = false;
            state.position_at = None;
        }
        self.publish();
        if !self.shutting_down.load(Ordering::SeqCst) {
            (self.notify)(EngineEvent::SessionEnded);
        }
    }
}

/// The context position that follows `cursor`, or `None` when the list is
/// over. Shuffled playback picks anywhere; its `pick` closure owns how.
fn next_list_index(
    len: usize,
    cursor: Option<usize>,
    shuffle: bool,
    repeat: RepeatMode,
    pick: impl FnOnce(usize) -> usize,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    if shuffle {
        return Some(pick(len));
    }
    match cursor {
        Some(cursor) if cursor + 1 < len => Some(cursor + 1),
        Some(_) => (repeat == RepeatMode::Context).then_some(0),
        None => Some(0),
    }
}

/// The volume as mpv's percent, from the interface's 16-bit level.
fn volume_percent(volume: u16) -> f64 {
    let percent = f64::from(volume) * 100.0 / f64::from(u16::MAX);
    (percent * 10.0).round() / 10.0
}

/// The named program on PATH, then the places Windows installs commonly
/// put it (pip's per-user Scripts folder, the portable mpv).
fn find_tool(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join(&exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    #[cfg(windows)]
    {
        let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
        if name == "mpv" {
            let mut candidates = vec![
                local.join("Programs").join("mpv").join(&exe),
                local.join("mpv").join(&exe),
            ];
            // Installers and package managers put mpv under Program Files
            // too; a PATH entry usually covers them, but not always.
            for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
                if let Some(directory) = std::env::var_os(variable) {
                    candidates.push(PathBuf::from(directory).join("mpv").join(&exe));
                }
            }
            for candidate in candidates {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        if name == "yt-dlp" {
            let mut roots = vec![local.join("Programs").join("Python")];
            if let Some(appdata) = std::env::var_os("APPDATA") {
                roots.push(PathBuf::from(appdata).join("Python"));
            }
            for root in roots {
                let Ok(versions) = std::fs::read_dir(root) else {
                    continue;
                };
                for version in versions.flatten() {
                    let candidate = version.path().join("Scripts").join(&exe);
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}

/// mpv's IPC address: a named pipe on Windows, a socket elsewhere.
fn ipc_path() -> PathBuf {
    let pid = std::process::id();
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\musify-{pid}"))
    }
    #[cfg(not(windows))]
    {
        std::env::temp_dir().join(format!("musify-{pid}.sock"))
    }
}

/// Waits for mpv to create the pipe and connects to it.
async fn connect_ipc(pipe: &Path) -> Result<BoxedIpc> {
    let deadline = tokio::time::Instant::now() + IPC_CONNECT_TIMEOUT;
    loop {
        #[cfg(windows)]
        let attempt: std::io::Result<BoxedIpc> =
            tokio::net::windows::named_pipe::ClientOptions::new()
                .open(pipe.as_os_str())
                .map(|client| Box::new(client) as BoxedIpc);
        #[cfg(not(windows))]
        let attempt: std::io::Result<BoxedIpc> = tokio::net::UnixStream::connect(pipe)
            .await
            .map(|stream| Box::new(stream) as BoxedIpc);
        match attempt {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(anyhow!("could not reach mpv's control pipe: {error}"));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// The command that runs yt-dlp, prepared to leave no trace on screen.
///
/// On Windows a pip-installed `yt-dlp.exe` is a console program, so a
/// console flashes over the interface every time it runs: once for the
/// shim and again for the Python it starts. When the windowless interpreter
/// of the same installation sits beside it, the module runs through that
/// instead and no console exists at all; otherwise the shim runs with
/// `CREATE_NO_WINDOW`. Either way the resolver is also started at
/// below-normal priority, so its work never fights the audio thread or the
/// interface.
fn yt_dlp_command(yt_dlp: &Path) -> tokio::process::Command {
    #[cfg(windows)]
    {
        if let Some(root) = yt_dlp.parent().and_then(Path::parent) {
            let pythonw = root.join("pythonw.exe");
            if pythonw.is_file() {
                let mut command = tokio::process::Command::new(pythonw);
                command.arg("-m").arg("yt_dlp");
                hide_console(&mut command);
                return command;
            }
        }
    }
    let mut command = tokio::process::Command::new(yt_dlp);
    hide_console(&mut command);
    command
}

#[cfg(windows)]
fn hide_console(command: &mut tokio::process::Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    command.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut tokio::process::Command) {}

/// One resolve: the native InnerTube client first, `yt-dlp` when it is
/// refused. A `yt:<video id>` URI (a locally stored track) names its video
/// directly, so the search round trip is skipped for it. The native path
/// is the quiet default; the fallback is loud in the log so a client
/// rotation is noticed.
async fn resolve(
    native: &yt_resolver::NativeResolver,
    yt_dlp: &Path,
    uri: &str,
    query: &str,
) -> Result<yt_resolver::Resolved> {
    let video = uri.strip_prefix("yt:");
    let native_result = match video {
        Some(video) => native.resolve_video(video).await,
        None => native.resolve(query).await,
    };
    match native_result {
        Ok(stream) => Ok(stream),
        Err(error) => {
            log::warn!("YouTube: the native client failed ({error:#}); using yt-dlp");
            match video {
                // yt-dlp takes a watch URL as the target directly.
                Some(video) => {
                    resolve_yt_dlp(yt_dlp, &format!("https://www.youtube.com/watch?v={video}"))
                        .await
                }
                None => resolve_yt_dlp(yt_dlp, query).await,
            }
        }
    }
}

/// The fallback resolve: the first stdout line of `yt-dlp -g` is the
/// direct audio URL. Nothing is downloaded.
///
/// The format selector is audio-only, and YouTube's player-stitched ads
/// (pre-roll, mid-roll) are separate media that never ride this stream:
/// mpv receives the song's own audio track, so playback carries no
/// platform ads without any ad-specific code. Sponsor reads inside the
/// content do remain; skipping those needs SponsorBlock (not included).
async fn resolve_yt_dlp(yt_dlp: &Path, query: &str) -> Result<yt_resolver::Resolved> {
    let mut command = yt_dlp_command(yt_dlp);
    command
        .arg("--ignore-config")
        .arg("--no-warnings")
        .arg("--no-playlist")
        .arg("--default-search")
        .arg("ytsearch")
        .arg("-f")
        .arg("bestaudio[ext=m4a]/bestaudio/best")
        .arg("-g")
        .arg("--")
        .arg(query)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(RESOLVE_TIMEOUT, command.output())
        .await
        .map_err(|_| anyhow!("yt-dlp took too long to answer"))?
        .context("unable to run yt-dlp")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("yt-dlp failed")
            .trim();
        return Err(anyhow!("YouTube could not answer for this song: {detail}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let url = stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("http"))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("YouTube has no audio for this song"))?;
    Ok(yt_resolver::Resolved {
        url,
        ttl: yt_resolver::DEFAULT_URL_TTL,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_without_metadata_waits_for_it() {
        let waiting = Entry::new("spotify:track:a".into(), None);
        assert!(waiting.waiting_for_metadata);
        assert!(!waiting.resolvable());

        let ready = Entry::new(
            "spotify:track:a".into(),
            Some(LocalTrack {
                uri: "spotify:track:a".into(),
                title: "Song".into(),
                ..LocalTrack::default()
            }),
        );
        assert!(!ready.waiting_for_metadata);
        assert!(ready.resolvable());
    }

    #[test]
    fn episodes_never_resolve() {
        let episode = Entry::new(
            "spotify:episode:e".into(),
            Some(LocalTrack {
                uri: "spotify:episode:e".into(),
                title: "Episode".into(),
                ..LocalTrack::default()
            }),
        );
        assert!(!episode.resolvable());
    }

    /// The context continues in order, wraps only for repeat-context, and
    /// shuffles to anywhere the pick names.
    #[test]
    fn the_next_context_position_follows_repeat_and_shuffle() {
        let in_order = |cursor| next_list_index(3, cursor, false, RepeatMode::Off, |len| len - 1);
        assert_eq!(in_order(None), Some(0));
        assert_eq!(in_order(Some(0)), Some(1));
        assert_eq!(in_order(Some(2)), None, "the list is over");
        assert_eq!(
            next_list_index(3, Some(2), false, RepeatMode::Context, |len| len - 1),
            Some(0),
            "repeat-context wraps"
        );
        assert_eq!(
            next_list_index(3, Some(1), true, RepeatMode::Off, |_| 2),
            Some(2),
            "shuffled playback takes the pick"
        );
        assert_eq!(
            next_list_index(0, None, false, RepeatMode::Off, |_| 0),
            None
        );
    }

    #[test]
    fn the_volume_maps_across_mpvs_full_range() {
        assert_eq!(volume_percent(0), 0.0);
        assert_eq!(volume_percent(u16::MAX), 100.0);
        assert!((volume_percent(u16::MAX / 2) - 50.0).abs() < 0.1);
    }

    #[test]
    fn repeat_track_replays_only_on_end_of_track() {
        // Next skips on: only the end-file handler consults repeat-track,
        // which is `end_file`'s job, tested through `next_list_index` for
        // the context and asserted here for the intent.
        assert_eq!(
            next_list_index(2, Some(1), false, RepeatMode::Track, |_| 0),
            None
        );
    }
}
