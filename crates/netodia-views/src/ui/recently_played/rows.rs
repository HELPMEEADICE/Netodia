//! The Slint models the view writes into, and the row mapper that fills the
//! Most Played grid.

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::ui::util::{clamp_i64_to_i32, opt_shared};
use netodia_core::entities::track::MostPlayedFavorite;
use netodia_ui::{
    AppWindow, EntityGridRow as UiEntityGridRow, EntityStripRow as UiEntityStripRow,
    RecentlyPlayed, TrackListRow as UiTrackListRow,
};

/// Bind empty Slint `VecModel`s for the Most Played grid, the Songs list and the
/// selection set. Subsequent updates locate them by downcasting back to
/// `VecModel<T>` from the UI thread.
pub(super) fn install_recently_played_models(ui: &AppWindow) {
    let g = ui.global::<RecentlyPlayed>();

    let most_played: Rc<VecModel<UiEntityGridRow>> = Rc::new(VecModel::default());
    g.set_most_played_rows(ModelRc::from(most_played));

    let tracks: Rc<VecModel<UiTrackListRow>> = Rc::new(VecModel::default());
    g.set_tracks(ModelRc::from(tracks));

    let sel: Rc<VecModel<i32>> = Rc::new(VecModel::default());
    g.set_selected_ids(ModelRc::from(sel));
}

/// Map a `MostPlayedFavorite` to its Slint card row. Subtitle is the artist
/// name; `play_count` rides in the `play_count` slot so the grid's
/// `show-play-count: true` reveals the badge.
pub fn to_slint_most_played_row(t: &MostPlayedFavorite) -> UiEntityStripRow {
    UiEntityStripRow {
        id: clamp_i64_to_i32(t.id),
        remote_id: SharedString::from(t.id.to_string()),
        title: SharedString::from(t.title.as_str()),
        subtitle: opt_shared(t.artist.as_deref()),
        artwork_path: opt_shared(t.artwork_path.as_deref()),
        play_count: t.play_count,
        is_favorite: t.is_favorite,
    }
}
