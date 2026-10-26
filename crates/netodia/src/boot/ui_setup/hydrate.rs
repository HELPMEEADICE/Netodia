//! Seeding the UI from persisted state, and the fetches that fill it before the user arrives.

use std::sync::Arc;

use netodia_app::services;
use netodia_app::state::AppState;
use netodia_artwork::media::image;
use netodia_core::error::describe;
use netodia_ui::{AppWindow, ArtistDetail, MiniPlayer, Nav, Player};
use netodia_views::ui;
use slint::ComponentHandle;

/// Push the current `PlayerState` into `Player.vm` / `Player.queue` so the
/// now-playing bar shows the persisted last-played source on launch.
pub fn seed_initial_view_model(
    app: &AppWindow,
    state: &AppState,
    cover_thumbs: &Arc<image::cover_thumbs::CoverThumbs>,
) {
    use netodia_engine::player::engine::state::lock_state;

    let s = lock_state(&state.player_state);
    let light = s.to_view_model_light();
    let qvm = s.to_queue_view_model();
    drop(s);

    let player = app.global::<Player>();
    player.set_vm(ui::shell::bridge::to_slint_player_vm(&light, cover_thumbs));
    // Seed the position scalars too, so a restored session shows its saved
    // position before the first playback-monitor tick lands.
    let pos = i32::try_from(light.position_ms).unwrap_or(i32::MAX);
    let dur = i32::try_from(light.duration_ms).unwrap_or(i32::MAX);
    player.set_position_ms(pos);
    player.set_duration_ms(dur);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "u64 → f64 → f32: ms positions stay below f53 mantissa range, f32 progress is for UI display only"
    )]
    let progress = if light.duration_ms > 0 {
        (light.position_ms as f64 / light.duration_ms as f64) as f32
    } else {
        0.0
    };
    player.set_progress(progress);
    player.set_queue(ui::shell::bridge::to_slint_queue_vm(&qvm));

    // The row tier is empty this early, so the seed above is guaranteed to be the cache-only
    // lookup's miss. A restored-but-paused session may not trigger another `view_model` push, so
    // warm the restored track's cover here.
    ui::shell::bridge::warm_vm_cover(
        app.as_weak(),
        &state.runtime,
        cover_thumbs,
        light.current_track.as_ref().and_then(|t| t.artwork_path.clone()).unwrap_or_default(),
    );
}

/// Apply every UI-visible persisted section to the Slint globals — sidebar
/// geometry from `settings.json`, per-view columns and collapse state from
/// `views.json`. Missing entries leave the defaults, which is first-launch
/// behaviour. A `None` snapshot re-reads from disk; boot passes what `main()`
/// already read to avoid a second parse.
pub fn hydrate_ui_from_settings(
    app: &AppWindow,
    state: &AppState,
    cached_settings: Option<&services::settings::SettingsData>,
    cached_view_state: Option<&services::view_state::ViewStateData>,
) {
    let owned_view_state;
    let vs: &services::view_state::ViewStateData = if let Some(v) = cached_view_state {
        v
    } else {
        owned_view_state =
            services::view_state::read_view_state(&state.paths).unwrap_or_else(|e| {
                log::warn!("hydrate_ui_from_settings: read view state failed: {}", describe(&e));
                services::view_state::ViewStateData::default()
            });
        &owned_view_state
    };
    // Ahead of anything that can bail: the column defaults are Rust's, so a list left unhydrated
    // has no columns at all rather than the stock set.
    ui::track_list_view::hydrate_all(app, vs);

    let owned_settings;
    let settings: &services::settings::SettingsData = match cached_settings {
        Some(s) => s,
        None => match services::settings::read_settings(&state.paths) {
            Ok(s) => {
                owned_settings = s;
                &owned_settings
            }
            Err(e) => {
                log::warn!("hydrate_ui_from_settings: read settings failed: {}", describe(&e));
                return;
            }
        },
    };
    apply_sidebar_width(app, settings);
    apply_sidebar_collapsed(app, settings);
    apply_startup_animation_suppression(app, settings);
    apply_mini_backdrop(app, settings);
    app.global::<ArtistDetail>().set_albums_collapsed(vs.artist_albums_collapsed);
    ui::settings::settings_page::seed_tab(app, vs.settings_tab);
    ui::settings::netease_account::start_initial_qr_login_if_active(app);
}

/// `sidebar.slint` already clamps `Nav.sidebar-width` at the use site, so no
/// Rust-side clamp is needed.
fn apply_sidebar_width(app: &AppWindow, settings: &services::settings::SettingsData) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "sidebar widths are small pixel counts; f32 precision is sufficient"
    )]
    let width = settings.sidebar_width as f32;
    app.global::<Nav>().set_sidebar_width(width);
}

fn apply_sidebar_collapsed(app: &AppWindow, settings: &services::settings::SettingsData) {
    app.global::<Nav>().set_sidebar_collapsed(settings.layout.sidebar_collapsed);
}

/// Raise the entrance-animation suppression for the launch mount. The flag only
/// has to be up before the first painted frame, and `ViewTransition` reads it
/// live and hands it back once settled — so this reaches exactly the view the
/// window opens on. A failed settings read leaves it down and the animation plays.
fn apply_startup_animation_suppression(
    app: &AppWindow,
    settings: &services::settings::SettingsData,
) {
    app.global::<Nav>().set_suppress_enter_animation(settings.motion.skip_startup_animation);
}

/// Seed the miniplayer's backdrop switch. Ordinary hydration, unlike its restart-gated neighbour
/// `apply_backdrop_style`, which has to run ahead of `install_views` because the artwork tiers
/// decide their shape from it: this one only mounts a stack that tier already fits.
fn apply_mini_backdrop(app: &AppWindow, settings: &services::settings::SettingsData) {
    app.global::<MiniPlayer>().set_backdrop_shown(settings.backdrop.mini_backdrop);
}

/// Kick off the initial Tracks fetch so the list is populated by the time the
/// user navigates to it. A fresh install falls back to title ascending, matching
/// the `Tracks` global default and the header `ui::tracks::install` seeds.
///
/// Off the same snapshot that header is seeded from, rather than `resolve_view_sort`'s own
/// `views.json` read: the two have to agree, and one source is what makes that structural.
pub fn spawn_initial_tracks_fetch(
    state: &AppState,
    tracks_ui: &Arc<ui::tracks::TracksUi>,
    view_state: Option<&services::view_state::ViewStateData>,
    weak: slint::Weak<AppWindow>,
) {
    // A hidden Songs tab needs neither a library-sized row snapshot nor its
    // prewarmed covers. The section gate consumes this on the first real enter
    // and requests a refresh using the then-current sort and filter.
    if !tracks_ui.section_active() {
        tracks_ui.mark_dirty();
        return;
    }
    let (sort_field, sort_dir) =
        ui::callbacks::persisted_sort(view_state, ui::track_list_view::view_id::TRACKS)
            .map_or_else(
                || ("title".to_owned(), "asc".to_owned()),
                |sort| (sort.field.clone(), sort.dir.as_str().to_owned()),
            );
    let s = state.clone();
    let tu = tracks_ui.clone();
    state.runtime.spawn(async move {
        if let Err(e) =
            ui::tracks::fetch_and_apply(&s, &tu, weak, sort_field, sort_dir, String::new()).await
        {
            log::warn!("initial tracks fetch: {}", describe(&e));
        }
    });
}

/// Kick off an entity grid's initial fetch so its cards are populated by the
/// time the user navigates to it.
///
/// A macro rather than a generic `fn` because each `fetch_grid` is a free function in
/// its own module, and no trait the four `*Ui` types share carries it. Tracks is
/// deliberately not among them, resolving a persisted sort and calling
/// `fetch_and_apply` instead.
macro_rules! initial_grid_fetch {
    ($(#[$doc:meta])* $name:ident, $module:ident, $handle:ty, $label:literal) => {
        $(#[$doc])*
        pub fn $name(state: &AppState, handle: &Arc<$handle>, weak: slint::Weak<AppWindow>) {
            let s = state.clone();
            let h = handle.clone();
            state.runtime.spawn(async move {
                if let Err(e) = ui::$module::fetch_grid(&s, &h, weak).await {
                    log::warn!(
                        "initial {} fetch: {}",
                        $label,
                        netodia_core::error::describe(&e)
                    );
                }
            });
        }
    };
}

initial_grid_fetch!(
    /// Kick off the initial Albums grid fetch.
    spawn_initial_albums_fetch,
    albums,
    ui::albums::AlbumsUi,
    "albums"
);
initial_grid_fetch!(
    /// Kick off the initial Artists grid fetch.
    spawn_initial_artists_fetch,
    artists,
    ui::artists::ArtistsUi,
    "artists"
);
initial_grid_fetch!(
    /// Kick off the initial Genres grid fetch.
    spawn_initial_genres_fetch,
    genres,
    ui::genres::GenresUi,
    "genres"
);
initial_grid_fetch!(
    /// Kick off the initial Playlists grid fetch.
    spawn_initial_playlists_fetch,
    playlists,
    ui::playlists::PlaylistsUi,
    "playlists"
);
