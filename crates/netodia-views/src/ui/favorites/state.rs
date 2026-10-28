//! Internal data structures for the Favorites view. Mirrors `src/ui/albums/state.rs` in shape,
//! concentrated here so the public [`crate::ui::favorites::FavoritesUi`] surface stays small.

use std::collections::{HashMap, HashSet};

use parking_lot::Mutex;

use crate::ui::hero_folds::{HeroFold, MostPlayedTotals};
use crate::ui::mosaic_hero::MosaicGuard;
use crate::ui::row_match::Needle;
use crate::ui::track_list_cache::TrackListCache;
use netodia_app::services::settings::{SortDir, ViewSort};
use netodia_core::entities::artist::FavoriteArtist;
use netodia_core::entities::track::{FavoriteStats, MostPlayedFavorite};

/// Per-section cached snapshots — every fetch lands here so callbacks can recover the underlying
/// Rust data without round-tripping through Slint. Each field is mutated only on its own fetch
/// path; the `Mutex`es keep them callable from any tokio worker without pinning the UI thread.
pub(crate) struct FavoritesUiState {
    pub albums: super::albums::AlbumsState,
    /// All favourites *before* the in-memory filter, plus the keys a filter and a sort need.
    /// Retaining the unfiltered set is what lets a keystroke re-walk without hitting `SQLite` and
    /// a header click re-sort in memory rather than re-issuing the query.
    pub tracks_all: TrackListCache,
    /// Unliked rows kept for undo until the user leaves Favorites.
    pub retained_tracks: Mutex<HashSet<i64>>,
    /// Active filter needle — cached so the live-refresh subscriber re-applies it without reading
    /// the Slint property from a tokio thread.
    pub filter: Mutex<Needle>,
    /// Active Songs-tab sort, written on `set-sort-*` and read on every re-fetch.
    pub sort: Mutex<ViewSort>,
    /// Active Favorite Artists sort. A second shadow rather than a shared one: the two tabs sort
    /// different entities over disjoint field sets. Read off the UI thread by
    /// `grids::sort::sort_cached_artists`, which is why it can't live on the global.
    pub artist_sort: Mutex<ViewSort>,
    /// Most recent successful `get_favorite_stats`, held because `hero_chips::publish_favorites`
    /// takes the band's count and running time off this handle rather than back off the
    /// properties written around it — the page is assembled from three fetches, so no call site
    /// holds all of it.
    pub stats: Mutex<FavoriteStats>,
    /// One composed cover/blur pair per visible tab, bounded to the Songs and Albums heroes.
    pub hero_cache:
        Mutex<HashMap<super::FavoritesTab, (Vec<String>, crate::ui::detail_artwork::DetailPair)>>,
    /// Grid-tab rows in Rust shape, so click handlers resolve `(id) -> entity` without re-fetching
    /// and a keystroke or column-count change re-chunks in memory. Refreshed in lockstep on
    /// `refresh_grids`.
    pub most_played: Mutex<Vec<MostPlayedFavorite>>,
    pub fav_artists: Mutex<Vec<FavoriteArtist>>,
    /// What the hero band's chips state about the two lists above them, folded on the worker that
    /// produced the rows rather than at publish time, so `publish_favorites` reads two `Copy`
    /// words instead of walking every favourite on the UI thread.
    ///
    /// Both describe the whole set, not the filtered view: the band names the page, and the counts
    /// that do follow the filter are the ones gating the grids' empty states. Songs retains its
    /// fold with the cached rows; Most Played releases its totals beside its transient grid.
    pub songs_fold: Mutex<HeroFold>,
    pub most_played_totals: Mutex<MostPlayedTotals>,
    /// `TrackListRow.id`s currently `selected: true` on the Slint model. The same diff-then-write
    /// Albums uses to keep updates O(changed) rather than O(rows).
    pub applied_selection: Mutex<HashSet<i32>>,
    /// The covers last composed into the banner, guarding against redoing four decodes, a
    /// 600² compose and a blur when a refresh yields the same top four.
    pub last_mosaic_paths: MosaicGuard,
    /// Hash of the mounted grid's last applied contents plus the tab and column count that shaped
    /// them. The same guard one surface down: a grid write is a `set_vec` reset that rebuilds
    /// every mounted card. Reset on section-leave, the models being cleared there, so a matching
    /// hash would otherwise skip the refill.
    pub last_grid_signature: Mutex<Option<u64>>,
}

impl FavoritesUiState {
    pub(super) fn new() -> Self {
        Self {
            albums: super::albums::AlbumsState::default(),
            tracks_all: TrackListCache::new(),
            retained_tracks: Mutex::new(HashSet::new()),
            filter: Mutex::new(Needle::default()),
            sort: Mutex::new(ViewSort { field: "track_number".to_owned(), dir: SortDir::Asc }),
            artist_sort: Mutex::new(ViewSort {
                field: "favorite_count".to_owned(),
                dir: SortDir::Desc,
            }),
            stats: Mutex::new(FavoriteStats {
                count: 0,
                total_duration_ms: 0,
                artwork_paths: Vec::new(),
            }),
            hero_cache: Mutex::new(HashMap::new()),
            most_played: Mutex::new(Vec::new()),
            fav_artists: Mutex::new(Vec::new()),
            songs_fold: Mutex::new(HeroFold::default()),
            most_played_totals: Mutex::new(MostPlayedTotals::default()),
            applied_selection: Mutex::new(HashSet::new()),
            last_mosaic_paths: MosaicGuard::default(),
            last_grid_signature: Mutex::new(None),
        }
    }
}

/// How many covers to decode up front when a grid tab becomes visible.
///
/// A screenful or two, matching `ui::albums::state::GRID_PREWARM_AHEAD` and **not** the tier's
/// capacity. The grids are uncapped, so prewarming everything would decode a large library's whole
/// favourite set every tick and evict all but the last capacity's worth before a card asked.
pub(super) const GRID_PREWARM_AHEAD: usize = 24;
