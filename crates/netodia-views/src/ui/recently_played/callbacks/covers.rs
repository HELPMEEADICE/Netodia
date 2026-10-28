//! `RecentlyPlayed`'s lazy cover-lookup callback — the Most Played grid cards.
//! (Songs rows resolve through the shared `RowCovers` global like every other
//! `TrackListRowItem`.) See [`super::wire`].

use std::sync::Arc;

use slint::ComponentHandle;

use netodia_app::state::AppState;
use netodia_ui::{AppWindow, RecentlyPlayed};

/// Wire the `request-most-played-cover` callback.
///
/// The private remote-cache token re-runs a mounted card's cover binding after a download.
pub(super) fn wire(ui: &AppWindow, state: &AppState) {
    let weak = ui.as_weak();
    let covers = Arc::new(crate::ui::remote_covers::RemoteCoverThumbs::new_for_cards(
        state.http_client().clone(),
        state.runtime.clone(),
        move || {
            let _ = weak.upgrade_in_event_loop(|ui| {
                let g = ui.global::<RecentlyPlayed>();
                g.set_most_played_covers_generation(
                    g.get_most_played_covers_generation().wrapping_add(1),
                );
            });
        },
    ));
    ui.global::<RecentlyPlayed>().on_request_most_played_cover(move |path, _generation| {
        covers.get_or_schedule(path.as_str())
    });
}
