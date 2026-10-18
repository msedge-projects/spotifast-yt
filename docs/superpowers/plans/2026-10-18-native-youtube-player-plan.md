# Native YouTube player: implementation plan

Date: 2026-10-18. Derived from
`docs/superpowers/specs/2026-10-18-native-youtube-player-design.md` (spec).
Repository: `msedge-projects/spotifast-yt`, work committed directly to `main`,
one topic per commit.

## Execution notes

- **Build loop**: this PC does not build the project (user constraint). CI is
  the compiler: `quality` (fmt, clippy both feature sets, docs) and `test`
  (four targets) run on every push; the Windows artifact `musify-windows-x64`
  builds too. `rustfmt` runs locally for syntax. Unless the user allows a
  local `cargo check` at kickoff, expect roughly 10-14 CI rounds.
- **Nix round**: the lockfile change re-rolls the `flake.nix` vendor hash;
  the first push after W1 is expected to fail only the nix job; read the
  reported `got:` hash and commit it immediately.
- **Fixtures**: ffmpeg is installed locally; fixture trimming can happen on
  this PC.
- **API checks without a compile**: the pinned librespot fork
  (`crmne/librespot`, rev `847125198bb251d1d4497f46bebbfa4868b7d5d7`) is not
  cached locally; read the needed files from
  `https://raw.githubusercontent.com/crmne/librespot/<rev>/...` when a work
  item needs them (`playback/src/decoder/mod.rs` for `AudioPacket`,
  `playback/src/convert.rs` for `Converter`, `playback/src/mixer/mod.rs` for
  `VolumeGetter`/`VolumeCtrl`, `playback/src/audio_backend/mod.rs` for
  `Sink`).
- **Repo rules honored**: regression tests per behavior change, no new
  settings (hidden verb + env switch only), dependencies from crates.io with
  `Cargo.toml` comments, docs updated when user-visible behavior changes.

## Stage 1: the probe (mpv untouched, default unchanged)

Exit gate: CI green on all four targets, and the user has heard several probe
songs plus a seek probe and says the sound is right.

### W1. Dependencies and module skeleton

- `Cargo.toml`:
  - `symphonia = { version = "0.5.5", default-features = false, features =
    ["aac", "isomp4", "mkv", "vorbis"] }`. Must stay 0.5.5 to share
    `symphonia-core` with librespot's copy; `mkv` and `vorbis` are declared
    explicitly because librespot's dependency does not enable them (today's
    lockfile has no `symphonia-format-mkv`).
  - `rusty-opus = "=1.0.2"` with a comment naming the swap path
    (`opus-decoder`, then C libopus as last resort).
- `src/yt_audio/mod.rs` with module documentation and empty submodules;
  `pub mod yt_audio;` in `src/lib.rs`.
- Acceptance: CI green after the nix-hash fix. No behavior change.

### W2. `yt_audio::source` (ranged HTTP stream)

- `RangeSource`: `Read + Seek + Send + Sync + MediaSource` over
  `reqwest::blocking`, per spec 4.2. Typed errors for expired (403), invalid
  range (416), and range-ignored (200 at offset > 0, fall back to discard).
  One pooled client per source; sequential open-ended `Range: bytes=N-`.
- Tests, no network: a local `TcpListener` stub serving canned bytes and
  honoring `Range`/`Content-Range`; cover sequential reads, forward and
  backward seeks re-issuing ranges, 206 vs 200 behavior, `byte_len`, error
  mapping.
- Acceptance: unit tests pass on all four targets.

### W3. `yt_audio::decode` and fixtures

- **W3a fixtures**: choose two short, license-clear audio sources (public
  domain or CC0, recorded in `src/testdata/audio/README.md` with source and
  license); fetch itag 140 and 251 with local yt-dlp; trim about three
  seconds with `ffmpeg -t 3 -c copy` (moov/section rewriting keeps each file
  demuxable); keep each fixture well under 100 KB; mark the directory binary
  in `.gitattributes`.
- **W3b decode** per spec 4.3: mime to container hint; `MediaSourceStream` +
  probe; decoder dispatch (AAC and Vorbis by symphonia, Opus by `rusty-opus`
  at 48 kHz stereo with pre-skip/CodecDelay discard); interleaved f32 via
  `SampleBuffer`; mono to stereo; rate conversion through
  `crate::resample::Resampler` when the source is not 44.1 kHz; 512-frame
  chunks as `AudioPacket::Samples` (f64); seek via `SeekMode::Accurate` +
  `decoder.reset()` + timestamp-based discard with the Opus 80 ms pre-roll;
  ten consecutive decode errors demote to stream failure.
- Tests: decode both fixtures from an in-memory `Read + Seek`; assert
  duration within tolerance, frame counts, non-silence (RMS), the 48k to
  44.1k path for Opus, chunk boundaries, and seek-discard arithmetic.
- Acceptance: tests pass on all four targets.

### W4. Sink chain constructor and accessors

- `src/sink.rs`: `AudioControl::seeked()` extracted from
  `handle_player_event` (single implementation, two callers);
  `played_frames()` returning played frames plus the output rate, fed by
  extending `register()`; keep the librespot path byte-identical in
  behavior.
- `src/sink.rs` or `src/player.rs`: `build_chain(...)` extracting the rodio
  path of `sink_builder` (Tapped over RodioSink, same arguments and flags);
  `SharedVolume` (`VolumeGetter` over an atomic level with the same cubic
  mapping as `player.rs:389-393`).
- Verify-first (GitHub raw reads): `AudioPacket::Samples`, `Converter::new`,
  `VolumeGetter` surface, `VolumeCtrl` usage.
- Tests: played counter absent before the first output; zeroed by `seeked()`;
  `SharedVolume` mapping equals the existing conversion; existing suite
  unchanged.
- Acceptance: green everywhere; Spotify mode untouched.

### W5. `yt_audio::worker`

- Worker thread per spec 4.4: `Load`/`Pause`/`Resume`/`Seek`/`Stop`/
  `Shutdown`; `Opened`/`Positioned`/`Ended`/`Failed`; `interrupt()`,
  `track_changed()`, `seeked()` drive the chain; position from the played
  counter with per-load/seek/renewal snapshots; EOF plus played time caught
  up emits `Ended`; pause parks through the chain's drain-and-park path.
- Tests with a recording fake sink (the `vis.rs` test pattern), no audio
  device: command sequencing, position math at a non-44.1k rate, EOF timing,
  load-after-interrupt gate behavior.
- Acceptance: tests green on all targets.

### W6. Resolver: maximum quality and candidates

- `src/yt_resolver.rs` per spec 4.7: score decodable audio formats by
  effective bitrate (known tiers 141, 251, 140, 250, 249, 139; unknown
  formats by `bitrate`, derived from `contentLength * 8 / duration` when
  absent); `Resolved` gains `mime`, `content_length`, and a ranked
  `fallbacks` list capped at three; yt-dlp selector becomes
  `bestaudio/best`; TTL handling unchanged.
- Tests: ranking table (including unknown-format scoring and the 139-last
  rule), fallback list excludes the chosen format, mime parsed from the
  yt-dlp URL's `mime=` parameter.
- Note: mpv still plays this stream fine, so listener quality improves the
  moment this lands.
- Acceptance: tests green; a manual probe (after W7) shows Opus picked for a
  typical song.

### W7. `probe-audio` command

- Plumbing exactly per the audited pattern (entrypoint `Control`,
  single-instance `ControlCommand` + parser + parser test, `model::Action`,
  `app` control-command handling and action apply, backend `Command` +
  dispatch + worker + `Event`, app event apply).
- Grammar: `probe-audio <query>` and `probe-audio seek=<seconds> <query>`;
  the query validated through the existing `search_text`.
- Behavior per spec 4.8: pause a playing YouTube engine (`PlayerCommand::
  Toggle` when the state says playing, restore after), resolve with the new
  ranking, build a chain and a worker, play, log the facts (mime, container,
  codec, rate, resample, first-audio latency, position every 5 s, seek
  timings, end line), start/finish toasts when a window is open.
- Tests: parser test; the routing test pattern the other control commands
  use.
- Acceptance: CI green; artifact installed; **then the user's listening
  round**: several normal probes and one with `seek=`, verdict recorded.
  This is the stage-1 exit gate.

## Stage 2: the swap (default becomes the new engine)

Exit gate: a stretch of real listening with no complaints, and the resource
table lands within spec 1 targets.

### W8. Engine rewiring

- `src/player_yt.rs` per spec 4.6: remove mpv runtime use (spawn, IPC,
  events), add `Message::Audio(AudioEvent)`, map `Positioned`/`Ended`/
  `Failed` into the existing state machine, keep resolve, cache, prefetch,
  skip budgets, and the failure surface. `CachedStream` carries mime,
  length, and fallbacks.
- Renewal: mid-stream failures re-resolve and resume at the same position
  (at most twice per track); a format that fails to open falls through the
  candidate list before any user-visible error.
- `MUSIFY_YT_PLAYER=mpv` env switch read once at engine connect
  (stage 2 only).
- Tests: adapt existing engine tests; new coverage for renewal budget,
  candidate fallthrough, `Ended` to advance.
- Verify-first: `AudioPacket`/`Converter` exact shapes via the GitHub raw
  reads listed in W4.
- Acceptance: green; artifact installed; user lives on it.

### W9. Parity hardening and the resource table

- Whatever the listening stretch surfaces (stalls, seek feel, odd streams);
  decide the spec 4.2 escape hatch (composite seek source) only if the ear
  says so.
- Resource harness, identical methodology on both backends, table per spec
  section 7: idle, paused, playing; RSS with mpv's process counted
  separately; paused CPU at the floor; playing CPU; cold and warm first
  sound.
- Acceptance: no recurring complaints; numbers meet or beat the targets;
  the table is reported to the user.

## Stage 3: surgery

Exit gate: no mpv references remain, CI green, final comparison reported.

### W10. Remove mpv entirely

- Delete the mpv branch of `find_tool`, spawn/IPC remnants, the env switch;
  update `src/ui/settings.rs:483, 518` copy and any README/docs mentioning
  mpv; yt-dlp stays required exactly as today.
- Tests: drop mpv-only tests; keep the rest.
- Acceptance: green; `grep -ri mpv src` clean except historical comments if
  any (none intended).

### W11. Wrap-up

- Final resource comparison and a plain-language summary for the user;
  confirm the four stage-1 goals are measured, not asserted.

## Open question for the user (kickoff)

- May the machine run a local `cargo check --locked` for this work (faster
  error loop, one-time dependency compile and a few GB of disk), or does the
  CI-only rule stand? Either is workable; CI-only roughly doubles the
  calendar time.
