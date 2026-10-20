//! A smoke probe for the YouTube playback engine.
//!
//! The probe drives `YtEngine` through the same commands the interface
//! sends and checks what comes back: the connect handshake, a two-song
//! load, the playing state, a seek, a volume set, pause and resume, the
//! end-of-song advance to the second entry, and a queued song playing on
//! Next. It needs `yt-dlp` where the engine finds it.
//!
//! With a real yt-dlp the probe resolves (and plays) two real songs:
//!
//! ```text
//! cargo run --example yt_probe
//! ```
//!
//! For a fast, deterministic run, put a stub `yt-dlp` first on PATH that
//! answers with a URL to a short local stream, and a `yt-dlp`-resolved
//! song length the probe expects:
//!
//! ```text
//! PATH=/path/to/stub/bin:$PATH cargo run --example yt_probe
//! ```
//!
//! Environment: `MUSIFY_YT_PROBE_TITLE_A` / `_B` change the search titles
//! (defaults "Musify probe A" / "Musify probe B"); `MUSIFY_YT_PROBE_WAIT_SECS`
//! caps every wait (default 30); `MUSIFY_YT_PROBE_SKIP_ADVANCE` skips the
//! wait for the first song to end, for long real songs; with a stub on
//! PATH, `MUSIFY_YT_PROBE_STUB_DIR` (the stub's directory) enables the
//! phase that fails one resolve on purpose and plays it on the retry.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use musify::player::{
    EngineConfig, EngineEvent, LoadSpec, LocalState, LocalTrack, Playback, PlaybackResume,
    PlayerCommand,
};
use musify::player_yt::YtEngine;
use musify::settings::ProxyConfig;
use musify::vis::AudioTap;

fn env_secs(name: &str, fallback: u64) -> Duration {
    Duration::from_secs(
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(fallback),
    )
}

/// Waits for a state the predicate accepts, printing the states on the way.
fn wait_for(
    events: &mpsc::Receiver<EngineEvent>,
    timeout: Duration,
    what: &str,
    mut done: impl FnMut(&LocalState) -> bool,
) -> Result<LocalState> {
    let deadline = Instant::now() + timeout;
    let mut last: Option<LocalState> = None;
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(EngineEvent::State(state)) => {
                if done(&state) {
                    return Ok(state);
                }
                last = Some(state);
            }
            Ok(EngineEvent::SessionEnded) => bail!("the engine ended while waiting for {what}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the event stream closed"),
        }
    }
    bail!(
        "timed out waiting for {what}; last state: {:?}",
        last.map(|state| (state.playback, state.track.map(|track| track.title)))
    );
}

/// Checks the resume point of a playing engine: the backend hands it to a
/// replacement engine, and it must carry the song's metadata, or the
/// replacement parks the row and the app never answers for a cached song.
fn some_resume_metadata(engine: &YtEngine) -> Result<()> {
    let Some(PlaybackResume::Track(spec)) = engine.resume_point() else {
        bail!("a playing engine has no resume point");
    };
    let Some(track) = spec.tracks.first() else {
        bail!("the resume point carries no metadata");
    };
    if track.title.is_empty() {
        bail!("the resume point's metadata has no title");
    }
    Ok(())
}

fn probe_track(uri: &str, title: String) -> LocalTrack {
    LocalTrack {
        uri: uri.into(),
        title,
        artists: vec![musify::api::models::ArtistRef {
            name: "Musify probe".into(),
            ..Default::default()
        }],
        duration_ms: 6_000,
        ..LocalTrack::default()
    }
}

fn main() -> Result<()> {
    let wait = env_secs("MUSIFY_YT_PROBE_WAIT_SECS", 30);
    let (sender, events) = mpsc::channel();
    let notify = Arc::new(move |event| {
        let _ = sender.send(event);
    });

    let config = EngineConfig {
        device_name: "Musify probe".into(),
        bitrate_kbps: 320,
        normalisation: false,
        autoplay: false,
        gapless: true,
        backend: None,
        audio_device: None,
        initial_volume: 32768,
        volume_dir: std::env::temp_dir().join("musify-yt-probe"),
        audio_cache_dir: None,
        audio_cache_limit: None,
        buffer_ms: musify::sink::DEFAULT_BUFFER_MS,
        tap: AudioTap::new(),
        eq: musify::eq::shared(),
        proxy: ProxyConfig::Off,
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let engine = runtime.block_on(YtEngine::connect(&config, notify))?;
    println!("connected as {}", engine.device_id());

    let title =
        |key: &str, fallback: &str| std::env::var(key).unwrap_or_else(|_| fallback.to_string());
    let a = probe_track(
        "spotify:track:probe-a",
        title("MUSIFY_YT_PROBE_TITLE_A", "Musify probe A"),
    );
    let b = probe_track(
        "spotify:track:probe-b",
        title("MUSIFY_YT_PROBE_TITLE_B", "Musify probe B"),
    );

    engine.command(PlayerCommand::Load(LoadSpec {
        uris: vec![a.uri.clone(), b.uri.clone()],
        tracks: vec![a.clone(), b.clone()],
        play: true,
        ..LoadSpec::default()
    }))?;
    let playing_a = wait_for(&events, wait, "the first song to play", |state| {
        state.playback == Playback::Playing
            && state.track.as_ref().is_some_and(|track| track.uri == a.uri)
    })?;
    println!("A is playing");

    // The playing song's metadata must survive an engine replacement:
    // the resume point is what the backend hands the next engine.
    some_resume_metadata(&engine)?;
    println!("the resume point carries the playing song's metadata");

    engine.command(PlayerCommand::Seek(1_000))?;
    wait_for(
        &events,
        Duration::from_secs(5),
        "the seek to answer",
        |state| state.seek_sequence > 0 && state.position_ms >= 900,
    )?;
    println!("seek answered");

    engine.command(PlayerCommand::Volume(12_000))?;
    wait_for(
        &events,
        Duration::from_secs(5),
        "the volume to echo",
        |state| state.volume == 12_000,
    )?;
    engine.command(PlayerCommand::Toggle)?;
    wait_for(&events, Duration::from_secs(5), "the pause", |state| {
        state.playback == Playback::Paused
    })?;
    engine.command(PlayerCommand::Toggle)?;
    wait_for(&events, wait, "the resume", |state| {
        state.playback == Playback::Playing
    })?;
    println!("volume and pause round-tripped");

    if std::env::var_os("MUSIFY_YT_PROBE_SKIP_ADVANCE").is_none() {
        wait_for(
            &events,
            wait,
            "the first song to end and B to start",
            |state| {
                state.playback == Playback::Playing
                    && !state.loading
                    && state.track.as_ref().is_some_and(|track| track.uri == b.uri)
            },
        )?;
        println!("the end of A advanced to B");

        // Within three seconds of the start, Previous goes back to the
        // song before; A is alone before B, so it is A again.
        engine.command(PlayerCommand::Previous)?;
        let back_on_a = wait_for(&events, wait, "Previous to go back to A", |state| {
            state.playback == Playback::Playing
                && !state.loading
                && state.track.as_ref().is_some_and(|track| track.uri == a.uri)
                && state.track_sequence > playing_a.track_sequence
        })?;
        println!("Previous went back to A");

        // Repeat-track: the song plays again on its own end.
        engine.command(PlayerCommand::Repeat(musify::player::RepeatMode::Track))?;
        engine.command(PlayerCommand::Seek(5_500))?;
        wait_for(&events, wait, "track repeat to replay A", |state| {
            state.playback == Playback::Playing
                && !state.loading
                && state.track.as_ref().is_some_and(|track| track.uri == a.uri)
                && state.track_sequence > back_on_a.track_sequence
        })?;
        println!("repeat-track replayed A");

        // Repeat-context: the list wraps back to its first song.
        engine.command(PlayerCommand::Repeat(musify::player::RepeatMode::Off))?;
        engine.command(PlayerCommand::Repeat(musify::player::RepeatMode::Context))?;
        engine.command(PlayerCommand::Seek(5_500))?;
        wait_for(&events, wait, "repeat-context to wrap to B", |state| {
            state.playback == Playback::Playing
                && !state.loading
                && state.track.as_ref().is_some_and(|track| track.uri == b.uri)
        })?;
        engine.command(PlayerCommand::Repeat(musify::player::RepeatMode::Off))?;
        println!("repeat-context wrapped the list");
    }

    engine.command(PlayerCommand::AddToQueue {
        uri: a.uri.clone(),
        track: Some(a.clone()),
    })?;
    engine.command(PlayerCommand::Next)?;
    wait_for(&events, wait, "queued A to play on Next", |state| {
        state.playback == Playback::Playing
            && !state.loading
            && state.track.as_ref().is_some_and(|track| track.uri == a.uri)
    })?;
    println!("queued A played on Next");

    // A row the app has no metadata for parks as loading; the
    // `SetTrackMetadata` answer plays it, the way the app answers a
    // deferred queue item it fetched.
    let c = probe_track("spotify:track:probe-c", "Musify probe A".into());
    engine.command(PlayerCommand::Load(LoadSpec {
        uris: vec![c.uri.clone()],
        play: true,
        ..LoadSpec::default()
    }))?;
    let parked = wait_for(&events, wait, "the metadata-less row to park", |state| {
        state.loading && state.track.as_ref().is_some_and(|track| track.uri == c.uri)
    })?;
    if !parked
        .track
        .as_ref()
        .is_some_and(|track| track.title.is_empty())
    {
        bail!("the parked row already had metadata");
    }
    engine.command(PlayerCommand::SetTrackMetadata {
        uri: c.uri.clone(),
        track: c.clone(),
    })?;
    wait_for(
        &events,
        wait,
        "the metadata answer to play the row",
        |state| {
            state.playback == Playback::Playing
                && !state.loading
                && state.track.as_ref().is_some_and(|track| track.uri == c.uri)
        },
    )?;
    println!("a metadata-less row waited and played on SetTrackMetadata");

    // A failed resolve must not leave the play button lying: the state
    // shows the error, and pressing play again starts the song (the stub
    // fails once on purpose).
    if let Some(stub) = std::env::var_os("MUSIFY_YT_PROBE_STUB_DIR") {
        let marker = std::path::PathBuf::from(stub).join("fail-once");
        std::fs::write(&marker, b"")?;
        let recover = probe_track("spotify:track:probe-recover", "Musify probe recover".into());
        engine.command(PlayerCommand::Load(LoadSpec {
            uris: vec![recover.uri.clone()],
            tracks: vec![recover.clone()],
            play: true,
            ..LoadSpec::default()
        }))?;
        wait_for(&events, wait, "the failed resolve to report", |state| {
            state.error.is_some() && !state.loading
        })?;
        println!("a failed resolve reports its error");
        engine.command(PlayerCommand::Toggle)?;
        wait_for(&events, wait, "play to start the song again", |state| {
            state.playback == Playback::Playing
                && !state.loading
                && state
                    .track
                    .as_ref()
                    .is_some_and(|track| track.uri == recover.uri)
        })?;
        println!("play after the failure started the song");
    }

    engine.shutdown();
    println!("PROBE PASS");
    Ok(())
}
