//! The per-view slices, their handles, and the Settings sections beside them.

use std::path::Path;
use std::sync::Arc;

use netodia_app::services;
use netodia_app::services::track_artwork::CachedCoverUrl;
use netodia_app::state::AppState;
use netodia_artwork::media::image;
use netodia_core::error::describe;
use netodia_ui::{AppWindow, MyLibrary, Nav, Playlists};
use netodia_views::ui;
use slint::ComponentHandle;

/// The per-view handles `install_views` hands back for the wiring
/// [`super::install_ui`] owns: the initial fetches, the playlist import/export pills
/// (which need the notifications stack), and the `cover_thumbs` consumers.
///
/// Only handles a *caller* reads live here. `BrowseUi` / `FavoritesUi` /
/// `RecentlyPlayedUi` / `SearchUi` deliberately don't: every wired closure
/// captures its own strong `Arc` clone and is owned by the `AppWindow` for the
/// life of the app, and there is no `Weak<…Ui>` anywhere in the tree — so a
/// field here would be a keepalive guarding nothing.
pub struct UiHandles {
    pub cover_thumbs: Arc<image::cover_thumbs::CoverThumbs>,
    pub tracks_ui: Arc<ui::tracks::TracksUi>,
    pub albums_ui: Arc<ui::albums::AlbumsUi>,
    pub artists_ui: Arc<ui::artists::ArtistsUi>,
    pub genres_ui: Arc<ui::genres::GenresUi>,
    pub playlists_ui: Arc<ui::playlists::PlaylistsUi>,
}

/// Resolve every `TrackListRowItem` thumbnail — all track tables, all views —
/// through the one shared row-tier LRU. Rows carry only the artwork path, so
/// only instantiated rows pay a lookup and nothing pins evicted buffers. Part of
/// no slice's `install`, serving every view.
fn install_row_covers(
    app: &AppWindow,
    state: &AppState,
    cover_thumbs: &Arc<image::cover_thumbs::CoverThumbs>,
) {
    let ct = cover_thumbs.clone();
    let weak = app.as_weak();
    let remote = Arc::new(ui::remote_covers::RemoteCoverThumbs::new(
        state.http_client().clone(),
        state.runtime.clone(),
        move || {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else { return };
                let covers = app.global::<netodia_ui::RowCovers>();
                covers.set_generation(covers.get_generation().wrapping_add(1));
            });
        },
    ));
    let resolver = state.track_artwork.clone();
    let state = state.clone();
    let weak = app.as_weak();
    // `generation` is read for its effect on the binding, never its value — see `RowCovers`.
    app.global::<netodia_ui::RowCovers>().on_request(move |source, artist, album, _generation| {
        if ui::remote_covers::is_remote_artwork_url(source.as_str()) {
            remote.get_or_schedule(source.as_str())
        } else if !source.is_empty() {
            let local = ct.get_or_schedule_opt(Some(source.as_str()));
            if local.size().width > 0 || !ct.has_cached_failure(Path::new(source.as_str())) {
                return local;
            }
            request_metadata_fallback(
                &resolver,
                &remote,
                &state,
                &weak,
                artist.as_str(),
                album.as_str(),
            )
        } else {
            request_metadata_fallback(
                &resolver,
                &remote,
                &state,
                &weak,
                artist.as_str(),
                album.as_str(),
            )
        }
    });

    ui::cover_generation::notify_on_decode(cover_thumbs, app, |app| {
        let covers = app.global::<netodia_ui::RowCovers>();
        covers.set_generation(covers.get_generation().wrapping_add(1));
    });
}

/// Resolve a missing local cover from its tagged artist + album, then let the shared row cache
/// fetch the returned URL and repaint all mounted consumers. Only the URL and decoded thumbnail
/// live in memory; the original track and artwork store are untouched.
fn request_metadata_fallback(
    resolver: &Arc<netodia_app::services::track_artwork::TrackArtworkResolver>,
    remote: &Arc<ui::remote_covers::RemoteCoverThumbs>,
    state: &AppState,
    weak: &slint::Weak<AppWindow>,
    artist: &str,
    album: &str,
) -> slint::Image {
    match resolver.cached_url(artist, album) {
        CachedCoverUrl::Found(url) => remote.get_or_schedule(&url),
        CachedCoverUrl::NotFound | CachedCoverUrl::Pending => slint::Image::default(),
        CachedCoverUrl::NotCached => {
            if resolver.claim_lookup(artist, album) {
                let resolver = Arc::clone(resolver);
                let artist = artist.to_owned();
                let album = album.to_owned();
                let client = state.http_client().clone();
                let weak = weak.clone();
                state.runtime.spawn(async move {
                    let found = resolver.lookup_claimed(&client, &artist, &album).await.is_some();
                    if !found {
                        tokio::time::sleep(
                            netodia_app::services::track_artwork::TRANSIENT_RETRY_DELAY,
                        )
                        .await;
                        if !matches!(
                            resolver.cached_url(&artist, &album),
                            CachedCoverUrl::NotCached
                        ) {
                            return;
                        }
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = weak.upgrade() else { return };
                        let covers = app.global::<netodia_ui::RowCovers>();
                        covers.set_generation(covers.get_generation().wrapping_add(1));
                    });
                });
            }
            slint::Image::default()
        }
    }
}

/// Re-run every card grid's cover binding once a scheduled decode lands.
///
/// **One notifier, because there is one grid tier** — `set_decoded_notifier` is a `OnceLock`, and
/// the seven tiers that used to sit in six view files (Favorites held two) share it now. Bumping a
/// generation whose page isn't mounted is a property write with no bindings to dirty, so answering
/// for every local grid costs nothing and saves the tier having to know which grid asked. Slint
/// globals share no trait to be generic over, hence the list.
fn install_grid_covers(app: &AppWindow) {
    ui::cover_generation::notify_on_decode(ui::grid_prewarm::tier(), app, |app| {
        macro_rules! repaint {
            ($($global:ty),+ $(,)?) => {$({
                let g = app.global::<$global>();
                g.set_covers_generation(g.get_covers_generation().wrapping_add(1));
            })+};
        }
        repaint!(
            netodia_ui::Albums,
            netodia_ui::Artists,
            netodia_ui::Browse,
            netodia_ui::Favorites,
            netodia_ui::Playlists,
        );
    });
}

/// Install every view slice and its callbacks. Seeds the persisted nav index
/// and reopens each view's persisted detail (if any). Returns the per-view
/// handles for downstream wiring.
pub fn install_views(
    app: &AppWindow,
    state: &AppState,
    startup_view_state: Option<&services::view_state::ViewStateData>,
) -> UiHandles {
    // The persisted nav index and My Library's tab, *before* any `wire_*` runs:
    // every section handle seeds its synchronous `section_active` shadow by
    // reading these at wire time, and a `SectionActiveGate` can't be relied on
    // to correct a wrong seed — its `ChangeTracker` baselines silently inside
    // `AppWindow::new()`, so a section seeded active for a hidden view stays
    // that way all session. See the gate's bullet in
    // `.claude/rules/ui-patterns.md` for what that costs.
    //
    // The Favorites *tab* seeds down beside the detail views instead, needing
    // the `favorites_ui` handle; My Library's needs none.
    if let Some(vs) = startup_view_state {
        // 4–7 were Albums / Artists / Genres / Playlists — a `views.json`
        // written by a released build still holds them, and they route nowhere.
        // 10 was Radio; old sessions land on My Library just like the retired
        // standalone library sections.
        let idx = ui::my_library::fold_retired_nav_index(vs.last_nav_index);
        if (0..=services::view_state::MAX_NAV_INDEX).contains(&idx) {
            app.global::<Nav>().set_selected_index(idx);
        }
        ui::my_library::seed_tab(app, vs.my_library_tab);
        // The visible library now opens on account playlists; legacy tabs remain drill targets.
        let library = app.global::<MyLibrary>();
        library.set_tab_idx(library.get_tab_playlists());
        let playlists = app.global::<Playlists>();
        playlists
            .set_category(vs.music_library_category.clamp(0, playlists.get_category_count() - 1));
    }

    ui::callbacks::wire_all(app, state);
    // The page's own three callbacks take no view handle, so they wire here
    // rather than after the five tabs.
    ui::my_library::install(app, state);

    // Ahead of the first `install`, every slice cloning the row cache into its handle. The grid
    // tier needs no clone and no `cx` field — `ui::grid_prewarm::tier()` is the whole of it.
    let cover_thumbs = Arc::new(image::cover_thumbs::CoverThumbs::new());
    install_row_covers(app, state, &cover_thumbs);
    install_grid_covers(app);

    let cx = ui::view_ctx::ViewCtx {
        app,
        state,
        cover_thumbs: &cover_thumbs,
        view_state: startup_view_state,
    };

    // Each `install` is models + handle + wiring, in that order, inside the
    // slice. The peer parameters below *are* the ordering — `artists::install`
    // cannot be written above `albums_ui` — so a cross-tab hand-off wired
    // against a handle that doesn't exist yet is a compile error, not a comment.
    let tracks_ui = ui::tracks::install(cx);
    ui::browse::install(cx);
    let albums_ui = ui::albums::install(cx);
    let artists_ui = ui::artists::install(cx, &albums_ui);
    let genres_ui = ui::genres::install(cx);
    let playlists_ui = ui::playlists::install(cx);
    ui::favorites::install(cx, &artists_ui);
    ui::recently_played::install(cx);
    ui::search::install(cx, &albums_ui, &artists_ui);

    // Every track-list view's right-click "Go to Album/Artist/Genre", after all
    // three target handles exist.
    ui::callbacks::wire_cross_tab_nav(app, state, &albums_ui, &artists_ui, &genres_ui);

    // Publish into the nav-history registry so Mouse-4/5 replay can dispatch
    // `open_*` futures without threading handles through `winit_filter`.
    *ui::nav_history::nav().handles().albums.lock() = Some(albums_ui.clone());
    *ui::nav_history::nav().handles().artists.lock() = Some(artists_ui.clone());
    *ui::nav_history::nav().handles().genres.lock() = Some(genres_ui.clone());
    *ui::nav_history::nav().handles().playlists.lock() = Some(playlists_ui.clone());

    // The five persisted-detail reopens stay adjacent here rather than folding
    // into each slice's `install`, because the history seed below depends on
    // none of them having landed yet. The tabbed pages' tab seeds *did* fold
    // in, their handle being the receiver.
    ui::albums::seed_detail_from_settings(app, state, &albums_ui);
    ui::artists::seed_detail_from_settings(app, state, &artists_ui);
    ui::genres::seed_detail_from_settings(app, state, &genres_ui);
    ui::playlists::seed_detail_from_settings(app, state, &playlists_ui);

    // Seed the nav-history with the boot view, so Mouse-4 has a target after the
    // first sidebar click — otherwise a boot landing on a section with no
    // persisted detail records only the destination and `back()` returns `None`.
    // Reads the detail global while it is still `-1`; the async
    // `seed_detail_from_settings` future appends its own entry on top once it
    // lands.
    ui::nav_history::record_current(app);

    ui::callbacks::wire_now_playing_favorite(app, state);
    // Retune the cover tiers to the real display. One call for every card grid, they all draw
    // the same card at the same size; Genres has no cover at all.
    //
    // **Deferred to the event loop, because this runs long before
    // `app.show()`**: inline, `with_winit_window` finds no window and the tier
    // silently keeps its construction fallback, while `scale_factor` answers 1.0
    // until the window is on a monitor and serves a `HiDPI` display the 1×
    // decode size. The closure's `Arc` clones keep the handles this scope drops.
    let tune_rows = cover_thumbs.clone();
    let weak = app.as_weak();
    let retune = move || {
        let Some(app) = weak.upgrade() else { return };
        ui::grid_prewarm::tune_for_display(&app);
        // The row tier is the other shared one and owes the same post-show read.
        tune_rows.set_thumb_size(image::cover_thumbs::row_cover_size(f64::from(
            app.window().scale_factor(),
        )));
    };
    // **And again on every resize.** Both answers move with the window, and a cap read once at
    // launch is the cap a later maximize overruns — the cards past it can only paint
    // placeholders, the lookup behind them scheduling against a tier that cannot hold them.
    // Setting the cap exactly rather than growing it: a smaller window really does draw fewer
    // cards. Nothing here re-warms or announces, and nothing has to: a retune leaves the tier's
    // buffers in place, so the cards keep painting what they had until each replacement lands.
    app.global::<netodia_ui::WindowChrome>().on_display_changed(retune.clone());
    if let Err(e) = slint::invoke_from_event_loop(retune) {
        log::warn!("Failed to schedule cover-cache display tuning: {}", describe(&e));
    }

    // The four handles not returned are deliberately dropped here — see
    // `UiHandles`.
    UiHandles { cover_thumbs, tracks_ui, albums_ui, artists_ui, genres_ui, playlists_ui }
}

/// Every Settings section, plus the notifications stack. The updater's Slint
/// state seeds here too; its daily-check task and callbacks wire later in
/// [`super::install_ui`], beside the onboarding card that defers the task.
pub fn install_library_settings_and_friends(
    app: &AppWindow,
    state: &AppState,
) -> Result<std::rc::Rc<ui::shell::notifications::NotificationsUi>, netodia_core::error::AppError> {
    ui::settings::library_settings::install(app, state)
        .map_err(|e| netodia_core::error::AppError::io("library_settings install", e))?;
    ui::callbacks::wire_library_settings(app, state);
    ui::settings::playback_settings::install_playback_settings(app, state);
    ui::settings::signal_path::install(app, state);
    ui::settings::output_settings::install(app, state);
    ui::equalizer::install_equalizer(app, state);
    ui::replaygain::install_replaygain(app, state);
    ui::visualizer::install_visualizer(app, state);
    ui::sleep_timer::install_sleep_timer(app, state);
    ui::settings::lyrics_settings::install(app, state);
    ui::settings::netease_account::install(app, state);
    let notifications = ui::shell::notifications::install(app);
    ui::settings::file_watching::install(app, state, &notifications);
    ui::settings::updater_settings::install(app, state);
    ui::settings::motion::install(app, state);
    ui::settings::about::install(app, state);
    // Takes the stack because it both toasts and, on the launch after a panic,
    // pushes the "crashed last time" notice itself.
    ui::settings::diagnostics::install(app, state, &notifications);
    ui::settings::settings_page::install(app, state);
    ui::chips::install(app);
    ui::hero_chips::install(app);
    ui::track_columns::install(app);
    // The stack is the one surface a language switch can't reach on its own: its rows carry
    // strings Rust resolved once, they outlive every navigation, and the file-watching
    // toggle two cards from the language picker raises one. Wired here rather than inline in
    // the locale callback because that runs before this handle exists.
    {
        let notifications = notifications.clone();
        if let Err(e) = ui::locale_refresh::on_locale_changed(state, app.as_weak(), move |ui| {
            notifications.refresh_for_locale(ui);
        }) {
            log::warn!("notifications locale refresher: {}", describe(&e));
        }
    }
    Ok(notifications)
}
