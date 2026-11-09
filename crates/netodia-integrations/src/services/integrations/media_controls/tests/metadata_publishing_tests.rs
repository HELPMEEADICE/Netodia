use std::io;

use netodia_engine::player::engine::now_playing::SourceId;

use super::*;

#[derive(Default)]
struct Panel {
    failures_remaining: usize,
    covers: Vec<Option<String>>,
    title: String,
    artist: String,
    album: String,
    duration: Option<Duration>,
}

impl Panel {
    fn send(&mut self, metadata: &MediaMetadata<'_>) -> io::Result<()> {
        self.covers.push(metadata.cover_url.map(str::to_owned));
        if self.failures_remaining > 0 {
            self.failures_remaining -= 1;
            return Err(io::Error::other("panel unavailable"));
        }
        if let Some(title) = metadata.title {
            title.clone_into(&mut self.title);
        }
        if let Some(artist) = metadata.artist {
            artist.clone_into(&mut self.artist);
        }
        if let Some(album) = metadata.album {
            album.clone_into(&mut self.album);
        }
        self.duration = metadata.duration;
        Ok(())
    }
}

fn source() -> SourceSummary<'static> {
    SourceSummary {
        id: SourceId::Track(1),
        title: "Current song",
        secondary: Some("Artist"),
        album: Some("Album"),
        artwork_path: None,
        duration_ms: Some(180_000),
    }
}

#[test]
fn a_remote_cover_waits_for_download_without_blocking_text() {
    let source = SourceSummary { artwork_path: Some("https://example.test/cover.jpg"), ..source() };
    let mut panel = Panel::default();

    let sent = publish_metadata(Some(&source), |metadata| panel.send(&metadata));

    assert!(sent);
    assert_eq!(panel.covers, [None]);
    assert_eq!(panel.title, source.title);
    assert_eq!(panel.artist, "Artist");
    assert_eq!(panel.album, "Album");
    assert_eq!(panel.duration, Some(Duration::from_mins(3)));
}

#[test]
fn absent_artist_and_album_clear_the_previous_songs_fields() {
    let source = SourceSummary { secondary: None, album: None, ..source() };
    let mut panel = Panel {
        artist: "Previous artist".to_owned(),
        album: "Previous album".to_owned(),
        ..Panel::default()
    };

    let sent = publish_metadata(Some(&source), |metadata| panel.send(&metadata));

    assert!(sent);
    assert_eq!(panel.artist, "");
    assert_eq!(panel.album, "");
}

#[test]
fn a_refused_local_cover_still_publishes_the_current_song() {
    let path = std::env::temp_dir().join("missing-cover.jpg");
    let path = path.to_string_lossy();
    let source = SourceSummary { artwork_path: Some(&path), ..source() };
    let mut panel = Panel { failures_remaining: 1, ..Panel::default() };

    let sent = publish_metadata(Some(&source), |metadata| panel.send(&metadata));

    assert!(sent);
    assert_eq!(panel.covers, [cover_url(&path), None]);
    assert_eq!(panel.title, source.title);
    assert_eq!(panel.duration, Some(Duration::from_mins(3)));
}

#[test]
fn a_text_failure_without_artwork_is_not_retried_in_the_same_sync() {
    let source = source();
    let mut panel = Panel { failures_remaining: 1, ..Panel::default() };

    let sent = publish_metadata(Some(&source), |metadata| panel.send(&metadata));

    assert!(!sent);
    assert_eq!(panel.covers, [None]);
    assert_eq!(panel.title, "");
}

#[test]
fn failed_text_and_artwork_attempts_are_bounded_and_throttled() {
    let path = std::env::temp_dir().join("missing-cover.jpg");
    let path = path.to_string_lossy();
    let source = SourceSummary { artwork_path: Some(&path), ..source() };
    let mut panel = Panel { failures_remaining: 2, ..Panel::default() };
    let mut retry = MetadataRetry::default();
    let now = Instant::now();

    let sent = retry.publish(Some(&source), now, |metadata| panel.send(&metadata));
    assert!(!sent);
    assert_eq!(panel.covers, [cover_url(&path), None]);

    let deferred = retry.publish(Some(&source), now, |metadata| panel.send(&metadata));
    assert!(!deferred);
    assert_eq!(panel.covers.len(), 2);

    let recovered = retry
        .publish(Some(&source), now + METADATA_RETRY_INTERVAL, |metadata| panel.send(&metadata));
    assert!(recovered);
    assert_eq!(panel.covers.len(), 3);
    assert_eq!(panel.title, source.title);
}

#[test]
fn a_new_song_does_not_wait_for_the_previous_songs_retry() {
    let source = source();
    let next = SourceSummary { title: "Next song", ..source };
    let mut panel = Panel { failures_remaining: 1, ..Panel::default() };
    let mut retry = MetadataRetry::default();
    let now = Instant::now();

    let refused = retry.publish(Some(&source), now, |metadata| panel.send(&metadata));
    assert!(!refused);

    let sent = retry.publish(Some(&next), now, |metadata| panel.send(&metadata));
    assert!(sent);
    assert_eq!(panel.title, next.title);
    assert_eq!(panel.covers.len(), 2);
}

#[test]
fn clearing_metadata_explicitly_empties_the_previous_text() {
    let mut panel = Panel {
        title: "Previous song".to_owned(),
        artist: "Previous artist".to_owned(),
        album: "Previous album".to_owned(),
        duration: Some(Duration::from_mins(3)),
        ..Panel::default()
    };

    let sent = publish_metadata(None, |metadata| panel.send(&metadata));

    assert!(sent);
    assert_eq!(panel.title, "");
    assert_eq!(panel.artist, "");
    assert_eq!(panel.album, "");
    assert_eq!(panel.duration, None);
    assert_eq!(panel.covers, [None]);
}

#[test]
fn a_failed_clear_remains_pending_until_a_later_sync() {
    let mut panel =
        Panel { failures_remaining: 1, title: "Previous song".to_owned(), ..Panel::default() };
    let mut retry = MetadataRetry::default();
    let now = Instant::now();

    let refused = retry.publish(None, now, |metadata| panel.send(&metadata));
    assert!(!refused);
    assert_eq!(panel.title, "Previous song");

    let deferred = retry.publish(None, now, |metadata| panel.send(&metadata));
    assert!(!deferred);
    assert_eq!(panel.covers.len(), 1);

    let cleared =
        retry.publish(None, now + METADATA_RETRY_INTERVAL, |metadata| panel.send(&metadata));
    assert!(cleared);
    assert_eq!(panel.title, "");
    assert_eq!(panel.covers.len(), 2);
}
