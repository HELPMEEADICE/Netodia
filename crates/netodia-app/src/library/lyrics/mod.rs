//! Lyrics for the playing track, and the one door onto them.
//!
//! Four places a sheet can come from, and a caller learns which one answered only by asking the
//! sheet itself. They resolve through one function so the two switches in front of them each have
//! a single place to guard: the feature switch in front of all four, the lookup switch in front of
//! the directory.
//!
//! **`is_enabled` and `online_lookup_enabled` may not move, and each is the one place its flag is
//! named.** A switch is enforced at the seam rather than per call site, so a fifth source added
//! later cannot reach a file or the network around it.
//!
//! **What stays reachable with lyrics off is what keeps the store honest or belongs to the tag
//! editor**: `forget_all` still runs on a retag, or the stale answer would be waiting when lyrics
//! come back on, and `resident_text` and `write_to_tag` are that dialog's, local and user-driven.
//!
//! **The order is sidecar, tag, store, lookup, and timings cut across it.** A sidecar is the one a
//! user places deliberately, so it outranks everything; the tag is the file's own and outranks
//! what a stranger uploaded; the store is only ever a copy of the last answer. That order is what
//! makes the feature correctable without a setting, since a wrong sheet is fixed by dropping a
//! `.lrc` next to the file.
//!
//! **But a timed sheet beats an untimed one wherever the sidecar has not spoken**, because this
//! feature is the follow rather than the words: a tagger who wrote plain prose into `USLT` has not
//! expressed a preference against the sung line being marked, and honouring the order strictly
//! left exactly those tracks showing a page nothing could follow while a timed sheet sat one
//! request away. The order still decides between two sheets of the same kind.
//!
//! **The store sits on the local side of the switch**, which is the whole of what the switch
//! sells. Turning the lookup off buys no traffic, not no lyrics: a sheet already fetched keeps
//! being read, because reading it costs nothing anybody asked to stop paying.

mod embedded;
mod lrc;
mod netease;
mod online;
mod romanize;
mod sidecar;
mod store;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::state::AppState;
use netodia_core::config::Paths;
use netodia_core::entities::lyrics::{LyricLine, Lyrics, LyricsOutcome};
use netodia_core::entities::tags::{FieldEdit, TagEdit};
use netodia_core::entities::track::TrackSummary;
use netodia_core::error::AppError;
use netodia_net::services::net::pacer::RequestPacer;

/// What this track has, from whichever source has it, ready to draw.
///
/// **This owns its `spawn_blocking`**, unlike [`crate::library::tags::read_lyrics`], whose caller
/// does. Two hops at most and neither is per source: [`resolve`] puts all three local reads on the
/// pool together, and [`romanized`] goes back only for a sheet that has something to romanize.
pub async fn for_track(state: &AppState, track: &TrackSummary) -> Result<LyricsOutcome, AppError> {
    ensure_enabled(state)?;
    if track.file_path.is_empty() {
        return romanized(state, netease::look_up(state, track).await).await;
    }
    romanized(state, resolve(state, track).await?).await
}

/// Whether lyrics run at all.
///
/// **The one place the decision is read**, Now Playing's reseed and close included, so the view
/// deciding whether to ask and this door deciding whether to answer cannot disagree.
pub fn is_enabled(state: &AppState) -> bool {
    state.lyrics_enabled.get()
}

/// A refusal when the user has switched lyrics off.
///
/// A refusal rather than [`LyricsOutcome::Absent`]: the panel is unmounted while off, so a call
/// still arriving is a stale edge worth a log line, where
/// [`online_lookup_enabled`]'s `false` is the ordinary state of most libraries.
fn ensure_enabled(state: &AppState) -> Result<(), AppError> {
    if is_enabled(state) {
        return Ok(());
    }
    Err(AppError::Settings("Lyrics are switched off".to_owned()))
}

/// The romanization drawn under each line, filled in off the UI thread.
///
/// All sources share the same reading policy, including the cloud lookup outside [`resolve`].
async fn romanized(state: &AppState, outcome: LyricsOutcome) -> Result<LyricsOutcome, AppError> {
    let LyricsOutcome::Sheet(mut sheet) = outcome else {
        return Ok(outcome);
    };
    sheet.lines = add_readings(state, sheet.lines).await?;
    Ok(LyricsOutcome::Sheet(sheet))
}

/// Preserve supplied readings; otherwise generate them only while reading display is enabled.
///
/// Also used when the display is enabled for a resident sheet, without another file or network
/// lookup. The engines always run on the blocking pool, including that toggle path.
pub async fn add_readings(
    state: &AppState,
    mut lines: Vec<LyricLine>,
) -> Result<Vec<LyricLine>, AppError> {
    if !state.lyrics_romanization_shown.get() || !romanize::needs_generation(&lines) {
        return Ok(lines);
    }
    state
        .runtime
        .spawn_blocking(move || {
            romanize::apply(&mut lines);
            lines
        })
        .await
        .map_err(AppError::io_source)
}

/// Which of the four sources answers for this track.
///
/// The three local ones share one trip to the blocking pool, so a track change costs one hop
/// rather than one per source.
async fn resolve(state: &AppState, track: &TrackSummary) -> Result<LyricsOutcome, AppError> {
    let path = PathBuf::from(&track.file_path);
    let paths = Arc::clone(&state.paths);
    let track_path = track.file_path.clone();

    let local = state
        .runtime
        .spawn_blocking(move || read_local(&path, &paths.lyrics_dir, &track_path))
        .await
        .map_err(AppError::io_source)??;

    let Local { own, is_sidecar, stored } = local;
    let own = match own {
        // A sidecar is the sheet a user placed on purpose, and a timed sheet is already the best a
        // lookup could answer with. Either ends it here.
        Some(own) if is_sidecar || own.is_synced() => return Ok(LyricsOutcome::Sheet(own)),
        own => own,
    };
    // The directory has answered for this track before, and a second request would fetch that
    // same answer.
    if let Some(stored) = stored {
        return Ok(timed_first(stored, own));
    }
    if !online_lookup_enabled(state) {
        return Ok(own.map_or(LyricsOutcome::Absent, LyricsOutcome::Sheet));
    }

    let fetched = online::look_up(state, track).await?;
    Ok(timed_first(fetched, own))
}

/// The sheet this track already has, as its author wrote it, or `None`.
///
/// **Local only, and that is the point.** The tag editor's Lyrics tab offers to write this into
/// the file, so what it hands over has to be text that exists rather than an answer a lookup might
/// give — and a dialog opening on a selection must not spend a request per track.
///
/// Raw rather than the parsed sheet: a promotion writes the sheet on, so re-serializing
/// [`LyricsOutcome`] back to LRC would drop the `[offset:]` tag and the gloss separator that made
/// it worth keeping.
pub async fn resident_text(state: &AppState, track_path: &str) -> Result<Option<String>, AppError> {
    let path = PathBuf::from(track_path);
    let paths = Arc::clone(&state.paths);
    let track_path = track_path.to_owned();

    state
        .runtime
        .spawn_blocking(move || read_local_text(&path, &paths.lyrics_dir, &track_path))
        .await
        .map_err(AppError::io_source)?
}

/// [`read_local`]'s three homes, answering with text instead of a sheet. Blocking.
///
/// **The same order, and it has to stay the same order**: a sidecar is the user's own, then
/// whichever of the tag and the store is timed, then the file's own tag over a copy of somebody
/// else's upload. What a reader sees and what a promotion writes are one answer, so a track whose
/// panel shows the directory's timed sheet does not hand the tag's plain one to the editor.
fn read_local_text(
    path: &Path,
    lyrics_dir: &Path,
    track_path: &str,
) -> Result<Option<String>, AppError> {
    if let Some(text) = sidecar::read_text(path)? {
        return Ok(Some(text));
    }
    let own = crate::library::tags::read_lyrics(path)?;
    let stored = store::read_text(lyrics_dir, track_path);

    if own.as_deref().is_some_and(lrc::is_timed) {
        return Ok(own);
    }
    if stored.as_deref().is_some_and(lrc::is_timed) {
        return Ok(stored);
    }
    Ok(own.or(stored))
}

/// What the local sources had.
///
/// Three answers rather than the first of three, because which one spoke decides whether the
/// directory may still be asked: a sidecar ends the question, a plain lyrics tag only floors it.
struct Local {
    /// The file's own sheet — the sidecar where there is one, otherwise the lyrics tag.
    own: Option<Lyrics>,
    /// Whether that sheet is the sidecar.
    is_sidecar: bool,
    /// What the directory last said about this track, where it has been asked.
    stored: Option<LyricsOutcome>,
}

/// The better of what the directory had and what the file carries.
///
/// **A timed sheet wins whatever wrote it**, this feature being the follow rather than the words.
/// Below that the file's own tag wins, a stranger's upload being the weaker claim — and a
/// directory calling the recording instrumental loses to a tag with words in it, for that reason
/// rather than as a special case.
fn timed_first(fetched: LyricsOutcome, own: Option<Lyrics>) -> LyricsOutcome {
    let Some(own) = own else {
        return fetched;
    };
    match &fetched {
        LyricsOutcome::Sheet(sheet) if sheet.is_synced() => fetched,
        _ => LyricsOutcome::Sheet(own),
    }
}

/// The three sources that need no network, in the order a user's own file wins. Blocking.
fn read_local(path: &Path, lyrics_dir: &Path, track_path: &str) -> Result<Local, AppError> {
    if let Some(lyrics) = sidecar::read(path)? {
        return Ok(Local { own: Some(lyrics), is_sidecar: true, stored: None });
    }
    let own = embedded::read(path)?;
    // Skipped where the tag is already timed: nothing the store holds could better it, and this
    // runs on every track change.
    let stored = match &own {
        Some(lyrics) if lyrics.is_synced() => None,
        _ => store::read(lyrics_dir, track_path),
    };
    Ok(Local { own, is_sidecar: false, stored })
}

/// Whether a track with no sheet of its own may be looked up online.
///
/// **The one reader of the setting.** A second one is a copy that can be got wrong separately,
/// which is why the feature switch is pinned to exactly one naming of its flag.
///
/// Answering `false` ends the chain in [`LyricsOutcome::Absent`] rather than an error. The panel
/// unmounts while lyrics are off, so a call arriving there is a bug worth reporting; a track with
/// no lyrics and no lookup is the
/// ordinary state of most libraries, and a panel saying so would be right.
fn online_lookup_enabled(state: &AppState) -> bool {
    state.lyrics_online_enabled.get()
}

/// Forget what the directory said about this track, so the next lookup asks again.
///
/// **The whole of what a refresh is.** A sheet the directory matched off wrong tags is right about
/// nothing except the tags it was asked with, and correcting those leaves the wrong answer sitting
/// in the store until it ages out. Touches only the store: a sidecar and a lyrics tag are the
/// user's own files, and a refresh that deleted either would be a delete nobody asked for.
pub async fn forget(state: &AppState, track_path: &str) -> Result<(), AppError> {
    forget_all(state, vec![track_path.to_owned()]).await
}

/// [`forget`] over a retagged batch, in one hop rather than one per track.
///
/// Every path is attempted whatever the ones before it did: an entry left standing is a stale
/// answer, and stopping at the first failure would leave the rest of the batch holding theirs over
/// something that had nothing to do with them. The first error is what comes back.
pub async fn forget_all(state: &AppState, track_paths: Vec<String>) -> Result<(), AppError> {
    let paths = Arc::clone(&state.paths);

    state
        .runtime
        .spawn_blocking(move || {
            let mut first_error = None;
            for track_path in &track_paths {
                if let Err(e) = store::forget(&paths.lyrics_dir, track_path) {
                    first_error = first_error.or(Some(e));
                }
            }
            first_error.map_or(Ok(()), Err)
        })
        .await
        .map_err(AppError::io_source)?
}

/// Write `text` into the track's own lyrics tag.
///
/// A tag edit like any other — through the same writer, the same self-write mark and the same
/// `library_changed` bump — so the file is rewritten once and the scan pipeline re-reads it rather
/// than a hand-built `UPDATE` leaving a fresh mtime beside a stale hash.
///
/// **Here rather than at the call site** because the two surfaces that offer it, the Edit Tags
/// dialog and the Now Playing menu, must not each build their own idea of a lyrics-only edit.
pub async fn write_to_tag(state: &AppState, track_id: i64, text: &str) -> Result<(), AppError> {
    let edit = TagEdit { lyrics: FieldEdit::Set(text.to_owned()), ..TagEdit::default() };
    let report = crate::library::tags::apply_tag_edit(state, vec![track_id], edit, None).await?;

    // **The writer reports per file rather than failing the batch**, so one track's refusal — a
    // read-only file, a container with no key for it — comes back as an `Ok` carrying a failure,
    // and a caller that only checked the `Result` would report a save that did not happen.
    if let Some((_, reason)) = report.failures.first() {
        return Err(AppError::io_other(reason.clone()));
    }
    if report.updated == 0 {
        return Err(AppError::io_other("The lyrics could not be written to the file"));
    }
    Ok(())
}

/// A fresh pacer for the directory, for `AppState` to hold.
///
/// **Here rather than at the construction site**, so the directory client is named only from
/// behind this door: `state` holds the pacer without being able to reach what it paces, which is
/// the property `crates/netodia/tests/lyrics_switch.rs` walks the tree for.
#[must_use]
pub fn pacer() -> RequestPacer {
    netodia_net::services::net::lyrics_directory::pacer()
}

/// Hold the on-disk store to its bounds.
///
/// Through the door like everything else here, so `tasks::lyrics_cache` names no submodule and the
/// naming scheme stays this module's business. Blocking; the task owns the `spawn_blocking`.
pub fn prune_store(paths: &Paths) -> Result<u32, AppError> {
    store::prune(&paths.lyrics_dir)
}

#[cfg(test)]
#[path = "tests/mod_tests.rs"]
mod tests;
