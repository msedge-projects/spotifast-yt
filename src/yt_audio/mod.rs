//! In-process playback for YouTube audio.
//!
//! The YouTube engine plays without a child process: a ranged HTTP source
//! (`source`), a demux and decode pipeline (`decode`), and a worker thread
//! (`worker`) that feeds the app's own sink chain. See
//! `docs/superpowers/specs/2026-10-18-native-youtube-player-design.md`.

pub mod decode;
pub mod source;
pub mod worker;
