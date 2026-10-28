//! Internal data structures for the Recently-Played view. Trimmed sibling of
//! `src/ui/favorites/state.rs` — one grid tab rather than two, and no sort state
//! at all: the Songs list keeps its recency fetch order and Most Played's title
//! names its own, so the cached sets are only ever filtered.

use std::collections::HashSet;

use parking_lot::Mutex;

use crate::ui::hero_folds::{HeroFold, MostPlayedTotals};
use crate::ui::mosaic_hero::MosaicGuard;
use crate::ui::row_match::Needle;
use netodia_core::entities::track::{MostPlayedFavorite, TrackListRow as RsTrackListRow};

/// What the Songs tab's band states about the recency set.
///
/// Favorites keeps the same pair on the `FavoriteStats` its hero query returns; there
/// is no stats query here, so the numbers are summed off the recency rows and need
/// somewhere that outlives the fetch — the band is per-tab, so a publish triggered
/// from the *grid* still has to be able to state them.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct SongsTotals {
    pub tracks: i32,
    pub duration_ms: i64,
}

/// Per-section cached snapshots — every fetch lands here so callbacks can
/// recover the underlying Rust data without round-tripping through Slint.
pub(crate) struct RecentlyPlayedUiState {
    /// The most-recently-played rows in fetch order, *before* the in-memory filter.
    /// Membership and order are both fixed to this set — the view never re-queries
    /// and never re-orders.
    pub tracks_all: Mutex<Vec<RsTrackListRow>>,
    /// Active filter needle — cached so the live-refresh subscriber can re-apply it
    /// off the UI thread. Shared by both tabs.
    pub filter: Mutex<Needle>,
    /// The account's all-time ranking, preserving server order, play counts and full song ids.
    pub most_played: Mutex<Vec<MostPlayedFavorite>>,
    /// What the band's chips state about the two tabs above them, folded on the
    /// worker that produced the rows rather than at publish time — so
    /// `publish_recently_played` reads `Copy` words instead of walking a play history
    /// on the UI thread, and a publish that beats a sibling fetch is stale rather
    /// than half-built.
    ///
    /// These describe the whole account history, not the filtered view. They stay with the
    /// in-memory snapshot while the page is hidden and reset together when the account changes.
    pub songs_totals: Mutex<SongsTotals>,
    pub songs_fold: Mutex<HeroFold>,
    pub most_played_totals: Mutex<MostPlayedTotals>,
    /// `TrackListRow.id`s currently `selected: true` on the Slint model. The same
    /// diff-then-write the other list views use.
    pub applied_selection: Mutex<HashSet<i32>>,
    /// The covers last composed into the banner, guarding against redoing four decodes, a
    /// 600² compose and a blur when a refresh yields the same top four — the common case.
    pub last_mosaic_paths: MosaicGuard,
    /// Hash of the Most Played grid's last applied contents plus the tab and column
    /// count that shaped them. The same guard one surface down: a grid write is a
    /// `set_vec` reset that rebuilds every mounted card. Reset on
    /// section-leave, the models being cleared there, so a matching hash would
    /// otherwise skip the refill.
    pub last_grid_signature: Mutex<Option<u64>>,
}

impl RecentlyPlayedUiState {
    pub(super) fn new() -> Self {
        Self {
            tracks_all: Mutex::new(Vec::new()),
            filter: Mutex::new(Needle::default()),
            most_played: Mutex::new(Vec::new()),
            songs_totals: Mutex::new(SongsTotals::default()),
            songs_fold: Mutex::new(HeroFold::default()),
            most_played_totals: Mutex::new(MostPlayedTotals::default()),
            applied_selection: Mutex::new(HashSet::new()),
            last_mosaic_paths: MosaicGuard::default(),
            last_grid_signature: Mutex::new(None),
        }
    }
}
