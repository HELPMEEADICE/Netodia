use std::sync::Arc;

use super::{
    CloudQueuePlan, CloudQueueShuffle, playback_candidates, queue_cloud_tracks,
    queue_track_metadata, resolve_track_metadata, track_summary,
};
use netodia_core::entities::cloud::{Album, Song};
use netodia_core::entities::track::TrackSummary;
use netodia_engine::player::engine::queue::QueueState;
use netodia_engine::player::engine::types::RepeatMode;
use rand::{SeedableRng, rngs::StdRng};

fn replace(queue: &mut QueueState, shuffle: CloudQueueShuffle, start_index: usize) {
    CloudQueuePlan {
        tracks: vec![track(19), track(20), track(21), track(22)],
        start_index,
        shuffle,
    }
    .apply(queue, &mut StdRng::seed_from_u64(42));
}

#[test]
fn explicit_ordered_playback_leaves_shuffle_and_starts_at_the_selected_row() {
    let mut queue = QueueState::default();
    queue.set_repeat_mode(RepeatMode::All);
    replace(&mut queue, CloudQueueShuffle::Shuffle, 2);
    assert!(queue.shuffle_enabled);
    assert_eq!(queue.get_current().map(|track| track.id), Some(21));

    replace(&mut queue, CloudQueueShuffle::Ordered, 0);

    assert!(!queue.shuffle_enabled);
    assert_eq!(ids(&queue), [19, 20, 21, 22]);
    assert_eq!(queue.get_current().map(|track| track.id), Some(19));
    assert_eq!(queue.repeat_mode, RepeatMode::All);
    assert_eq!(
        playback_candidates(4, 1, RepeatMode::Off, CloudQueueShuffle::Ordered.enabled(true)),
        [1, 2, 3]
    );
}

#[test]
fn row_activation_preserves_both_transport_shuffle_preferences() {
    let mut queue = QueueState::default();
    replace(&mut queue, CloudQueueShuffle::Shuffle, 0);

    replace(&mut queue, CloudQueueShuffle::Preserve, 2);
    assert!(queue.shuffle_enabled);
    assert_eq!(queue.get_current().map(|track| track.id), Some(21));

    replace(&mut queue, CloudQueueShuffle::Ordered, 0);
    replace(&mut queue, CloudQueueShuffle::Preserve, 2);
    assert!(!queue.shuffle_enabled);
    assert_eq!(ids(&queue), [19, 20, 21, 22]);
    assert_eq!(queue.get_current().map(|track| track.id), Some(21));
}

fn track(id: i64) -> Arc<TrackSummary> {
    Arc::new(track_summary(Song { id, name: format!("Song {id}"), ..Song::default() }, None, false))
}

#[tokio::test]
async fn known_queue_metadata_does_not_need_a_song_detail_request()
-> netodia_core::error::AppResult<()> {
    let cloud = crate::cloud::tests::service_without_network_for_test();
    let cached = track(3_000_000_001);

    let (summary, _) = resolve_track_metadata(&cloud, cached.id, Some(cached.clone())).await?;

    assert!(Arc::ptr_eq(&summary, &cached));
    Ok(())
}

#[tokio::test]
async fn uncached_metadata_still_requires_a_successful_detail_request() {
    let cloud = crate::cloud::tests::service_without_network_for_test();

    assert!(resolve_track_metadata(&cloud, 3_000_000_001, None).await.is_err());
}

#[tokio::test]
async fn song_detail_covers_survive_queueing_and_defer_to_the_playback_download()
-> netodia_core::error::AppResult<()> {
    let cloud = crate::cloud::tests::service_without_network_for_test();
    let mut queue = QueueState::default();
    let cover = "https://p1.music.126.net/cover.jpg";
    let tracks = [3_000_000_001, 3_000_000_002]
        .into_iter()
        .map(|id| {
            Arc::new(track_summary(
                Song {
                    id,
                    name: format!("Song {id}"),
                    album: Album { cover_url: Some(cover.to_owned()), ..Album::default() },
                    ..Song::default()
                },
                None,
                false,
            ))
        })
        .collect();
    queue_cloud_tracks(&mut queue, tracks, false);

    for queued in &queue.tracks {
        assert_eq!(queued.artwork_path.as_deref(), Some(cover));
        let (playback, cover_url) =
            resolve_track_metadata(&cloud, queued.id, Some(queued.clone())).await?;

        assert_eq!(playback.id, queued.id);
        assert_eq!(playback.artwork_path, None);
        assert_eq!(cover_url.as_deref(), Some(cover));
        assert_eq!(queued.artwork_path.as_deref(), Some(cover));
    }
    Ok(())
}

#[test]
fn queued_metadata_keeps_large_ids_and_defers_remote_artwork() {
    let mut summary = (*track(3_000_000_001)).clone();
    summary.artwork_path = Some("https://p1.music.126.net/cover.jpg".to_owned());
    summary.is_favorite = true;
    let cached = Arc::new(summary);

    let (playback, cover_url) = queue_track_metadata(cached.clone());

    assert_eq!(playback.id, 3_000_000_001);
    assert_eq!(playback.title, "Song 3000000001");
    assert!(playback.is_favorite);
    assert_eq!(playback.artwork_path, None);
    assert_eq!(cover_url.as_deref(), Some("https://p1.music.126.net/cover.jpg"));
    assert_eq!(cached.artwork_path.as_deref(), Some("https://p1.music.126.net/cover.jpg"));
}

#[test]
fn queued_metadata_preserves_downloaded_artwork_without_copying() {
    let mut summary = (*track(19)).clone();
    summary.artwork_path =
        Some(std::path::Path::new("artwork").join("cover.jpg").to_string_lossy().into_owned());
    let cached = Arc::new(summary);

    let (playback, cover_url) = queue_track_metadata(cached.clone());

    assert!(Arc::ptr_eq(&playback, &cached));
    assert_eq!(cover_url, None);
    assert_eq!(playback.artwork_path, cached.artwork_path);
}

fn ids(queue: &QueueState) -> Vec<i64> {
    queue.tracks_in_play_order().iter().map(|track| track.id).collect()
}

#[tokio::test]
async fn roaming_switch_reserves_once_and_stop_rejects_the_delayed_batch()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::cloud_playback::CloudPlayback;
    use crate::state::fixtures::TestPlayback;
    use netodia_engine::player::engine::state::lock_state;
    use netodia_engine::player::engine::types::PlaybackStatus;

    let fixture = TestPlayback::empty().await?;
    let playback = CloudPlayback::new(
        crate::cloud::tests::service_without_network_for_test(),
        reqwest::Client::new(),
        fixture.tmp.path().join("artwork"),
        fixture.ctx.engine.clone(),
        fixture.ctx.player_state.clone(),
        fixture.ctx.sinks.clone(),
    );
    lock_state(&fixture.ctx.player_state).status = PlaybackStatus::Playing;
    let old = playback.reserve_roaming_playback();
    let current = playback.reserve_roaming_playback();

    assert_eq!(lock_state(&fixture.ctx.player_state).status, PlaybackStatus::Stopped);
    assert!(!fixture.ctx.engine.is_playback_request_current(old.1));
    assert!(fixture.ctx.engine.is_playback_request_current(current.1));
    assert!(!playback.play_roaming_queue(Vec::new(), old.0, old.1).await?.started);
    assert!(fixture.ctx.engine.is_playback_request_current(current.1));

    crate::library::playback::player_stop(&fixture.ctx)?;

    assert!(!playback.play_roaming_queue(Vec::new(), current.0, current.1).await?.started);
    assert_eq!(lock_state(&fixture.ctx.player_state).status, PlaybackStatus::Stopped);
    Ok(())
}

#[test]
fn play_next_preserves_large_ids_and_the_batch_order() {
    let mut queue = QueueState::default();
    queue.add_tracks(vec![track(19), track(20)]);
    queue.current_index = Some(0);

    queue_cloud_tracks(&mut queue, vec![track(2_147_483_648), track(3_000_000_000)], true);

    assert_eq!(ids(&queue), [19, 2_147_483_648, 3_000_000_000, 20]);
    assert_eq!(queue.current_index, Some(0));
}

#[test]
fn add_to_queue_appends_without_replacing_the_current_track() {
    let mut queue = QueueState::default();
    queue.add_tracks(vec![track(19), track(20)]);
    queue.current_index = Some(0);

    queue_cloud_tracks(&mut queue, vec![track(2_147_483_648), track(3_000_000_000)], false);

    assert_eq!(ids(&queue), [19, 20, 2_147_483_648, 3_000_000_000]);
    assert_eq!(queue.current_index, Some(0));
}

#[test]
fn play_next_on_an_empty_queue_keeps_the_exact_ids() {
    let mut queue = QueueState::default();

    queue_cloud_tracks(&mut queue, vec![track(2_147_483_648), track(3_000_000_000)], true);

    assert_eq!(ids(&queue), [2_147_483_648, 3_000_000_000]);
    assert_eq!(queue.current_index, None);
}
