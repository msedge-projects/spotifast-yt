# HANDOFF — spotifast-yt (YouTube playback engine fork)

Session handoff for continuing this project on another machine. Everything needed is in this repository. Read this file, then `YT-ENGINE-PLAN.md` (the full implementation map with file:line references), then `notes/mpv-spike/` (raw integration evidence).

## Mission

A personal fork of Spotifast (native Rust Spotify client, egui UI), renamed to **Musify**, that lets a **free (non-Premium) Spotify account** play music, with audio streamed from YouTube instead of Spotify's servers. UI, browsing, playlists, and everything else stay the upstream client's. Premium accounts keep the unmodified librespot engine. This fork is **personal**: upstream's `AGENTS.md` forbids alternate audio sources (that is a contribution/product policy for crmne's repository, not a license restriction; the fork is MIT). Do not upstream it; do not file issues about it.

## Non-negotiable constraints (from the user)

- Free tier only; no Premium anywhere.
- CPU is the hard constraint (machine: 2014-era i3-4150, 2C/4T). The user rejected download/transcode pipelines (ffmpeg bursts). The accepted design is a ~1 s flag-only `yt-dlp` resolve per track change plus ~1–3% of one core while mpv decodes.
- ("no yt-dlp" was the user's wish, **proven impossible** — see Decisions. Do not retry removing it; do not propose heavier alternatives.)
- Native Rust, light; no Electron; the UI stays the upstream client's.
- Builds happen via GitHub Actions, not local compilation ("instead of compiling in my pc ... use actions").

## Where things are

- Repo (public): https://github.com/msedge-projects/spotifast-yt — account `msedge-projects` (gh CLI). On the new machine: `gh auth login` (that account), then `gh repo clone msedge-projects/spotifast-yt`.
- Base: crmne/spotifast v0.13.2, full history; `upstream` remote points at crmne/spotifast — never push there.
- `YT-ENGINE-PLAN.md` — implementation map: exact files, line refs, ordered edit list, risks. **Read first.**
- `notes/mpv-spike/` — snapshot of the yt-dlp + mpv integration proof from the original machine: exact `yt-dlp` output shapes, mpv JSON IPC log, the scripts used. Re-run a small spike on the new machine if anything is unclear.
- `.github/workflows/build.yml` — the CI that builds the exe. Upstream also ships ~9 other workflows that fire in this fork (board, docs, release, issue triage...); ignore their noise or trim them. `ci.yml` is upstream's test suite if you want free test coverage.

## State (2026-10-15)

- On top of upstream: the Windows build workflow, the README notice, the
  handoff docs, the **Musify rename** (`cb10ba3`; red branding, binary
  `musify.exe`, artifact `musify-windows-x64`, app id
  `io.github.msedge_projects.Musify`, fresh data folders), the rename's
  follow-up fixes (`0e30835` tray template mask, `8814d66` flake vendor
  hash, `0ee3d5c` Packaging trigger), and the **YouTube engine**
  (`b6d4466`, `src/player_yt.rs`).
- Free accounts route to the YouTube engine automatically once the account
  check answers; Premium keeps the untouched librespot engine. Metadata
  rides `LoadSpec::tracks`, `AddToQueue { uri, track }` and the deferred
  `SetTrackMetadata`; the engine owns queue advancement. In YouTube mode
  the queue panel is built from app state (the user's rows, then the rest
  of the context); Spotify's queue endpoint is not polled for it.
- Verified locally (Linux, `--no-default-features`): the full test suite
  (1185 lib tests plus the integration suites), `--all-features` clippy and
  rustdoc with `-D warnings`, and `examples/yt_probe.rs` end to end with a
  stub yt-dlp and mpv 0.37 (connect, resolve, play, seek, volume,
  pause/resume, end-of-track advance, queued Next, deferred
  `SetTrackMetadata`).
- The Windows artifact builds green, carries the red icon at every size
  (verified inside the exe), and imports no MSVC runtime. The artifact of
  record is the `build` run on the final green `main` commit.
- Ads: the resolved stream is audio-only, so YouTube's player-stitched ads
  never reach mpv; in-content sponsor reads remain (SponsorBlock would be a
  separate, explicit feature). Faking Premium stays out.
- Not yet run: the real YouTube resolve from the fork's UI on the user's
  Windows machine with real yt-dlp + mpv, and any macOS runtime testing.

The workflow (`.github/workflows/build.yml`) is:

```yaml
name: build
on:
  push:
    branches: [main]
  workflow_dispatch:
jobs:
  windows:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: Swatinem/rust-cache@v2
      - name: Build
        run: cargo build --release --no-default-features
      - uses: actions/upload-artifact@v4
        with:
          name: musify-windows-x64
          path: target/release/musify.exe
          if-no-files-found: error
```

## Decisions already made (do not relitigate)

1. **Native, no-subprocess YouTube streaming is impossible.** Proven live from the user's IP: every InnerTube client profile (Android / Android VR / Android Music / iOS / Web / TV-embed / Web-embed) answered HTTP 400 or `LOGIN_REQUIRED` ("Sign in to confirm you're not a bot") — YouTube's playback attestation (PoToken) wall. yt-dlp is the resolver in every workable design; its upkeep is the yt-dlp community's job.
2. **Architecture (user approved):** per track change, `yt-dlp` resolves a direct audio URL in flag-only mode (`-g`; no download, no ffmpeg); an **mpv child process** decodes/plays it, driven over mpv's JSON IPC (Windows named pipe). This is Spotube's architecture rebuilt inside Musify.
3. **Engine placement:** a second engine (`src/player_yt.rs`, `YtEngine`) lives BESIDE the librespot engine (`Worker.yt` in `backend.rs`). Free accounts route to it; the Premium path is untouched. Free accounts' Web API browsing/search/playlists already work without any engine (verified in code) — only audio was gated.
4. **The app never advances tracks** — librespot's Spirc does that today. The YtEngine must own queue advancement (mpv EOF → next item, push `LocalState` with `track_sequence+1`); the app's existing `on_now_playing_changed` then consumes queue rows exactly as for Premium. `docs/_reference/queue.md` is the contract.
5. **Metadata flow:** the engine cannot resolve `spotify:track:` URIs itself (no Web tokens). `LoadSpec` gains `tracks: Vec<LocalTrack>`; `PlayerCommand::AddToQueue` becomes `AddToQueue { uri, track: Option<LocalTrack> }`. For items that still lack metadata when their turn comes (long playlists), **defer**: park the item (Loading + placeholder track) and add `PlayerCommand::SetTrackMetadata { uri, track }`; the app already fetches `ApiRequest::Track` for a newly playing uncached uri (app.rs ≈3074-3081, cache insert ≈7215) — in YT mode it forwards the fetched metadata to the engine, which then resolves and plays. (v1 fallback if needed: rely on `track_cache` only and error on missing metadata — playlists will be hit-or-miss; the deferred design is the correct one.)
6. Adopted from upstream's `AGENTS.md` anyway: network/playback work on the runtime (never block the UI thread); focused changes; tests updated in the same change; honest platform-coverage claims.
7. Do not touch either company's server-side gates: no Spotify credential forging, no YouTube attestation circumvention. yt-dlp + mpv act client-side against public streams; keep it that way.

## Implementation summary (implemented in `b6d4466`; full detail in YT-ENGINE-PLAN.md)

- `Cargo.toml`: tokio features += `"process"` ("net" already covers Windows named pipes).
- `src/lib.rs`: `pub mod player_yt;`.
- `src/player.rs`: `LoadSpec.tracks`; new `AddToQueue { uri, track }` payload; add `Heard::from_state(Arc<Mutex<LocalState>>)` pub(crate) constructor (its field is private; the YT engine hands volume over with it).
- NEW `src/player_yt.rs`: `YtEngine` mirroring `Engine`'s surface (connect / command / shutdown / device_id / heard / interrupted / resume_point / resume; Toggle, Next, Previous, Seek, Volume, VolumePreview, Shuffle, Repeat, AddToQueue, ClearQueue, Load; Transfer = no-op). Internals: spawn mpv (`--idle=yes --no-video --no-terminal --really-quiet --input-ipc-server=\\.\pipe\musify-<pid> --ytdl=no --volume=N`), connect the tokio named pipe, observe `time-pos` / `duration` / `pause`, handle `end-file` reasons (`eof` → advance; `error` → error state), guard double-advance, no seek when duration is 0. Volume: `u16` ↔ mpv percent = `v * 100 / 65535`. Resolve command: `yt-dlp --ignore-config --no-warnings --no-playlist --default-search ytsearch -f "bestaudio[ext=m4a]/bestaudio/best" -g -- "<title> <artists>"`, 30 s timeout, first stdout line. Tool discovery: PATH + known Windows fallbacks (`%LOCALAPPDATA%\Programs\mpv\mpv.exe`; `%LOCALAPPDATA%\Programs\Python\*\Scripts\yt-dlp.exe`).
- `src/backend.rs` (line refs in the plan): `Worker.yt` field/init; `Command::YtEngineConnected` + handler; `Command::Player` routes to `yt` when `engine` is None; `start_yt_engine()` sibling of `connect_engine` (no browser, no grant; emits Connecting → Ready{device_id} / Failed); `authorize_playback` free branch → `start_yt_engine()`; `resume_engine` free branch → `start_yt_engine()`; `take_engine_for_resume` captures `yt.resume_point()`; `sign_out` and run-exit take `yt` down; NetworkStatus condition includes `yt`. Keep the hard gate at backend.rs:3306 (librespot 0.8 `exit(1)`s on free accounts — never call `Engine::connect` for free).
- `src/app.rs`: `yt_playback: bool` flag (set where `ApiResponse::Me` reads `product` ≈5788-5801; suppress `Dialog::PremiumNeeded` for free); `Action::EnablePlayback` (≈10445) unifies: set `settings.playback_authorized` + send `Command::AuthorizePlayback` (the backend decides tier); `local_load` (≈11859) becomes a method: in YT mode materialize context loads via `context_track_uris` (≈7815) and fill `tracks` from `track_cache`; skip `autoplay_seed` (≈11892) in YT mode; queue paths (≈3028, 8838, 8888, 8916) use the new payload and `PlayerCommand` (not the Web API) in YT mode; update the free-account dialog test (≈25577 — it encodes the old blocking behavior).

## Environment setup on the new machine (for testing the built exe)

- gh CLI, authenticated as `msedge-projects` (`repo` + `workflow` scopes).
- `pip install yt-dlp` (or a release binary).
- mpv: `winget install shinchiro.mpv` fails on UAC in unattended shells (exit 2). Working method: download `mpv-x86_64-*.7z` from github.com/shinchiro/mpv-winbuild-cmake/releases, extract to `%LOCALAPPDATA%\Programs\mpv\` (`tar -xf` cannot read .7z — fetch `7zr.exe` from 7-zip.org, or use 7-Zip).
- Rust toolchain: **not** needed locally unless the machine is strong (heavy dependency tree; CI is the compiler). If installed anyway: repo pins 1.98.0 via `rust-toolchain.toml`, fetched automatically.
- `.cargo/config.toml` sets `+crt-static` for MSVC — the CI exe is self-contained (no VC++ redistributable needed).

## Gotchas (hard-won)

- PowerShell 5.1 harness quirks: `[System.Net.Http.*]` types are not loaded (use `Invoke-RestMethod` / `Invoke-WebRequest`); `C:\Windows\system32\winget` is a broken alias in pipelines — call `%LOCALAPPDATA%\Microsoft\WindowsApps\winget.exe` directly or `cmd /c winget`.
- `git status` / `gh` can block for a minute or more while other heavy processes run — use generous timeouts.
- `cargo` commands share one target-dir lock: never run two cargo commands in the checkout at once (a background `cargo test` makes a foreground `cargo check` time out waiting).
- Local builds want the CI's system packages on Linux: `libasound2-dev libpulse-dev libxkbcommon-dev libwayland-dev libgl1-mesa-dev cmake libclang-dev` (plus `ruby` for `packaging/test-launchers.py`).
- The Packaging workflow is manual-only now: it needs published release assets, and this fork has none. The launcher regressions in `ci.yml` are the per-push checks.
- Public repo = free Actions minutes; artifacts expire (default 90 days); every push rebuilds.

## Next steps (in order)

1. Fetch the artifact from the green `build` run:
   `gh run download <id> --repo msedge-projects/spotifast-yt --name musify-windows-x64`
2. On the Windows test machine (yt-dlp + mpv installed, see above): run
   `musify.exe`, sign in with the free account, open Settings → playback
   ("Free accounts play through YouTube…"), press Enable playback here, then
   test a single song, an album, a playlist; next/previous/seek/volume;
   end-of-track advance; adding to the queue and playing Next; and that a
   Premium login still uses librespot.
3. Report honestly what was run vs what only compiled.

## User context

Terse operator; wants speed and results ("do it", "spawn multiple agents"). Low-end hardware (i3-4150, 2C/4T) — keep local work light, prefer CI. Casual English, typos common. Bring evidence (measurements, test outputs) when closing a route; "impossible" needs proof — the InnerTube test results and CPU measurements above are that proof, reuse them.
