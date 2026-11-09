//! harness-native's look: the small moving parts of the sidebar on den's spring system, and the per-kind icons and tones.
//!
//! Every value (colours, sizes, spring tunes) is in the scene (scene/00-tokens.flecs, scene/10-harness.flecs); this file
//! reads prefabs by name and keeps only runtime state: a breath (a chat status, a running check's mark), a part shown
//! while its row is hovered (or only while it is not), a disclosure chevron's turn.
use harness_native_api::spring::{Spring, Tune};
use harness_native_api::{APP, Fold, HoverAmt, moving, settle};
use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{Padding, Size};
use harness_native_ui_input_api::Pressed;
use harness_native_ui_look_api::{Fill, HoverLook, Inks, PressGive, Rgba};
use harness_native_ui_motion_api::{Motion, Pace, Pose};
use harness_native_ui_text_api::{Icon, Mark};
use harness_native_ui_timeline_api::GlyphsArrive;
use harness_native_ui_view as view;

/// A breath, what is at work (a chat's thinking icon, a running check's mark; a working row's title has a Sheen instead): a spring chasing 0 and 1 in turn (`up`
/// its current target), the node's opacity going between full and `low` with it. Each user says how far it dims.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Breath {
    pub spring: Spring,
    pub up: bool,
    pub low: f32,
}

/// What waits a moment before it shows (den's PendingLook): unseen for `delay` seconds after it first shows, then faded in
/// on the spring of the same prefab's `Motion` (a cell's Loading and Skeleton, scene/widgets/genui.flecs hn.GenPending),
/// so a wait that ends at once never flickers.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ShowAfter {
    pub delay: f32,
}

/// A band sweeping along a span (den ui/progress.rs's shimmer): across in `period` seconds, eased so it lingers at each end,
/// `share` of the span long, its alpha falling to `low` of full as an end of the span cuts it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct Sweep {
    pub period: f32,
    pub share: f32,
    pub low: f32,
}

/// A part shown only while row `row` is hovered (its opacity the row's hover spring), or with `away` only while it is not
/// (what a hover-only part takes the place of), or always when `keep`. Row 0: the part's nearest ancestor with a row's
/// hover look or a [`HoverGroup`] (what a scene template writes: it cannot name its instance's id).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct HoverShow {
    pub row: u64,
    pub keep: bool,
    pub away: bool,
}

/// What a [`HoverShow`] part of row 0 counts as its row besides a node with a hover look: a group with no wash of its own
/// (a pasted picture, whose remove button shows only while the pointer is on it; a sent message's row, its steps button).
/// Its host gives it a `HoverAmt`, which the Hover system steps.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct HoverGroup;

/// A disclosure chevron: whether what it discloses is open. Its turn (0 closed, 1 open: a quarter turn) is the `Fold`
/// spring on the same entity.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Disclose {
    pub open: bool,
}

/// Dragging the sidebar's edge (src/resize.rs, src/app/side_drag.rs), all points: its width's range, the extra travel
/// past half of `min` that closes or reopens it (`latch`), the most an over-drag past `max` stretches and how soon
/// (`give`, `soft`), how far back from `max` before touching it pulses again (`rearm`), how near the edge a press grabs
/// it (`grab`; folded, a press within `reach` of the window's left edge, clear of AppKit's own resize band), and the
/// least the main card keeps beside it (`keep`, which lowers `max` in a narrow window).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct SideDrag {
    pub min: f32,
    pub max: f32,
    pub latch: f32,
    pub give: f32,
    pub soft: f32,
    pub rearm: f32,
    pub grab: f32,
    pub reach: f32,
    pub keep: f32,
}

/// Dragging the browser pane's left edge (src/app/web_drag.rs), all points: its width's range, how near the edge a press
/// grabs it (`grab`), and the least the main card keeps beside it (`keep`, which lowers `max` in a narrow window).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct PaneDrag {
    pub min: f32,
    pub max: f32,
    pub grab: f32,
    pub keep: f32,
}

/// How the chat's effort moves under a sideways swipe (`hn.Theme.chat_pick`, src/app/chat/pick.rs): the fingers' travel
/// a stop takes (`step`, points), how long after the last step the effort is sent (`commit`, seconds), and how long the
/// control keeps saying it in words once it stops moving (`word`, seconds).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct PickFeel {
    pub step: f32,
    pub commit: f32,
    pub word: f32,
}

/// The chat's `@` mention preview (`hn.Theme.chat_mention`, src/app/chat/mention.rs): the `cols` characters a file's line
/// is read to, the height of a line of code (`line`) and of a folder's entry (`entry`), and the `image_w` by `image_h`
/// box (points) under the path: a picture is drawn whole in it, and as many lines or entries are read as fit it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct MentionLook {
    pub cols: u32,
    pub line: f32,
    pub entry: f32,
    pub image_w: f32,
    pub image_h: f32,
}

/// How a sidebar row's content comes in as the row folds in (and goes as it folds away), on the row's one fold spring
/// (src/app/fold.rs): its words show only once the row has `lead` of its height (0 to 1), easing in over the rest, and
/// settle down `rise` points into place as they do.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct FoldIn {
    pub lead: f32,
    pub rise: f32,
}

/// How held a [`PressGive`] control is (0 to 1, on its spring).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct PressAmt {
    pub spring: Spring,
}

pub fn register(world: &World) {
    world.component::<SideDrag>().writable();
    world.component::<PaneDrag>().writable();
    world.component::<PickFeel>().writable();
    world.component::<MentionLook>().writable();
    world.component::<FoldIn>().writable();
    world.component::<PressAmt>();
    // The row templates (scene/widgets/rows.flecs) set these.
    world.component::<Breath>().writable();
    world.component::<HoverShow>().writable();
    world.component::<HoverGroup>();
    world.component::<Disclose>().writable();
    world.component::<ShowAfter>().writable();
    world.component::<Sweep>().writable();
}

/// The theme's `role` (`hn.Theme.<role>`, scene/10-harness.flecs). The scene is compiled in, so a role it lacks is a bug.
pub fn role<'a>(w: WorldRef<'a>, role: &str) -> EntityView<'a> {
    view::prefab(w, &format!("hn.Theme.{role}"))
        .unwrap_or_else(|| panic!("the scene's hn.Theme has no {role}"))
}

/// A role's fill (`card`, `ink_muted`, `kind_claude`): the theme's colour roles are scene/00-tokens.flecs's `tone_<role>`.
pub fn tone(w: WorldRef<'_>, name: &str) -> Fill {
    role(w, name).try_cloned::<&Fill>().unwrap_or_else(|| panic!("hn.Theme.{name} has no Fill"))
}

/// A role's spring (`breath`, `pill`).
pub fn tune(w: WorldRef<'_>, name: &str) -> Tune {
    let m = role(w, name)
        .try_cloned::<&Motion>()
        .unwrap_or_else(|| panic!("hn.Theme.{name} has no Motion"));
    Tune { omega: m.omega, zeta: m.zeta }
}

/// A role's fixed width (`indent`).
pub fn width(w: WorldRef<'_>, name: &str) -> f32 {
    role(w, name)
        .try_cloned::<&Size>()
        .unwrap_or_else(|| panic!("hn.Theme.{name} has no Size"))
        .w
        .value
}

/// A role's glyph arrival (`chat_stream`: how words streaming in fade in).
pub fn arrive(w: WorldRef<'_>, name: &str) -> GlyphsArrive {
    role(w, name)
        .try_cloned::<&GlyphsArrive>()
        .unwrap_or_else(|| panic!("hn.Theme.{name} has no GlyphsArrive"))
}

/// A role's pace (`chat_stream`: how fast words that came in a burst show).
pub fn pace(w: WorldRef<'_>, name: &str) -> Pace {
    role(w, name).try_cloned::<&Pace>().unwrap_or_else(|| panic!("hn.Theme.{name} has no Pace"))
}

/// The sidebar drag's numbers (`hn.Theme.side_drag`).
pub fn side_drag(w: WorldRef<'_>) -> SideDrag {
    role(w, "side_drag").try_cloned::<&SideDrag>().unwrap_or_else(|| panic!("hn.Theme.side_drag has no SideDrag"))
}

/// The browser pane drag's numbers (`hn.Theme.web_drag`).
pub fn web_drag(w: WorldRef<'_>) -> PaneDrag {
    role(w, "web_drag").try_cloned::<&PaneDrag>().unwrap_or_else(|| panic!("hn.Theme.web_drag has no PaneDrag"))
}

/// The chat effort's swipe and commit timings (`hn.Theme.chat_pick`).
pub fn pick_feel(w: WorldRef<'_>) -> PickFeel {
    role(w, "chat_pick").try_cloned::<&PickFeel>().unwrap_or_else(|| panic!("hn.Theme.chat_pick has no PickFeel"))
}

/// The `@` mention preview's look (`hn.Theme.chat_mention`).
pub fn mention_look(w: WorldRef<'_>) -> MentionLook {
    role(w, "chat_mention").try_cloned::<&MentionLook>().unwrap_or_else(|| panic!("hn.Theme.chat_mention has no MentionLook"))
}

/// How a sidebar row's content folds in and away (`hn.Theme.tree_fold`).
pub fn fold_in(w: WorldRef<'_>) -> FoldIn {
    role(w, "tree_fold").try_cloned::<&FoldIn>().unwrap_or_else(|| panic!("hn.Theme.tree_fold has no FoldIn"))
}

/// A role's padding (`badge_on`).
pub fn padding(w: WorldRef<'_>, name: &str) -> Padding {
    role(w, name)
        .try_cloned::<&Padding>()
        .unwrap_or_else(|| panic!("hn.Theme.{name} has no Padding"))
}

fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    Rgba { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: a.a + (b.a - a.a) * t }
}

/// How near its end a breath turns back (of its 0..1 travel).
const BREATH_TURN: f32 = 0.04;
/// The springs: breaths, chevrons' turns, icon buttons' inks and presses.
pub fn install(w: WorldRef<'_>) {
    // A control with Inks follows its hover spring (the Hover system steps it); one with a PressGive, its press spring.
    w.observer_named::<&Inks>("InksHover").on_set().each_entity(|e, _| {
        if !e.has(HoverAmt::id()) {
            e.set(HoverAmt::default());
        }
    });
    // A node with a HoverLook (a menu's item, a row's wash) fades it in and out on its hover spring.
    w.observer_named::<&HoverLook>("HoverLookAmt").on_set().each_entity(|e, _| {
        if !e.has(HoverAmt::id()) {
            e.set(HoverAmt::default());
        }
    });
    w.observer_named::<&PressGive>("PressGiveAmt").on_set().each_entity(|e, _| {
        if !e.has(PressAmt::id()) {
            e.set(PressAmt::default());
        }
    });
    // Its glyph's ink: `rest`, eased to `hover` as the pointer comes on (written only on change).
    w.system_named::<(&Inks, &HoverAmt)>("InkHover").each_iter(|it, row, (inks, h)| {
        let k = h.spring.x.clamp(0.0, 1.0);
        let f = Fill { dark: mix(inks.rest.dark, inks.hover.dark, k), light: mix(inks.rest.light, inks.hover.light, k) };
        it.entity(row).each_child(|c| {
            if (c.has(Icon::id()) || c.has(Mark::id())) && c.try_cloned::<&Fill>() != Some(f) {
                c.set(f);
            }
        });
    });
    // Held, it scales and dims toward its PressGive, and springs back when let go (its Pose; written only on change).
    w.system_named::<(&PressGive, &mut PressAmt)>("PressGive").each_iter(|it, row, (g, p)| {
        let (e, w) = (it.entity(row), it.world());
        let held = w.lookup(APP).target(Pressed::id(), 0).is_some_and(|t| t.id() == e.id());
        let tune = Tune { omega: g.omega.max(1.0), zeta: g.zeta.max(0.05) };
        if settle(&mut p.spring, f32::from(u8::from(held)), tune, it.delta_time()) {
            moving(&w);
        }
        let x = p.spring.x;
        let pose = Pose { shrink: (1.0 - g.scale) * x, fade: (1.0 - g.dim) * x, ..e.try_cloned::<&Pose>().unwrap_or_default() };
        if e.try_cloned::<&Pose>() != Some(pose) {
            e.set(pose);
        }
    });
    let breath = tune(w, "breath");
    w.system_named::<&mut Breath>("Breathe").each_iter(move |it, _row, b| {
        let target = f32::from(u8::from(b.up));
        b.spring.step(target, breath, it.delta_time());
        // Turn back a little short of the end: a breath never rests.
        if (b.spring.x - target).abs() < BREATH_TURN {
            b.up = !b.up;
        }
    });
    // A chevron a template made starts turned the way it is (its Fold), so it does not swing in on its first frame.
    w.observer_named::<&Disclose>("DiscloseFold").on_set().each_entity(|e, d| {
        if !e.has(Fold::id()) {
            e.set(Fold(Spring::at(f32::from(u8::from(d.open)))));
        }
    });
    let turn = tune(w, "turn");
    w.system_named::<(&Disclose, &mut Fold)>("Turn").each_iter(move |it, _row, (d, f)| {
        let w = it.world();
        let to = f32::from(u8::from(d.open));
        if harness_native_api::reduced(it.world()) {
            f.0 = Spring::at(to);
        } else if settle(&mut f.0, to, turn, it.delta_time()) {
            moving(&w);
        }
    });
}

/// How much of a node's own opacity its look leaves: a breath's dimming, a hover-only (or hover-away) part's fade.
pub fn alpha(w: WorldRef<'_>, e: EntityView<'_>) -> f32 {
    let mut a = 1.0;
    if let Some(b) = e.try_cloned::<&Breath>() {
        let lo = b.low.clamp(0.0, 1.0);
        a *= lo + (1.0 - lo) * b.spring.x.clamp(0.0, 1.0);
    }
    if let Some(h) = e.try_cloned::<&HoverShow>().filter(|h| !h.keep) {
        let amt = if h.row == 0 {
            std::iter::successors(e.parent(), |n| n.parent())
                .take(4)
                .find(|n| n.has(HoverLook::id()) || n.has(HoverGroup::id()))
                .and_then(|n| n.try_cloned::<&HoverAmt>())
        } else {
            w.entity_from_id(h.row).try_cloned::<&HoverAmt>()
        };
        let hovered = amt.map_or(0.0, |h| h.spring.x.clamp(0.0, 1.0));
        a *= if h.away { 1.0 - hovered } else { hovered };
    }
    a
}

/// A disclosure chevron's turn in radians (a quarter turn open), when `e` is one.
pub fn turn(e: EntityView<'_>) -> Option<f32> {
    e.has(Disclose::id())
        .then(|| e.try_cloned::<&Fold>().map_or(0.0, |f| f.0.x) * std::f32::consts::FRAC_PI_2)
}

/// Mark `part` shown only while `row` is hovered, unless `keep`.
pub fn hover_show(part: EntityView<'_>, row: u64, keep: bool) {
    let want = HoverShow { row, keep, away: false };
    if part.try_cloned::<&HoverShow>().is_none_or(|h| h.row != row || h.keep != keep || h.away) {
        part.set(want);
    }
}

/// Set disclosure chevron `part` open or closed (its spring turns it).
pub fn disclose(part: EntityView<'_>, open: bool) {
    if part.try_cloned::<&Disclose>().is_none_or(|d| d.open != open) {
        if !part.has(Fold::id()) {
            part.set(Fold(Spring::at(f32::from(u8::from(open)))));
        }
        part.set(Disclose { open });
    }
}

/// Whether anything breathes (the window then keeps drawing, at a calm rate). A breath of `low` 1 dims nothing (a
/// template's breath at rest: a re-run cannot remove it), so it does not count.
pub fn breathing(w: WorldRef<'_>) -> bool {
    let mut any = false;
    w.new_query::<&Breath>().each_entity(|_, b| any |= b.low < 1.0);
    any
}

/// A kind's icon: an SF Symbols–like codicon of the bundled Symbols Nerd Font.
pub fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "claude" => "\u{EA67}",  // cod-person: an agent, one weight (no "AI" sparkle)
        "codex" => "\u{EC81}",   // cod-openai
        "cursor" => "\u{F01BF}", // md-cursor_default_outline
        "shell" => "\u{EA85}",   // cod-terminal
        "stage" => "\u{EC07}",   // cod-circle_small
        _ => "\u{EB8A}",         // cod-circle_small_filled
    }
}

/// Show kind `kind`'s mark on `part` (a row's glyph, a 16 point box): what the kind's prefab `hn.Kind.<kind>` holds, a
/// vector `Mark` (ui.Icon: Claude, Codex, Cursor) or a Nerd Font `Icon` (ui.Symbol: a shell), in the prefab's Fill. A
/// kind with no prefab (a pipeline stage, a process) gets [`kind_icon`] in the shell's tone. Written only on change.
pub fn kind_mark(w: WorldRef<'_>, part: EntityView<'_>, kind: &str) {
    let pre = view::prefab(w, &format!("hn.Kind.{kind}"));
    let fill = pre.and_then(|p| p.try_cloned::<&Fill>()).unwrap_or_else(|| tone(w, "kind_shell"));
    match pre.and_then(|p| p.try_cloned::<&Mark>()) {
        Some(m) => show_mark(part, m),
        None => {
            let icon =
                pre.and_then(|p| p.try_cloned::<&Icon>()).filter(|i| !i.glyph.as_str().is_empty());
            show_glyph(part, icon.as_ref().map_or(kind_icon(kind), |i| i.glyph.as_str()));
        }
    }
    if part.try_cloned::<&Fill>() != Some(fill) {
        part.set(fill);
    }
}

/// `part` drawn as vector mark `m` (its `Icon`, if any, no longer drawn).
fn show_mark(part: EntityView<'_>, m: Mark) {
    if part.try_cloned::<&Mark>().as_ref() != Some(&m) {
        part.set(m);
    }
    if part.has(Icon::id()) {
        part.remove(Icon::id());
    }
}

/// `part` drawn as `glyph` set in type (a Nerd Font icon, or a character), its `Mark`, if any, gone.
pub fn show_glyph(part: EntityView<'_>, glyph: &str) {
    if part.has(Mark::id()) {
        part.remove(Mark::id());
    }
    if part.try_cloned::<&Icon>().is_none_or(|i| i.glyph.as_str() != glyph) {
        part.set(Icon { glyph: glyph.into() });
    }
}

/// A run's icon (a small dot, tinted by how it ended), the disclosure chevron and the Archive's (cod-archive).
pub const RUN_ICON: &str = "\u{EB8A}";
pub const CHEVRON: &str = "\u{EAB6}";
pub const ARCHIVE_ICON: &str = "\u{EA98}";
/// The palette's commands, each a codicon like the rest of its icons (one stroke weight, one optical size, outlined): kill
/// (cod-close), rename (cod-edit), toggle the sidebar (cod-layout-sidebar-left) and zoom in and out (cod-zoom-in,
/// cod-zoom-out).
pub const KILL_ICON: &str = "\u{EA76}";
pub const RENAME_ICON: &str = "\u{EA73}";
pub const SIDEBAR_ICON: &str = "\u{EBF3}";
pub const ZOOM_IN_ICON: &str = "\u{EB81}";
pub const ZOOM_OUT_ICON: &str = "\u{EB82}";
