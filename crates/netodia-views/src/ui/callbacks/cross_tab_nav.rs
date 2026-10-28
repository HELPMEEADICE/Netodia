//! Single-home wiring for the `go-to-album` / `go-to-artist` / `go-to-genre` callbacks
//! every row context menu emits — the track-list views and the queue sheet. Its own module
//! because each per-view callback file would otherwise need all three target UI handles;
//! instead this runs after every `wire_*`, so the handles are guaranteed to exist.
//!
//! Four steps, mirroring the older cross-tab hand-offs: stamp
//! `*Detail.origin-nav-index` synchronously from where the user is standing, mark the
//! drill-in slide, spawn `open_*_with` and move the nav index and tab inside its
//! completion closure, and persist `last_detail_ids[target_view]` so a restart reopens
//! the same detail.
//!
//! **An origin is a section, not a position.** The four destinations are all tabs of My
//! Library, so a drill starting *inside* that page ends inside it: the tab bar names the
//! detail's own tab for the whole visit, and an arrow restoring the tab it came from
//! contradicts the bar beside it. Such a drill stamps `-1`. The **`tab` half of
//! [`Origin`] survives** for the other question it answers — "did the user navigate away
//! mid-fetch", which the nav index alone stopped being able to tell once the four
//! destinations became tabs of one page.
//!
//! Search is the exception: its row menu calls the existing `Search.open-album` /
//! `open-artist`, so only `go-to-genre` is wired here.

use std::sync::Arc;

use slint::{ComponentHandle, Weak};

use crate::ui::albums::{self as albums_ui_mod, AlbumsUi};
use crate::ui::artists::{self as artists_ui_mod, ArtistsUi};
use crate::ui::genres::{self as genres_ui_mod, GenresUi};
use crate::ui::my_library::{NAV_MY_LIBRARY, NO_TAB, go_to_tab, tab_of_section};
use crate::ui::nav_transition;
use crate::ui::track_list_view::view_id;
use netodia_app::library;
use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_ui::{
    AlbumDetail, Albums, AppWindow, ArtistDetail, Browse, Favorites, GenreDetail, MyLibrary, Nav,
    NavEnterFrom, PlaylistDetail, Queue, RecentlyPlayed, Search, Tracks,
};

/// Where the user is standing when a "Go to …" fires: the nav index, plus the My Library
/// tab when that index *is* My Library. Both halves are read synchronously, before any
/// await — the tab half guards the mid-fetch flip, and [`Origin::stamp`] decides what
/// the destination's back arrow records.
#[derive(Clone, Copy)]
pub(in crate::ui) struct Origin {
    nav: i32,
    tab: i32,
}

impl Origin {
    /// A section that has no tabs — Favorites, Search and the rest.
    pub(in crate::ui) fn section(nav: i32) -> Self {
        Self { nav, tab: NO_TAB }
    }

    /// Read the current position off the globals. UI thread only.
    pub(in crate::ui) fn read(ui: &AppWindow) -> Self {
        let nav = ui.global::<Nav>().get_selected_index();
        Self { nav, tab: tab_of_section(ui, nav) }
    }

    /// What the destination stamps as its `origin-nav-index`. See [`origin_stamp`].
    fn stamp(self) -> i32 {
        origin_stamp(self.nav)
    }

    /// Whether the user is still where the drill started. Guards the destination flip
    /// inside the fetch's completion closure — moving them after they navigated away
    /// mid-fetch is exactly what this exists to prevent, and the nav index alone stopped
    /// being able to tell once the four destinations became tabs of one page.
    pub(super) fn still_current(self, ui: &AppWindow) -> bool {
        ui.global::<Nav>().get_selected_index() == self.nav
            && (self.nav != NAV_MY_LIBRARY || ui.global::<MyLibrary>().get_tab_idx() == self.tab)
    }
}

/// The `origin-nav-index` a drill starting at section `nav` records: `-1` when that
/// section *is* My Library, the index itself otherwise.
///
/// **An origin means "another section".** The four destinations are all tabs of one
/// page, so a drill that starts there ends there — the tab bar names the detail's own
/// tab for the whole visit, and a back arrow restoring the tab it came from contradicts
/// the bar beside it. The arrow means "close this detail"; Mouse-4/5 walks the real
/// history. A free fn as well as a method, so the rule is testable without an `AppWindow`.
pub(in crate::ui) fn origin_stamp(nav: i32) -> i32 {
    if nav == NAV_MY_LIBRARY { -1 } else { nav }
}

/// Wire every `go-to-*` callback to its cross-tab handler. Must run *after* the three
/// target `install`s, so their UI handles exist.
pub fn wire_cross_tab_nav(
    ui: &AppWindow,
    state: &AppState,
    albums_ui: &Arc<AlbumsUi>,
    artists_ui: &Arc<ArtistsUi>,
    genres_ui: &Arc<GenresUi>,
) {
    super::artist_picker::wire(ui, state, artists_ui);
    super::now_playing::wire_navigation(ui, state, albums_ui);
    let weak = ui.as_weak();

    // One trio per global, spelled once. A `macro_rules!` rather than a generic fn: these
    // nine globals, `Tracks`, `Browse` and `Queue` among them, share these three setters and
    // nothing else, so a trait for them would need an impl macro of its own and save nothing.
    macro_rules! wire_go_to {
        ($($global:ident),+ $(,)?) => {
            $({
                let g = ui.global::<$global>();
                g.on_go_to_album(make_go_to_album(state, albums_ui, weak.clone()));
                g.on_go_to_artist(make_go_to_artist(state, artists_ui, weak.clone()));
                g.on_go_to_genre(make_go_to_genre(state, genres_ui, weak.clone()));
            })+
        };
    }

    // The row menu hides the entry naming the surface you are already on, so the detail
    // globals still wire all three. `Queue` is the queue sheet's row menu, which dismisses
    // the sheet before invoking.
    wire_go_to!(
        Tracks,
        Browse,
        Favorites,
        PlaylistDetail,
        AlbumDetail,
        ArtistDetail,
        GenreDetail,
        RecentlyPlayed,
        Queue,
    );

    // Search is the exception: album and artist reuse `Search.open-album` / `open-artist`,
    // wired elsewhere.
    let g = ui.global::<Search>();
    g.on_go_to_genre(make_go_to_genre(state, genres_ui, weak.clone()));

    // The Albums grid's card menu wants only "Go to Artist", so it takes the helper directly
    // rather than joining the macro, which installs all three on every global it names.
    ui.global::<Albums>().on_go_to_artist(make_go_to_artist(state, artists_ui, weak));
}

fn make_go_to_album(
    state: &AppState,
    albums_ui: &Arc<AlbumsUi>,
    weak: Weak<AppWindow>,
) -> impl Fn(i32) + 'static {
    let s = state.clone();
    let au = albums_ui.clone();
    move |album_id| {
        let id = i64::from(album_id);
        let Some(ui) = weak.upgrade() else { return };
        let origin = Origin::read(&ui);
        nav_transition::mark(&ui, NavEnterFrom::Right);
        open_album_cross_tab(&s, &au, &weak, id, origin, "cross_tab_nav::go_to_album");
    }
}

fn make_go_to_artist(
    state: &AppState,
    artists_ui: &Arc<ArtistsUi>,
    weak: Weak<AppWindow>,
) -> impl Fn(i32) + 'static {
    let s = state.clone();
    let au = artists_ui.clone();
    move |artist_id| {
        let id = i64::from(artist_id);
        let Some(ui) = weak.upgrade() else { return };
        let origin = Origin::read(&ui);
        nav_transition::mark(&ui, NavEnterFrom::Right);
        open_artist_cross_tab(&s, &au, &weak, id, origin, "cross_tab_nav::go_to_artist");
    }
}

/// The shared "open Album Detail under the Albums tab" hand-off, for every track-list
/// "Go to Album" and the Search-result album card: stamp `origin-nav-index` so the back
/// arrow returns to the section the user came from, spawn `open_album_with` and move to
/// the Albums tab inside its completion closure — guarded, so a nav or tab switch
/// mid-fetch doesn't yank the user away — then persist the detail id.
///
/// The view-transition direction is deliberately **not** set here; `open_album_with`
/// marks it from its argument. A caller that also wants an early `nav_transition::mark`,
/// as the track-list menus do for the same-tab case, calls it before this.
pub(in crate::ui) fn open_album_cross_tab(
    state: &AppState,
    albums_ui: &Arc<AlbumsUi>,
    weak: &Weak<AppWindow>,
    album_id: i64,
    origin: Origin,
    log_tag: &'static str,
) {
    let Some(ui) = weak.upgrade() else { return };
    ui.global::<AlbumDetail>().set_origin_nav_index(origin.stamp());
    let target_tab = ui.global::<MyLibrary>().get_tab_albums();

    let s_fetch = state.clone();
    let au_fetch = albums_ui.clone();
    let weak_fetch = weak.clone();
    state.runtime.clone().spawn(async move {
        if let Err(e) = albums_ui_mod::open_album_with(
            &s_fetch,
            &au_fetch,
            weak_fetch,
            album_id,
            NavEnterFrom::Right,
            move |ui: &AppWindow| {
                if origin.still_current(ui) {
                    go_to_tab(ui, target_tab);
                }
            },
        )
        .await
        {
            log::warn!("{log_tag}({album_id}): {}", describe(&e));
        }
    });

    let s_disk = state.clone();
    state.runtime.spawn_blocking(move || {
        if let Err(e) = library::settings::set_last_detail_id(
            &s_disk.paths,
            view_id::ALBUM_DETAIL,
            Some(album_id),
        ) {
            log::warn!("{log_tag} persist: {}", describe(&e));
        }
    });
}

/// Artist counterpart of [`open_album_cross_tab`], moving to the Artists tab.
pub(in crate::ui) fn open_artist_cross_tab(
    state: &AppState,
    artists_ui: &Arc<ArtistsUi>,
    weak: &Weak<AppWindow>,
    artist_id: i64,
    origin: Origin,
    log_tag: &'static str,
) {
    let Some(ui) = weak.upgrade() else { return };
    ui.global::<ArtistDetail>().set_origin_nav_index(origin.stamp());
    let target_tab = ui.global::<MyLibrary>().get_tab_artists();

    let s_fetch = state.clone();
    let au_fetch = artists_ui.clone();
    let weak_fetch = weak.clone();
    state.runtime.clone().spawn(async move {
        if let Err(e) = artists_ui_mod::open_artist_with(
            &s_fetch,
            &au_fetch,
            weak_fetch,
            artist_id,
            NavEnterFrom::Right,
            move |ui: &AppWindow| {
                if origin.still_current(ui) {
                    go_to_tab(ui, target_tab);
                }
            },
        )
        .await
        {
            log::warn!("{log_tag}({artist_id}): {}", describe(&e));
        }
    });

    let s_disk = state.clone();
    state.runtime.spawn_blocking(move || {
        if let Err(e) = library::settings::set_last_detail_id(
            &s_disk.paths,
            view_id::ARTIST_DETAIL,
            Some(artist_id),
        ) {
            log::warn!("{log_tag} persist: {}", describe(&e));
        }
    });
}

fn make_go_to_genre(
    state: &AppState,
    genres_ui: &Arc<GenresUi>,
    weak: Weak<AppWindow>,
) -> impl Fn(i32) + 'static {
    let s = state.clone();
    let gu = genres_ui.clone();
    move |genre_id| {
        let id = i64::from(genre_id);
        let Some(ui) = weak.upgrade() else { return };

        let origin = Origin::read(&ui);
        // Stamp the origin synchronously so `genres::on_close_detail` can restore
        // the originating *section* on back-press — `-1`, and no restore at all,
        // for a drill that started on another tab of this same page.
        ui.global::<GenreDetail>().set_origin_nav_index(origin.stamp());
        let target_tab = ui.global::<MyLibrary>().get_tab_genres();
        nav_transition::mark(&ui, NavEnterFrom::Right);

        let s_fetch = s.clone();
        let gu_fetch = gu.clone();
        let weak_fetch = weak.clone();
        s.runtime.clone().spawn(async move {
            if let Err(e) = genres_ui_mod::open_genre_with(
                &s_fetch,
                &gu_fetch,
                weak_fetch,
                id,
                NavEnterFrom::Right,
                move |ui: &AppWindow| {
                    if origin.still_current(ui) {
                        go_to_tab(ui, target_tab);
                    }
                },
            )
            .await
            {
                log::warn!("cross_tab_nav::go_to_genre({id}): {}", describe(&e));
            }
        });

        let s_disk = s.clone();
        s.runtime.spawn_blocking(move || {
            if let Err(e) = library::settings::set_last_detail_id(
                &s_disk.paths,
                view_id::GENRE_DETAIL,
                Some(id),
            ) {
                log::warn!("cross_tab_nav::go_to_genre persist: {}", describe(&e));
            }
        });
    }
}

#[cfg(test)]
#[path = "tests/cross_tab_nav_tests.rs"]
mod tests;
