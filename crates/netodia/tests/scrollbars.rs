//! Source pin for the scrollbar convention: nothing in the Slint tree paints
//! Slint's stock scrollbar.
//!
//! Also for what a scroller does with a *drag*, which lands here rather than beside itself
//! because it is answered on the same two elements by the same block walk.
//!
//! `CLAUDE.md` states the rule — both policies `always-off` on every scroller, with a
//! sibling `OverlayScrollbar` pinned by absolute coordinates — because std-widgets' bar
//! paints inside padded containers and can't be reskinned. A drifted scroller reads as a
//! different app the moment its content overflows, and looks fine in the file it lives in.
//!
//! Two checks, because the two failures are different shapes. A wrong *value* is the one
//! you can grep for. An *omitted* policy is what only the per-block walk sees: a flat
//! search would let a scroller borrow the opt-out of a scroller nested inside it, which
//! is exactly the arrangement both composite views have.

use netodia_testkit::{
    MIN_SLINT_SOURCES, UI_DIR, blocks_named, normalize_ws as normalized, stripped_sources,
};

/// The elements both block walks recognise. `Flickable` is deliberately absent, and
/// the reason differs per walk: it owns no scrollbar pair to turn off, and it spells
/// drag-panning `interactive` rather than `mouse-drag-pan-enabled`. The tree declares
/// none — held by [`the_tree_declares_no_bare_flickable`], since one that appears is
/// outside every walk in this file.
const SCROLLERS: [&str; 2] = ["ScrollView", "ListView"];
const AXES: [&str; 2] = ["horizontal", "vertical"];
const OPT_OUT: &str = "always-off";

/// Everything under this directory mounts inside the dialog card, so its scrollbars sit
/// on `surface0` rather than on the page. `multiline-input.slint` is named beside it
/// because it is only ever mounted there, despite living a directory up.
const DIALOG_DIR: &str = "components/dialog/";
const DIALOG_STRAYS: [&str; 1] = ["multiline-input.slint"];
const CARD_TRACK: &str = "track-color: Theme.scrollbar-track-on-card;";

/// The two components that own a `TrackList`'s bar — the plain page and the
/// nested-under-another-scroller case. They are the only files allowed to bind a list's
/// vertical metrics onto an `OverlayScrollbar`.
const SCROLLBAR_COMPONENTS: [&str; 2] =
    ["components/track-list-scrollbars.slint", "components/composite-scrollbars.slint"];

/// The band a `TrackList`'s vertical bar has to clear and the height of what's left —
/// the two metrics **only** a `TrackList` publishes, only it having a column header
/// above its scrolling region.
///
/// Deliberately not `v-viewport-height` / `v-visible-height`, which every entity card
/// grid publishes too: those mount a *single* bar over a body with no header and no
/// horizontal axis. Either name inside an `OverlayScrollbar` block is a hand-rolled
/// pair; both appear in the converted hosts as hand-overs *outside* one, which is what
/// the block walk separates.
const TRACK_LIST_V_METRICS: [&str; 2] = ["body-y", "body-height"];

/// The whole tree, comment-stripped, paired with the file it came from.
fn sources() -> Vec<(String, String)> {
    stripped_sources(UI_DIR, "slint", MIN_SLINT_SOURCES)
}

/// Whether `src` mounts `component` at all.
fn mounts(src: &str, component: &str) -> bool {
    !blocks_named(src, component).is_empty()
}

/// Every scroller declared in `src`, as `(element, body)` — [`blocks_named`] over [`SCROLLERS`].
fn scroller_blocks(src: &str) -> Vec<(&'static str, &str)> {
    SCROLLERS
        .iter()
        .flat_map(|name| blocks_named(src, name).into_iter().map(move |body| (*name, body)))
        .collect()
}

/// The values `body` binds `key` to at its **own** nesting depth. Depth is the whole
/// point: the composite views nest a `TrackList`'s scrollers inside their own, so a
/// scroller that binds nothing would otherwise pass on its child's answer.
fn own_property<'a>(body: &'a str, key: &str) -> Vec<&'a str> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => depth = depth.saturating_sub(1),
            _ if depth == 0 && !in_string => {
                if bytes[i..].starts_with(key.as_bytes())
                    && let Some(end) = body[i..].find(';').map(|rel| rel + i)
                {
                    out.push(body[i + key.len()..end].trim());
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// The `<axis>-scrollbar-policy` values `body` sets at its own depth.
fn own_policies(body: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    for axis in AXES {
        let key = format!("{axis}-scrollbar-policy:");
        out.extend(own_property(body, &key).into_iter().map(|value| (axis, value)));
    }
    out
}

/// The check that catches an omission, which is the shape most drift takes.
#[test]
fn every_scroller_opts_out_of_the_stock_scrollbar() {
    let sources = sources();
    let mut blocks = 0usize;
    let mut offenders = Vec::new();

    for (path, src) in &sources {
        for (element, body) in scroller_blocks(src) {
            blocks += 1;
            let set = own_policies(body);
            for axis in AXES {
                if !set.iter().any(|(a, v)| *a == axis && *v == OPT_OUT) {
                    offenders.push(format!(
                        "{path}: {element} does not set {axis}-scrollbar-policy: {OPT_OUT}"
                    ));
                }
            }
        }
    }

    assert!(blocks >= 20, "only {blocks} scrollers found — the walk is broken");
    assert!(
        offenders.is_empty(),
        "every ScrollView/ListView must turn both stock scrollbars off and mount an \
         OverlayScrollbar instead (CLAUDE.md, Slint Conventions):\n{}",
        offenders.join("\n")
    );
}

/// Scrolling must work on the first touch and must leave mouse drags to row reordering.
/// The shared widgets own that switch, so no host can silently inherit a style's mouse default.
#[test]
fn every_scroller_uses_the_touch_aware_widgets() {
    let sources = sources();
    let mut blocks = 0usize;
    let mut offenders = Vec::new();

    for (path, src) in &sources {
        for (element, body) in scroller_blocks(src) {
            blocks += 1;
            let imports_shared = src.lines().any(|line| {
                line.starts_with("import {")
                    && line.contains(element)
                    && line.contains("touch-scroll-view.slint")
            });
            if !imports_shared || !own_property(body, "mouse-drag-pan-enabled:").is_empty() {
                offenders.push(format!("{path}: {element} bypasses the shared touch policy"));
            }
        }
    }

    assert!(blocks >= 20, "only {blocks} scrollers found — the walk is broken");
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
}

/// Both block walks key on [`SCROLLERS`], so what makes them complete is that nothing
/// declares a bare `Flickable` — which spells drag-panning `interactive`, carries the `true`
/// default the opt-outs exist to pin against, and is invisible to every needle here.
#[test]
fn the_tree_declares_no_bare_flickable() {
    let offenders: Vec<_> = sources()
        .iter()
        .filter(|(_, src)| mounts(src, "Flickable"))
        .map(|(path, _)| format!("{path}: declares a bare Flickable"))
        .collect();

    assert!(
        offenders.is_empty(),
        "a bare `Flickable` sits outside every walk in this file and opts into drag-to-pan \
         by writing nothing at all — reach for `ScrollView` or `ListView`:\n{}",
        offenders.join("\n")
    );
}

/// A bar on a dialog card takes a track colour of its own. The default is `surface0` at
/// half alpha, which over a `surface0` card composites to exactly `surface0` — zero
/// contrast, so the thumb floats with nothing saying how far the list runs. It looks
/// like a missing feature rather than a wrong colour, and a new dialog scrollbar
/// inherits it by writing nothing at all.
///
/// Most dialog scrollers reach it through `DialogScrollPane`, which states it once for every
/// host it serves; what this still covers is that pane plus the bars whose scroller is shaped
/// differently enough to hand-roll one. So the floor is well under the count — a host moving
/// onto the pane *lowers* it, which is the direction that means the colour is stated in fewer
/// places rather than in none.
#[test]
fn every_dialog_scrollbar_takes_the_track_that_reads_on_a_card() {
    let sources = sources();
    let mut bars = 0usize;
    let mut offenders = Vec::new();

    for (path, src) in &sources {
        let in_dialog =
            path.contains(DIALOG_DIR) || DIALOG_STRAYS.iter().any(|stray| path.ends_with(stray));
        if !in_dialog {
            continue;
        }
        for body in blocks_named(src, "OverlayScrollbar") {
            bars += 1;
            if !normalized(body).contains(CARD_TRACK) {
                offenders.push(format!("{path}: OverlayScrollbar does not set {CARD_TRACK}"));
            }
        }
    }

    assert!(bars >= 3, "only {bars} dialog scrollbars found — the walk is broken");
    assert!(
        offenders.is_empty(),
        "an OverlayScrollbar on a dialog card composites the default track into the \
         card exactly, leaving only the thumb visible:\n{}",
        offenders.join("\n")
    );
}

/// The check that catches a wrong value, wherever it is spelled — including on an
/// element this walk doesn't know about yet.
#[test]
fn no_scrollbar_policy_asks_for_the_stock_bar() {
    let sources = sources();
    let mut policies = 0usize;
    let mut offenders = Vec::new();

    for (path, src) in &sources {
        let mut from = 0;
        while let Some(at) = src[from..].find("scrollbar-policy:").map(|rel| rel + from) {
            let value_at = at + "scrollbar-policy:".len();
            from = value_at;
            let Some(end) = src[value_at..].find(';').map(|rel| rel + value_at) else {
                continue;
            };
            policies += 1;
            let value = src[value_at..end].trim();
            if value != OPT_OUT {
                offenders.push(format!("{path}: scrollbar-policy: {value}"));
            }
        }
    }

    assert!(policies >= 40, "only {policies} policies found — the walk is broken");
    assert!(
        offenders.is_empty(),
        "the only scrollbar in the tree is OverlayScrollbar, so every policy reads \
         `{OPT_OUT}` (CLAUDE.md, Slint Conventions):\n{}",
        offenders.join("\n")
    );
}

/// **A `TrackList`'s bar comes from a component; nobody hand-rolls it again.**
///
/// Six pages had — the three Songs tabs and the Album / Genre / Playlist detail bodies —
/// and they had already drifted into two spellings of the vertical anchor:
/// `tl.absolute-position.y - root.absolute-position.y + tl.body-y` in the details, a bare
/// `tl.y + tl.body-y` in the tabs. Both are right at their own nesting depth, and the
/// short one stops being right the moment a wrapper appears above it — silently, since
/// nothing about it reads wrong.
///
/// **Anchored on the header-band metrics, inside a bar's own block.** The six hosts still
/// spell `body-y:` and `body-height:` — as property hand-overs to `TrackListScrollbars`,
/// *outside* any `OverlayScrollbar { … }` — so the block walk is what tells a hand-over
/// from a mount. Every entity card grid stays out for the same reason and is worth naming,
/// since widening the needle would sweep them in: each mounts a lone vertical bar over a
/// body with no header.
#[test]
fn no_page_hand_rolls_a_track_lists_scrollbars() {
    let sources = sources();
    let mut bars = 0usize;
    let mut offenders = Vec::new();

    for (path, src) in &sources {
        if SCROLLBAR_COMPONENTS.iter().any(|owner| path.ends_with(owner)) {
            continue;
        }
        for body in blocks_named(src, "OverlayScrollbar") {
            bars += 1;
            if let Some(metric) = TRACK_LIST_V_METRICS.iter().find(|m| body.contains(**m)) {
                offenders.push(format!("{path}: OverlayScrollbar binds {metric}"));
            }
        }
    }

    assert!(bars >= 10, "only {bars} non-component scrollbars found — the walk is broken");
    assert!(
        offenders.is_empty(),
        "a plain `TrackList` page mounts `components/track-list-scrollbars.slint`'s \
         `TrackListScrollbars` as the last child of its root at `x: 0; y: 0; width: 100%; \
         height: 100%` — never a hand-rolled bar, which drifts on the anchor. A list nested \
         *under* another scroller wants `CompositeScrollbars` instead:\n{}",
        offenders.join("\n")
    );
}
