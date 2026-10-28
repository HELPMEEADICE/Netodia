//! The values that cross into Slint as **free strings**, and the `.slint` literals that branch
//! on them.
//!
//! Two of them: `RepeatMode` and `PlaybackStatus` reach `PlayerViewModel` through
//! `ui::shell::bridge` as whatever `as_str` returns. Nothing on either side is checked by a
//! compiler, and neither failure is loud: a renamed `RepeatMode::Off` leaves the playback-mode
//! button showing the repeat icon in sequential mode, because the shared icon asks `== "off"`.
//!
//! Here rather than beside either tree, and this is the case `CLAUDE.md` draws the line for: the
//! claim is about two trees at once, so neither the enum's own suite nor a `.slint` pin can hold
//! it. The enums' Rust halves stay where they are, in `engine/tests/types_tests.rs`.
//!
//! The direction is deliberate. **Every literal the sheet compares against must be one the enum
//! can produce**, not the reverse: `RepeatMode::All` is legitimately never named in a comparison,
//! the shared icon and tooltip distinguishing "off", "one", and the remaining mode. A rename on *either* side still fails it,
//! since renaming the Rust variant is what makes the sheet's literal unproducible.

use std::collections::BTreeSet;

use netodia_engine::player::engine::types::{PlaybackStatus, RepeatMode};
use netodia_testkit::{MIN_SLINT_SOURCES, UI_DIR, stripped_sources};

/// Vacuity floors, one per walk. Each is under what the tree holds today and none is an
/// inventory: what they guard is a walk that stopped matching, which every assertion below
/// would otherwise pass. Deliberately loose, `status` most of all — its comparisons sit in two
/// bindings an ordinary edit could drop either of, so anything but 1 is a floor that fails on one.
// Repeat comparisons live in shared icon/tooltip bindings, rather than each transport button.
const MIN_REPEAT_SITES: usize = 2;
const MIN_STATUS_SITES: usize = 1;

/// The literal `src` compares `.field` against at `at`, where `at` is the offset of the `.`.
///
/// `None` when the comparison is against anything but a string, which is every read that is not
/// one of these contracts. The field name must end at a non-identifier byte, so `.status` does
/// not match a longer name starting with it.
fn compared_literal<'a>(src: &'a str, at: usize, field: &str) -> Option<&'a str> {
    let bytes = src.as_bytes();
    let mut i = at + 1 + field.len();
    if bytes.get(i).is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-') {
        return None;
    }
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if !matches!(src.get(i..i + 2), Some("==" | "!=")) {
        return None;
    }
    i += 2;
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let open = i + 1;
    src[open..].find('"').map(|end| &src[open..open + end])
}

/// Every string literal `src` compares `.field` against, in source order.
fn compared_literals_in<'a>(src: &'a str, field: &str) -> Vec<&'a str> {
    let needle = format!(".{field}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = src[from..].find(&needle).map(|rel| rel + from) {
        from = at + needle.len();
        if let Some(literal) = compared_literal(src, at, field) {
            out.push(literal);
        }
    }
    out
}

/// [`compared_literals_in`] over the whole Slint tree, paired with the file each came from.
fn compared_literals(field: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (path, src) in stripped_sources(UI_DIR, "slint", MIN_SLINT_SOURCES) {
        for literal in compared_literals_in(&src, field) {
            out.push((path.clone(), literal.to_owned()));
        }
    }
    out
}

#[test]
fn every_repeat_mode_the_sheet_branches_on_is_one_the_enum_produces() {
    let produced: BTreeSet<&str> = [RepeatMode::Off, RepeatMode::All, RepeatMode::One]
        .iter()
        .map(RepeatMode::as_str)
        .collect();

    let sites = compared_literals("repeat_mode");
    assert!(
        sites.len() >= MIN_REPEAT_SITES,
        "only {} `repeat_mode` comparisons found under {UI_DIR}; the walk has stopped matching \
         and every assertion standing on it now passes vacuously",
        sites.len()
    );

    for (path, literal) in &sites {
        assert!(
            produced.contains(literal.as_str()),
            "{path} branches on `repeat_mode == \"{literal}\"`, which no `RepeatMode` variant \
             produces (it produces {produced:?}). The branch is dead: the repeat button paints \
             one state for every mode, with nothing failing to say so"
        );
    }
}

#[test]
fn every_playback_status_the_sheet_branches_on_is_one_the_enum_produces() {
    let produced: BTreeSet<&str> =
        [PlaybackStatus::Stopped, PlaybackStatus::Playing, PlaybackStatus::Paused]
            .iter()
            .map(PlaybackStatus::as_str)
            .collect();

    let sites = compared_literals("status");
    assert!(
        sites.len() >= MIN_STATUS_SITES,
        "only {} `status` comparisons found under {UI_DIR}; the walk has stopped matching",
        sites.len()
    );

    for (path, literal) in &sites {
        assert!(
            produced.contains(literal.as_str()),
            "{path} branches on `status == \"{literal}\"`, which no `PlaybackStatus` variant \
             produces (it produces {produced:?})"
        );
    }
}
