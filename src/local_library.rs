//! The library the app owns itself, for listening without Spotify: liked
//! songs and playlists of YouTube tracks, kept as one JSON file.
//!
//! A track is stored with everything a row needs to draw it (title,
//! artists, album, duration, artwork) plus the video id the engine resolves
//! it by, as a `yt:<video id>` URI. A stored track therefore plays with no
//! search and no Spotify request at all.
//!
//! The format stays forward compatible: unknown fields are ignored on load
//! and missing ones default, so a newer build's file still opens here.
//! Writing follows the repository's file rules: small, atomic (a temporary
//! file and a rename), and, once the interface writes through it, handed to
//! the app's `fastframe_file::Writer` rather than written on the UI thread.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::yt_resolver::YtTrack;

/// One playlist of the app's own library.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocalPlaylist {
    pub name: String,
    pub tracks: Vec<YtTrack>,
}

/// The app's own library: everything that needs no account. The liked
/// songs come first, then the playlists in the order they were added.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocalLibrary {
    pub liked: Vec<YtTrack>,
    pub playlists: Vec<LocalPlaylist>,
    #[serde(skip)]
    path: PathBuf,
}

impl LocalLibrary {
    /// Reads the library from its file. A missing file is an empty
    /// library, and a damaged one is reported and starts empty: the file
    /// is the app's own data, never a reason to refuse to start.
    pub fn load(path: PathBuf) -> Self {
        let library = match std::fs::read_to_string(&path) {
            Ok(json) => match serde_json::from_str(&json) {
                Ok(library) => library,
                Err(error) => {
                    log::warn!("Library: the file could not be read ({error}); starting empty");
                    LocalLibrary::default()
                }
            },
            Err(_) => LocalLibrary::default(),
        };
        Self { path, ..library }
    }

    /// The library as its file holds it.
    pub fn encode(&self) -> Option<String> {
        serde_json::to_string_pretty(self)
            .map_err(|error| log::warn!("unable to encode the library: {error}"))
            .ok()
    }

    /// Writes the library back to its file: a temporary file and a rename,
    /// so a crash mid-write cannot truncate the library.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).context("unable to create the library folder")?;
        }
        let json = serde_json::to_string_pretty(self).context("unable to encode the library")?;
        let temporary = self.path.with_extension("tmp");
        std::fs::write(&temporary, json).context("unable to write the library")?;
        std::fs::rename(&temporary, &self.path).context("unable to replace the library file")?;
        Ok(())
    }

    /// Adds a track to the liked songs, or takes it back out; answers
    /// whether it is liked now.
    pub fn toggle_liked(&mut self, track: &YtTrack) -> bool {
        if let Some(index) = self
            .liked
            .iter()
            .position(|liked| liked.video_id == track.video_id)
        {
            self.liked.remove(index);
            false
        } else {
            self.liked.push(track.clone());
            true
        }
    }

    pub fn is_liked(&self, video_id: &str) -> bool {
        self.liked.iter().any(|track| track.video_id == video_id)
    }

    /// A new empty playlist; answers false when the name is taken.
    pub fn add_playlist(&mut self, name: &str) -> bool {
        if self.playlists.iter().any(|playlist| playlist.name == name) {
            return false;
        }
        self.playlists.push(LocalPlaylist {
            name: name.to_string(),
            tracks: Vec::new(),
        });
        true
    }

    /// Adds a track to a named playlist; answers false when there is no
    /// such playlist.
    pub fn add_to_playlist(&mut self, playlist: &str, track: &YtTrack) -> bool {
        let Some(playlist) = self
            .playlists
            .iter_mut()
            .find(|candidate| candidate.name == playlist)
        else {
            return false;
        };
        playlist.tracks.push(track.clone());
        true
    }

    /// Removes a named playlist; answers whether one was removed.
    pub fn remove_playlist(&mut self, name: &str) -> bool {
        let before = self.playlists.len();
        self.playlists.retain(|playlist| playlist.name != name);
        self.playlists.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> YtTrack {
        YtTrack {
            video_id: id.to_string(),
            title: "Alright".to_string(),
            artists: "Kendrick Lamar".to_string(),
            album: "To Pimp A Butterfly".to_string(),
            duration_ms: 233_000,
            art_url: Some("https://lh3.googleusercontent.com/art".to_string()),
            official_audio: true,
        }
    }

    #[test]
    fn the_library_round_trips_through_its_file() {
        let dir = std::env::temp_dir().join(format!("musify-library-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a writable temporary folder");
        let path = dir.join("library.json");

        let mut library = LocalLibrary::load(path.clone());
        assert!(!library.is_liked("67vr-3kpX3Q"), "nothing is liked yet");

        assert!(library.toggle_liked(&track("67vr-3kpX3Q")), "liking adds");
        assert!(
            !library.toggle_liked(&track("67vr-3kpX3Q")),
            "liking again removes"
        );
        assert!(library.toggle_liked(&track("67vr-3kpX3Q")));

        assert!(library.add_playlist("Night drive"), "a new name takes");
        assert!(
            !library.add_playlist("Night drive"),
            "a taken name does not"
        );
        assert!(
            library.add_to_playlist("Night drive", &track("67vr-3kpX3Q")),
            "the track lands in the playlist"
        );
        assert!(
            !library.add_to_playlist("Morning drive", &track("67vr-3kpX3Q")),
            "an unknown playlist answers false"
        );

        library.save().expect("the library saves");
        let mut reloaded = LocalLibrary::load(path.clone());
        assert!(reloaded.is_liked("67vr-3kpX3Q"));
        assert_eq!(reloaded.playlists.len(), 1);
        assert_eq!(reloaded.playlists[0].name, "Night drive");
        assert_eq!(reloaded.playlists[0].tracks[0].video_id, "67vr-3kpX3Q");
        assert_eq!(reloaded.playlists[0].tracks[0].duration_ms, 233_000);

        assert_eq!(reloaded.playlists[0].tracks[0], track("67vr-3kpX3Q"));
        assert!(reloaded.remove_playlist("Night drive"));
        assert!(!reloaded.remove_playlist("Night drive"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
