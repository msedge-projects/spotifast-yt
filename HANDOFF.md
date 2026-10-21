# Session handoff: Musify / spotifast-yt

Written at the end of a long working session, for the next session to take over.
Date: Mon Oct 19 2026. Everything below is verified state unless marked otherwise.
Updated Tue Oct 20 2026: stage 3 landed (the mpv backend is deleted, the engine is
native-only) and that build is installed.

---

## 1. The project

- Repo: `C:\Users\DELL\spotifast-yt` (fork of crmne/musify, "Musify"), remote `https://github.com/msedge-projects/spotifast-yt`.
- Rule: commit to the fork's `main` only. Never touch upstream crmne/spotifast or crmne/musify.
- Fork identity: fully YouTube-based playback. Spotify is used for browsing/metadata; audio comes from YouTube via the app's own in-process engine (no mpv in the default path).
- Code head: `aa3d529` "Put the formatter's lines where it asked" (2026-10-20), on top of the stage 3 commit `5947f69`. At the handoff itself it was `7473c3f` "Be born the size the island already is". The working tree is clean; `origin/main` and `origin/scratch/native-player` carry the same code (scratch is force-pushed from main; the VPS check script checks out scratch).

Repo docs worth reading (older context, partly superseded):
- `YT-ENGINE-PLAN.md` — the original implementation map with file:line references for the first YouTube engine. Still useful for the resolver/metadata design; the playback path has since been rebuilt (this handoff supersedes it for anything engine-related).
- `notes/mpv-spike/` — raw evidence from the original yt-dlp + mpv integration (yt-dlp output shapes, mpv JSON IPC logs). mpv is now only the `MUSIFY_YT_PLAYER=mpv` escape, but the evidence is still the reference for resolve behaviour.
- `.github/workflows/` — upstream's CI (build.yml builds a Windows exe, ci.yml runs the suite). For this fork the VPS is the primary build/test path; CI is a fallback only.
- This file itself was rewritten in place: the previous `HANDOFF.md` (2026-10-15, mpv-era) remains in git history (see the tree at commit `b0783cb`).

## 2. Standing user requirements

- **Never compile on the PC.** All builds, checks, tests, and renders happen on the VPS. The PC is a low-end i3-4150 (global AGENTS.md says use InstaBox for heavy work; the working machine is the InstaBox VM named vps1).
- Do not open/run/test the Musify app on the PC unless the user asks. Exceptions they have been fine with: `musify.exe --version`, killing/starting the app to install a build, and screen/window screenshots for diagnosis.
- Search UX parity target is music.apple.com: frosting, pill, live suggestion panel; values were measured from Apple earlier.
- The user hands over design references as web code (React/TSX/CSS) and expects faithful ports into the app's native egui renderer. This app must not embed a browser engine (repo AGENTS.md). Port numbers/curves exactly; disclose what cannot map (real desktop backdrop blur, web fonts).
- Never use em dashes in public repo/issue copy. In chat, keep the same calm style (full stops, commas).
- Windows exe delivery flow: cross-build on vps1, scp to PC, MD5-verify, install with a backup, relaunch.
- Never print the InstaBox API key or any token. The key lives in `C:\Users\DELL\.instabox.key` and `$env:INSTABOX_API_KEY`.
- Repo rules (see repo AGENTS.md): work on main, one topic per commit, linear history, no merge commits, no CI babysitting. CI is the last resort for a Windows exe; the VPS is preferred.

## 3. State of the work (what is done and verified)

### 3.1 Native YouTube audio engine (stage 2, wired in)
- `src/yt_audio/` (source.rs, worker.rs, decode.rs): symphonia demux for fragmented MP4/AAC and WebM/Opus (plus rare Vorbis), Opus through `rusty-opus =1.0.2`, decode into interleaved stereo f64 at 44.1 kHz, 512-frame packets.
- `source.rs`: a range-backed burst buffer. Hard-won wire facts: send only a `Range: bytes=N-` header (the `&range=` query param compounds), set a UA, `http1_only`, drain bodies at full speed, in-window seeks free, 30 s stall ceiling.
- The worker builds the same sink chain as Spotify mode (`sink::build_chain` + `SharedVolume`), so EQ and visualizers work on YouTube songs.
- `src/player_yt.rs`: the engine actor drives the worker and nothing else. Stage 3 landed on 2026-10-20 (commit `5947f69`): the mpv child process, its IPC plumbing and event folding, mpv's half of `find_tool`, and the `MUSIFY_YT_PLAYER` escape are gone, and the settings copy no longer names mpv. User-chosen changes fade; natural transitions do not; one re-resolve on failure then an honest error.

### 3.2 Duration bug (fixed twice)
- YouTube's fragmented stream header states its first segment's length (~4.19 s) and used to overwrite the true song length with it (user saw 0:04 on "Creepin'"). Metadata won from then on, but a row that arrived with no length still fell back to the header: "Maine Royaan (Lofi Remix)" played for 5:05 while the bar read 0:06 of 0:06 and seeking stopped at six seconds (2026-10-20).
- The resolver already read `videoDetails.lengthSeconds` to score formats, then threw it away. `Resolved` now carries `duration_ms`, `CachedStream` keeps it, and `Actor::adopt_duration` gives it to a `yt:` row, whose length it is because the row names the video. Metadata rows keep their own length and only take one when they have none. The header feeds a length only when nothing else knows one; a header more than 5 s off is logged and never kept. The resolve log now prints the length it found, and `learn_duration` writes a played song's length into the library, so one play fixes the tables.
- Verified on the PC on 2026-10-20: `play-search` resolved the song at 305 s and the bar read 0:12 of 5:05.

### 3.3 Search behavior (Apple parity, done)
- Typing updates only the suggestion panel; the results page keeps its last committed query and loads on Enter or on clicking a row (song plays, artist/album/playlist opens, keyword runs the search).
- State split in `SearchState`: `results` (page, committed) vs `suggestions` + `suggestions_playlists` (panel, typed preview). `typed: Option<String>` marks the newest typed run. Response handlers in `app.rs` have preview branches. `run_search` clears the preview state. Search history semantics preserved (a submitted entry survives later typing in the same run; tests cover it).
- Panel height bug fixed: the ScrollArea alone could stay at the first row's height, so the panel showed 1-2 rows then scrolled. `widgets.rs::search_suggestions` now computes `content_height` from the rows (36/58 px), caps at `(screen * 0.62).clamp(240, 640)`, and calls `ui.set_height(height)`. Do not remove that explicit height.
- Demo: the search demo seeds `app.search.suggestions = app.search.results.clone()` so the panel draws.

### 3.4 Title strip ("transparent head") — reverted by the user
- The app's Mica attempt exists (`window::set_backdrop_mica`, Win11 22H2+) but the user's PC is **Windows 10 build 18362**, so it fails harmlessly and the caption tint path runs. The user asked for the head to be reverted; their `settings.json` now has `"custom_titlebar": false` and the standard window strip is back.
- The Mica block is skipped for the mini/island windows (`entrypoint.rs` run_logic: `window_hwnd != 0 && !app.settings.winamp_window`).
- If they ever move to Win11, Mica will make the main title strip glass automatically.

### 3.5 Glass mini player — built, then deleted at the user's request
- `src/ui/glass.rs` was implemented (frosted flop, floating panel with margin/shadow, EQ with ten bands, a way-home button, tests that clicks are not eaten). The user reverted it in favour of the island. The module and its tests are deleted; `Ctrl+M` opens the classic Winamp mini player again (`winamp::initial_size` restored; topbar tooltip reverted to "Winamp mini player (Ctrl+M)").
- Keep the lesson (section 9): the background drag layer must sense click_and_drag, not drag alone.

### 3.6 The island (Dynamic Island) — built and installed now
This is the current feature and the most recent work. Basis: the user supplied a full React `Sound.tsx` + `Sound.css` reference (a 260-wide pill that morphs into a player card). Their choices:
- Variant **B**: nothing visible until the pointer reaches the top-centre of the screen; a thin strip catches it.
- It replaces the mini-player idea; it appears when the main window is **minimized**.

Behavior implemented in `src/ui/island.rs` + wiring:
- While `app.island_window` is true the app is a borderless, transparent, always-on-top window (no taskbar entry), created at 260x8 (`RESTING_SIZE`), centred at the top of the monitor by `place_window` on the first frame.
- Hover the strip → window grows to the pill (260x78) and the pill slides down (appear tween 0.32 s quart-out), the whole pane fading in.
- Click the pill → the card opens (260x189): one quart-out tween of 460 ms at the design's `morph=50`, driving height 78→189, sleeve 40→64, radii (sleeve corner 10→16; box corner 20→26, kept concentric), type sizes 13→15.5 / 11→12, the transport from 24/30/24 right-aligned (gap 5) to 34/46/34 centred (gap 14), the rail from the bar's foot to `RAIL_Y=96`, and a `late` window (p>0.6) for the clock and heart. Close-only gather on its own linear clock (`sin(pi*u^1.5)`, zoom to 0.965). The play mark is drawn as two morphing quadrilaterals with the goo pull/tilt/squash, 300 ms swing.
- Leave the card ~0.7 s → folds to the pill; leave the pill ~0.5 s → folds away and the window returns to the strip. Clicking the sleeve/words folds the card immediately (the reference's tap semantics: whole bar shut, sleeve+words open).
- Real data wired: album art, title/artist from now-playing, rail + clock from actual position/duration (`1:29` / `−1:30` style), play/restart/next control real playback, heart is the real Liked state with the beat-on-like.
- **One disclosed addition**: a way-home button (expand icon) at the right end of the transport row, because a borderless strip has no taskbar entry. `Escape` also returns to the main window. Action: `Action::IslandToMain` sets `island_window=false`, `switch_intent=true`, closes the window.
- Trigger: in `App::frame_ui`, when not already the island and `viewport().minimized == Some(true)`, set `island_window=true`, `switch_intent=true`, close; the shell loop then creates the island window via `MiniWindow::wanted`'s island branch.
- Persistence: `island_window` is an in-memory App field, never saved; the app always opens as the main window. `IslandState` lives on `App` (`app.island`).
- Window sizing: `MiniWindow::wanted` uses `island::wanted_size(&app.island)`; resize commands are sent only between shapes (growing immediately, shrinking only after the morph/appear has landed).
- Demo surfaces in `demo.rs`: `"island"` (strip), `"island-pill"` (`settle_pill`), `"island-open"` (`settle_open`). `settle_*` set `seen_at = f64::INFINITY` so the grace does not fold a settled state.
- Tests: `app.rs`: `the_island_wakes_opens_and_goes_home`, `the_island_folds_when_its_sleeve_is_clicked`, `a_minimized_window_hands_over_in_the_logic_pass`. `island.rs` unit tests: rate knob ends, geometry (78/189/96/156), tween turn-around, gather endpoints, late window, transport slide, mark blend, box scale about top-centre.
- Fixed 2026-10-20: minimizing showed no island at all. The hand-over lived in `frame_ui`, and eframe paints nothing for a minimized window, running only `App::logic`. It now lives in `App::hand_over_to_island`, called from `background_frame` (the logic pass) and from `frame_ui`; a new test runs a logic pass with the viewport marked minimized. Verified on the PC at 1366 wide: the 260x8 strip at x=553 (top centre) after minimizing, the 260x78 pill on hover with the sleeve, title, artist and transport.
- Fixed 2026-10-20, after the user reported the pill looked liquid and its edge was cut: the pane was 92% opaque, so the wallpaper tinted it, and it is now solid (`22,24,28` dark, `250,250,250` light); and the pill slid in while the window was still the strip's 8 points, so the window clipped its bottom. The slide now waits for `has_pill_room` (the window reports the pill's height), and only a pointer still on the island starts it, so a fold is never restarted (the fold test caught that).
- Fixed 2026-10-20, from inspecting the live island window: it still carried `WS_CAPTION` (style `0x16CA0000`), so Windows kept the caption on screen by moving the window down and clipped a pixel off the client, and `App::island.placed` meant the park-at-top-centre step ran only once per process, so a replaced island window kept the window manager's position (measured at `788,21`). `window::strip_frame` now clears the caption and frame bits when the island window is made, `attach` forgets the old placement so each new island window parks itself at `(screen - 260) / 2, 0`, and the Mica/caption-tint block skips the island as it already skipped the Winamp player.
- VPS renders verified the pill and the card, dark and light, matching the reference arrangement (files on vps1: `~/shots/island-pill.png`, `island-card.png`, `island-card-light.png`; local copies in `C:\Users\DELL\AppData\Local\Temp\opencode\glass-shots\`).

### 3.7 Channel-balance evidence tooling
- `worker::channel_balance` + `probe-audio`: the probe decodes ~3 s of the resolved stream and logs `Probe: channels left rms X, right rms Y over N ms` to the app log.
- A fixture test proves AAC and Opus decode paths agree channel-for-channel (same-channel correlation above cross-channel, sane balance, quiet fixtures accepted). No decode-side imbalance found.
- The user reported "left ear gets most of the music" once; it was never reproduced. If it returns: run `musify.exe probe-audio "creepin"` on the PC and read `%LOCALAPPDATA%\msedge-projects\musify\data\musify.log`. Also watch for `audio queue ran dry` stutter warnings (one 241 ms instance was seen on 2026-10-18 09:13:26).

## 4. Verified state at handoff

- VPS gates: run23 was fully green on `aa3d529` (check, clippy `-D warnings`, 1273 tests passed, 0 failed, 4 ignored). Logs: `~/check-run23.out`, plus `check.log`/`clippy.log`/`test.log` on the box. run20 and run21 were the earlier green island runs.
- `cargo fmt --all --check` is not clean repo-wide, but only in files these sessions did not touch: `src/app.rs` (3 hunks), `src/ui/island.rs` (4), `src/yt_audio/decode.rs` (1). `src/player_yt.rs` and `src/ui/settings.rs` are clean. The VPS script does not run fmt; `~/musify-check2.sh` on the box does, and is worth keeping.
- Windows exe: built from `aa3d529` on vps1 with `cargo xwin build --release --locked --no-default-features --target x86_64-pc-windows-msvc` (`~/xwin3.log`, EXIT 0; the only warning is the cosmetic LNK4099 about MSVC CRT PDBs). md5 `9EFE3DABCEA7680E927049550991D604`, 31,074,304 bytes, installed over the island build; the replaced file is `musify.exe.bak-7473c3f`. Relaunched after install as PID 8728 and `--version` printed `musify 0.13.2`.
- Backups in the same folder: `musify.exe.bak-a836909` (previous build), `.bak-e766ed9`, `.bak-f51e12d`, `.bak-90106ac`, `.bak-c8f1796`, `.bak-473d7dc-era`, plus many older `.bak-<sha>` from previous sessions.
- PC settings (persisted): `%APPDATA%\msedge-projects\musify\config\settings.json`: `winamp_window=false`, `custom_titlebar=false`, `youtube_mode=true`, theme now dark (it was light at the handoff), volume 19660, and an EQ curve in the first five bands (8.8, 7.2, 5.6, 3.2, 0.8). The user's display is 1366x768 at 100% DPI. Windows 10 build 18362 (no Mica).

## 5. NEXT STEPS (prioritized)

1. **The island still has no verdict.** On 2026-10-20 the user played music but has not commented on the island. Expect feedback on:
   - The wake strip: an 8 px always-on-top strip at the top-centre catches clicks there while minimized (they accepted this trade-off with variant B).
   - Timing: `MORPH=50` → 460 ms morph, appear 0.32 s, fold graces 0.5 s (pill) / 0.7 s (card). All are constants at the top of `island.rs` plus `GRACE_S`/`CARD_GRACE_S`.
   - Fidelity of the card: the restore button is the one addition; everything else follows the reference.
   - They may want Ctrl+M to summon the island instead of the classic mini player, or the island to exist while the app is open. Both are easy follow-ups.
2. **Stage 3 is done** (`5947f69`, formatting follow-up `aa3d529`, installed 2026-10-20). What remains of its spec is the resource comparison: with no player child process left there is nothing to compare against on the PC, so either cite the old mpv evidence or drop that line from the spec. Measured on the PC on 2026-10-20: the app had no child processes at all, working set 105.6 MB and private 82.8 MB after ten minutes of use, 15.9 s of CPU. `mpv.exe` is still installed at `%LOCALAPPDATA%\Programs\mpv`; nothing starts it any more, and it can be deleted whenever.
3. **Live listening proof** happened on 2026-10-20 at 20:15: the log shows `YouTube: resolving Creepin'`, one `LOGIN_REQUIRED` refusal answered by a retry, a resolve in 594 ms, the audio output opened, and the next song prefetched in 146 ms. That is the first recorded native playback on the PC. The VPS IP still cannot play YouTube (LOGIN_REQUIRED), so any further listening test must happen on the PC. If the left-ear thing returns, use the probe above; watch for `audio queue ran dry` if stutters are reported.
4. **Search panel rows**: if the user still finds the panel too short on a long list, the height math is in `widgets.rs::search_suggestions` (cap 0.62 * screen height).
5. **Mica** stays dormant for Win11; nothing to do on their Win10 machine.

## 6. Infrastructure: the VPS (vps1) — exact recipes

- Machine: name `vps1`, InstaBox id `m_agquvtirvrxwrrsodr73nzpms4`, hostname `instabox-vps1`. API base `https://api.instabox.sh`. Key file `C:\Users\DELL\.instabox.key` (never print).
- Useful endpoints (discovered via `GET /v1/openapi.json`): `POST /v1/machines/{id}/wake`, `POST /v1/machines/{id}/exec` (json `{"cmd":"..."}`, bash -lc, request times out but detached jobs survive), `GET /v1/machines/{id}` , `GET /v1/machines/{id}/connection-info`, `POST /v1/machines/{id}/ssh-tokens` (returns `ssh -p 2222 <token>@api.instabox.sh`, ttl up to 30 days), `GET /v1/machines/{id}/files?path=`.
- SSH from the PC: only Git's OpenSSH works: `"C:\Program Files\Git\usr\bin\ssh.exe"`. Mint a fresh token via the API if the old one expired, then use `-p 2222 -o StrictHostKeyChecking=accept-new <token>@api.instabox.sh`. `scp.exe -O -P 2222` for files. Exec through the API also works and avoids tokens.
- Machine auto-suspends after ~30 min idle. Wake and wait ~30-90 s; the API may answer `machine is still changing state` briefly (retry).
- Box layout: checkout `~/musify`; the check script `~/musify-check.sh`; renders `~/shots`. Rust: `/opt/rust/rustup` toolchain 1.99.0 for check/test; `~/.rustup` holds 1.98.0 + the windows-msvc target for `cargo xwin`; `~/.cache/cargo-xwin` (~1.1 GB, keep). Disk `/data` is 20 GB and was ~3 GB free; the msvc target dir and debug incremental are the big consumers (delete `target/debug/incremental` when needed).
- Gates: `~/musify-check.sh` fetches `origin/scratch/native-player`, checks out, then runs check, clippy `-D warnings`, and the full test suite with debuginfo off. `~/musify-check2.sh` on the box is the same with a `cargo fmt --all --check` step in front (added 2026-10-20). Launch either detached and poll:
  `grep -E 'fmt-ok|check-ok|clippy-ok|test-ok|FAILED|EXIT:' ~/check-runN.out`.
  Local copy of the older script: `C:\Users\DELL\AppData\Local\Temp\opencode\remote-check.sh`.
- Long scripts: base64 them and run over ssh:
  `$b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes((Get-Content script.sh -Raw))); ssh ... "echo $b64 | base64 -d | tr -d '\r' | bash"`.
- Windows cross build (proven pattern; use exactly this shape):
```
{"cmd":"cd ~/musify && setsid nohup bash -c 'export RUSTUP_HOME=\"$HOME/.rustup\"; export PATH=\"$HOME/.cargo/bin:/usr/lib/llvm-19/bin:$PATH\"; cd ~/musify && cargo xwin build --release --locked --no-default-features --target x86_64-pc-windows-msvc > ~/xwinN.log 2>&1; echo EXIT:$? >> ~/xwinN.log' < /dev/null > /dev/null 2>&1 & echo xwin-started"}
```
  Do not run this from an inline shell script with altered quoting: one attempt exported the wrong rustup and failed with "can't find crate for core; the x86_64-pc-windows-msvc target may not be installed". The tree must have been checked out first (fetch `origin/scratch/native-player`; the check script does it; the xwin command above relies on the current checkout).
  Build takes ~2-4 minutes warm. Exe: `~/musify/target/x86_64-pc-windows-msvc/release/musify.exe` (~31 MB).
- Renders (demo feature): on the box `cargo build --locked --no-default-features --features demo`, then
  `LIBGL_ALWAYS_SOFTWARE=1 xvfb-run -a -s "-screen 0 1400x1100x24" ./target/debug/musify --demo-page home --demo-show "island-open,light" --demo-shot ~/shots/x.png`.
  Demo surfaces include: `search`, `winamp`, `playlist`, `eq`, `island`, `island-pill`, `island-open`, `light`, `dark`, etc.
  Gotcha: a check/test run can replace `target/debug/musify` with a non-demo binary; then the app exits 2 with "unexpected argument '--demo-page'". Rebuild with `--features demo` before shots.
- Install on the PC: stop the app, `Move-Item musify.exe musify.exe.bak-<tag>`, copy the new exe, `Start-Process`, verify the PID and that `--version` prints `musify 0.13.2`. Compare MD5 from the box (`md5sum`) with `Get-FileHash -Algorithm MD5` locally before installing.
- Screenshots of the live app on the PC: PowerShell with `System.Drawing` + `GetWindowRect` (collect the window rect, `CopyFromScreen`). Transparent windows composite properly on the user's PC, so the mini player/island show the desktop through.

## 7. Code map (the files that matter now)

- `src/ui/island.rs` — the island. Geometry constants from the reference (`W=260`, `SHUT=78`, `OPEN=189`, `PAD=10`, sleeve 40→64, `CORNER=16`, `RAIL_Y=96`, `OPS_Y=156`, `LEAD=46`, `LIKE=30`), `Tween` (clock = egui input time in **seconds**), `IslandState` (visible, open, appear/morph/gather/mark/beat tweens, seen_at, placed, asked), `place_window`, `step` (hover/appear/fold state machine, resize policy, repaint requests), `open_card`/`close_card`, `box_rect`, `tap_rect`, `ops_rects`, `restore_rect`, `like_rect`, `paint` + paint_cover/words/track/transport/heart, `paint_mark`, `round_button`, `playtime`, unit tests.
- `src/app.rs` — `island_window` + `island` fields; `frame_ui` minimize trigger and island branch (order: island, then winamp_window, then main); `Action::IslandToMain`; the search split (`run_typed_search` preview semantics, response preview branches, `run_search` clearing); island interaction tests; search split test; history tests.
- `src/model.rs` — `SearchState` preview fields (`typed`, `suggestions`, `suggestions_playlists`); new `Action::IslandToMain`.
- `src/ui/widgets.rs` — `search_suggestions` height (rows sum, cap 0.62 screen, `ui.set_height`).
- `src/ui/topbar.rs` — pill; suggestion rows read `app.search.suggestions`; Enter/commit.
- `src/entrypoint.rs` — `MiniWindow::wanted` (island branch first, then winamp mini), `clear_color` transparent for island/winamp, Mica/tint block skipping mini/island, staging, demo options.
- `src/demo.rs` — island surfaces; search demo seeds suggestions.
- `src/player_yt.rs` — engine actor, the in-process worker (the only backend since stage 3), duration guard, prefetch, fades.
- `src/yt_audio/{source,worker,decode}.rs` — playback source/worker/decoder; `channel_balance` probe helper; fixture channel-parity test.
- `src/yt_resolver.rs` — quality ranking (141 > 251 > 140 > 250 > 249 > 139) and resolver.
- `src/sink.rs` — `build_chain`, `SharedVolume`, `AudioControl`, chain config (the shared sink chain).
- `src/window.rs` — DWM caption colour and Mica (Win11 only).
- Deleted: `src/ui/glass.rs`. `src/ui/mod.rs` declares `pub mod island` in its place.
- Branch `scratch/native-player` is a mirror of main for box checkouts (force-pushed whenever main moves).

## 8. Recent commits (newest first, at handoff plus the 2026-10-20 follow-up)

```
f73e658 Take the song's length from the video, not its first fragment
6a9b57c Say in the handoff that the engine has one backend
aa3d529 Put the formatter's lines where it asked
5947f69 Keep only the engine that lives in this process
339ec12 Hand the session over: the island, the engine, and the way the box works
7473c3f Be born the size the island already is
ed4aa22 Settle the pill for a screenshot too
60b9fc2 Close the braces the probes left open
54df8b4 Count the morph in seconds, not milliseconds
9f2112b Probe the island's state machine
afdbdf8 Probe the island transport's entry
b251412 Probe the island's way-home click
2633872 Measure the words before drawing them, and quiet the leftovers
1cb23d5 Trade the mini player for the island at the top of the screen
7103474 Grow the suggestions panel to its rows on the frame they arrive
6bc882b Let the headless frames drop their texture deltas
8482641 Expose the rows the tests click through
67b95e5 Press the glass buttons, not the window
fb79e58 Prove the glass player's controls by clicking them
e766ed9 Let the mini player show the desktop, float, and lead home
f51e12d Float the panel in its window with room for its edge
```
(The probe commits are leftovers in history but their probes were removed; the tree is clean of eprintln probes except none. The glass commits' module was deleted in `1cb23d5`.)

## 9. Gotchas and lessons (hard-won, do not relearn)

- **egui press stealing**: a full-background widget with `Sense::drag()` is marked dragged the instant any press lands inside it, even on a button above it. That press then becomes a window move (StartDrag) and the button's click never completes. Any drag background must use `Sense::click_and_drag()`, which postpones the decision until the pointer really moves. This was the "buttons don't work" bug in the old glass mini player.
- **Headless tests**: `ctx.run_ui(RawInput { screen_rect, time, events }, |ui| app.frame_ui(ui))`; clear `output.textures_delta` before dropping a FullOutput or epaint panics ("Dropped TexturesDelta with unapplied deltas"). Clicks: move frame, press frame, release frame; the click fires on the release frame.
- **Island tween clock**: `Tween.dur` is in **seconds** (egui input time). `BASE_MS=460` must be divided by 1000. A unit bug here made the card morph over 460 seconds and broke the tests in a subtle way (p stayed ~0.006).
- `Tween::retarget` is a no-op when the target is unchanged (so `settle_*`/open states do not restart), and it reads its current value for continuous turn-around. `replay` (heart beat) starts from zero each time.
- `settle_open`/`settle_pill` set `seen_at = f64::INFINITY`; otherwise the grace folds a settled card on the first frame.
- Overlapping widgets: later `ui.interact` calls win. Register the tap/background first, controls after.
- Windows 10 on the user's PC: no Mica. The caption tint is the fallback. `custom_titlebar` works there but was reverted by the user.
- Their screenshots are often crops; a transparent app window shows the desktop through, so a dark region inside the window bounds may just be the wallpaper/app behind, not a paint bug. Capture the full screen or the window rect before concluding.
- `musify.log` is replaced at every app start (`%LOCALAPPDATA%\msedge-projects\musify\data\musify.log`); read it soon after a session or ask the user to reproduce.
- The VPS IP cannot play YouTube (LOGIN_REQUIRED). Live playback tests must happen on the PC; the VPS is for gates and renders.
- eframe's drawing is skipped for a minimized, hidden or occluded window; only `App::logic` runs (through `update_logic_only`, with fresh viewport info and no egui pass). A state hand-over that must happen while minimized belongs in `background_frame`, not `frame_ui`. The island's hand-over sat in the drawing and never ran, which is why minimizing showed nothing.
- Windows `CloseMainWindow` on this app goes through its close-to-tray path: it hides the window and keeps running, so a scripted install that sends a close and then force-kills leaves the next launch hidden. Stop the process directly when installing, or show the window afterwards.
- The island window must be a bare surface. It was observed carrying `WS_CAPTION` even though the viewport asked for no decorations, with its client a pixel lower than its outer rect: Windows keeps a caption on screen by moving the window down, and clips that pixel, which read as a cut-off pill. `window::strip_frame` clears the style after creation.
- `App::island.placed` is per process, not per window. A replaced island window inherits the window manager's restored position unless the flag is reset when the window is made (`attach` does it now).
- vps1 auto-suspends after about thirty minutes idle, so an overnight gap leaves it suspended and `scp` fails silently when its stderr is discarded. Check the machine state and wake it before copying.
- rustfmt keeps a method chain under 60 columns, so a chain longer than that breaks after the receiver even when the whole line would fit the width; matching the formatter beats trusting your eye. `cargo fmt --all --check` is in `CONTRIBUTING.md` but was not in the VPS script, so drift had collected in `src/app.rs`, `src/ui/island.rs` and `src/yt_audio/decode.rs` (it is still there; nothing in this session touched those hunks).
- `git` warns about LF→CRLF on commit; that is normal (`core.autocrlf=true`). Pushes use the fork remote only.
- InstaBox exec API: transient request failures happen; retry. When the machine is resuming, the API returns `operation_conflict`; wait and retry. Detached jobs need `setsid nohup ... < /dev/null > /dev/null 2>&1 &` and are verified by tailing their log files over ssh.

## 10. Quick reference: the user's last known state and what to ask

- The app now runs the stage 3 build (md5 `9EFE3DABCEA7680E927049550991D604`, from `aa3d529`), installed 2026-10-20 after the user chose stage 3 and a live listening proof together. They played Creepin' on the previous build; the new one changes no playback behaviour, it only removes the unused backend.
- Ask/confirm when they return: how the island feels (wake, slide-in, open, fold timings), whether the awake strip position/top-centre placement is right, whether they want Ctrl+M to summon the island instead of the classic player, and whether the restore button (right end of the transport row) or Escape as the way home is acceptable.
- Ask whether the engine is confirmed by ear. It is the only backend now, so a problem is the engine's to fix rather than a reason to switch back; the deleted code is in git at `339ec12` if a comparison is ever needed.
- If they report the left-ear imbalance again, use `musify.exe probe-audio "<song>"` and read the log line `Probe: channels left rms ..., right rms ...`.

---

End of handoff. The handoff file itself is untracked in the repo root (`HANDOFF.md`); delete or move it freely, nothing depends on it.
