//! The Slint models the view writes into, and the row mappers that fill them.

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::ui::util::{clamp_i64_to_i32, opt_shared};
use netodia_core::entities::artist::FavoriteArtist;
use netodia_core::entities::track::MostPlayedFavorite;
use netodia_ui::{
    AppWindow, EntityGridRow as UiEntityGridRow, EntityStripRow as UiEntityStripRow, Favorites,
    TrackListRow as UiTrackListRow,
};

/// Bind empty Slint `VecModel`s for the two grid tabs, the Songs list, the
/// selection set, and the mosaic-path string list. Subsequent updates locate
/// them by downcasting back to `VecModel<T>` from the UI thread.
pub(super) fn install_favorites_models(ui: &AppWindow) {
    let g = ui.global::<Favorites>();

    let most_played: Rc<VecModel<UiEntityGridRow>> = Rc::new(VecModel::default());
    g.set_most_played_rows(ModelRc::from(most_played));

    let artists: Rc<VecModel<UiEntityGridRow>> = Rc::new(VecModel::default());
    g.set_artist_rows(ModelRc::from(artists));

    let tracks: Rc<VecModel<UiTrackListRow>> = Rc::new(VecModel::default());
    g.set_tracks(ModelRc::from(tracks));

    let sel: Rc<VecModel<i32>> = Rc::new(VecModel::default());
    g.set_selected_ids(ModelRc::from(sel));
}

/// Map a `MostPlayedFavorite` to its Slint card row. Subtitle is the
/// artist name. `play_count` rides in the `play_count` slot so the grid's
/// `show-play-count: true` reveals the badge.
pub fn to_slint_most_played_row(t: &MostPlayedFavorite) -> UiEntityStripRow {
    UiEntityStripRow {
        id: clamp_i64_to_i32(t.id),
        remote_id: SharedString::default(),
        title: SharedString::from(t.title.as_str()),
        subtitle: opt_shared(t.artist.as_deref()),
        artwork_path: opt_shared(t.artwork_path.as_deref()),
        play_count: t.play_count,
        is_favorite: t.is_favorite,
    }
}

/// Map a `FavoriteArtist` + caller-supplied subtitle to its Slint card
/// row. The subtitle is the translated `"{n} favorite[s]"` count line and
/// must be resolved on the UI thread via `Favorites.artist-favorite-subtitle(count)`
/// (Slint 1.16 doesn't expose `translate_from_bundle` to Rust). `play_count`
/// is unused.
pub fn to_slint_fav_artist_row(a: &FavoriteArtist, subtitle: SharedString) -> UiEntityStripRow {
    UiEntityStripRow {
        id: clamp_i64_to_i32(a.id),
        remote_id: SharedString::default(),
        title: SharedString::from(a.name.as_str()),
        subtitle,
        artwork_path: opt_shared(a.image_path.as_deref()),
        play_count: 0,
        is_favorite: false,
    }
}
