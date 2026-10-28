//! Getting a sheet for whatever is playing, and handing it back.
//!
//! **Three unrelated edges reach [`reseed`], and dropping any one of them shows an empty panel**:
//! a track change, the view re-opening and the header's switch. It is idempotent by
//! [`super::LyricsUi::holds`], so they may overlap freely — whichever gets there first pays.

use std::rc::Rc;
use std::sync::Arc;

use async_compat::Compat;
use slint::{ComponentHandle, SharedString, Weak};

use super::rows::rows_for;
use super::{LyricsUi, NowPlayingState, RowKind, follow};
use netodia_app::library;
use netodia_app::state::AppState;
use netodia_core::entities::lyrics::{LyricLine, Lyrics as Sheet, LyricsOutcome, LyricsSource};
use netodia_core::entities::track::TrackSummary;
use netodia_core::utils::toast::{self, ToastKind};
use netodia_ui::{AppWindow, Lyrics, LyricsState};

/// The sheet for `track`, or `None` where the panel should be left as it is.
///
/// A failure is logged and reads as "nothing found": a sidecar in some old codepage or a directory
/// that is down are both, to a reader, a panel with no words in it, and neither is worth a toast.
async fn fetch(state: &AppState, track: &TrackSummary) -> LyricsOutcome {
    match library::lyrics::for_track(state, track).await {
        Ok(outcome) => outcome,
        Err(e) => {
            log::debug!(
                "ui::now_playing lyrics for {}: {}",
                track.id,
                netodia_core::error::describe(&e)
            );
            LyricsOutcome::Absent
        }
    }
}

/// Bring the panel in line with whatever is playing, looking a sheet up if it has to.
///
/// **Three unrelated edges reach this, and dropping any one of them shows an empty panel.** The
/// sheet is handed back on every close, which is the trade this feature makes against holding a
/// resident copy for the life of the process — so a re-open of the *same* track has nothing to
/// paint, and the artwork's already-applied guard has no way to know that. The three are a track
/// change, the view re-opening and the header's switch.
///
/// Idempotent by [`LyricsUi::holds`], so the edges may overlap freely: whichever gets there first
/// pays, and the claim is taken before the first `.await` rather than after the last.
fn reseed(weak: &Weak<AppWindow>, state: &AppState, np_state: &Rc<NowPlayingState>) {
    let Some(ui) = weak.upgrade() else { return };
    let ly = &np_state.lyrics;

    let track = playing_track(np_state);
    // **Both terms, and the mount is the one that is not obvious.** The switch is the persisted
    // setting rather than "the panel is mounted", so on its own it answers `true` for a closed
    // view behind which the source-change path is still running. That would spend a request per
    // track on a panel nobody can see. Two surfaces mount one, the full view and the
    // miniplayer's column; the card and the strip have no slot for it whatever they paint on.
    let wanted = np_state.renders_panel() && library::lyrics::is_enabled(state);
    let Some(track) = track.filter(|_| wanted) else {
        // Closed, switched off, or no track to look up. Either way the
        // previous song's sheet must not sit under it.
        if ly.holding.borrow().is_some() {
            release(&ui, ly);
        }
        return;
    };
    let track_key = lyrics_track_key(&track);
    if ly.holds(&track_key) {
        return;
    }

    mark_loading(&ui, ly, &track_key);
    let weak = weak.clone();
    let state = state.clone();
    let np_state = np_state.clone();
    let res = slint::spawn_local(Compat::new(async move {
        let outcome = fetch(&state, &track).await;
        let Some(ui) = weak.upgrade() else { return };
        // The song may have moved under the lookup; the claim taken above is what says so, and it
        // is the same test the two other edges dedupe on.
        if !np_state.lyrics.holds(&track_key) {
            return;
        }
        apply(
            &ui,
            &np_state.lyrics,
            &track_key,
            &outcome,
            state.lyrics_online_enabled.get(),
            !track.file_path.is_empty(),
        );
        ensure_readings(&ui, &state, &np_state.lyrics);
    }));
    if let Err(e) = res {
        log::warn!(
            "ui::now_playing lyrics reseed task spawn_local: {}",
            netodia_core::error::describe(&e)
        );
    }
}

/// Fill a resident sheet without fetching it again. Only readings are written back, so stamps,
/// interludes, translations and the current line keep their state.
pub(super) fn ensure_readings(ui: &AppWindow, state: &AppState, ly: &Rc<LyricsUi>) {
    if !state.lyrics_romanization_shown.get()
        || ly.has_readings.get()
        || ly.rows.borrow().is_empty()
    {
        return;
    }
    let lines = ly
        .rows
        .borrow()
        .iter()
        .filter(|row| matches!(row.kind, RowKind::Words))
        .map(|row| LyricLine {
            at_ms: None,
            end_ms: None,
            text: row.text.to_string(),
            romanization: row.romanization.as_ref().map(ToString::to_string),
            translation: None,
        })
        .collect();
    let request = ly.reading_request.get().wrapping_add(1);
    ly.reading_request.set(request);
    let weak = ui.as_weak();
    let state = state.clone();
    let ly = ly.clone();
    let res = slint::spawn_local(Compat::new(async move {
        let lines = match library::lyrics::add_readings(&state, lines).await {
            Ok(lines) => lines,
            Err(e) => {
                log::debug!("lyrics readings: {}", netodia_core::error::describe(&e));
                return;
            }
        };
        let Some(ui) = weak.upgrade() else { return };
        if ly.reading_request.get() != request {
            return;
        }
        ly.has_readings.set(lines.iter().any(|line| {
            line.romanization.as_deref().is_some_and(|reading| !reading.trim().is_empty())
        }));
        for (row, line) in ly
            .rows
            .borrow_mut()
            .iter_mut()
            .filter(|row| matches!(row.kind, RowKind::Words))
            .zip(lines)
        {
            row.romanization = line.romanization.map(SharedString::from);
        }
        follow::republish(&ui, &ly);
    }));
    if let Err(e) = res {
        log::warn!("lyrics readings spawn_local: {}", netodia_core::error::describe(&e));
    }
}

/// Wire the menu's two actions, which need the same `NowPlayingState` the reseed hook does.
///
/// **Both act on whatever is playing, read at click time rather than captured.** The menu is a
/// popup over a view that outlives any one track, so a handle taken at wire time names the wrong
/// song by the second verse.
pub(crate) fn wire_menu(ui: &AppWindow, state: &AppState, np_state: &Rc<NowPlayingState>) {
    let global = ui.global::<Lyrics>();

    {
        let weak = ui.as_weak();
        let state = state.clone();
        let np_state = np_state.clone();
        global.on_refresh(move || {
            let Some(track) = playing_track(&np_state) else {
                return;
            };
            let cloud_track = track.file_path.is_empty();
            let Some(ui) = weak.upgrade() else { return };

            // Given up before the lookup, not after: the claim is what stops the reseed below
            // deduping this against the sheet it is meant to replace.
            release(&ui, &np_state.lyrics);

            let weak = weak.clone();
            let state = state.clone();
            let np_state = np_state.clone();
            let res = slint::spawn_local(Compat::new(async move {
                if !cloud_track
                    && let Err(e) = library::lyrics::forget(&state, &track.file_path).await
                {
                    log::warn!("lyrics refresh: {}", netodia_core::error::describe(&e));
                }
                if weak.upgrade().is_some() {
                    np_state.lyrics.kick();
                }
            }));
            if let Err(e) = res {
                log::warn!(
                    "ui::now_playing lyrics refresh spawn_local: {}",
                    netodia_core::error::describe(&e)
                );
            }
        });
    }

    {
        let weak = ui.as_weak();
        let state = state.clone();
        let np_state = np_state.clone();
        global.on_save_to_tag(move || {
            let Some(track) = playing_track(&np_state) else {
                return;
            };
            if track.file_path.is_empty() {
                return;
            }
            let state = state.clone();
            let weak = weak.clone();
            let np_state = np_state.clone();
            let res = slint::spawn_local(Compat::new(async move {
                let found = library::lyrics::resident_text(&state, &track.file_path).await;
                let text = match found {
                    Ok(Some(text)) => text,
                    // The row is only offered against a sheet on screen, so nothing here is the
                    // store having lost the copy it was resolved from.
                    Ok(None) => {
                        log::debug!("lyrics save: nothing resident for {}", track.id);
                        return;
                    }
                    Err(e) => return report_save_failure(&e),
                };
                if let Err(e) = library::lyrics::write_to_tag(&state, track.id, &text).await {
                    return report_save_failure(&e);
                }
                toast::notify(ToastKind::LyricsSaved, track.title.clone());
                // The tag now holds what the panel is showing, so the row that offered this has
                // nothing left to do — and the sheet is the file's own from here.
                let Some(ui) = weak.upgrade() else { return };
                ui.global::<Lyrics>().set_can_save_to_tag(false);
                np_state.lyrics.holding.borrow_mut().take();
                np_state.lyrics.kick();
            }));
            if let Err(e) = res {
                log::warn!(
                    "ui::now_playing lyrics save spawn_local: {}",
                    netodia_core::error::describe(&e)
                );
            }
        });
    }
}

/// Say a save did not happen, where it was asked for by name.
///
/// **A toast rather than a log line**: this runs only because somebody
/// pressed a control that says it writes to their file, and a control that quietly does nothing is
/// worse than one that says why.
fn report_save_failure(e: &netodia_core::error::AppError) {
    let reason = netodia_core::error::describe(e);
    log::warn!("lyrics save: {reason}");
    toast::notify(ToastKind::OperationFailed, reason);
}

/// The track on the deck, or `None` when the queue is empty.
fn playing_track(np_state: &NowPlayingState) -> Option<Arc<TrackSummary>> {
    np_state.current_source.borrow().as_ref().and_then(|s| s.track.clone())
}

/// Wire the reseed hook, once the `NowPlayingState` this panel's state is a field of exists.
pub(crate) fn wire_reseed(ui: &AppWindow, state: &AppState, np_state: &Rc<NowPlayingState>) {
    let weak_ui = ui.as_weak();
    let state = state.clone();
    let weak_np = Rc::downgrade(np_state);
    *np_state.lyrics.reseed.borrow_mut() = Some(Box::new(move || {
        let Some(np_state) = weak_np.upgrade() else {
            return;
        };
        reseed(&weak_ui, &state, &np_state);
    }));
}

/// Write an outcome into the panel.
///
/// `online_enabled` comes from the caller rather than being read here, because it decides only
/// which of two sentences an empty panel shows and the caller is the half holding `AppState`.
fn apply(
    ui: &AppWindow,
    ly: &Rc<LyricsUi>,
    track_path: &str,
    outcome: &LyricsOutcome,
    online_enabled: bool,
    can_save_to_tag: bool,
) {
    ly.pinned.set(None);
    let global = ui.global::<Lyrics>();

    // Recorded whichever of the three this came to, so a re-open that finds the same track already
    // answered does not pay for a second lookup — including the two that draw no rows, which are
    // answers rather than absences.
    *ly.holding.borrow_mut() = Some(track_path.to_owned());

    match outcome {
        LyricsOutcome::Sheet(sheet) => {
            take_sheet(ly, sheet);
            follow::republish(ui, ly);
            global.set_synced(sheet.is_synced());
            // Every source but the file's own tag is worth offering to write into it. A sidecar
            // counts: it is the user's file, but it is not the one that travels with the track.
            global.set_can_save_to_tag(can_save_to_tag && sheet.source != LyricsSource::Tag);
            global.set_state(LyricsState::Ready);
        }
        LyricsOutcome::Instrumental => {
            clear(ui, ly);
            global.set_state(LyricsState::Instrumental);
        }
        LyricsOutcome::Absent => {
            clear(ui, ly);
            // The one state that names a setting, being the only one a reader can act on here.
            global.set_state(if online_enabled { LyricsState::Missing } else { LyricsState::Off });
        }
        // **The claim above is still recorded, deliberately.** Nothing retries on its own, so
        // without it every tick that reaches `reseed` would start another lookup against a service
        // that has just told us to stop. Refresh is the retry, and it drops the claim itself.
        LyricsOutcome::Unavailable => {
            clear(ui, ly);
            global.set_state(LyricsState::Unavailable);
        }
    }
}

/// Local lyrics use their path as identity; cloud paths are empty, so key those by `NetEase` ID.
fn lyrics_track_key(track: &TrackSummary) -> String {
    if track.file_path.is_empty() {
        format!("netease:{}", track.id)
    } else {
        track.file_path.clone()
    }
}

/// Say a lookup is out, so the panel is not claiming "not found" while it is still looking.
///
/// **Claims the track before the `.await`, not after it.** The lookup can reach a file and then a
/// socket, and a reseed landing in that window would otherwise see a sheet for the *previous*
/// track and start a second one for the same song.
fn mark_loading(ui: &AppWindow, ly: &Rc<LyricsUi>, track_path: &str) {
    clear(ui, ly);
    *ly.holding.borrow_mut() = Some(track_path.to_owned());
    ui.global::<Lyrics>().set_state(LyricsState::Loading);
}

/// Hand back the rows and the table, on the view's own teardown.
///
/// Gives up the claim with them, so the next open looks the sheet up again rather than believing
/// it is still on screen. That belief is what left a re-opened view blank until a restart.
pub(crate) fn release(ui: &AppWindow, ly: &Rc<LyricsUi>) {
    ly.holding.borrow_mut().take();
    clear(ui, ly);
    ui.global::<Lyrics>().set_state(LyricsState::Idle);
}

fn clear(ui: &AppWindow, ly: &Rc<LyricsUi>) {
    ly.reading_request.set(ly.reading_request.get().wrapping_add(1));
    ly.has_readings.set(false);
    ly.rows.borrow_mut().clear();
    ly.offsets.borrow_mut().clear();
    ly.pinned.set(None);
    ly.model.set_vec(Vec::new());

    let global = ui.global::<Lyrics>();
    global.set_synced(false);
    global.set_can_save_to_tag(false);
    global.set_active_index(-1);
    global.set_active_offset(0.0);
    global.set_active_height(0.0);
}

/// Keep the sheet's text and stamps. Heights are not kept, following the width instead.
fn take_sheet(ly: &Rc<LyricsUi>, sheet: &Sheet) {
    ly.has_readings.set(sheet.lines.iter().any(|line| {
        line.romanization.as_deref().is_some_and(|reading| !reading.trim().is_empty())
    }));
    *ly.rows.borrow_mut() = rows_for(sheet);
}
