//! A smoke probe for the YouTube playback engine.
//!
//! The probe drives `YtEngine` through the same commands the interface
//! sends and checks what comes back: the connect handshake, a two-song
//! load, the playing state, a seek, a volume set, pause and resume, the
//! end-of-song advance to the second entry, and a queued song playing on
//! Next. It needs `mpv` and `yt-dlp` where the engine finds them.
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
//! PATH=/path/to/stub/bin:$PATH MPV_HOME=/tmp/mpv-null cargo run --example yt_probe
//! ```
//!
//! Environment: `MUSIFY_YT_PROBE_TITLE_A` / `_B` change the search titles
//! (defaults "Musify probe A" / "Musify probe B"); `MUSIFY_YT_PROBE_WAIT_SECS`
//! caps every wait (default 30); `MUSIFY_YT_PROBE_SKIP_ADVANCE` skips the
//! wait for the first song to end, for long real songs.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use musify::player::{
    EngineConfig, EngineEvent, LoadSpec, LocalState, LocalTrack, Playback, PlayerCommand,
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
    wait_for(&events, wait, "the first song to play", |state| {
        state.playback == Playback::Playing
            && state.track.as_ref().is_some_and(|track| track.uri == a.uri)
    })?;
    println!("A is playing");

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
                    && state.track.as_ref().is_some_and(|track| track.uri == b.uri)
            },
        )?;
        println!("the end of A advanced to B");
    }

    engine.command(PlayerCommand::AddToQueue {
        uri: a.uri.clone(),
        track: Some(a.clone()),
    })?;
    engine.command(PlayerCommand::Next)?;
    wait_for(&events, wait, "queued A to play on Next", |state| {
        state.playback == Playback::Playing
            && state.track.as_ref().is_some_and(|track| track.uri == a.uri)
    })?;
    println!("queued A played on Next");

    engine.shutdown();
    println!("PROBE PASS");
    Ok(())
}
