# Musify (spotifast-yt)

A personal fork of [crmne/spotifast](https://github.com/crmne/spotifast) (MIT), renamed to **Musify**, for local personal use only. It stays a small native Rust client (egui), and adds a YouTube playback engine so a **free (non-Premium) Spotify account** can play music: each track resolves through YouTube's InnerTube API in-process (the VISIONOS client yt-dlp itself prefers), with `yt-dlp` (flag-only, no download) as the fallback, and plays through the app's own in-process audio engine (no player process). Premium accounts keep the untouched librespot engine.

- Repository name stays `spotifast-yt`; the app, binary (`musify.exe`), app ids, and data folders are Musify's.
- Not affiliated with or endorsed by Spotify, YouTube, or the Spotifast project.
- Playback streams public YouTube audio client-side, the way Spotube does; it circumvents neither service's server-side protections. Personal use only.
- Windows binaries are built from source via GitHub Actions (artifact `musify-windows-x64`, containing `musify.exe`). See [LICENSE](LICENSE) (MIT).
- Do not open issues about this fork upstream; upstream's contribution policy forbids alternate audio sources.
- **YouTube mode** (`"youtube_mode": true` in `settings.json`): the app opens with no Spotify account at all. Search answers from YouTube Music's own search and every result plays through the YouTube engine; no Spotify request goes out. The library and playlists follow in later milestones.
