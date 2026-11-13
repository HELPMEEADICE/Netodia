//! What the OS panel is told back.
//!
//! A dedupe that reads two different songs as one leaves the panel showing the previous one.

use netodia_engine::player::engine::fixtures::{test_track, test_view_model};
use netodia_engine::player::engine::now_playing::SourceId;

use super::*;

/// One track's worth of panel. Spelled out rather than defaulted: `SourceSummary` has no
/// `Default`, every field on it being something a surface actually draws.
fn summary() -> SourceSummary<'static> {
    SourceSummary {
        id: SourceId::Track(1),
        title: "Sunset Drive",
        secondary: Some("The Coastliners"),
        album: Some("Long Way Round"),
        artwork_path: Some("artwork/ab/abcdef.jpg"),
        duration_ms: Some(214_000),
    }
}

/// An empty panel over an empty deck is the one pairing with nothing to write: there is no
/// metadata to clear and no round trip worth spending to clear it.
#[test]
fn nothing_held_and_nothing_playing_needs_no_write() {
    assert!(PublishedMetadata::still_current(None, None));
}

#[test]
fn a_source_arriving_or_leaving_is_always_a_write() {
    let source = summary();
    let held = PublishedMetadata::from(&source);

    assert!(
        !PublishedMetadata::still_current(None, Some(&source)),
        "the panel is empty and the deck is not",
    );
    assert!(
        !PublishedMetadata::still_current(Some(&held), None),
        "and the deck emptied under a panel still showing a song",
    );
}

#[test]
fn a_source_that_would_paint_the_same_panel_needs_no_write() {
    let source = summary();
    let held = PublishedMetadata::from(&source);

    assert!(PublishedMetadata::still_current(Some(&held), Some(&source)));
}

/// Every field the panel draws, moved one at a time. Two of these are the defect the key was taken
/// off the identity for: the source's secondary text moving, and `title` or `album` moving on a
/// track re-tagged in place. Both keep the id they had, so a comparison that
/// drops one of these fields is invisible except as a panel that stops keeping up.
#[test]
fn each_field_the_panel_draws_is_a_write_of_its_own() {
    let reference = summary();
    let held = PublishedMetadata::from(&reference);
    let differs =
        |source: &SourceSummary<'_>| !PublishedMetadata::still_current(Some(&held), Some(source));

    let retitled = SourceSummary { title: "Sunset Drive (Remastered)", ..reference };
    assert!(differs(&retitled), "a track re-tagged in place");

    let announced = SourceSummary { secondary: Some("Night Bus"), ..reference };
    assert!(differs(&announced), "a source changing its secondary text");

    let recompiled = SourceSummary { album: Some("Singles"), ..reference };
    assert!(differs(&recompiled), "the album alone");

    let recovered = SourceSummary { artwork_path: Some("artwork/cd/cdef01.jpg"), ..reference };
    assert!(differs(&recovered), "the cover alone");

    let remastered = SourceSummary { duration_ms: Some(215_000), ..reference };
    assert!(differs(&remastered), "the length alone");
}

const NO_CHANGES: Changes = Changes {
    metadata: false,
    status: false,
    position: false,
    volume: false,
    shuffle: false,
    repeat: false,
    rate: false,
};

fn playing() -> PlayerViewModelLight {
    test_view_model(
        Some(test_track("Sunset Drive", Some("The Coastliners"), Some("Long Way Round"))),
        214_000,
    )
}

/// A snapshot that has already recorded `vm` while playing.
fn published_after(vm: &PlayerViewModelLight) -> Published {
    let mut published = Published::default();
    let changes = published.changes(vm, PlaybackStatus::Playing);
    published.record(vm, PlaybackStatus::Playing, changes);
    published
}

/// The steady state, which is most syncs: a view model re-emitted for a queue edit or a rating
/// must spend no platform call.
#[test]
fn a_view_model_already_recorded_changes_nothing() {
    let vm = playing();
    let published = published_after(&vm);

    let changes = published.changes(&vm, PlaybackStatus::Playing);

    assert_eq!(changes, NO_CHANGES);
}

#[test]
#[cfg(windows)]
fn a_failed_metadata_write_stays_pending_while_other_changes_are_recorded() {
    let vm = playing();
    let mut published = Published::default();
    let changes = Changes { metadata: false, ..published.changes(&vm, PlaybackStatus::Playing) };

    published.record(&vm, PlaybackStatus::Playing, changes);

    assert_eq!(
        published.changes(&vm, PlaybackStatus::Playing),
        Changes { metadata: true, ..NO_CHANGES },
    );
}

#[test]
#[cfg(windows)]
fn a_failed_clear_keeps_the_previously_published_metadata() {
    let vm = playing();
    let mut published = published_after(&vm);
    let empty = PlayerViewModelLight { current_track: None, ..vm.clone() };
    let changes = Changes { metadata: false, ..published.changes(&empty, PlaybackStatus::Stopped) };

    published.record(&empty, PlaybackStatus::Stopped, changes);

    assert!(PublishedMetadata::still_current(published.metadata.as_ref(), vm.source().as_ref()));
    assert!(published.changes(&empty, PlaybackStatus::Stopped).metadata);
}

/// Each part moved alone flags that part and no other. A flag leaking into a neighbour is a signal
/// nobody asked for, and on MPRIS a position leaking into `status` is exactly the periodic
/// `PlaybackStatus` announcement this snapshot exists to stop.
#[test]
fn each_part_of_the_panel_is_flagged_on_its_own() {
    let reference = playing();
    let published = published_after(&reference);
    let flagged = |vm: &PlayerViewModelLight, status| published.changes(vm, status);

    assert_eq!(
        flagged(&reference, PlaybackStatus::Paused),
        Changes { status: true, ..NO_CHANGES },
        "a pause"
    );

    let advanced = PlayerViewModelLight { position_ms: 61_000, ..reference.clone() };
    assert_eq!(
        flagged(&advanced, PlaybackStatus::Playing),
        Changes { position: true, ..NO_CHANGES },
        "playback advancing"
    );

    let quieter = PlayerViewModelLight { volume: 40, ..reference.clone() };
    assert_eq!(
        flagged(&quieter, PlaybackStatus::Playing),
        Changes { volume: true, ..NO_CHANGES },
        "the volume"
    );

    let muted = PlayerViewModelLight { is_muted: true, ..reference.clone() };
    assert_eq!(
        flagged(&muted, PlaybackStatus::Playing),
        Changes { volume: true, ..NO_CHANGES },
        "a mute at the same level, which a panel shows as silence"
    );

    let shuffled = PlayerViewModelLight { shuffle_enabled: true, ..reference.clone() };
    assert_eq!(
        flagged(&shuffled, PlaybackStatus::Playing),
        Changes { shuffle: true, ..NO_CHANGES },
        "shuffle"
    );

    let repeating = PlayerViewModelLight { repeat_mode: RepeatMode::One, ..reference.clone() };
    assert_eq!(
        flagged(&repeating, PlaybackStatus::Playing),
        Changes { repeat: true, ..NO_CHANGES },
        "repeat"
    );

    let next_track = PlayerViewModelLight {
        current_track: Some(test_track("Night Bus", Some("The Coastliners"), None)),
        ..reference
    };
    assert_eq!(
        flagged(&next_track, PlaybackStatus::Playing),
        Changes { metadata: true, ..NO_CHANGES },
        "the next track"
    );
}
