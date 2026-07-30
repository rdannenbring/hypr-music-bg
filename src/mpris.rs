//! MPRIS listener.
//!
//! Event-driven off D-Bus `PropertiesChanged` rather than polling. Every
//! existing tool in this space busy-loops on `playerctl metadata`, which burns
//! CPU all session to notice a change that D-Bus would have pushed.

use crate::config::PlayerConfig;
use crate::track::{PlaybackStatus, TrackInfo};
use anyhow::Result;
use futures_util::StreamExt;
use std::collections::HashMap;
use zbus::Connection;
use zbus::zvariant::{OwnedValue, Value};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";

#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2",
    assume_defaults = false
)]
trait Player {
    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;
}

/// What the daemon reacts to.
#[derive(Debug, Clone)]
pub enum Event {
    /// A different album is playing.
    Album(TrackInfo),
    /// Playback paused or stopped, with no album change.
    Idle(PlaybackStatus),
}

pub struct MprisWatcher {
    connection: Connection,
    config: PlayerConfig,
}

impl MprisWatcher {
    pub async fn new(config: PlayerConfig) -> Result<Self> {
        Ok(Self {
            connection: Connection::session().await?,
            config,
        })
    }

    /// Bus names of every running MPRIS player, ignore-list applied.
    async fn players(&self) -> Result<Vec<String>> {
        let dbus = zbus::fdo::DBusProxy::new(&self.connection).await?;
        let names = dbus.list_names().await?;

        let mut found: Vec<String> = names
            .into_iter()
            .map(|n| n.to_string())
            .filter(|n| n.starts_with(MPRIS_PREFIX))
            .filter(|n| {
                let short = n.trim_start_matches(MPRIS_PREFIX).to_ascii_lowercase();
                // Browsers register on MPRIS and would otherwise take over the
                // wallpaper with whatever video tab happens to be open.
                !self
                    .config
                    .ignore
                    .iter()
                    .any(|ignored| short.contains(&ignored.to_ascii_lowercase()))
            })
            .collect();

        // Preferred players first, in the order they were configured.
        found.sort_by_key(|n| {
            let short = n.trim_start_matches(MPRIS_PREFIX).to_ascii_lowercase();
            self.config
                .prefer
                .iter()
                .position(|p| short.contains(&p.to_ascii_lowercase()))
                .unwrap_or(usize::MAX)
        });

        Ok(found)
    }

    /// The player we should be following: the most preferred one that is
    /// actually playing, else the most preferred one at all.
    pub async fn active_player(&self) -> Result<Option<String>> {
        let players = self.players().await?;
        let mut first_present = None;

        for name in players {
            let Ok(proxy) = self.player_proxy(&name).await else {
                continue;
            };
            if first_present.is_none() {
                first_present = Some(name.clone());
            }
            if let Ok(status) = proxy.playback_status().await
                && PlaybackStatus::parse(&status) == PlaybackStatus::Playing
            {
                return Ok(Some(name));
            }
        }

        Ok(first_present)
    }

    async fn player_proxy(&self, bus_name: &str) -> Result<PlayerProxy<'_>> {
        Ok(PlayerProxy::builder(&self.connection)
            .destination(bus_name.to_string())?
            .build()
            .await?)
    }

    pub async fn snapshot(&self, bus_name: &str) -> Result<(TrackInfo, PlaybackStatus)> {
        let proxy = self.player_proxy(bus_name).await?;
        let metadata = proxy.metadata().await?;
        let status = PlaybackStatus::parse(&proxy.playback_status().await?);
        Ok((parse_metadata(&metadata, bus_name), status))
    }

    /// Yield an event whenever the playing album or the playback state changes.
    ///
    /// Album changes are keyed on the album rather than the track: every song on
    /// a record shares one cover, so this does not re-render mid-album.
    ///
    /// Both `Metadata` and `PlaybackStatus` are subscribed. That is not
    /// redundant: `PropertiesChanged` carries only the properties that actually
    /// changed, so a pause emits `PlaybackStatus` with no `Metadata` at all.
    /// Listening on `Metadata` alone makes pause and stop invisible.
    pub async fn watch<F, Fut>(&self, mut on_event: F) -> Result<()>
    where
        F: FnMut(Event) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let dbus = zbus::fdo::DBusProxy::new(&self.connection).await?;
        // Catch players starting and stopping, not just property changes.
        let mut owner_changes = dbus.receive_name_owner_changed().await?;

        let mut last_album: Option<String> = None;
        let mut last_status: Option<PlaybackStatus> = None;
        let mut current = self.active_player().await?;

        // Emit the current state immediately so a daemon started mid-song does
        // not sit on the wrong wallpaper until the next track.
        if let Some(name) = &current
            && let Ok((track, status)) = self.snapshot(name).await
        {
            last_album = Some(track.album_key());
            last_status = Some(status);
            if status == PlaybackStatus::Playing && !track.is_empty() {
                on_event(Event::Album(track)).await;
            } else {
                on_event(Event::Idle(status)).await;
            }
        }

        loop {
            // The proxy has to outlive the streams it hands out, so bind it for
            // the whole iteration. Re-subscribing each pass is deliberate: the
            // player we follow can change under us.
            let proxy = match &current {
                Some(name) => self.player_proxy(name).await.ok(),
                None => None,
            };

            match (&proxy, current.clone()) {
                (Some(player), Some(name)) => {
                    let mut metadata_changes = player.receive_metadata_changed().await;
                    let mut status_changes = player.receive_playback_status_changed().await;

                    // Either property changing means re-read and re-evaluate.
                    let still_alive = tokio::select! {
                        change = metadata_changes.next() => change.is_some(),
                        change = status_changes.next() => change.is_some(),
                        owner = owner_changes.next() => {
                            if owner.is_some() {
                                current = self.active_player().await?;
                            }
                            continue;
                        }
                    };

                    if !still_alive {
                        // Player vanished.
                        current = self.active_player().await?;
                        continue;
                    }

                    if let Ok((track, status)) = self.snapshot(&name).await {
                        self.dispatch(
                            track,
                            status,
                            &mut last_album,
                            &mut last_status,
                            &mut on_event,
                        )
                        .await;
                    }
                }
                _ => {
                    // Nothing to follow; wait for a player to appear.
                    if owner_changes.next().await.is_some() {
                        current = self.active_player().await?;
                    } else {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn dispatch<F, Fut>(
        &self,
        track: TrackInfo,
        status: PlaybackStatus,
        last_album: &mut Option<String>,
        last_status: &mut Option<PlaybackStatus>,
        on_event: &mut F,
    ) where
        F: FnMut(Event) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let album_key = track.album_key();
        let album_changed = last_album.as_deref() != Some(album_key.as_str());

        let action = decide(Transition {
            album_changed,
            status,
            last_status: *last_status,
            track_empty: track.is_empty(),
        });

        *last_album = Some(album_key);
        *last_status = Some(status);

        match action {
            Action::ApplyArt => on_event(Event::Album(track)).await,
            Action::GoIdle => on_event(Event::Idle(status)).await,
            Action::Nothing => {}
        }
    }
}

/// The inputs to a single event decision.
#[derive(Debug, Clone, Copy)]
struct Transition {
    album_changed: bool,
    status: PlaybackStatus,
    last_status: Option<PlaybackStatus>,
    track_empty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    ApplyArt,
    GoIdle,
    Nothing,
}

/// Decide what a property change means.
///
/// Pulled out of `dispatch` as a pure function purely so it can be tested:
/// both of the bugs this replaced were invisible because nothing exercised the
/// pause and resume paths.
fn decide(t: Transition) -> Action {
    // Resuming must re-apply even when the album has not changed. Otherwise
    // play A -> stop (wallpaper restored) -> resume A leaves the restored
    // wallpaper up for the rest of the record.
    let resumed = t.status == PlaybackStatus::Playing
        && t.last_status
            .is_some_and(|previous| previous != PlaybackStatus::Playing);

    if t.status == PlaybackStatus::Playing && (t.album_changed || resumed) && !t.track_empty {
        return Action::ApplyArt;
    }

    // Only on an actual transition, so repeated Paused notifications (players
    // emit them alongside position updates) do not re-restore continuously.
    if t.status != PlaybackStatus::Playing && t.last_status != Some(t.status) {
        return Action::GoIdle;
    }

    Action::Nothing
}

/// Flatten an MPRIS metadata dict into `TrackInfo`.
pub fn parse_metadata(metadata: &HashMap<String, OwnedValue>, bus_name: &str) -> TrackInfo {
    TrackInfo {
        artist: metadata
            .get("xesam:artist")
            .and_then(as_string)
            .unwrap_or_default(),
        album_artist: metadata.get("xesam:albumArtist").and_then(as_string),
        album: metadata
            .get("xesam:album")
            .and_then(as_string)
            .unwrap_or_default(),
        title: metadata
            .get("xesam:title")
            .and_then(as_string)
            .unwrap_or_default(),
        art_url: metadata.get("mpris:artUrl").and_then(as_string),
        track_url: metadata.get("xesam:url").and_then(as_string),
        player: bus_name.trim_start_matches(MPRIS_PREFIX).to_string(),
    }
}

/// Coerce a D-Bus value to a string.
///
/// `xesam:artist` and `xesam:albumArtist` are typed `as` (array of string) in
/// the spec, but plenty of players send a bare string, so both shapes have to
/// be handled or artists silently come out empty.
fn as_string(value: &OwnedValue) -> Option<String> {
    match value.downcast_ref::<&str>() {
        Ok(s) if !s.is_empty() => return Some(s.to_string()),
        _ => {}
    }

    if let Ok(array) = value.downcast_ref::<&zbus::zvariant::Array>() {
        for item in array.iter() {
            if let Value::Str(s) = item
                && !s.as_str().is_empty()
            {
                return Some(s.as_str().to_string());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(
        album_changed: bool,
        status: PlaybackStatus,
        last_status: Option<PlaybackStatus>,
    ) -> Transition {
        Transition {
            album_changed,
            status,
            last_status,
            track_empty: false,
        }
    }

    use PlaybackStatus::{Paused, Playing, Stopped};

    #[test]
    fn a_new_album_applies_art() {
        assert_eq!(decide(t(true, Playing, Some(Playing))), Action::ApplyArt);
    }

    #[test]
    fn same_album_while_already_playing_does_nothing() {
        // Track changes within one record must not re-render.
        assert_eq!(decide(t(false, Playing, Some(Playing))), Action::Nothing);
    }

    /// Regression: pause and stop were previously invisible, because only
    /// `Metadata` was subscribed and a pause changes `PlaybackStatus` alone.
    #[test]
    fn pausing_and_stopping_go_idle() {
        assert_eq!(decide(t(false, Paused, Some(Playing))), Action::GoIdle);
        assert_eq!(decide(t(false, Stopped, Some(Playing))), Action::GoIdle);
    }

    /// Regression: resuming the same album after a restore left the restored
    /// wallpaper up for the rest of the record.
    #[test]
    fn resuming_the_same_album_reapplies_art() {
        assert_eq!(decide(t(false, Playing, Some(Stopped))), Action::ApplyArt);
        assert_eq!(decide(t(false, Playing, Some(Paused))), Action::ApplyArt);
    }

    #[test]
    fn repeated_paused_notifications_restore_only_once() {
        assert_eq!(decide(t(false, Paused, Some(Playing))), Action::GoIdle);
        // Players emit Paused repeatedly alongside position updates.
        assert_eq!(decide(t(false, Paused, Some(Paused))), Action::Nothing);
    }

    #[test]
    fn metadata_less_players_are_ignored() {
        // Chromium exposes only mpris:artUrl, with no artist, album or title.
        let mut transition = t(true, Playing, Some(Playing));
        transition.track_empty = true;
        assert_eq!(decide(transition), Action::Nothing);
    }

    #[test]
    fn first_ever_event_while_playing_applies_art() {
        assert_eq!(decide(t(true, Playing, None)), Action::ApplyArt);
    }
}
