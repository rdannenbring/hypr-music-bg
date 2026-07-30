//! The normalized view of "what is playing right now".

/// Metadata for the currently playing track, flattened out of MPRIS.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackInfo {
    pub artist: String,
    pub album_artist: Option<String>,
    pub album: String,
    pub title: String,
    /// Raw `mpris:artUrl`. May be `file://`, `https://`, or absent.
    pub art_url: Option<String>,
    /// Raw `xesam:url`. For local playback this points at the audio file,
    /// which makes its parent directory a good place to look for cover art.
    pub track_url: Option<String>,
    /// The MPRIS bus name suffix, e.g. `Feishin`.
    pub player: String,
}

impl TrackInfo {
    /// The artist to search remote catalogs with. Album artist is preferred:
    /// on a compilation, `xesam:artist` is the track performer, which will not
    /// find the album.
    pub fn search_artist(&self) -> &str {
        self.album_artist
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.artist)
    }

    /// Identity for caching and change detection.
    ///
    /// Deliberately keyed on the *album*, not the track: every song on a record
    /// shares one cover, so this stops the wallpaper from re-rendering on each
    /// track change within an album.
    pub fn album_key(&self) -> String {
        format!(
            "{}\u{1}{}",
            crate::matching::normalize(self.search_artist()),
            crate::matching::normalize(&self.album)
        )
    }

    /// True when there is not enough here to search for anything.
    pub fn is_empty(&self) -> bool {
        self.album.trim().is_empty() && self.title.trim().is_empty()
    }

    /// Whether this metadata is good enough to query a remote catalogue with.
    ///
    /// Both an artist and an album are required. Without them a search is not
    /// merely useless, it is actively harmful: it spends a rate-limited
    /// MusicBrainz lookup and then records a negative-cache entry, so the album
    /// stays poisoned even after better metadata arrives.
    ///
    /// Internet radio is the usual offender — streams commonly report no artist
    /// and put the station name in `album`, or pack `Artist - Title` into
    /// `title` with nothing else set. Parsing those out is a planned separate
    /// step; it belongs *before* this check, normalizing a stream's fields into
    /// `artist`/`album` so that this gate then passes, rather than being
    /// special-cased inside every source.
    pub fn is_searchable(&self) -> bool {
        !self.search_artist().trim().is_empty() && !self.album.trim().is_empty()
    }
}

/// Playback state, narrowed to what we act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

impl PlaybackStatus {
    pub fn parse(s: &str) -> Self {
        match s {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            _ => Self::Stopped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(artist: &str, album_artist: Option<&str>, album: &str, title: &str) -> TrackInfo {
        TrackInfo {
            artist: artist.into(),
            album_artist: album_artist.map(Into::into),
            album: album.into(),
            title: title.into(),
            ..TrackInfo::default()
        }
    }

    #[test]
    fn album_artist_wins_for_searching() {
        // On a compilation the track performer will not find the album, so the
        // album artist has to take precedence when present.
        let t = track(
            "Various Performer",
            Some("Various Artists"),
            "Now 42",
            "A Song",
        );
        assert_eq!(t.search_artist(), "Various Artists");
    }

    #[test]
    fn falls_back_to_track_artist_when_album_artist_is_missing() {
        // Three of four players on a real session omit xesam:albumArtist.
        let t = track("D12", None, "Devil's Night", "Fight Music");
        assert_eq!(t.search_artist(), "D12");

        // Present but blank counts as missing.
        let t = track("D12", Some("   "), "Devil's Night", "Fight Music");
        assert_eq!(t.search_artist(), "D12");
    }

    #[test]
    fn full_metadata_is_searchable() {
        assert!(track("D12", None, "Devil's Night", "Fight Music").is_searchable());
    }

    /// Internet radio: no artist, station name in `album`. Passes `is_empty`
    /// because album is set, so without this gate it would spend a rate-limited
    /// MusicBrainz lookup and then poison the negative cache for that "album".
    #[test]
    fn stream_metadata_is_not_searchable() {
        let stream = track("", None, "WLIR Radio", "");
        assert!(
            !stream.is_empty(),
            "album is set, so the basic guard passes"
        );
        assert!(
            !stream.is_searchable(),
            "but there is no artist to search with"
        );
    }

    #[test]
    fn missing_album_is_not_searchable() {
        assert!(!track("D12", None, "", "Fight Music").is_searchable());
    }

    #[test]
    fn album_key_ignores_track_and_edition_differences() {
        let a = track("D12", None, "Devil's Night", "Fight Music");
        let b = track(
            "D12",
            None,
            "Devil\u{2019}s Night (Deluxe Edition)",
            "Purple Pills",
        );
        // Same record: the wallpaper must not re-render between these.
        assert_eq!(a.album_key(), b.album_key());
    }
}
