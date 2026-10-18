# Audio test fixtures

Three-second excerpts used by the `yt_audio` decode tests. Both come from the
same source, so the AAC and Opus paths decode identical audio.

- Source: Big Buck Bunny, the official Blender Foundation upload
  (https://www.youtube.com/watch?v=YE7VzlLtp-4), audio from the first three
  seconds.
- Copyright 2008 Blender Foundation / www.bigbuckbunny.org, licensed under
  Creative Commons Attribution 3.0
  (https://creativecommons.org/licenses/by/3.0/).
- `aac.m4a`: YouTube itag 140 (AAC-LC, 44.1 kHz stereo), fetched with yt-dlp
  and cut with ffmpeg (`--download-sections "*0-3"`, stream copy).
- `opus.webm`: YouTube itag 251 (Opus, 48 kHz stereo), same method.

Note: ffmpeg rewraps both into plain containers, so these fixtures exercise
decoding, not the fragmented (DASH) packaging the live streams use; the
`probe-audio` command covers the live case.
