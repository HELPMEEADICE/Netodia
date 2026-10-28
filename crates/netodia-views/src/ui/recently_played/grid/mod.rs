//! The Most Played tab — a virtualized `EntityCardGrid` over the signed-in account's all-time
//! listening ranking. Its snapshot, order and play counts come from `NetEase`, independently of
//! the local library. Remote covers are loaded lazily for visible cards.
//!
//! Every fetch / apply path re-walks the cached `Vec` through the current
//! `RecentlyPlayed.filter` before writing the model, on the same
//! [`crate::ui::row_match::most_played_matches`] the Songs list beside it runs.
//!
//! [`fetch`] runs the query, [`apply`] turns the cache into a model, [`warm`]
//! names which prepared hash belongs to the tab on screen. No `sort` sibling —
//! this tab's title states its own ordering.

mod apply;
mod fetch;
mod warm;

pub(super) use apply::apply_filtered_grid;
pub use apply::{apply_filtered_grid_now, apply_filtered_grid_settled};
pub use fetch::refresh_grid;

#[cfg(test)]
#[path = "../tests/grid_tests.rs"]
mod tests;
