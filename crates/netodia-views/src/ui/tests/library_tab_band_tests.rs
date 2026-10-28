//! Source-level pins on `crates/netodia-ui/ui/components/hero/library-tab-band.slint`.
//!
//! The My Library page's band. Six pins are the header-row fixes it ports verbatim from
//! `MosaicTabHero` — each paid for once, invisible in review, and exactly what a copy
//! loses — so they are worded like their twins in `mosaic_tab_hero_tests.rs`. The rest
//! are this band's own: five about the morph, and three about the **seam** with its
//! mount sheet, a band nobody drives passing all eight of the others.
//!
//! Here rather than under a host for the `tab_bar_tests.rs` reason: no Rust module owns
//! the file.

use netodia_testkit::{binding_value as binding, strip_line_comments};

const BAND: &str = include_str!("../../../../netodia-ui/ui/components/hero/library-tab-band.slint");

/// The mount the seam pins are about. Three of the pins below check the host rather than
/// the component, a band nobody feeds passing every check on its own source.
///
/// **Deliberately one sheet.** These pins guard a page whose details morph the band.
const SHEET: &str = include_str!("../../../../netodia-ui/ui/views/my-library-view.slint");

/// The band with its comments dropped, so prose about a fix can neither satisfy a pin nor
/// bound a region early.
fn code() -> String {
    strip_line_comments(BAND)
}

/// The body of the `if root.hero-shown:` branch that paints `needle`, cut at its own
/// closing brace — three branches carry that condition and a `split` alone runs each into
/// the next. Net braces per line, the `"\u{200e}"` escapes balancing out.
fn hero_branch(code: &str, needle: &str) -> String {
    let branch = code
        .split("if root.hero-shown:")
        .skip(1)
        .find(|branch| branch.contains(needle))
        .unwrap_or_default();

    let mut depth = 0usize;
    let mut opened = false;
    let mut body: Vec<&str> = Vec::new();
    for line in branch.lines() {
        body.push(line);
        depth += line.matches('{').count();
        opened |= depth > 0;
        depth = depth.saturating_sub(line.matches('}').count());
        if opened && depth == 0 {
            break;
        }
    }
    body.join("\n")
}

/// A mirrored width only reaches the bar through `changed width`, and `changed` doesn't
/// fire when the first layout settles directly on the final value — which is every window
/// opened at its size. Without the mount timer the seed is never corrected and a roomy
/// window draws icon-only tabs until something resizes it.
///
/// The mirror is the band's; **what it is seeded to** is `TabSearchHeader.row-floor`,
/// pinned once for all three hosts by `tab_search_header_tests`.
#[test]
fn the_page_width_mirror_has_a_mount_seed() {
    assert!(
        BAND.contains("changed width => { self.page-w = self.width; }"),
        "the band must mirror its width imperatively — a live `root.width` read feeding a \
         child's size re-enters layout"
    );

    // Anchored on the *anonymous* `Timer`, since the band has a second one now — the
    // named `collapse :=` that defers the hero teardown.
    let timer = BAND
        .split_once("\n    Timer {")
        .and_then(|(_, rest)| rest.split_once("\n    }"))
        .map_or("", |(body, _)| body);
    assert!(
        timer.contains("root.page-w = root.width"),
        "the band's mount Timer must re-run the `page-w` mirror — `changed` never fires for a \
         window born at its final size"
    );
}

/// Unlike the mosaic band, this bar sits on two surfaces: `Theme.base` idle, the entity's
/// solved blur with a detail open. So each of its four brushes is a *pair*, and dropping
/// either half only shows in one state — a theme token on the hero arm washes out over a
/// cover, a hero tier on the idle arm paints the previous entity's colours onto a flat
/// pane.
///
/// `active-color` is the one this exists for: it drives the selected label, its FILL=1
/// icon and the underline from one input, so an omission is three surfaces at once, and
/// `Theme.accent` carries no contrast floor against a blur where `HeroBackdrop.chrome` is
/// solved to clear one.
///
/// Asserted as "reads *some* `HeroBackdrop` tier" rather than pinning which: which tier is
/// a design call that may move, where reaching for `Theme.*` on the hero side is a bug at
/// any tier.
#[test]
fn every_tab_bar_brush_crosses_from_a_theme_token_to_a_backdrop_tier() {
    let code = code();
    let hero_state = code
        .split_once("hero when root.detail-open: {")
        .and_then(|(_, rest)| rest.split_once("\n            in {"))
        .map_or(String::new(), |(body, _)| body.to_owned());
    assert!(!hero_state.is_empty(), "the band no longer declares its `hero` state");

    for (prop, input, mirror) in [
        ("label-color", "label-brush", "hero-label"),
        ("active-color", "active-brush", "hero-active"),
        ("hover-fill", "hover-brush", "hero-hover"),
        ("divider-color", "divider-brush", "hero-divider"),
    ] {
        let idle = binding(&code, &format!("property <brush> {input}:"));
        assert!(
            !idle.is_empty(),
            "the band no longer declares `{input}` — the bar would fall back to the component's \
             `Theme.*` default on both surfaces"
        );
        assert!(
            idle.contains("Theme."),
            "`{input}` must default to a `Theme.*` token — that default *is* the idle arm, the \
             band being a flat pane there, and a solved hero tier is the previous entity's colour"
        );
        assert!(
            hero_state.contains(&format!("{input}: root.{mirror};")),
            "the `hero` state must cross `{input}` to `{mirror}` — a pair with no hero arm leaves \
             the bar painting theme values over a cover"
        );
        assert!(
            binding(&code, &format!("property <brush> {mirror}:")).contains("HeroBackdrop."),
            "`{mirror}` must take a `HeroBackdrop` tier — a theme value answers a question about \
             the *page* background, and under a hero there isn't one"
        );
        assert!(
            code.contains(&format!("{prop}: root.{input};")),
            "the band's TabBar must pass `{prop}` the `{input}` pair rather than a token directly"
        );
    }
}

/// **The palette crosses on a state transition, and nothing else will do.**
///
/// The hero half of every pair arrives late — `HeroBackdrop` is solved from the artwork
/// decode's measurement, so it lands well into the morph. An animated *binding* restarts
/// on a dirtied dependency rather than a changed value, so a plain `animate` is re-based
/// at that moment and finishes a full `dur-spatial` after the decode, long past the height
/// it was meant to move with. A transition anchors on the **state change**.
///
/// The mirrors are the other half: the entry curve spends most of its travel early, so a
/// decode landing mid-morph would otherwise be adopted in one frame — and easing them is
/// the only thing covering a decode landing *after* the morph, where the transition's own
/// progress is already 1.
///
/// **Split `in` / `out` to take the morph's two curves**, so each arm is pinned against
/// the arm of `animate hero-t` it belongs to. `Theme` cannot carry an easing, so those
/// curves are spelled at both sites and this holds the copies together.
#[test]
fn the_palette_crossing_is_anchored_to_the_morph() {
    let code = code();
    let declarations =
        code.split_once("states [").map_or(String::new(), |(before, _)| before.to_owned());
    assert!(!declarations.is_empty(), "the band no longer declares a `states` block");

    for input in ["label-brush", "active-brush", "hover-brush", "divider-brush"] {
        assert!(
            !declarations.contains(&format!("animate {input}")),
            "`{input}` must cross inside the state's transition, never on an `animate` of its own \
             — a binding animation restarts on every dependency tick, and the hero tier it reads \
             is written from a decode that lands mid-morph"
        );
    }

    // Off the `animate` block, so the transition is checked against the geometry rather
    // than a second copy of the numbers.
    let morph = binding(&code, "easing: root.detail-open");
    let (entry, exit) = morph
        .split_once(':')
        .map_or_else(Default::default, |(a, b)| (a.replace('?', ""), b.to_owned()));
    let entry = entry.trim().to_owned();
    let exit = exit.trim().to_owned();
    assert!(
        entry.starts_with("cubic-bezier(") && exit.starts_with("cubic-bezier("),
        "`hero-t` must ease on a per-direction ternary of two curves — the entry is gentler than \
         the collapse because only the collapse has a tail (the count line arriving over its \
         second half), so one curve reads as a snap going in; got entry {entry:?}, exit {exit:?}"
    );
    assert_ne!(
        entry, exit,
        "the two arms must be different curves — collapsed to one, the entry goes back to \
         spending most of its travel in the first quarter and the fix is silently undone"
    );

    for (direction, curve) in [("in", &entry), ("out", &exit)] {
        let transition = code
            .split_once(&format!("\n            {direction} {{"))
            .and_then(|(_, rest)| rest.split_once("\n            }"))
            .map_or(String::new(), |(body, _)| body.to_owned());
        assert!(
            !transition.is_empty(),
            "the `hero` state must declare an `{direction}` transition — an `in-out` block can \
             only carry one curve, and the morph now has two"
        );
        for input in ["label-brush", "active-brush", "hover-brush", "divider-brush"] {
            assert!(
                transition.contains(input),
                "the `hero` state's `{direction}` transition must animate `{input}` — left out, \
                 that brush steps on the frame the detail opens while its three siblings cross"
            );
        }
        assert!(
            transition.contains("duration: Theme.dur-spatial;"),
            "the `{direction}` crossing must run the morph's own duration — it is the same \
             gesture, and a second number here is one that can drift from `hero-t`, the collapse \
             timer and the body's own `ViewTransition`"
        );
        assert!(
            transition.contains(&format!("easing: {curve};")),
            "the `{direction}` crossing must ease on {curve}, the arm of `animate hero-t` it \
             belongs to — a transition anchored to the morph's start time but running a different \
             shape drifts from the geometry in the middle, which is where the whole crossing is"
        );
    }

    assert!(
        code.contains("animate hero-label, hero-active, hero-hover, hero-divider {"),
        "the four hero mirrors must ease together — unanimated they hand the transition a step, \
         which lands whole once the morph is past its first quarter and entirely once it is over"
    );
}

/// **Nothing may ease *from* a `HeroBackdrop` value the band was not painting**, and on a
/// tabbed page that is a live hazard: a tab leave clears no detail id, so the colour set
/// is held for the pick that morphs that banner back open and routinely describes a hero
/// this band stopped painting several tabs ago.
///
/// Bound to the tiers unconditionally the four mirrors settle on exactly that and the
/// transition crosses out of it — leave a genre for a detail-less tab, open a playlist
/// there, and the bar rushes toward a mirror still easing off the genre's pink. The gate
/// makes the resting value the one the bar is already painting, and stops a write to the
/// global dirtying these at all while the band is flat, Slint subscribing to a dependency
/// only on the arm it evaluates.
///
/// The floor is the same rule one layer down, pinned by `hero_blur_backdrop_tests`; the
/// mount that joins them is asserted here, the shared component defaulting to ungated so
/// a band that stops passing it looks right in both files.
#[test]
fn no_hero_tier_outlives_the_banner_it_was_solved_for() {
    let code = code();

    for (mirror, idle) in [
        ("hero-label", "Theme.text"),
        ("hero-active", "Theme.accent"),
        ("hero-hover", "Theme.surface0"),
        ("hero-divider", "Theme.surface1"),
    ] {
        let value = binding(&code, &format!("property <brush> {mirror}:"));
        // At the head of the value rather than searched: a negated `!root.detail-open ?`
        // is a substring match and would be reported by the arm assert below as an
        // inversion — which it is, spelled the other way round. `trim_start` because
        // `binding` cuts right after the name, and `hero-hover`'s value wraps.
        assert!(
            value.trim_start().starts_with("root.detail-open ?"),
            "`{mirror}` must fall back to its idle half while no detail is open — held across a \
             tab leave, the tier it reads is the last banner's and the transition crosses out of \
             it on the next open"
        );

        // Each arm on its own, not the value as a whole: an *inverted* ternary carries
        // both tokens too, reads correctly at a glance, and paints the theme's grey over
        // every open hero. No arm here holds a `?` or a `:` of its own, so the two splits
        // land exactly on the halves.
        let (open_arm, idle_arm) =
            value.split_once('?').and_then(|(_, rest)| rest.split_once(':')).unwrap_or(("", ""));
        assert!(
            open_arm.contains("HeroBackdrop."),
            "`{mirror}`'s open arm must be the solved tier — the other way round the bar takes \
             `{idle}` for the whole of every detail and the entity's tone for the flat band"
        );
        assert!(
            idle_arm.contains(idle),
            "`{mirror}`'s idle arm must be `{idle}`, the same token its own pair takes on the flat \
             band — a third value here is a colour the bar crosses through on every morph"
        );
    }

    // The backdrop is one mount carrying both stacks, so this is the single binding standing
    // between an artwork-less detail and a backdrop easing out of the previous tab's stops. The
    // wrapper defaults `shown` true, for the two mosaic bands, which never stop painting a hero,
    // so an omitted binding here is silently ungated.
    let mount = code
        .split_once("HeroBackdropStack {")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map_or("", |(body, _)| body);
    assert!(
        mount.contains("shown: root.detail-open;"),
        "the band must gate `HeroBackdropStack`'s `shown` on `detail-open` — the wrapper ANDs the \
         setting in itself, so this term is the band's whole half of the deal and covers both \
         stacks at once. The wrapper owes the other half, pinned by `backdrop_mounts`"
    );
}

/// The band forwards everything the shared header publishes **that this page consumes**,
/// under the names its mount sheet already reads.
///
/// The row itself is pinned by `tab_search_header_tests`; what only this file can check is
/// that the forwards exist, a band that mounts the header and drops them compiling,
/// painting correctly, and silently losing the body fade's arming and every compact
/// tooltip. Two-way aliases, so a write can't orphan one.
///
/// `tab-enter-from` is among them. What stops it composing with the morph into a diagonal
/// is `morphing`, read on the page's own `slide` — so dropping this forward doesn't
/// restore that fix, it strands the five tab bodies on a direction they can't reach.
#[test]
fn the_band_forwards_what_the_shared_row_publishes() {
    for prop in ["tab-enter-from", "tab-anim-armed", "tip-w", "tip-h", "tip-label", "tip-visible"] {
        assert!(
            BAND.contains(&format!("{prop} <=> header.{prop};")),
            "the band must re-publish `{prop}` off the shared header — its sheet reads that name"
        );
    }
    // The two positional anchors can't be plain aliases: they are relative to the
    // header, and the sheet's frame is relative to the band.
    for prop in ["tip-x", "tip-y"] {
        assert!(
            binding(BAND, &format!("out property <length> {prop}:"))
                .contains(&format!("header.{prop}")),
            "the band's `{prop}` must offset the header's own by their `absolute-position` delta"
        );
    }
}

/// **The one that costs most to get wrong.** Slint reports a component root's bound
/// dimension as both `min` and `max`, so an animated `height` here puts the *window's* own
/// minimum height on the morph and dragging the bottom edge inward chases a floor still
/// easing — `tab-bar.slint`'s width bug one axis over, identical in source.
///
/// The split buys that freedom by letting the element be drawn shorter than it asked for,
/// so the clip isn't decoration either: on the shrink leg the hero contents are still full
/// height while the band is already compact.
#[test]
fn the_band_negotiates_its_height() {
    let code = code();

    for constraint in [
        "min-height: root.compact-h;",
        "preferred-height: root.compact-h + (root.hero-h - root.compact-h) * root.hero-t;",
        "max-height: root.hero-h;",
    ] {
        assert!(
            code.contains(constraint),
            "the band's root must spell `{constraint}` — the three-way split is what keeps an \
             animated height off the window's own resize floor"
        );
    }
    assert!(
        !code.contains("\n    height:"),
        "the band's root must not bind `height` — Slint reports a bound root dimension as both \
         `min` and `max`, so the window's minimum height would ease with the morph"
    );
    assert!(
        code.contains("\n    clip: true;"),
        "the band's root must clip — the min/preferred/max split lets it be drawn shorter than it \
         asked for, and the hero contents would paint into the body below"
    );
}

/// `hero-t` is **seeded** by its binding and **owned** by the `changed` handler, and each
/// half fails differently. An animated *binding* restarts whenever a dependency is marked
/// dirty rather than when its value changes, so left bound the morph re-bases every time a
/// detail id moves under it. Dropping the seed for a mount `Timer` is the other failure: a
/// page re-entered with a detail already open grows into the hero on every arrival, the
/// first evaluation no longer landing in `NotAnimating`.
#[test]
fn the_morph_progress_is_seeded_by_its_binding_and_written_by_its_handler() {
    assert!(
        BAND.contains("property <float> hero-t: root.detail-open ? 1.0 : 0.0;"),
        "`hero-t` must keep its binding as the seed — without it the first evaluation animates and \
         a page entered on an open detail grows into the hero every time"
    );

    let handler = BAND
        .split_once("changed detail-open =>")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map_or("", |(body, _)| body);
    assert!(
        handler.contains("self.hero-t ="),
        "`hero-t` must be written from `changed detail-open` — an animated binding restarts on \
         dependency dirtiness, not on a value change"
    );
}

/// **The band publishes whether its height is still travelling, and it has to answer
/// `true` on the frame the morph starts.**
///
/// The page's body is the stretchy sibling under this band, so a morph moves that body's
/// `y` by the whole distance between the two floors — hence `slide: !band.morphing` at the
/// sheet. The tempting spelling is `hero-t > 0`, which compiles, reads plausibly, and is
/// **false** on exactly the frame that matters: a drill-in starts at `hero-t == 0`, so the
/// body takes its own rise and the diagonal is back, reversed into a bounce by the two
/// curves disagreeing about where travel goes.
///
/// Comparing against the target also keeps the answer independent of where
/// `changed detail-open` falls relative to the repeater mounting the body.
#[test]
fn the_band_publishes_whether_its_height_is_still_moving() {
    let code = code();
    let morphing = binding(&code, "out property <bool> morphing");
    assert!(
        !morphing.is_empty(),
        "`LibraryTabBand` must publish `morphing` — the mount sheet has no other way to ask \
         whether the band is already translating the body it is about to mount"
    );
    assert!(
        morphing.contains("hero-t") && morphing.contains("detail-open"),
        "`morphing` must compare `hero-t` against the floor `detail-open` names, so it is true \
         from the frame the morph starts; got {morphing:?}"
    );
    assert!(
        !morphing.contains('>') && !morphing.contains('<'),
        "`morphing` must not be a threshold on `hero-t` — `hero-t > 0` is false on a drill-in's \
         first frame, which is the one frame the body must be told to hold still; got {morphing:?}"
    );
}

/// **The column that mounts `HeroChipStrip` carries no `if`, and every other piece of the
/// hero mounts on `detail-open || hero-t > 0`.** Two failures meet here.
///
/// The morph is *written*, so it genuinely starts at 0, and a branch gated on
/// `detail-open` therefore mounts a frame before one gated on `hero-t > 0`: the title
/// block spends that frame as its row's only child, hard left under the count line, then
/// jumps a tile's width right. `hero-shown` is true at both ends.
///
/// The strip is the part that may not be dropped **at all**: its `changed watched-w`
/// tracker watches a layout property the surviving parent re-dirties on every frame of the
/// morph, so dropping the branch panics whoever writes the condition — an animated
/// predicate, a `changed` handler and a timer's `triggered` all lose the same race. Hence
/// a permanent mount and a brush fade; the mechanism is `.claude/rules/slint-pitfalls.md`'s.
#[test]
fn the_chip_strip_outlives_every_morph_it_is_painted_in() {
    let code = code();

    assert!(
        code.contains("property <bool> hero-shown: root.detail-open || root.hero-t > 0;"),
        "the hero's gate must be the detail *or* a running morph. On `detail-open` alone it leaves \
         on the frame the id clears, while the tile beside it fades for the full duration; on \
         `hero-t > 0` alone it arrives a frame late, laid out where the tile is not yet"
    );

    let lines: Vec<&str> = code.lines().collect();
    let mount = lines.iter().position(|line| line.contains("HeroChipStrip {"));
    assert!(mount.is_some(), "the band no longer mounts `HeroChipStrip`");
    let mount = mount.unwrap_or_default();

    // By brace depth rather than indent — the band has `if`-gated *siblings* at shallower
    // depths and an indent walk reads those as parents. Net per line, so the `"\u{200e}"`
    // escapes balance out.
    let mut ancestors: Vec<&str> = Vec::new();
    for line in &lines[..mount] {
        let opens = line.matches('{').count();
        let closes = line.matches('}').count();
        for _ in closes..opens {
            ancestors.push(line.trim());
        }
        for _ in opens..closes {
            ancestors.pop();
        }
    }
    let enclosing = ancestors.iter().find(|line| line.starts_with("if "));
    assert!(
        enclosing.is_none(),
        "no `if` may enclose `HeroChipStrip` — found `{}`. The strip's `changed watched-w` tracker \
         is queued by every frame of the morph, so whatever drops the branch drops it with a live \
         entry in `run_change_handlers`' taken list and the next evaluation unwraps a dead weak \
         handle",
        enclosing.map_or("", |line| line)
    );
}

/// One hero, one clock — and **nothing that carries content fades on a plain
/// `opacity: root.hero-t`.** `Opacity::need_layer`'s two bails and the line-box crop are
/// `.claude/rules/slint-pitfalls.md`'s; what it costs here is three separate things.
///
/// The **text** half's texture is sized to child geometry, and a `Text`'s geometry is its
/// line box rather than its ink — so an Arabic hero title loses the hamza above its alifs
/// and the dots under its final yas for the whole morph.
///
/// The **artwork tile** looks exempt on ink and isn't exempt on cost: `ArtworkImage`'s
/// root has children so the wrapper layers anyway, and this band clips against an
/// animating height, which puts the layer on `render_layer`'s allocate-a-fresh-texture
/// branch every frame. It takes the component's own `fade` float instead.
///
/// The **back disc** takes `IconButton`'s `fade`. Folding the alpha into its brushes is
/// what's unreachable — `IconButton` owns two `animate`s over them — so the glyph moved
/// out instead, leaving the disc childless. Its bias stays `clamp(root.hero-t * 2.0, …)`,
/// which its scaling size obliges and the third assertion pins.
///
/// So the needle is the bare property rather than one spelling of it: `opacity:
/// root.hero-t` exactly would never catch the disc, whose fade is a bias, not the clock.
#[test]
fn the_hero_fades_on_the_morph_at_both_ends() {
    let code = code();
    let fades = code
        .split("if root.hero-shown:")
        .skip(1)
        .filter(|branch| branch.contains("opacity:"))
        .count();
    assert_eq!(
        fades, 0,
        "no `if root.hero-shown:` half may fade on an `opacity` — over anything with children it \
         costs an offscreen texture for the length of the morph, and on this band a freshly \
         allocated one per frame, the layer being sized against a clip that is itself animating. \
         The artwork tile and the back disc were the last two, and take `ArtworkImage`'s and \
         `IconButton`'s `fade` instead. The needle is the bare property rather than one spelling \
         of it, so the pin is deliberately blunter than `need_layer`: an `opacity` on a lone \
         childless child really would be free, and the answer there is still a brush alpha rather \
         than an exemption carved into this walk"
    );

    // The other half: the tile still *fades*, and still off the morph's only clock. A
    // second animation would need keeping in step, and the reversal would stop being free.
    let tile = hero_branch(&code, "ArtworkImage {");
    assert!(
        tile.contains("fade: root.hero-t;"),
        "the artwork tile must still fade, through `ArtworkImage`'s `fade` and off `hero-t` \
         itself — dropping the binding leaves it snapping in at full opacity while the title \
         beside it eases, which reads as the morph being broken rather than as a missing line"
    );

    // The text half. A layer crops what it composites, so the fade lives in the brushes.
    let title = hero_branch(&code, "+ root.title;");
    assert!(
        !title.is_empty(),
        "the title is no longer painted inside an `if root.hero-shown:` branch — the pins below \
         bound against it"
    );
    // The walk's upper edge, and the only assertion that fails *loudly* when it over-runs:
    // everything below is satisfied by the tail of the file as readily as by the branch,
    // so an unbalanced brace widens the region instead of breaking it.
    assert!(
        !title.contains("HeroChipStrip"),
        "the brace walk ran past the title block and into the column's later children — the pins \
         below stop meaning anything the moment the region is wider than the branch"
    );
    assert!(
        !title.contains("opacity:"),
        "the title block must not fade on `opacity`. The layer that buys is sized to child \
         *geometry*, and a `Text`'s geometry is its line box: every glyph whose ink leaves that \
         box is cropped for the length of the morph and pops back at the end, which on the \
         patched faces is every Arabic mark and nothing in Latin — invisible in review, and the \
         whole of the bug"
    );
    assert_eq!(
        title.matches("color: HeroBackdrop.on-backdrop.transparentize(1.0 - root.hero-t);").count(),
        2,
        "the title *and* the subtitle must carry the fade in their own brush. The subtitle is the \
         block's last child, so it is the edge a layer cut from below — fixing only the title \
         moves the crop rather than removing it"
    );
    // Every tier here carries alpha of its own on the aurora arm, so a fade multiplies into it;
    // `with-alpha` would set it instead and paint the block opaque at rest, dropping the whole
    // mechanism, which is the wash reading through.
    assert!(
        title.contains("icon-color: HeroBackdrop.chrome.transparentize(1.0 - root.hero-t);"),
        "the smart-playlist badge fades with the text beside it — left on a flat brush it is the \
         one piece of the block that arrives before the morph does"
    );

    // The third fade is the back disc's, a *bias* off that same clock rather than the
    // clock. Whitespace-normalized: how the call wraps is formatting, not contract.
    let flat = code.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("fade: clamp(root.hero-t * 2.0, 0.0, 1.0);"),
        "the back disc's alpha must lead the morph rather than track it. It is the one piece of \
         the hero whose size rides `hero-t` too, so a plain `root.hero-t` multiplies into the \
         scale and its presence goes as the square — half size at half alpha is a quarter of a \
         button, which is an entry that animates but reads as one that pops. It rides \
         `IconButton`'s `fade` rather than an `opacity`, for the reason the assertion at the \
         top of this test gives"
    );

    assert!(
        code.contains("HeroChipStrip { fade: root.hero-t; show-rows: root.chips-shown; }"),
        "the chip strip fades through its own `fade` input, on the same clock — it cannot take an \
         `opacity` and it cannot be unmounted, so this is the only way it leaves with the title \
         above it"
    );
    let strip = include_str!("../../../../netodia-ui/ui/components/hero-chip-strip.slint");
    assert!(
        strip.contains("chip-fill: HeroBackdrop.chip-fill-at(root.fade * root.arrive-t);")
            && strip.contains(
                "chip-label-color: HeroBackdrop.chrome-text\
                 .transparentize(1.0 - root.fade * root.arrive-t);"
            ),
        "both of the strip's brushes must carry the fade: the pill through the global's own \
         function so its weight is stated once, the label through `transparentize`, since \
         `with-alpha` *sets* alpha and `chrome-text` carries its own on the aurora arm"
    );
}

/// **The chips fade in on their own clock, being the one hero fact that can arrive after
/// the banner.** A tab re-entered onto an open detail morphs open on artwork and colours
/// held for it while these are still behind the re-fetch, so stepping them in reads as a
/// glitch beside a band that has already settled. The mutation to catch is multiplying
/// only one of the two brushes: the pill fades while its label steps.
#[test]
fn the_chip_row_fades_in_when_it_lands() {
    let strip = include_str!("../../../../netodia-ui/ui/components/hero-chip-strip.slint");
    let flat = strip.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        flat.contains("property <float> arrive-t: root.rows.length > 0 ? 1.0 : 0.0;"),
        "the arrival fade must ride the row model's own emptiness — nothing else knows when \
         the chips have landed, and a host-driven flag would need one per hero"
    );
    assert!(
        flat.contains("animate arrive-t { duration: Theme.dur-med; easing: ease-in-out; }"),
        "`arrive-t` must ease on `dur-med` — the weight the blur crossfade and the palette \
         mirrors either side of it already use, so the band settles as one thing rather than \
         gaining a part that snaps"
    );
    assert_eq!(
        flat.matches("root.fade * root.arrive-t").count(),
        2,
        "both brushes must carry both factors. A float multiplied into the brushes, never an \
         `opacity`: this strip carries `MetaChipStrip`'s change tracker so it cannot be \
         unmounted, and `Opacity::need_layer` bails only at exactly 1.0 — a permanently \
         mounted subtree under one blends an offscreen layer on every frame forever"
    );
}

/// The hero's teardown rides the *end* of the collapse. Every fact the band paints is a
/// ternary over the detail id at the mount sheet, so releasing the cover, the blur pair,
/// the shared tiers and the chip row when that id clears leaves the band spending the
/// whole morph collapsing a fallback glyph over a reset gradient — an exit *from* a
/// placeholder. The `Dialog.closed()` shape, for the same reason it has it.
#[test]
fn the_collapse_defers_the_hero_teardown() {
    let timer = BAND
        .split_once("collapse := Timer {")
        .and_then(|(_, rest)| rest.split_once("\n    }"))
        .map_or("", |(body, _)| body);
    assert!(!timer.is_empty(), "the band no longer declares its `collapse` timer");

    assert!(
        timer.contains("interval: Theme.dur-spatial;"),
        "the collapse timer must run the morph's own duration — a shorter one tears the hero down \
         mid-fade, a longer one holds its buffers past the point anyone can see them"
    );
    assert!(
        timer.contains("root.hero-collapsed();"),
        "the collapse timer is what fires `hero-collapsed` — nothing else knows the morph is done"
    );
    assert!(
        timer.contains("running: false;"),
        "the collapse timer must start idle: a page mounted with no detail never transitioned into \
         one and has nothing to hand back"
    );

    let handler = BAND
        .split_once("changed detail-open =>")
        .and_then(|(_, rest)| rest.split_once("\n    }"))
        .map_or("", |(body, _)| body);
    assert!(
        handler.contains("collapse.running = !self.detail-open;"),
        "the same edge that drives the morph must arm *and* cancel the timer, so a re-drill inside \
         the window can't have the previous hero's teardown land on the new one"
    );

    assert!(
        SHEET.contains("hero-collapsed => { MyLibrary.hero-collapsed(); }"),
        "the sheet must forward `hero-collapsed` — unforwarded, the band collapses correctly and \
         the hero it was painting is never released"
    );
}

/// The back slot's trailing gap is **its own width**, not the row's `spacing`. As layout
/// spacing the row loses a whole `pad-md` on the frame the slot unmounts, and on the way
/// out that frame is the last one — after the height and every brush have settled — so it
/// reads as the tab bar jumping left. Opening, the same step lands on frame one under
/// 400 ms of movement, which is why it only ever showed one way.
#[test]
fn the_back_slot_carries_its_own_gap() {
    let code = code();

    assert!(
        binding(&code, "property <length> back-slot-w:").contains("Theme.pad-md"),
        "the back slot must fold its trailing gap into its own eased width"
    );
    assert!(
        !code.contains("back-slot-gap"),
        "the stepped `back-slot-gap` must be gone — it *is* the jump, and a width that folds the \
         gap in makes the whole inset continuous"
    );
    // *Handed* to the row as `lead-w` as well as drawn into it, or the bar budgets against
    // a width the back button is already occupying and the tabs draw under it.
    assert!(
        code.contains("lead-w: root.back-slot-w;"),
        "the band must hand the slot's width to the header as `lead-w`, or the bar's budget \
         double-counts the space the back button is standing in"
    );
}

/// **The disc inside that slot is a uniform scale of its settled self**, and pinning it at
/// full size is the mistake that looks like a rounding error and isn't: the slot eases to
/// `pill-h + pad-sm + pad-md` while the disc wants `pill-h` of it, so a fixed `diameter`
/// doesn't fit until well into the entry and the slot's `clip` spends until then cutting a
/// straight edge down the middle of a circle. Scaling both dimensions by the slot's own
/// factor holds the disc's share constant for any value of the three tokens.
///
/// Fading it is *not* the same fix: that hides the frames where the geometry is wrong and
/// leaves the disc exceeding its slot for anyone who changes a token.
#[test]
fn the_back_disc_scales_with_the_slot_it_sits_in() {
    let code = code();
    let slot = code
        .split_once("if root.hero-shown: Rectangle {")
        .and_then(|(_, rest)| rest.split_once("clicked =>"))
        .map_or("", |(body, _)| body);
    assert!(!slot.is_empty(), "the band no longer mounts its back slot");

    assert!(
        slot.contains("diameter: root.pill-h * root.hero-t;"),
        "the disc's diameter must scale on the morph — pinned at `pill-h` it is wider than the \
         slot for the first ~60 % of every entry, and the slot clips it into a half circle"
    );
    assert!(
        slot.contains("icon-size: 20px * root.hero-t;"),
        "the glyph must take the same factor as the disc around it — scaling only the disc \
         overruns it with a full-size arrow, which is the same clip one element in"
    );
    assert!(
        slot.contains("clip: true;"),
        "the slot keeps its clip: it is what stops an incompressible disc drawing over the first \
         tab, and the scale above is what stops it ever having to"
    );
}

/// **The count line and the pill row take opposite anchors, and that is the point.** A
/// detail's pills belong on whichever floor is current, so they ride `root.height`. The
/// count sentence belongs where it already sits: anchored to the *compact* floor, its
/// travel an offset off that anchor rather than a ride on the height, which dragged it out
/// of the page and shoved it back in.
///
/// **It leaves down and comes back from the left, and `detail-open` is the only thing in
/// scope that can tell those apart** — `hero-t` knows how far along the morph is, not
/// which way it is going. Down, because the back slot eases open on the same clock and a
/// sideways exit crosses an arriving button through the same pixels.
///
/// Both directions read `hero-t`, so the travel is the morph's own clock with no second
/// animation to keep in step, and the alpha is a bias off it folded into the brush rather
/// than an `opacity` layer standing for the length of every detail.
#[test]
fn the_count_line_drops_out_of_a_fixed_anchor_and_returns_from_the_left() {
    let code = code();
    let count = code
        .split_once("\n    Text {")
        .and_then(|(_, rest)| rest.split_once("\n    }"))
        .map_or(String::new(), |(body, _)| body.to_owned());
    assert!(
        count.contains("root.count-text"),
        "the band's first root-level `Text` must be the count line — the pins below all bound \
         against it"
    );
    assert_eq!(
        code.matches("root.count-text").count(),
        1,
        "the count must be drawn once — a second reader is a second anchor to keep in step"
    );

    assert!(
        count.contains("y: root.compact-h - Theme.pad-lg - root.pill-h + root.count-dy;"),
        "the count must anchor to the *compact* band's floor and take its drop as an offset off \
         it — off `root.height` it rides the morph, which is the vertical travel it exists to \
         not have"
    );
    let drop = binding(&code, "property <length> count-dy:");
    assert_eq!(
        drop.split_whitespace().collect::<Vec<_>>().join(" "),
        "root.detail-open ? root.count-drop * root.count-t : 0px",
        "the drop must be gated on `detail-open` and *linear* in `count-t`: ungated it also fires \
         on the way back, where the line is sliding in from the left, and shaped — it was squared \
         once — it holds the sentence still for the first frames of a band whose pill row is \
         already travelling, which is the one thing on that floor it has to leave in step with"
    );
    assert!(
        count.contains(
            "x: Theme.pad-lg - (root.detail-open ? 0px : root.count-slide * root.hero-t);"
        ),
        "the *return* must keep its leftward slide and the exit must take none — one axis per \
         direction is the whole shape, and a slide left on the way in is what crossed the back \
         button"
    );
    assert!(
        count.contains("color: Theme.text.with-alpha(1.0 - root.count-t);")
            && code.contains("property <float> count-t: clamp(root.hero-t * 2.0, 0.0, 1.0);"),
        "the count must fade by alpha inside its brush, on a bias biased ahead of the morph so \
         the artwork tile crossing behind it is never legible through the sentence — pinned at \
         both ends so the window can't drift alone, the drop reading the same float"
    );
    assert!(
        !count.contains("opacity:"),
        "the count must not use `opacity` — it settles at 0 with a detail open, so the layer would \
         stand for the whole length of every detail rather than for the morph"
    );
    assert!(
        count.contains("width: root.page-w - 2 * Theme.pad-lg - pills.width"),
        "the count's width must be bound and must reserve the pill row — a `Text` in a non-layout \
         parent reports the untruncated string as its preferred width, so elide alone lets a long \
         count paint straight through the pills"
    );

    let pill_row = code
        .split_once("pills := HorizontalLayout {")
        .and_then(|(before, _)| before.rsplit_once("\n    HorizontalLayout {"))
        .map_or(String::new(), |(_, slot)| slot.to_owned());
    assert!(
        !pill_row.is_empty(),
        "the pill slot must stay a root-level `HorizontalLayout` wrapping `pills` — a bare \
         Rectangle reports 0×0 over the `if`-conditional rows the host hands down"
    );
    assert!(
        pill_row.contains("y: root.height - Theme.pad-lg - root.pill-h;"),
        "the pill row must keep riding `root.height` — a detail's actions sit on the hero's floor, \
         which is the anchor the count gave up"
    );
    assert!(
        pill_row.contains("alignment: end;"),
        "the pill row must pack right now that it is `@children` alone — `space-between` over a \
         single child packs it at the *start*"
    );
}

/// The back button sits *in the tab row*, beside labels already painted in hero tiers,
/// rather than floating over a full-bleed hero — so it takes the `MetaChip` pair and not
/// `Theme.floating-chrome-bg`, which answers a question about the theme's own surface
/// ladder rather than about what this glyph contrasts against.
///
/// `hover-bg` is the half that fails silently: `IconButton` defaults it to an opaque
/// `Theme.surface0`, so an omission builds, reads correctly at rest, and paints the
/// theme's grey over the entity's blur the moment the pointer lands.
#[test]
fn the_back_button_takes_every_brush_from_the_backdrop() {
    let button = code()
        .split_once("icon: \"arrow_back\";")
        .and_then(|(_, rest)| rest.split_once("clicked =>"))
        .map_or(String::new(), |(body, _)| body.to_owned());
    assert!(
        !button.is_empty(),
        "the band no longer mounts a back button ahead of its click handler"
    );

    for (prop, tier) in [
        ("idle-bg", "HeroBackdrop.disc-fill"),
        ("hover-bg", "HeroBackdrop.disc-hover"),
        ("idle-fg", "HeroBackdrop.chrome"),
    ] {
        assert!(
            button.contains(&format!("{prop}: {tier};")),
            "the back button must take `{prop}` from `{tier}` — it paints on the solved band, and \
             a theme token has no contrast floor there"
        );
    }
}

/// The idle pane is full-bleed and animating for the whole morph, exactly the shape that
/// must not use `opacity`: a non-unit opacity renders to an offscreen layer, so this one
/// costs a window-width layer every frame of every drill-in. Alpha in the brush is one
/// blend instead.
#[test]
fn the_idle_pane_folds_its_alpha_into_the_brush() {
    let pane = code()
        .split_once("idle-pane := Rectangle {")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map_or(String::new(), |(body, _)| body.to_owned());
    assert!(!pane.is_empty(), "the band no longer declares `idle-pane`");

    assert!(
        pane.contains("background: Theme.base.with-alpha(1.0 - root.hero-t);"),
        "the idle pane must fade by alpha inside its brush"
    );
    assert!(
        !pane.contains("opacity:"),
        "the idle pane must not use `opacity` — a full-bleed element with a non-unit opacity costs \
         an offscreen layer for the length of the morph"
    );
}

/// Data-agnostic, the `MosaicTabHero` contract: `@tr` folds msgids at codegen, so a string
/// spelled here is one the host can no longer vary per tab — and the five tabs differ in
/// every one of them.
#[test]
fn the_band_states_no_string_of_its_own() {
    assert!(
        !code().contains("@tr("),
        "the band must take every literal as a property — the five tabs differ in all of them, and \
         a msgid folded here can't follow the mounted tab"
    );
}

/// The morph runs off a derivation, never a literal: a literal `false` silently retires
/// the whole hero half, and everything else in this file goes on passing because the
/// band's own source is still correct. The derivation names all four ids — naming three
/// leaves one detail opening under an idle band, which reads as that page failing rather
/// than as a missing clause.
#[test]
fn the_morph_is_driven_by_the_sheets_own_derivation() {
    let sheet: String = SHEET.split_whitespace().collect::<Vec<_>>().join(" ");

    assert!(
        sheet.contains("detail-open: root.detail-open;"),
        "my-library-view.slint must hand the band its own derived `detail-open` — a literal is \
         what held the hero half unreachable while the detail views still drew their own header"
    );
    let derived = sheet
        .split_once("property <bool> detail-open:")
        .and_then(|(_, rest)| rest.split_once(';'))
        .map_or("", |(value, _)| value);
    for open in ["album-open", "artist-open", "genre-open", "playlist-open"] {
        assert!(
            derived.contains(open),
            "the sheet's `detail-open` must name `{open}` — a detail left out of it opens under \
             an idle band, with its own body mounted below"
        );
    }
}

/// The hero tile shows a cover only when the open detail actually has one.
///
/// Slint has no empty-`image` literal, so the sheet's `cover` ternary has to bind *some*
/// global's cover on the Genre arm, whose tile is a name-hashed gradient with no image
/// behind it. `ArtworkImage` gates on `cover.width` alone, and more than one detail is
/// routinely open — `seed_detail_from_settings` restores one per view whichever tab boot
/// resumes — so ungated, a genre hero paints whichever sibling happened to be restored.
#[test]
fn the_hero_tile_suppresses_a_cover_the_open_detail_does_not_own() {
    let tile = code()
        .split_once("ArtworkImage {")
        .and_then(|(_, rest)| rest.split_once("tile-size:"))
        .map_or(String::new(), |(body, _)| body.to_owned());
    assert!(
        !tile.is_empty(),
        "the band must mount an `ArtworkImage` ahead of its `tile-size` binding"
    );
    assert!(
        tile.contains("has-cover: root.artwork-path != \"\";"),
        "the band's tile must gate its cover on `artwork-path` — the sheet cannot withhold \
         `cover` on the arm that owns none, so this is where it says so"
    );
}

/// The band draws the back arrow; the page owns what it means. `MyLibrary.back()` routes
/// to the mounted tab's own `close-detail`, so every teardown that button already triggers
/// stays where it is. Unhandled, the arrow is drawn, hovers, and does nothing.
#[test]
fn the_back_arrow_routes_to_the_pages_own_close() {
    assert!(
        BAND.contains("clicked => { root.back-clicked(); }"),
        "the band's back button must emit `back-clicked` — it states no route of its own"
    );
    let sheet: String = SHEET.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        sheet.contains("back-clicked => { MyLibrary.back(); }"),
        "my-library-view.slint must route `back-clicked` to `MyLibrary.back()`, which is what \
         reaches the mounted tab's `close-detail` and the teardown behind it"
    );
}

/// Every hero fact the band declares is one the sheet feeds. The band is data-agnostic —
/// four detail globals with nothing between them — so each fact arrives as an `in
/// property`, and one added here and not bound there sits at its default: an artwork tile
/// that never fills, a badge that never shows. Nothing fails.
#[test]
fn every_hero_fact_the_band_declares_is_fed_by_the_sheet() {
    const FACTS: [&str; 13] = [
        "title",
        "subtitle",
        "title-badge",
        "artwork-path",
        "cover",
        "circular-artwork",
        "fallback-icon",
        "tile-bg",
        "tile-icon-color",
        "blur-a",
        "blur-b",
        "use-a",
        "has-blur",
    ];

    let sheet: String = SHEET.split_whitespace().collect::<Vec<_>>().join(" ");
    let mount = sheet
        .split_once("band := LibraryTabBand {")
        .and_then(|(_, rest)| rest.split_once("MyLibraryTabPills"))
        .map_or("", |(body, _)| body);
    assert!(
        !mount.is_empty(),
        "my-library-view.slint no longer mounts the band ahead of its pill row"
    );

    for fact in FACTS {
        assert!(
            BAND.contains(&format!("{fact}:")) || BAND.contains(&format!("{fact};")),
            "library-tab-band.slint no longer declares `{fact}`; drop it from this list too"
        );
        assert!(
            mount.contains(&format!("{fact}:")),
            "my-library-view.slint must bind the band's `{fact}` — left unbound it sits at the \
             component's default, which is wrong for every detail and fails nothing"
        );
    }
}
