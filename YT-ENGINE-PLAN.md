# Musify (spotifast-yt) patch map: YouTube playback engine (`player_yt`)

Source: read-only analysis of `C:\Users\DELL\spotifast-yt` @ v0.13.2 (2026-10-14).
Policy note: upstream `AGENTS.md` forbids alternate audio sources (contribution/product policy);
this is a fork-level decision (MIT), keep the repo personal, do not report upstream.
Status: implemented, then partly superseded. The mpv backend this plan describes was removed
on 2026-10-20 (stage 3); the resolver, metadata and queue design stand.

## Key facts

- Free-account Web API browsing/search/playlists/lyrics/history already work with NO engine
  (`backend.rs:3508-3529` only tears down engine work; no premium checks under `src/api/`;
  requests fall through to the Web API in `backend.rs:4433-4957`). Only the audio path is missing.
- The app NEVER advances tracks itself; Spirc does (`player.rs:952-959` returns false for
  `EndOfTrack`; no `EndOfTrack` anywhere in `app.rs`). A YT engine must advance internally on
  EOF and push a new `LocalState` with `track_sequence+1`; the existing chain
  `handle_local` (`app.rs:2616-2734`) -> `on_now_playing_changed` (`app.rs:3010-3095`)
  then consumes queue rows exactly as today.
- Engine surface to mirror (`src/player.rs`):
  - `Engine::connect(config: &EngineConfig, proxy, credentials, cache, notify)` (`328-451`)
  - `PlayerCommand` variants (`279-300`): Toggle, Next, Previous, ClearQueue,
    AddToQueue(String), Seek(u32 absolute ms), Volume(u16), VolumePreview(u16),
    Shuffle(bool), Repeat(RepeatMode), Load(LoadSpec), Transfer.
  - `EngineEvent { State(LocalState), SessionEnded }` (`302-306`); notify only on change
    (`788-821`).
  - `LocalState` (`164-193`) invariants to copy: Loading clears error/sets loading (`851-862`);
    Playing/Paused set position+position_at and `start_replay` (`863-877`, `832-840`);
    Seeked bumps `seek_sequence` (`886-893`); TrackChanged replaces track, bumps
    `track_sequence`, sets `replay_pending` for same-URI replay (`894-906`); Unavailable sets
    error + clears loading (`907-927`); volume echo (`928`); `connected` stays true.
  - `LocalTrack.uri` MUST remain the original Spotify URI (app keys everything by it).
  - `LoadSpec` (`239-254`) carries no titles: add optional `LoadTrack` metadata
    (title, artists, duration_ms, is_episode, art) populated by the app at play time from
    `track_cache`; context-only loads (`local_load` `app.rs:11859-11889`) must be materialized
    via `context_track_uris` (`app.rs:7815-7845`).

## Insertion design: parallel engine field (smallest true diff)

- `Worker` gains `yt: Option<Arc<YtEngine>>` next to `engine: Option<Arc<Engine>>`
  (`backend.rs:1609`, init `1664-1730`). Every existing `self.engine` use keeps compiling and
  sees `None` for free accounts.
- New `Command::YtEngineConnected` mirroring `EngineConnected` (`backend.rs:691-698`,
  `2165-2177`) + `on_yt_engine_connected` modeled on `3384-3457` minus grant persistence.
- `Command::Player` routing (`backend.rs:2014-2028`): engine -> else yt -> else error string.
- `on_account_checked(Somefalse)) ` (free branch `backend.rs:3508-3529`) starts the YT engine
  instead of only emitting Failed. Keep the hard gate at `3306-3312` (librespot 0.8 `exit(1)`s
  for free accounts, see comment `3505-3507`).
- Free branches: `resume_engine` (`3030-3060`), `authorize_playback` (`3180-3213`).
- Resume: `take_engine_for_resume` (`3166-3175`), `verify_resume` (`3649-3673`).
- Restart/proxy: `3065-3074`, `2004-2010`, `1747-1751`.
- Reconnect/backoff: `3081-3151` (adjust early-returns `3031-3037`, `3134-3142`).
- Shutdown: `2978-2980`, `2501-2503`.
- App gates:
  - Suppress `Dialog::PremiumNeeded` for free (new flag) `app.rs:5790-5801`; update test
    `25577-25596`.
  - `Action::EnablePlayback` free branch (`app.rs:10445-10466`) sends the enable command
    instead of the Premium toast.
  - `play_request`/`play_queue_item` need NO change once `LocalState.connected == true` and
    `LocalPlayback::Ready` arrive (`app.rs:8030-8041`, `5070-5139`).
- Devices picker shows the this-computer row when `local_ready && device_id` (`ui/devices.rs:220-241`);
  `Transfer` no-op is fine but add pause-on-transfer policy (`app.rs:1355-1367`, `8640-8663`).
- Media overlay/tray/macOS menu come for free from `now_playing()` (`app.rs:3959-4027`).

## YtEngine module (`src/player_yt.rs`)

Surface: `connect(config, proxy, notify)`, `command(PlayerCommand)`, `shutdown`, `device_id`,
`heard`, `interrupted`, `resume_point`, `resume`. Reuse `LocalState`, `LocalTrack`, `Playback`,
`RepeatMode`, `PlayerCommand`, `EngineEvent`, `Notify`, `Interrupted`, `PlaybackResume`, `Heard`.

Internals: yt-dlp resolve per track (one flag-only call, no download), mpv child
(`--idle=yes --no-video --input-ipc-server=<unique pipe>`), JSON IPC tokio task; observe
`time-pos`, `duration`, `pause`, `eof-reached`, `idle-active`, `volume`; Toggle -> `cycle pause`;
Seek -> `set_property time-pos`; Volume -> `set_property volume` + echo; EOF -> advance
(manual queue first, then context list; Repeat Track/Context engine-side); pre-resolve next URL
near track end for approximate gapless.

Cargo: tokio add `process` feature; `net` already covers Windows named pipes (`io-util` present).

## Command mapping (YT mode)

Toggle/Next/Previous/Seek/Volume/VolumePreview/AddToQueue/ClearQueue/Load required;
Shuffle/Repeat engine-side; Transfer no-op; Previous mirrors librespot's 3s rule.

## Risks / gotchas

- Episodes out of scope -> `LocalState.error` (avoid the "This item isn't available" prefix,
  `app.rs:2673`).
- EOF: use `eof-reached`, debounce vs `idle-active`; no double-advance on seek-to-end; keep
  play_request_id-style filtering.
- Live/duration-less streams: disable seeking (`duration_ms = 0` breaks `position_now`).
- Queue panel polls Spotify's queue (`app.rs:5043-5063`, `5390-5410`) -> stale "Next up" in YT
  mode; consider gating.
- Visualizers/EQ tap is wired into librespot sinks (`player.rs:694-760`) -> silent/inert in YT
  mode unless PCM is routed through `AudioTap` (separate work, NOT v1).
- Volume assumptions: u16<->percent conversions (`app.rs:11744-11750`, `8538-8541`).
- Update tests + i18n catalogs that encode premium blocking (`app.rs:14771-14777`, `17996+`,
  `25577`; `backend.rs:5744+`, `5771`).
- YT uses its own proxy handling for yt-dlp/mpv; do not reuse `engine_proxy`.

## Ordered edit list

1. `Cargo.toml` tokio `process` feature (+ comment).
2. `src/lib.rs` add `pub mod player_yt;`.
3. New `src/player_yt.rs`.
4. `LoadSpec` + `LoadTrack` metadata (`player.rs:239-254`).
5. `app.rs` materialize tracks at play (`8030-8041`, `11859-11889`).
6. `app.rs` queue-add path while not yet active (`8836-8843`, `8915-8919`).
7. `app.rs` restore queue via `AddToQueue` in YT mode (`3028-3040`).
8. Suppress dialog + flag + tests (`5790-5801`, `25577-25596`).
9. `EnablePlayback` branch (`10445-10466`).
10. YT error vocabulary next to `PREMIUM_NEEDED` (`backend.rs:37`, `4202-4228`).
11. Worker field/init (`1609`, `1664-1730`).
12. `Command::YtEngineConnected` (`691-698`, `2165-2177`).
13. Player routing (`2014-2028`).
14. `resume_engine` free branch (`3030-3060`), `authorize_playback` (`3180-3213`).
15. `start_yt_engine` sibling (`3297-3382` area), `on_yt_engine_connected`.
16. `on_account_checked` (`3508-3529`).
17. Restart/proxy (`3065-3074`, `2004-2010`, `1747-1751`).
18. Reconnect/backoff (`3081-3151`).
19. Resume capture/verify (`3166-3175`, `3649-3673`).
20. Shutdown (`2978-2980`, `2501-2503`).
21. Tests (`backend.rs:5744+`, `app.rs:14771`, `17996+`, `25577`).
