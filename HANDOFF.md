# HANDOFF — spotifast-yt (YouTube playback engine fork)

Session handoff for continuing this project on another machine. Everything needed is in this repository. Read this file, then `YT-ENGINE-PLAN.md` (the full implementation map with file:line references), then `notes/mpv-spike/` (raw integration evidence).

## Mission

A personal fork of Spotifast (native Rust Spotify client, egui UI) that lets a **free (non-Premium) Spotify account** play music, with audio streamed from YouTube instead of Spotify's servers. UI, browsing, playlists, and everything else stay Spotifast. Premium accounts keep the unmodified librespot engine. This fork is **personal**: upstream's `AGENTS.md` forbids alternate audio sources (that is a contribution/product policy for crmne's repository, not a license restriction; Spotifast is MIT). Do not upstream it; do not file issues about it.

## Non-negotiable constraints (from the user)

- Free tier only; no Premium anywhere.
- CPU is the hard constraint (machine: 2014-era i3-4150, 2C/4T). The user rejected download/transcode pipelines (ffmpeg bursts). The accepted design is a ~1 s flag-only `yt-dlp` resolve per track change plus ~1–3% of one core while mpv decodes.
- ("no yt-dlp" was the user's wish, **proven impossible** — see Decisions. Do not retry removing it; do not propose heavier alternatives.)
- Native Rust, light; no Electron; the UI stays Spotifast's.
- Builds happen via GitHub Actions, not local compilation ("instead of compiling in my pc ... use actions").

## Where things are

- Repo (public): https://github.com/msedge-projects/spotifast-yt — account `msedge-projects` (gh CLI). On the new machine: `gh auth login` (that account), then `gh repo clone msedge-projects/spotifast-yt`.
- Base: crmne/spotifast v0.13.2, full history; `upstream` remote points at crmne/spotifast — never push there.
- `YT-ENGINE-PLAN.md` — implementation map: exact files, line refs, ordered edit list, risks. **Read first.**
- `notes/mpv-spike/` — snapshot of the yt-dlp + mpv integration proof from the original machine: exact `yt-dlp` output shapes, mpv JSON IPC log, the scripts used. Re-run a small spike on the new machine if anything is unclear.
- `.github/workflows/build.yml` — the CI that builds the exe. Upstream also ships ~9 other workflows that fire in this fork (board, docs, release, issue triage...); ignore their noise or trim them. `ci.yml` is upstream's test suite if you want free test coverage.

## State at packing (2026-10-14)

- Fork commits on top of upstream: (1) `Add GitHub Actions Windows release build workflow`; (2) `Replace README with fork notice`; (3) the docs commit that added this file, `YT-ENGINE-PLAN.md`, and `notes/`.
- **No code changes yet.** The fork is pristine upstream aside from the above. Implementation has not started.
- Baseline CI: at packing time the pristine `build` run (id 38610927318) and upstream's `CI` were both `in_progress` (~13 min; cold cache, large dependency tree). **Check before anything else:**
  `gh run list --repo msedge-projects/spotifast-yt`
  - Green: pipeline proven. `gh run download <id> --repo msedge-projects/spotifast-yt` fetches artifact `spotifast-yt-windows-x64` (contains `spotifast.exe`).
  - Red: `gh run view <id> --repo msedge-projects/spotifast-yt --log-failed`. Likely causes: git dependency fetch (all are public `crmne/*` repos) or a toolchain quirk (repo pins Rust 1.98.0 via `rust-toolchain.toml`; CI installs it automatically).
  - Git dependency revisions, for diagnosis: fastframe `v0.6.0`; egui `ba6790fe7cf46e58e8d27ce1524cbfdee745e938`; winit `ed7caa9023f10b397f5b6ec8284a840cbd8a6f65`; hyper-proxy2 `efca4811371ddae81c06be168de54194938d70b1`; librespot `847125198bb251d1d4497f46bebbfa4868b7d5d7`; projectm-rs `93852ff7299e5a63ed1315437593fc70915c0863` (milkdrop feature only; the workflow disables it).
- **Get a green baseline before patching.**

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
          name: spotifast-yt-windows-x64
          path: target/release/spotifast.exe
          if-no-files-found: error
```

## Decisions already made (do not relitigate)

1. **Native, no-subprocess YouTube streaming is impossible.** Proven live from the user's IP: every InnerTube client profile (Android / Android VR / Android Music / iOS / Web / TV-embed / Web-embed) answered HTTP 400 or `LOGIN_REQUIRED` ("Sign in to confirm you're not a bot") — YouTube's playback attestation (PoToken) wall. yt-dlp is the resolver in every workable design; its upkeep is the yt-dlp community's job.
2. **Architecture (user approved):** per track change, `yt-dlp` resolves a direct audio URL in flag-only mode (`-g`; no download, no ffmpeg); an **mpv child process** decodes/plays it, driven over mpv's JSON IPC (Windows named pipe). This is Spotube's architecture rebuilt inside Spotifast.
3. **Engine placement:** a second engine (`src/player_yt.rs`, `YtEngine`) lives BESIDE the librespot engine (`Worker.yt` in `backend.rs`). Free accounts route to it; the Premium path is untouched. Free accounts' Web API browsing/search/playlists already work without any engine (verified in code) — only audio was gated.
4. **The app never advances tracks** — librespot's Spirc does that today. The YtEngine must own queue advancement (mpv EOF → next item, push `LocalState` with `track_sequence+1`); the app's existing `on_now_playing_changed` then consumes queue rows exactly as for Premium. `docs/_reference/queue.md` is the contract.
5. **Metadata flow:** the engine cannot resolve `spotify:track:` URIs itself (no Web tokens). `LoadSpec` gains `tracks: Vec<LocalTrack>`; `PlayerCommand::AddToQueue` becomes `AddToQueue { uri, track: Option<LocalTrack> }`. For items that still lack metadata when their turn comes (long playlists), **defer**: park the item (Loading + placeholder track) and add `PlayerCommand::SetTrackMetadata { uri, track }`; the app already fetches `ApiRequest::Track` for a newly playing uncached uri (app.rs ≈3074-3081, cache insert ≈7215) — in YT mode it forwards the fetched metadata to the engine, which then resolves and plays. (v1 fallback if needed: rely on `track_cache` only and error on missing metadata — playlists will be hit-or-miss; the deferred design is the correct one.)
6. Adopted from upstream's `AGENTS.md` anyway: network/playback work on the runtime (never block the UI thread); focused changes; tests updated in the same change; honest platform-coverage claims.
7. Do not touch either company's server-side gates: no Spotify credential forging, no YouTube attestation circumvention. yt-dlp + mpv act client-side against public streams; keep it that way.

## Implementation summary (full detail in YT-ENGINE-PLAN.md)

- `Cargo.toml`: tokio features += `"process"` ("net" already covers Windows named pipes).
- `src/lib.rs`: `pub mod player_yt;`.
- `src/player.rs`: `LoadSpec.tracks`; new `AddToQueue { uri, track }` payload; add `Heard::from_state(Arc<Mutex<LocalState>>)` pub(crate) constructor (its field is private; the YT engine hands volume over with it).
- NEW `src/player_yt.rs`: `YtEngine` mirroring `Engine`'s surface (connect / command / shutdown / device_id / heard / interrupted / resume_point / resume; Toggle, Next, Previous, Seek, Volume, VolumePreview, Shuffle, Repeat, AddToQueue, ClearQueue, Load; Transfer = no-op). Internals: spawn mpv (`--idle=yes --no-video --no-terminal --really-quiet --input-ipc-server=\\.\pipe\spotifast-yt-<pid> --ytdl=no --volume=N`), connect the tokio named pipe, observe `time-pos` / `duration` / `pause`, handle `end-file` reasons (`eof` → advance; `error` → error state), guard double-advance, no seek when duration is 0. Volume: `u16` ↔ mpv percent = `v * 100 / 65535`. Resolve command: `yt-dlp --ignore-config --no-warnings --no-playlist --default-search ytsearch -f "bestaudio[ext=m4a]/bestaudio/best" -g -- "<title> <artists>"`, 30 s timeout, first stdout line. Tool discovery: PATH + known Windows fallbacks (`%LOCALAPPDATA%\Programs\mpv\mpv.exe`; `%LOCALAPPDATA%\Programs\Python\*\Scripts\yt-dlp.exe`).
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
- Upstream tests encode the old behavior (free accounts told playback needs Premium — app.rs:25577). Update those expectations with the patch; upstream `ci.yml` (if kept) runs the test suite.
- Public repo = free Actions minutes; artifacts expire (default 90 days); every push rebuilds.

## Next steps (in order)

1. Auth gh on the new machine; clone the repo; read `YT-ENGINE-PLAN.md` and this file; glance at `notes/mpv-spike/`.
2. Confirm a GREEN baseline `build` run (or fix it) before touching code.
3. Implement the patch in order: Cargo.toml → lib.rs → player.rs payloads → player_yt.rs → backend.rs → app.rs. Match the codebase style (anyhow results, `log::*`, poisoned-mutex pattern `unwrap_or_else(|p| p.into_inner())`, comments that say why).
4. Update tests the behavior change touches; keep the tree compiling for `--no-default-features`.
5. Push; watch the `build` run; fix compile errors from `--log-failed`; iterate until green.
6. Download the artifact; test on Windows with yt-dlp + mpv installed: sign in with the free account, enable playback ("Set up playback here"), play a single song, an album, a playlist; check next/previous/seek/volume, end-of-track advance, and that a Premium login still uses librespot.
7. Report honestly what was tested vs merely compiled.

## User context

Terse operator; wants speed and results ("do it", "spawn multiple agents"). Low-end hardware (i3-4150, 2C/4T) — keep local work light, prefer CI. Casual English, typos common. Bring evidence (measurements, test outputs) when closing a route; "impossible" needs proof — the InnerTube test results and CPU measurements above are that proof, reuse them.
