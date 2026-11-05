//! `SlintEventSink` translates OS media-control events into
//! `library::*` calls on the tokio runtime. It does **not** import Slint —
//! the name reflects "where it ships to" (the UI binary), not what it
//! depends on. Keeping the media controls decoupled from Slint matters for the
//! `EventSink` trait contract in `services::integrations::media_controls`.

use netodia_app::library;
use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_engine::player::engine::event_sink::{EventSink, PlayerEvent};

pub struct SlintEventSink {
    pub state: AppState,
}

impl EventSink for SlintEventSink {
    fn handle(&self, ev: PlayerEvent) {
        let s = self.state.clone();
        self.state.runtime.spawn(async move {
            let ctx = s.playback_ctx();
            let r = match ev {
                PlayerEvent::Pause | PlayerEvent::PlayPause | PlayerEvent::Stop
                    if s.cloud_playback.cancel_queue_track_loading() =>
                {
                    Ok(())
                }
                PlayerEvent::Play | PlayerEvent::PlayPause
                    if s.cloud_playback.restored_cloud_track_waiting() =>
                {
                    s.cloud_playback.resume_restored_queue().await
                }
                PlayerEvent::Play => library::playback::player_play(&ctx),
                PlayerEvent::Pause => library::playback::player_pause(&ctx),
                PlayerEvent::PlayPause => library::playback::player_toggle_play_pause(&ctx),
                PlayerEvent::Next => library::playback::player_next(&ctx),
                PlayerEvent::Previous => library::playback::player_previous(&ctx),
                PlayerEvent::Stop => library::playback::player_stop(&ctx),
                PlayerEvent::SeekTo(ms) => library::playback::player_seek(&ctx, ms),
                PlayerEvent::SetVolume(v) => {
                    library::playback::player_set_volume_committed(&ctx, v).await
                }
                PlayerEvent::SetShuffle(enabled) => library::queue::queue_set_shuffle(&s, enabled),
                PlayerEvent::SetRepeat(mode) => library::queue::queue_set_repeat(&s, mode),
                PlayerEvent::SetSpeed(speed) => {
                    library::playback::player_set_playback_speed_committed(&ctx, speed).await
                }
            };
            if let Err(e) = r {
                log::warn!("media controls -> library error: {}", describe(&e));
            }
        });
    }
}
