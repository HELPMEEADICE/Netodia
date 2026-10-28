//! `RecentlyPlayed.*` callbacks, split by concern:
//!
//! * [`covers`] — the Most Played grid's lazy cover-lookup callback.
//! * [`subviews`] — the Most Played card actions, the tab switch, and the
//!   grid's column-count push.
//! * [`tracklist`] — the Songs tab: row actions, filter, column visibility,
//!   modifier-aware selection, and its Shuffle pill.
//! * [`lifecycle`] — section enter/leave cache management + the joined
//!   `library_changed` + `stats_changed` re-fetch subscriber.

mod covers;
mod lifecycle;
mod subviews;
mod tracklist;

use std::sync::Arc;

use crate::ui::recently_played::RecentlyPlayedUi;
use netodia_app::state::AppState;
use netodia_ui::AppWindow;

/// Wire every `RecentlyPlayed.*` callback.
///
/// Called by [`super::install`], which is what guarantees the models are in place first — a
/// pairing, rather than two statements a boot-file reorder could separate. `wire_all` still has to
/// have run before it.
pub(super) fn wire(ui: &AppWindow, state: &AppState, rp_ui: &Arc<RecentlyPlayedUi>) {
    covers::wire(ui, state);
    subviews::wire(ui, state, rp_ui);
    tracklist::wire(ui, state, rp_ui);
    lifecycle::wire(ui, state, rp_ui);
}
