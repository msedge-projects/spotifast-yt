# mpv + yt-dlp integration evidence

Snapshot captured 2026-10-14 on the original machine (Windows; mpv 0.41.0 portable; yt-dlp via pip; ffmpeg via winget) by an integration-proof agent. The agent's final written report was not delivered before packing — this folder is the raw evidence, kept for the engine implementation.

- `resolve-*.txt|.json|.err.txt` — exact `yt-dlp` outputs (search resolve, `-g` direct audio URLs, `-J` metadata).
- `mpv-spike.ps1`, `run-capture.ps1`, `check-imports.ps1` — the scripts used.
- `mpv-ipc-log.txt` (or `.tail.txt`) — mpv JSON IPC session over a Windows named pipe.
- `mpv-s1.*` — mpv stdout/stderr from the spike session.
- `dllcheck.py`, `pe_imports.py`, `imports*.txt` — environment checks (Python DLL/PE imports), informational only.

Key facts already incorporated into `YT-ENGINE-PLAN.md` and `HANDOFF.md`: yt-dlp `-g` returns the direct googlevideo URL on the first stdout line (music tracks; URLs carry `expire=` and `ip=` params, treat as short-lived); mpv's `--input-ipc-server` works on the shinchiro Windows build; newline-delimited JSON requests get `{"request_id":N,...,"error":"success"}` replies; property changes arrive as `{"event":"property-change","name":...,"data":...}`.
