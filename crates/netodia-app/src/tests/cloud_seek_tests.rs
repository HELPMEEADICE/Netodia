//! Cloud seeks use the real decoder and a loopback media response; no account or audio device.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{
    ActiveStreamTicket, CloudPlayback, CloudQueuePlaybackHandler, open_buffered_source,
    track_summary,
};
use crate::state::fixtures::TestPlayback;
use netodia_core::entities::cloud::{Quality, Song, StreamTicket};
use netodia_engine::player::engine::event_sink::ExternalTrackPlayback;
use netodia_engine::player::engine::state::{lock_state, with_state_emit};
use netodia_engine::player::engine::types::PlaybackStatus;
use netodia_playback::player::playback::replaygain::TrackReplayGain;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn playing_cloud(
    paused: bool,
) -> Result<(TestPlayback, CloudPlayback), Box<dyn std::error::Error>> {
    let fixture = TestPlayback::empty().await?;
    let bytes =
        std::fs::read(std::path::Path::new(netodia_testkit::ASSETS_DIR).join("silence.flac"))?;
    let size = u64::try_from(bytes.len())?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://{}/track.flac", listener.local_addr()?);
    let server = super::tests::serve_media(listener, bytes);
    let source_url = url.clone();
    let (source, reader) = tokio::task::spawn_blocking(move || {
        open_buffered_source(&source_url, "flac", Some(size), Duration::ZERO, None)
    })
    .await??;
    server.join().map_err(|_| std::io::Error::other("media server panicked"))??;
    let playback = CloudPlayback::new(
        crate::cloud::tests::service_for_test(),
        reqwest::Client::new(),
        fixture.tmp.path().join("artwork"),
        fixture.ctx.engine.clone(),
        fixture.ctx.player_state.clone(),
        fixture.ctx.sinks.clone(),
    );
    let summary = Arc::new(track_summary(
        Song { id: 812, name: "Cloud song".into(), duration_ms: 1_000, ..Song::default() },
        None,
        false,
    ));
    with_state_emit(&fixture.ctx.player_state, &fixture.ctx.sinks, |state| {
        state.source = Some(summary.clone());
        state.queue.add_tracks(vec![summary]);
        state.queue.current_index = Some(0);
        state.duration_ms = 1_000;
        state.position_ms = 100;
        state.status = if paused { PlaybackStatus::Paused } else { PlaybackStatus::Playing };
    });
    let generation = fixture.ctx.engine.reserve_playback_request();
    assert!(fixture.ctx.engine.play_prepared_source(
        generation,
        source,
        1.0,
        1.0,
        None,
        TrackReplayGain::default(),
    ));
    if paused {
        fixture.ctx.engine.pause();
    }
    *playback.active_stream_ticket.write() = Some(ActiveStreamTicket {
        ticket: StreamTicket {
            song_id: 812,
            url,
            format: "flac".into(),
            expires_after_seconds: 3600,
            trial: false,
            size: Some(size),
        },
        quality: Quality::default(),
        expires_at: Instant::now() + Duration::from_hours(1),
        reader: Some(reader),
    });
    Ok((fixture, playback))
}

#[tokio::test]
async fn cloud_seek_silences_old_audio_before_the_future_is_polled() -> TestResult {
    let (fixture, playback) = playing_cloud(false).await?;
    assert!(!fixture.ctx.engine.is_paused());

    let seek = playback.seek(812, Duration::from_millis(500));

    assert!(fixture.ctx.engine.is_paused());
    assert_eq!(lock_state(&fixture.ctx.player_state).status, PlaybackStatus::Playing);
    assert!(seek.await?.started);
    assert!(!fixture.ctx.engine.is_paused());
    assert_eq!(lock_state(&fixture.ctx.player_state).position_ms, 500);
    Ok(())
}

#[tokio::test]
async fn cloud_seek_preserves_a_user_pause() -> TestResult {
    let (fixture, playback) = playing_cloud(true).await?;

    assert!(playback.seek(812, Duration::from_millis(500)).await?.started);

    assert!(fixture.ctx.engine.is_paused());
    assert_eq!(lock_state(&fixture.ctx.player_state).status, PlaybackStatus::Paused);
    assert_eq!(lock_state(&fixture.ctx.player_state).position_ms, 500);
    Ok(())
}

#[tokio::test]
async fn cloud_seek_latest_click_wins_even_if_futures_run_in_reverse_order() -> TestResult {
    let (fixture, playback) = playing_cloud(false).await?;
    let first = playback.seek(812, Duration::from_millis(200));
    let latest = playback.seek(812, Duration::from_millis(500));

    assert!(latest.await?.started);
    assert!(!first.await?.started);

    assert_eq!(lock_state(&fixture.ctx.player_state).position_ms, 500);
    assert_eq!(lock_state(&fixture.ctx.player_state).status, PlaybackStatus::Playing);
    assert!(!fixture.ctx.engine.is_paused());
    Ok(())
}

#[tokio::test]
async fn cloud_seek_cannot_restart_after_a_user_pause() -> TestResult {
    let (fixture, playback) = playing_cloud(false).await?;
    let seek = playback.seek(812, Duration::from_millis(500));
    crate::library::playback::player_pause(&fixture.ctx)?;

    assert!(!seek.await?.started);

    assert!(fixture.ctx.engine.is_paused());
    assert_eq!(lock_state(&fixture.ctx.player_state).status, PlaybackStatus::Paused);
    Ok(())
}

#[tokio::test]
async fn cloud_seek_failure_leaves_a_recoverable_pause() -> TestResult {
    let (fixture, mut playback) = playing_cloud(false).await?;
    playback.cloud = crate::cloud::tests::service_without_network_for_test();

    assert!(playback.seek(812, Duration::from_millis(500)).await.is_err());

    assert!(fixture.ctx.engine.is_paused());
    let state = lock_state(&fixture.ctx.player_state);
    assert_eq!(state.status, PlaybackStatus::Paused);
    assert_eq!(state.position_ms, 100);
    assert_eq!(state.source.as_ref().map(|track| track.id), Some(812));
    assert_eq!(state.queue.get_current().map(|track| track.id), Some(812));
    Ok(())
}

#[tokio::test]
async fn cloud_seek_cannot_override_a_later_manual_resume() -> TestResult {
    let (fixture, playback) = playing_cloud(true).await?;
    let seek = playback.seek(812, Duration::from_millis(500));
    let track = lock_state(&fixture.ctx.player_state).source.clone().ok_or("missing track")?;
    let handler =
        CloudQueuePlaybackHandler { playback, runtime: tokio::runtime::Handle::current() };
    handler.resume(track);

    assert!(!seek.await?.started);
    Ok(())
}
