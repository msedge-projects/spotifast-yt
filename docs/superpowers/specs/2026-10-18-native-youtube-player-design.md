# Musify's own YouTube player: in-process playback engine

Date: 2026-10-18. Status: approved in chat; this document is the input to the
implementation plan. Stage 3 (deleting the mpv backend) landed on 2026-10-20,
so the removal planned here is complete. Repository: the
`msedge-projects/spotifast-yt` fork.

## 1. Goal

Replace the `mpv` child process with an in-process playback engine for
YouTube mode:

1. **No mpv process.** mpv is no longer required, discovered, or started in
   normal use.
2. **Same feel.** Playing, pausing, seeking, skipping, and volume behave at
   least as well as with mpv; no new glitches or delays.
3. **Visualizers and EQ join in.** YouTube audio flows through the app's own
   audio chain (equalizer, visualizer tap, limiter, device handling), exactly
   as Spotify audio does.
4. **Lighter by multiples.** No mpv process exists at idle or while playing;
   the engine thread parks when paused. Targets, measured with the same
   harness on both engines: idle and paused cost improves several-fold (three
   or more; mpv's resident process is gone), first sound on a cold engine is
   faster by the spawn and handshake savings (once warm, no worse than mpv),
   and playback CPU measures at or below mpv's.
5. **Maximum quality.** Always the best stream YouTube offers for the song
   (Opus around 160 kbps ahead of AAC 128 kbps; a 256 kbps m4a leads if
   ever served), decoded directly with no lossy re-encode. Rate conversion
   uses only the existing stateful polyphase-sinc resamplers: one at decode
   when the source is not 44.1 kHz (Opus always), plus the output-stage one
   the chain already applies for the device's own rate. EQ and limiter
   behavior stay exactly as Spotify mode's.
6. **No audio work when silent.** Paused or idle: the decode thread parked,
   the device parked, measured audio CPU at the measurement floor (zero
   within timer resolution).

Parity rule (the user's bar): whatever YouTube actually serves for audio must
play, and at the best quality it serves it. In practice that is Opus in WebM
and AAC in fragmented MP4, plus the rare legacy Vorbis in WebM, with seeking
and stall recovery.

## 2. Non-goals

- No changes to Spotify mode or to search, library, or general UI. The only
  user-visible change is YouTube audio behavior and, in stage 3, the removal
  of mpv from the setup copy.
- No gapless playback promise. Track transitions keep today's small gap; the
  architecture leaves room for prefetch later.
- No new settings (repo policy). During stage 2 only, a hidden environment
  switch can bring the old engine back; stage 3 deletes it.
- No per-song automatic mpv failover. Parity is the goal; the hidden switch
  is the only escape hatch while it exists.
- No EQ or visualizer changes. They simply start receiving YouTube audio.

## 3. Current state (what the design builds on)

### 3.1 The YouTube engine (`src/player_yt.rs`)

- An actor owning the queue, `LocalState`, and resolution. Its outward
  interface is snapshot-based: `EngineEvent::State(LocalState)` and
  `EngineEvent::SessionEnded` (`EngineEvent` itself is defined in
  `player.rs:330-333`; the engine produces both variants), consumed in
  `backend.rs:4131-4144` and applied in `app.rs`.
- mpv specifics to remove: tool discovery (`find_tool`, 1145-1199), process
  spawn and args (111-132), the named-pipe IPC (`ipc_path` 1202-1212,
  `connect_ipc` 1215-1237, writer/reader 371-416), JSON commands
  (`loadfile` 563/602, `seek` 977-979, `pause` 756/921/933, `volume` 986,
  `stop` 555/1074), and the event folding in `mpv_event` (735-822) and
  `property_change` (1012-1066).
- Worth keeping unchanged: `start`/`advance`/`previous`/`toggle`, the URL
  cache with per-stream TTL (`CachedStream` 298-302, `url_ttl` in
  `yt_resolver.rs:1290`), prefetch/warm (679-733), the resolve-failure skip
  budget of 3 (612-620), the one re-resolve rule (`retried_uri` 810-815),
  and the failure surface (`fail` 1079 writes `state.error`, which becomes
  the "Local playback: ..." toast via `app.rs:2436`).
- Position updates are throttled to 500 ms (`POSITION_NOTIFY_MS`, 47/1035)
  and the UI interpolates between them via `position_at` (`position_now`,
  220-232); the same cadence is kept.

### 3.2 The audio chain (`src/player.rs`, `src/sink.rs`, `src/vis.rs`)

- The Spotify path builds `Tapped` wrapping `RodioSink` (`sink_builder`,
  `player.rs:747-790`). `Tapped` runs EQ, then the visualizer tap, then the
  limiter, then hands audio to `RodioSink`, which resamples to the device
  rate and applies volume at output (`sink.rs:617-661`).
- Constructors are plain argument lists, so the same chain can be built
  outside librespot:
  - `RodioSink::new(device, on_error, volume, buffer_ms, control)`
    (`sink.rs:633-651`),
  - `Tapped::new(inner, control, tap, volume, applies_volume, eq,
    normalisation)` (`vis.rs:163-183`).
- `AudioControl` (`sink.rs:87-189`) already exposes `interrupt()`,
  `track_changed()`, `stopped()`, and `take_processing_reset()`; the
  `Seeked` arm of `handle_player_event` (117-128) is the seek transition we
  need publicly.
- `Queued` (`sink.rs:193-246`) already counts frames handed to rodio
  (`appended`) and frames finished (`consumed`); the difference is what is
  still queued. "Frames actually played" is derivable from it.
- A paused player costs no audio work because the device stream pauses when
  the queue drains (`sink.rs:719-739`, issue #636).
- The fixed source format across the chain is stereo 44.1 kHz
  (`NUM_CHANNELS`, `SAMPLE_RATE`), and a stateful polyphase resampler for
  other rates already exists (`src/resample.rs`).

### 3.3 The resolver (`src/yt_resolver.rs`)

- `pick_audio` (1256-1288) prefers itags `[140, 251, 139]` (m4a AAC-LC,
  WebM Opus, m4a HE-AAC) and otherwise any audio format with a direct URL.
- `Resolved { url, ttl }` (26-29); the TTL comes from the URL's own signed
  `expire` parameter (1290-1296). The yt-dlp fallback resolves the same URL
  shape (`player_yt.rs:1317-1358`).
- Two native failures arm a session latch that sends everything to yt-dlp
  (`yt_resolver.rs:244, 252-253, 271-303`).
- Known good news from research: googlevideo serves byte ranges
  (`Range:` header, optionally paired with a matching `&range=` query
  parameter; the header alone is the portable path), fragment offsets are
  seekable via the SIDX (m4a) and Cues (WebM) indexes, and URLs live about
  6 hours.

### 3.4 Repo constraints that shape the work

- Dependencies only from crates.io (`check-dependency-sources.py`); new
  crates need a comment in `Cargo.toml` and a concrete benefit.
- No new `Settings` fields (`check-new-settings.py`).
- CI: quality (fmt, clippy both feature sets, docs), tests on
  Windows x64/ARM64, macOS, Linux (`ci.yml`), and a `nix build` job that
  requires the `flake.nix` vendor hash to be refreshed whenever
  `Cargo.lock` changes.
- The Windows artifact this project installs for testing (`build.yml`) is
  built with `--no-default-features`, so new dependencies must build without
  projectm/CMake. Release builds use default features and must keep
  building too.
- Builds happen in CI, not on this PC; iteration is: commit, CI, download
  `musify-windows-x64`, install, test.

## 4. Design

### 4.1 Components

```
resolve (unchanged, yt_resolver)
   -> yt_audio::source   ranged HTTP stream as a symphonia MediaSource
   -> yt_audio::decode   probe + demux + decode + resample + packetize
   -> yt_audio::worker   a thread feeding the app's own sink chain
   -> sink chain         Tapped(EQ -> tap -> limiter) over RodioSink
   -> device             (fastframe-audio; volume, fades, pause, recovery)
```

New module `src/yt_audio/` with three files (`source.rs`, `decode.rs`,
`worker.rs`). `src/player_yt.rs` keeps its actor and queue and swaps the mpv
layer for the worker. `src/sink.rs` gains two small public accessors.
`src/player.rs` gains a shared chain constructor. `src/yt_resolver.rs`
carries two extra format facts. A hidden `probe-audio` command exercises the
whole path before anything switches over.

### 4.2 `yt_audio::source`: the ranged HTTP stream

- A `RangeSource` implementing `symphonia_core::io::MediaSource`
  (`Read + Seek + Send + Sync`, plus `is_seekable() -> true` and
  `byte_len()`).
- Built on `reqwest::blocking::Client` (the `blocking` feature is already
  enabled). One client per source; connection pooling gives the "reuse one
  connection" behavior that keeps seeks cheap.
- Reads are sequential GETs (`Range: bytes=N-`, open-ended). Audio streams
  are a few megabytes, well under the ~10 MiB region where googlevideo
  throttling kicks in for chunked downloads, and playback needs about
  16 KiB/s.
- `seek` drops the current response and issues a new ranged GET. If the
  server answers 200 (range ignored) at an offset > 0, fall back to reading
  from the start and discarding; that path is logged, not silent.
- `byte_len` comes from the resolver's content length when present, else
  from `Content-Range`/`Content-Length` on the first response.
- 403/416 (expired or invalid link) surface a typed error the worker turns
  into a renewal request.

### 4.3 `yt_audio::decode`: unwrap, decode, resample, packetize

- Probe with a container hint from mime (`.m4a`/`.mp4` vs `.webm`/`.mka`)
  through `get_probe().format(...)` with default options. Seek indexes fill
  while demuxing; `prebuild_seek_index` stays off (it would scan the whole
  remote file at open).
- Decoder dispatch on the track's codec:
  - **AAC** (itag 140) and **Vorbis** (legacy): symphonia's own decoders
    (`symphonia::default::get_codecs().make(...)`).
  - **Opus** (itag 251, WebM; the usual pick under the maximum-quality
    ranking): symphonia demuxes, and packets are decoded with a pure-Rust
    Opus decoder at 48 kHz stereo. This is the only place symphonia has no
    decoder of its own.
- Output normalization before packetizing:
  - mono to stereo (the chain's frame accounting expects two channels),
  - any non-44.1 kHz rate through the existing stateful resampler
    (`crate::resample::Resampler`), Opus always being 48 kHz.
  - Chunks of about 512 frames become `AudioPacket::Samples` (f64 stereo),
    matching librespot's 4-13 ms packet granularity so the sink's queue
    limits and backpressure keep their meaning.
- Seeking: `format.seek(SeekMode::Accurate, SeekTo::Time { .. })` lands at
  the last packet boundary at or before the target; `decoder.reset()` then
  decode-and-discard to the exact time using packet timestamps (never sample
  counting). For Opus, decoding starts at least 80 ms early (RFC 7845 4.6)
  so the decoder state converges, and the pre-skip/CodecDelay output is
  discarded.
- A single bad packet is discarded and logged (symphonia's own guidance);
  about ten consecutive failures demote to a stream failure, which the
  worker reports for renewal or failure.
- Quality and cost discipline: no lossy re-encode and no hidden processing;
  the only PCM steps are mono to stereo duplication, rate conversion
  through the existing stateful polyphase-sinc resampler when the source is
  not 44.1 kHz, then EQ and limiter exactly as Spotify mode. Decode buffers
  are reused across packets rather than reallocated per frame (the Opus
  decoder is allocation-free per frame after warm-up); nothing is fetched
  ahead beyond the sink's bounded queue, so memory stays flat.

### 4.4 `yt_audio::worker`: the thread that owns playback

- One dedicated `std::thread` owns the sink chain, the source, and a
  librespot `Converter`. Commands arrive over a channel (`Load`, `Pause`,
  `Resume`, `Seek`, `Stop`, `Shutdown`); events return over the actor's
  channel (`Opened { mime, container, codec, rate, resampled }`,
  `Positioned(ms)`, `Ended`, `Failed { stage, detail }`).
- `Load { url, format hint, duration, position_ms, seq }` opens the source,
  probes, seeks when position > 0, and streams packets into the chain.
  `control.interrupt()` runs first on user-requested changes (fade-out and
  queue discard, exactly like the Spotify skip path); the first write after
  a load opens the gate (`track_changed()`). Natural track changes do not
  fade (same as Spotify mode's gapless handoff).
- `Seek { ms, seq }` uses `control.seeked()` (the transition extracted from
  `handle_player_event`), reopens at the target, and resumes. The engine's
  existing `seek_sequence` guard keeps duplicates harmless.
- Pause stops feeding and parks the output through the same drain-and-park
  path with the transport fade that Spotify mode's pause uses
  (`sink.rs:719-739`); resume brings it back with the existing fade-in. No
  decoding happens until resume.
- Position accounting: the chain counts frames actually played
  (`AudioControl::played_frames()`, section 4.5) in device-rate frames, so
  the worker converts with the output's own rate (`frames / rate`) rather
  than assuming 44.1 kHz. The worker snapshots the counter whenever a track
  starts, seeks, or renews; position = the snapshot's base time plus the
  frames played since, published every 500 ms and on demand, preserving the
  UI's interpolation behavior. (Interrupt and seek replace the output,
  zeroing the counter; the snapshot covers natural track changes, which
  keep one continuous output.)
- End of track: when the source reports EOF, the worker waits until played
  time catches up to the decoded duration (minus one chunk's worth) and
  emits `Ended`, which the engine routes into the existing repeat/advance
  code.
- `Stop` drains briefly and pauses the device; `Shutdown` ends the thread
  (engine shutdown, reconnect, or the stage-2 escape switch).

### 4.5 Chain construction and the two added accessors

- `src/sink.rs`:
  - `AudioControl::seeked()` becomes public; `handle_player_event`'s
    `Seeked` arm delegates to it (one implementation, two callers).
  - `AudioControl::played_frames()` reads the current output's `Queued`
    (registered when the sink (re)opens, extending `register()`) and
    returns `appended - queued` together with the output's sample rate,
    since those frames are at the device rate (`sink.rs:778-800`). It is
    absent before the first output exists.
- `src/player.rs`: a shared constructor (`sink::build_chain(...)`) builds
  the same composition `sink_builder`'s rodio path builds today
  (`Tapped` over `RodioSink`, `applies_volume` flag copied from that path,
  the same volume getter for the limiter ceiling, the app's shared
  `AudioTap` and `SharedEq`). The YouTube engine receives the same audio
  device and buffer settings the librespot path uses, so both modes honor
  the same audio settings.
- Volume: a tiny `SharedVolume` implementing librespot's `VolumeGetter`
  over an atomic 16-bit level, using the same mapping as Spotify mode
  (`VolumeCtrl::Cubic`, `player.rs:389-393`). `PlayerCommand::Volume` and
  `VolumePreview` set it (replacing mpv's percent math,
  `player_yt.rs:1137-1141`), and `RodioSink` applies it at output exactly
  as in Spotify mode.
- Normalisation: YouTube audio has none; the chain is given the
  no-normalisation default (factor 1.0). The limiter and tap behavior are
  unchanged, so visualizers show the same post-EQ, pre-volume signal they
  show for Spotify.

### 4.6 Engine rewiring (`src/player_yt.rs`)

- Removed: mpv discovery, spawn, IPC, command plumbing, observers, and the
  mpv event folding. `find_tool` keeps only the yt-dlp branch (yt-dlp stays
  as the resolver fallback; the requirement to have it installed is
  unchanged).
- Added: the worker handle and `Message::Audio(AudioEvent)`:
  - `Positioned(ms)` updates `position_ms`/`position_at` and publishes on
    the existing 500 ms throttle.
  - `Ended` runs the existing end-of-track code (`end_file`'s eof path,
    781-792).
  - `Failed` first tries the existing one-re-resolve rule; mid-stream
    failures get up to two renewals that re-resolve and resume at the
    current position; when the budget is empty the existing `fail(message)`
    surface runs unchanged (toast "Local playback: ...").
  - `Opened` only clears any stale error and logs codec facts.
- `start` (511-586) keeps its shape: cached URL or fresh resolve, then a
  worker `Load`. Nothing about the "yt: rows play before metadata" path
  changes; packets flow as soon as metadata arrives, exactly as mpv's
  `loadfile` did.
- Duration: stream-reported duration (SIDX or track header) updates
  `track.duration_ms` as mpv's `duration` property did (1040-1062); the
  resolver's known duration remains the fallback.
- Worker death (panic) maps onto the existing disconnected path
  (`SessionEnded` -> backend `Reconnect`); a normal `Shutdown` does not.
- Stage 2 only: `MUSIFY_YT_PLAYER=mpv` selects the old backend, read once
  at engine connect. Default is the new engine. Stage 3 deletes the branch.

### 4.7 Resolver addition

- `Resolved` gains `mime: Option<String>` and `content_length:
  Option<u64>`, taken from the chosen format's `mimeType` and
  `contentLength`. The yt-dlp fallback derives mime from the URL's own
  `mime=` query parameter and leaves length unknown.
- The engine's own `CachedStream` (`player_yt.rs:298-302`) carries the same
  facts (mime, length, ranked fallbacks) next to the URL, so cache hits and
  prefetches still give the worker its hint and its fallback chain; a
  missing hint falls back to the URL's `mime=` parameter.
- Format preference changes from `[140, 251, 139]` to maximum quality:
  every audio format the engine can decode (AAC-LC, Opus, Vorbis, by
  mimeType) is scored by effective bitrate and the highest wins. Known
  tiers: `141` 256 kbps AAC-LC, `251` about 160 kbps Opus, `140` 128 kbps
  AAC-LC, `250` 70 kbps, `249` 50 kbps, and `139` (48 kbps HE-AAC) ranking
  last because at most a base LC core can decode. Unknown decodable formats
  score by their `bitrate` field, derived from `contentLength * 8 /
  duration` when the field is absent.
- The top candidates travel with the resolve (a short ranked list, capped at
  three), and a format that fails to open or decode falls through to the
  next candidate before anything user-visible happens (section 6). The
  yt-dlp fallback's selector becomes `bestaudio/best` for the same reason.

### 4.8 The probe command

`musify.exe probe-audio <query>` plays normally; `musify.exe probe-audio
seek=<seconds> <query>` additionally exercises seeking after 5 seconds of
playback. The control channel's one-argument format
(`single_instance.rs:289-342`) carries both as a single string, exactly the
way `play-search` splits its own argument; the query follows the existing
`search_text` validation (`single_instance.rs:357-362`). Wired exactly like
`backfill-durations` (entrypoint `Control`, single-instance
`ControlCommand` + parser + its test, `Action`, app control-command
handling, backend `Command`/dispatch/worker/`Event`, app event apply).

Behavior: stops current YouTube playback, resolves the query
(native-first, yt-dlp fallback), builds a chain of its own, and plays the
song through `yt_audio`, logging the facts the verdict needs:

- picked mime/format, probed container, codec, sample rate, resample on/off,
- first-audio latency (resolve end to first packet written),
- a position line every 5 seconds,
- when `seek` is given, one seek after 5 seconds with before/after
  positions and the time the seek took,
- at the end: `Probe: ended at {position} of {duration}` plus any decode
  errors.

The CLI reply stays the existing fire-and-forget "ok"; results live in the
log and in start/finish toasts when the window is open. The probe is a
hidden maintenance verb, not a user feature.

### 4.9 Dependencies

- `symphonia = { version = "0.5.5", features = ["aac", "isomp4"] }`:
  must stay 0.5.x to share `symphonia-core` with librespot's copy. Default
  features keep mkv and vorbis available. Commented in `Cargo.toml` as the
  YouTube unwrap/decode path.
- `rusty-opus = "=1.0.2"`: pure-Rust Opus decoder, zero dependencies,
  BSD-3-Clause, pinned exactly (the research above saw 1.0.1; the lock
  resolved the newer 1.0.2 patch). Fallbacks if it disappoints, in order:
  `opus-decoder` (pure Rust) or the C libopus bindings (adds CMake; last
  resort). The probe validates it against real streams; the fixture test
  validates it in CI.
- No other new crates. `reqwest` already has `blocking`; tokio and the
  rest cover the worker's channels.

## 5. Behavior mapping (parity)

| mpv behavior today | new engine |
|---|---|
| `loadfile` a direct URL | range stream, probe, decode, packets |
| play / pause | worker feed gate; device parks on pause (#636) |
| absolute seek | accurate seek + decode-skip (escape hatch below) |
| volume percent | shared volume, identical mapping to Spotify mode |
| eof -> next | EOF + queue drained -> `Ended` -> existing advance |
| mid-stream error | re-resolve and resume at position (mpv cannot) |
| one re-resolve per track | same rule, reused |
| position / duration observers | played-frames counter, same 500 ms cadence |
| device error reporting | unchanged (`RodioSink` `on_error` path) |

## 6. Error handling

- Resolve failures: unchanged (skip up to 3 rows, then the existing error
  toast).
- Open/probe failure on a fresh resolve: one re-resolve, then fail, exactly
  like mpv's failed-load path.
- Mid-stream read/decode failure: renewal (re-resolve + resume at the same
  position), at most twice per track, then fail.
- Single bad packets: discarded and logged; ten in a row counts as a stream
  failure.
- A format that fails to open or decode (for example, if the LC-only
  decoder rejects an HE-AAC stream) falls through to the next ranked
  candidate from the resolve; only when every candidate is exhausted does
  the existing failure surface run, naming the format.
- Worker panic: the existing disconnected/reconnect path.
- Device failures: unchanged, already routed through the sink's error hook.

## 7. Testing

- Unit tests (inline, repo style):
  - `source`: range math and seek behavior against a tiny local HTTP
    server honoring `Range`; expired-link error mapping.
  - `decode`: hint selection, mono to stereo, 48k to 44.1k resampling,
    packet chunking, Opus pre-roll discard arithmetic, consecutive-failure
    demotion.
  - `worker`: state transitions with a recorded sink (the pattern exists in
    `vis.rs` tests); position math; EOF -> `Ended` timing.
  - engine: existing `player_yt` tests adapted where mpv-coupled; new
    tests for renewal budget and `Ended` -> advance.
- Fixtures: two small real files in `src/testdata/audio/` (a few seconds of
  AAC m4a and Opus WebM from freely licensed content, each well under
  100 KB, marked binary in `.gitattributes`). Tests decode them from an
  in-memory `Read + Seek` source and assert duration within tolerance,
  frame counts, and non-silence. They run on all four CI targets.
- Manual acceptance via the probe: several songs, one seek run; then
  skipping, pausing, volume, and a normal listening session on the new
  engine.
- Resources: the same harness used before, with identical methodology on
  both engines, reported as a comparison table and held against section 1's
  targets: app idle with nothing playing, paused, and playing; total RSS
  (with mpv's own process counted separately while it exists; several-fold
  improvement expected where mpv would be resident); paused CPU at the
  measurement floor; playback CPU at or below mpv's; first sound measured
  cold (first song after app start) and warm (subsequent songs). The
  numbers are part of the acceptance verdict.

## 8. Rollout stages

1. **Probe** (mpv untouched, default unchanged): `yt_audio`, the shared
   chain constructor, the accessors, fixtures, and the `probe-audio` verb.
   Exit: CI green on all targets; the user has listened to several probes
   and a seek probe and says the sound is right.
2. **Swap**: the engine runs on the worker by default; the environment
   switch keeps mpv reachable. Hardening: renewals, stalls, odd streams,
   resource comparison. Exit: a stretch of real listening with no
   complaints.
3. **Surgery**: delete the mpv backend, `find_tool`'s mpv branch, the
   environment switch, and every piece of setup copy that mentions mpv
   (`ui/settings.rs:483, 518`; `player_yt.rs:100-105`; README/docs as
   found). Re-run the resource comparison and report.

## 9. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Symphonia's fragmented-MP4 seeking walks fragments and keeps parsed moofs in memory (upstream #586); seek speed unknown | Measure with the probe's seek test. Escape hatch, only if needed: a composite source (cached init segment + `Range` splice + moof resync, the ffmpeg trick) documented but not built. Streams are only a few MB. |
| WebM seeking without Cues relies on the index build during demux | Accuracy is guaranteed by decode-skip regardless; only speed is at stake. Measured by the probe. `prebuild_seek_index` stays off to avoid a full remote scan at open. |
| Opus pre-skip / CodecDelay handling | Fixture duration test in CI; discard logic uses demuxer timestamps; RFC 7845 pre-roll on seek. |
| HE-AAC (itag 139) may not open in the LC-only decoder | It ranks last, and the candidate fallback chain moves to the next format automatically; a song serving only 139 is a documented gap (mpv, on ffmpeg, would play it). |
| `rusty-opus` is young | Exact version pin; real-stream probe; swap path to `opus-decoder` or C libopus documented in `Cargo.toml`. |
| Opus-first default raises the stakes on the young Opus decoder, the WebM demuxer, and the 48k to 44.1k resample | Both decode paths exist and are fixture-tested either way; the probe exercises Opus explicitly; flipping back to AAC-first is a one-line change if the ears say so. |
| googlevideo throttling | Bitrate needs are tiny; open-ended ranges; chunked ranges with matching `&range=` parameter only if a stall is ever seen. |
| `flake.nix` vendor hash after the lockfile change | Expected one extra CI round; read the hash from the failing nix job and commit it. |
| Position accuracy vs mpv | Counter-based position with fixture-tested math; the user's ear is the final check. |

## 10. Verification items for implementation

- librespot 0.8 (pinned fork): confirm `AudioPacket::Samples` construction,
  `Converter::new()`, the exact `VolumeGetter` trait surface, and the
  `Sink::write` contract as used in `sink.rs:741`.
- symphonia 0.5.5: confirm how the mkv demuxer exposes CodecDelay and
  whether fMP4 seeks re-read the file header (the probe answers this
  empirically).
- Whether the AAC decoder opens an HE-AAC (itag 139) stream at all; if it
  errors, the candidate fallback chain covers normal songs and the gap is
  only songs that serve 139 alone.
- Where the app's shared `AudioTap`/`SharedEq` and audio settings reach
  `start_yt_engine` today, so the chain constructor is wired with the same
  instances (`app.rs:826-827, 12828-12845`; `backend.rs:8556`).
- Fixture licensing and size before committing them.

## 11. Cleanup checklist (stage 3)

- `src/player_yt.rs`: mpv branch of `find_tool`, spawn/IPC code, env switch.
- `src/ui/settings.rs:483, 518`: copy loses "mpv"; yt-dlp stays.
- README and docs mentions of mpv.
- One commit series: deps + pipeline; probe verb; engine swap; surgery.
