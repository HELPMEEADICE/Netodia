//! Pull-side glue: every `Player.on_*` callback declared in the Slint UI is wired here to
//! a `library::*` function, on the event-loop thread, dispatching the work onto tokio.
//!
//! What is left here is the cross-cutting set, answering to no single view. `wire_all`
//! wires the `Player` global itself plus the `Nav` persist callback; each view slice owns
//! its wiring under `ui/<view>/callbacks/`.

// `pub(in crate::ui)` rather than private: each view slice owns its own `callbacks/`
// submodule, so the wiring reaching these isn't a descendant of this module. Still sealed
// inside `ui::` — nothing in `library/`, `player/` or `boot/` can name either.
pub(in crate::ui) mod macros;

mod artist_picker;
pub(in crate::ui) mod card_actions;
pub(in crate::ui) mod card_selection;
mod clipboard;
mod cloud_tracks;
pub(in crate::ui) mod cross_tab_nav;
mod dialog_closed;
pub(in crate::ui) mod index_persist;
mod library_settings;
mod now_playing;
mod tags;
pub(in crate::ui) mod track_detail;
pub(in crate::ui) mod track_list;
mod updater;

use std::sync::Arc;

use rand::RngExt;
use slint::{ComponentHandle, Model, ModelRc};

use netodia_app::library;
use netodia_app::services::settings::{SortDir, ViewSort};
use netodia_app::services::view_state::ViewStateData;
use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_engine::player::engine::state::lock_state;
use netodia_ui::{AppWindow, CloudEntityDetail, Dialog, Nav, Player};

use index_persist::IndexPersist;
use macros::{spawn_logged, spawn_logged_sync, wire_pb, wire_sync, wire_sync_pb};

pub use cross_tab_nav::wire_cross_tab_nav;
pub use library_settings::wire_library_settings;
pub use now_playing::wire_now_playing_favorite;
pub use tags::wire_tags;
pub use updater::{check_painter, wire as wire_updater};

/// A Slint `[int]` callback param as `Vec<i64>`, for every track-row context-menu callback
/// — single-row mode emits a 1-element array, multi-select the whole view selection.
pub(super) fn collect_track_ids(ids: &ModelRc<i32>) -> Vec<i64> {
    ids.iter().map(i64::from).collect()
}

/// The variant that drops `id == 0` — Browse's disk-only rows, which aren't in the
/// library and can't be queued or favorited. `browse/selection.rs` already filters them
/// upstream; this guards the single-row context-menu path.
pub(super) fn collect_nonzero_track_ids(ids: &ModelRc<i32>) -> Vec<i64> {
    ids.iter().filter(|&id| id != 0).map(i64::from).collect()
}

/// Resolve the queue slot a `play-row` activation should start on. The row index a view
/// hands over indexes its *displayed* rows, the same space as `ids` everywhere but Browse,
/// which also shows disk-only files that `current_in_library_ids` drops — so fall back to
/// a lookup by id, and to the head of the queue if the track isn't in the list at all.
pub(super) fn play_row_start(ids: &[i64], track_id: i64, idx: i32) -> Option<usize> {
    if let Ok(i) = usize::try_from(idx)
        && ids.get(i) == Some(&track_id)
    {
        return Some(i);
    }
    ids.iter().position(|&id| id == track_id)
}

/// Track ids of a live `VecModel<TrackListRow>`, in display order. Only Search needs this:
/// every other view caches its displayed rows on a `*Ui` handle, where Search's visible
/// set is a sorted, truncated projection of `last_results` assembled at render time, so
/// the model is the only place its display order exists. LIMIT-bounded, so the UI-thread
/// walk is cheap.
pub(super) fn model_track_ids(rows: &ModelRc<netodia_ui::TrackListRow>) -> Vec<i64> {
    rows.iter().map(|r| i64::from(r.id)).collect()
}

/// Which raise intent held the `Dialog` global when a menu started fetching a body for it.
///
/// **Take it synchronously at the click and ask [`Self::holds`] past the `await`.** The chrome a
/// menu wrote is stale the moment anything else claims the global, so a body filled after that
/// paints one dialog's rows under another's heading and hands its Accept to the wrong branch of
/// the dispatcher.
///
/// Two things can retire a claim and `open` sees only one of them. Six openers deliberately leave
/// `open` false while Rust loads the body or hops a tick to raise it, so through that window two
/// menus both read "nothing is up"; `Dialog.request-generation` is what separates them, and the
/// second menu's synchronous write is exactly what would otherwise ride out under the first one's
/// rows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct DialogClaim(i32);

impl DialogClaim {
    pub(super) fn take(ui: &AppWindow) -> Self {
        Self(ui.global::<Dialog>().get_request_generation())
    }

    /// [`Self::take`] for a callback that holds only the weak handle. `None` where the window has
    /// gone, which is also the answer to whether the raise is still worth resolving for.
    pub(super) fn take_from(weak: &slint::Weak<AppWindow>) -> Option<Self> {
        weak.upgrade().as_ref().map(Self::take)
    }

    /// Whether this claim may still fill the global.
    pub(super) fn holds(self, ui: &AppWindow) -> bool {
        !ui.global::<Dialog>().get_open() && Self::take(ui) == self
    }
}

/// Replace the queue with `ids`, open on a random one, then turn shuffle on — the header
/// Shuffle pill on the six views that carry one.
///
/// The two halves have to be sequenced, shuffle re-anchoring around whatever is playing.
/// `queue_set_shuffle` rather than a read-then-toggle pair: the pill means "on", and a
/// toggle racing the transport's own button would turn it off. The start slot is random
/// because the shuffle anchors the current track at the front, so starting at the head
/// opens every press on the same song.
pub(super) fn spawn_play_then_shuffle(state: &AppState, tag: &'static str, ids: Vec<i64>) {
    // Not just an early exit: `random_range` panics on an empty range, and a filter
    // matching nothing leaves a live pill over an empty list, the header gating on the
    // view's *unfiltered* count.
    if ids.is_empty() {
        return;
    }
    let start = rand::rng().random_range(..ids.len());
    let state = state.clone();
    state.runtime.clone().spawn(async move {
        if let Err(e) =
            library::playback::player_play_tracks(&state.playback_ctx(), ids, Some(start)).await
        {
            log::warn!("{tag} play: {}", describe(&e));
            return;
        }
        if let Err(e) = library::queue::queue_set_shuffle(&state, true) {
            log::warn!("{tag} shuffle: {}", describe(&e));
        }
    });
}

/// Resolve a sort-pill (or column-header) click against the sort in force: the field
/// already sorted on flips direction, a different one starts ascending.
///
/// `cur_dir` is read the way [`SortDir::from_token`] reads it — only `"desc"` is
/// descending — rather than testing for `"asc"`. The two differ only on a token neither
/// side can currently produce, but disagreeing leaves that pill unable to reach descending
/// at all.
pub(super) fn next_sort(cur_field: &str, cur_dir: &str, clicked: &str) -> (String, SortDir) {
    next_sort_with_natural(cur_field, cur_dir, clicked, None)
}

/// [`next_sort`] for a view with an order of its own to fall back to: ascending,
/// descending, then `natural`.
///
/// A *synthetic* order — one no column header can ask for — is otherwise unreachable the
/// moment anything else is clicked, and the sort persists across restarts. Playlist Detail
/// is the only view naming one (`TrackDetail::NATURAL_SORT`): `"position"` is what
/// drag-to-reorder is gated on, so one click on Title used to retire reordering for good.
/// Nothing paints the third state, no header cell matching the natural field.
pub(super) fn next_sort_with_natural(
    cur_field: &str,
    cur_dir: &str,
    clicked: &str,
    natural: Option<&str>,
) -> (String, SortDir) {
    if cur_field != clicked {
        return (clicked.to_owned(), SortDir::Asc);
    }
    if cur_dir != "desc" {
        return (clicked.to_owned(), SortDir::Desc);
    }
    (natural.unwrap_or(clicked).to_owned(), SortDir::Asc)
}

/// Persist `view_id`'s sort into `views.json`, fire-and-forget: the in-memory re-sort
/// already applied, so the only loss is across a restart. Shared by every
/// `on_request_sort`.
pub(super) fn persist_view_sort(
    state: &AppState,
    view_id: &'static str,
    field: String,
    dir: SortDir,
) {
    let s = state.clone();
    let sort = ViewSort { field, dir };
    state.runtime.spawn_blocking(move || {
        if let Err(e) = library::settings::set_view_sort(&s.paths, view_id.to_owned(), sort) {
            log::warn!("{view_id}::set_view_sort: {}", describe(&e));
        }
    });
}

/// `view_id`'s persisted sort, or `None` on a fresh install, where the caller keeps its
/// Slint-declared default. Each view's `wire_*` uses it to seed the sort header at startup,
/// and `boot::ui_setup` for the Tracks fetch that has to agree with one.
///
/// Takes the state `boot` already read rather than reaching for `views.json` itself: this runs
/// once per view on the main thread ahead of `app.show()`, and the file has been parsed since
/// `main`, so a second read per view buys nothing but the chance to disagree with the first.
///
/// Borrowed rather than handed back as owned strings: most callers only push the field into a
/// `SharedString`, and the ones that keep it clone where they keep it.
pub fn persisted_sort<'a>(
    view_state: Option<&'a ViewStateData>,
    view_id: &str,
) -> Option<&'a ViewSort> {
    view_state?.view_sort.get(view_id)
}

/// Wire every Slint `Player.*` callback to its `library::*` counterpart, and every callback that
/// answers to no single view. Call once after constructing `AppWindow`.
pub fn wire_all(ui: &AppWindow, state: &AppState) {
    // The two card-grid globals: selection state shared across every grid, and the actions its
    // right-click menu fires. Here rather than in a view slice because no one grid owns them.
    card_selection::wire(ui);
    card_actions::wire(ui, state);
    // Every row and card menu's Copy entries, answering to no single view for the same reason.
    clipboard::wire(ui, state);
    cloud_tracks::wire(ui, state);
    dialog_closed::wire(ui, state);

    let player = ui.global::<Player>();
    wire_transport(&player, state);
    wire_seek(ui, state);
    wire_set_volume(&player, state);
    wire_set_playback_speed(&player, state);
    // Player.toggle-favorite is wired in `wire_now_playing_favorite` after the
    // per-view handles exist. Only cloud songs can change favorite membership.

    wire_persist_selected_index(ui, state);
    wire_reveal_in_folder(ui, state);
}

/// Play/pause, next, previous, stop, the volume commit, mute, shuffle and repeat.
fn wire_transport(player: &Player<'_>, state: &AppState) {
    let s = state.clone();
    player.on_play_pause(move || {
        let s = s.clone();
        s.runtime.clone().spawn(async move {
            let result = if s.cloud_playback.restored_cloud_track_waiting() {
                s.cloud_playback.resume_restored_queue().await
            } else {
                library::playback::player_toggle_play_pause(&s.playback_ctx())
            };
            if let Err(error) = result {
                log::warn!("play_pause: {}", netodia_core::error::describe(&error));
            }
        });
    });
    wire_sync_pb!(player, on_next, state, "next", library::playback::player_next);
    wire_sync_pb!(player, on_previous, state, "previous", library::playback::player_previous);
    wire_sync_pb!(player, on_stop, state, "stop", library::playback::player_stop);
    wire_pb!(
        player,
        on_commit_volume,
        state,
        "commit_volume",
        library::playback::commit_player_settings
    );
    wire_pb!(player, on_toggle_mute, state, "toggle_mute", library::playback::player_toggle_mute);
    wire_sync!(
        player,
        on_toggle_shuffle,
        state,
        "toggle_shuffle",
        library::queue::queue_toggle_shuffle
    );
    wire_sync!(player, on_cycle_repeat, state, "cycle_repeat", library::queue::queue_cycle_repeat);
    wire_sync!(
        player,
        on_cycle_playback_mode,
        state,
        "cycle_playback_mode",
        library::queue::queue_cycle_playback_mode
    );
}

/// Hold the slider at the requested position until the backend reports a
/// matching update (`Player.seek_pending_ms`).
fn wire_seek(ui: &AppWindow, state: &AppState) {
    let s = state.clone();
    let weak = ui.as_weak();
    ui.global::<Player>().on_seek(move |position_ms| {
        if let Some(ui) = weak.upgrade() {
            ui.global::<Player>().set_seek_pending_ms(position_ms.max(0));
        }
        let s = s.clone();
        let pos = u64::try_from(position_ms.max(0)).unwrap_or(0);
        let cloud_track_id = {
            let player = lock_state(&s.player_state);
            player.source.as_ref().filter(|track| track.file_path.is_empty()).map(|track| track.id)
        };
        if let Some(song_id) = cloud_track_id {
            let weak = weak.clone();
            s.runtime.clone().spawn(async move {
                let result =
                    s.cloud_playback.seek(song_id, std::time::Duration::from_millis(pos)).await;
                let failed = match result {
                    Ok(result) => !result.started,
                    Err(error) => {
                        log::warn!("cloud seek: {}", describe(&error));
                        true
                    }
                };
                if failed {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            let player = ui.global::<Player>();
                            if player.get_seek_pending_ms()
                                == i32::try_from(pos).unwrap_or(i32::MAX)
                            {
                                player.set_seek_pending_ms(-1);
                            }
                        }
                    });
                }
            });
            return;
        }
        spawn_logged_sync!(s, "seek", library::playback::player_seek(&s.playback_ctx(), pos));
    });
}

/// Clamp + cast before dispatch.
fn wire_set_volume(player: &Player<'_>, state: &AppState) {
    let s = state.clone();
    player.on_set_volume(move |level| {
        let s = s.clone();
        // Negative → 0 (try_from fails); then cap at the volume ceiling.
        let vol = u32::try_from(level)
            .unwrap_or(0)
            .min(netodia_engine::player::engine::state::MAX_VOLUME);
        spawn_logged_sync!(
            s,
            "set_volume",
            library::playback::player_set_volume(&s.playback_ctx(), vol)
        );
    });
}

/// Apply to the live player *and* persist, speed surviving restarts as repeat,
/// shuffle and volume do.
fn wire_set_playback_speed(player: &Player<'_>, state: &AppState) {
    let s = state.clone();
    player.on_set_playback_speed(move |speed| {
        let s = s.clone();
        spawn_logged!(
            s,
            "set_playback_speed",
            library::playback::player_set_playback_speed_committed(
                &s.playback_ctx(),
                f64::from(speed)
            )
        );
    });
}

/// Fired after every sidebar click. Persists `last_nav_index` on the blocking
/// pool and records a history entry so Mouse-4/5 can walk back through tab
/// switches. `record_current` reads on the UI thread ahead of any disk hop, off
/// the post-click index and detail id, so the entry is what the user is about to
/// see.
fn wire_persist_selected_index(ui: &AppWindow, state: &AppState) {
    let nav = ui.global::<Nav>();
    let s = state.clone();
    let ui_weak = ui.as_weak();
    // `Nav.persist-selected-index` can fire twice in one tick — `nav_history::replay`
    // closes the departing detail first, and a close restores a cross-section origin.
    let persist = Arc::new(IndexPersist::new(nav.get_selected_index()));
    nav.on_persist_selected_index(move |idx| {
        if let Some(ui) = ui_weak.upgrade() {
            // Cloud details are a temporary overlay on the current page. A sidebar route change
            // leaves that page, so dismiss the overlay before recording the new location.
            if ui.global::<CloudEntityDetail>().get_open() {
                ui.global::<CloudEntityDetail>().invoke_close();
            }
            crate::ui::nav_history::record_current(&ui);
        }
        persist.publish(idx);
        // Spelled out rather than through `spawn_blocking_logged!`, which takes a
        // string *literal*: a failure has to name the section it dropped.
        let s_disk = s.clone();
        let persist = Arc::clone(&persist);
        s.runtime.spawn_blocking(move || {
            persist.write_if_current(idx, || {
                if let Err(e) = library::settings::set_last_nav_index(&s_disk.paths, idx) {
                    log::warn!("nav: set_last_nav_index({idx}): {}", describe(&e));
                }
            });
        });
    });
}

/// "Open Containing Folder", in every track-row context menu.
fn wire_reveal_in_folder(ui: &AppWindow, state: &AppState) {
    let s = state.clone();
    ui.global::<Nav>().on_reveal_in_folder(move |track_id| {
        let s = s.clone();
        let id = i64::from(track_id);
        s.runtime.clone().spawn(async move {
            if let Err(e) = library::tracks::reveal_in_file_manager(&s.db, id).await {
                log::warn!("nav: reveal_in_folder({id}): {}", describe(&e));
            }
        });
    });
}

#[cfg(test)]
#[path = "tests/play_row_tests.rs"]
mod tests;

// A second flat `#[path]` mod rather than nesting both under `tests`: the play-row
// helpers reach `super::` for the fns they exercise, and nesting moves that one level
// away from this file.
#[cfg(test)]
#[path = "tests/index_persist_tests.rs"]
mod index_persist_tests;
