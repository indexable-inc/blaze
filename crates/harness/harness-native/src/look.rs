//! harness-native's look: the small moving parts of the sidebar on den's spring system, and the per-kind icons and tones.
//!
//! Every value (colours, sizes, spring tunes) is in the scene (scene/00-tokens.flecs, scene/10-harness.flecs); this file
//! reads prefabs by name and keeps only runtime state: a running dot's breath, a part shown while its row is hovered, a
//! disclosure chevron's turn, and the selection pill's spring between rows.
use harness_native_api::spring::{Spring, Tune};
use harness_native_api::{Fold, HoverAmt, moving, settle};
use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{Padding, Size};
use harness_native_ui_look_api::Fill;
use harness_native_ui_motion_api::{Motion, Reveal};
use harness_native_ui_view as view;

/// A running status dot's breath: a spring chasing 0 and 1 in turn (`up` its current target).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Breath {
    pub spring: Spring,
    pub up: bool,
}

/// A part shown only while row `row` is hovered (its opacity the row's hover spring), or always when `keep`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct HoverShow {
    pub row: u64,
    pub keep: bool,
}

/// A disclosure chevron: whether what it discloses is open. Its turn (0 closed, 1 open: a quarter turn) is the `Fold`
/// spring on the same entity.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Disclose {
    pub open: bool,
}

pub fn register(world: &World) {
    world.component::<Breath>();
    world.component::<HoverShow>();
    world.component::<Disclose>();
}

/// The theme's `role` (`hn.Theme.<role>`, scene/10-harness.flecs). The scene is compiled in, so a role it lacks is a bug.
pub fn role<'a>(w: WorldRef<'a>, role: &str) -> EntityView<'a> {
    view::prefab(w, &format!("hn.Theme.{role}")).unwrap_or_else(|| panic!("the scene's hn.Theme has no {role}"))
}

/// A role's fill (`running`, `kind_claude`).
pub fn tone(w: WorldRef<'_>, name: &str) -> Fill {
    role(w, name).try_cloned::<&Fill>().unwrap_or_else(|| panic!("hn.Theme.{name} has no Fill"))
}

/// A role's spring (`breath`, `pill`).
pub fn tune(w: WorldRef<'_>, name: &str) -> Tune {
    let m = role(w, name).try_cloned::<&Motion>().unwrap_or_else(|| panic!("hn.Theme.{name} has no Motion"));
    Tune { omega: m.omega, zeta: m.zeta }
}

/// A role's `Reveal` alpha0 (`breath`: how far a breath dims).
pub fn floor(w: WorldRef<'_>, name: &str) -> f32 {
    role(w, name).try_cloned::<&Reveal>().unwrap_or_else(|| panic!("hn.Theme.{name} has no Reveal")).alpha0
}

/// A role's fixed width (`indent`).
pub fn width(w: WorldRef<'_>, name: &str) -> f32 {
    role(w, name).try_cloned::<&Size>().unwrap_or_else(|| panic!("hn.Theme.{name} has no Size")).w.value
}

/// A role's padding (`badge_on`).
pub fn padding(w: WorldRef<'_>, name: &str) -> Padding {
    role(w, name).try_cloned::<&Padding>().unwrap_or_else(|| panic!("hn.Theme.{name} has no Padding"))
}

/// How near its end a breath turns back (of its 0..1 travel).
const BREATH_TURN: f32 = 0.04;
/// A pill jump longer than this (points: a far row, a scrolled tree) lands at once instead of sliding.
const PILL_LEAP: f32 = 600.0;

/// The springs: breaths, chevrons' turns.
pub fn install(w: WorldRef<'_>) {
    let breath = tune(w, "breath");
    w.system_named::<&mut Breath>("Breathe").each_iter(move |it, _row, b| {
        let target = f32::from(u8::from(b.up));
        b.spring.step(target, breath, it.delta_time());
        // Turn back a little short of the end: a breath never rests.
        if (b.spring.x - target).abs() < BREATH_TURN {
            b.up = !b.up;
        }
    });
    let turn = tune(w, "turn");
    w.system_named::<(&Disclose, &mut Fold)>("Turn").each_iter(move |it, _row, (d, f)| {
        let w = it.world();
        if settle(&mut f.0, f32::from(u8::from(d.open)), turn, it.delta_time()) {
            moving(&w);
        }
    });
}

/// How much of a node's own opacity its look leaves: a breath's dimming, a hover-only part's fade.
pub fn alpha(w: WorldRef<'_>, e: EntityView<'_>, lo: f32) -> f32 {
    let mut a = 1.0;
    if let Some(b) = e.try_cloned::<&Breath>() {
        a *= lo + (1.0 - lo) * b.spring.x.clamp(0.0, 1.0);
    }
    if let Some(h) = e.try_cloned::<&HoverShow>().filter(|h| !h.keep) {
        a *= w.entity_from_id(h.row).try_cloned::<&HoverAmt>().map_or(0.0, |h| h.spring.x.clamp(0.0, 1.0));
    }
    a
}

/// A disclosure chevron's turn in radians (a quarter turn open), when `e` is one.
pub fn turn(e: EntityView<'_>) -> Option<f32> {
    e.has(Disclose::id()).then(|| e.try_cloned::<&Fold>().map_or(0.0, |f| f.0.x) * std::f32::consts::FRAC_PI_2)
}

/// Mark `part` shown only while `row` is hovered, unless `keep`.
pub fn hover_show(part: EntityView<'_>, row: u64, keep: bool) {
    let want = HoverShow { row, keep };
    if part.try_cloned::<&HoverShow>().is_none_or(|h| h.row != row || h.keep != keep) {
        part.set(want);
    }
}

/// Let `dot` breathe (or stop).
pub fn breathe(dot: EntityView<'_>, on: bool) {
    match (on, dot.has(Breath::id())) {
        (true, false) => {
            dot.set(Breath { spring: Spring::at(1.0), up: false });
        }
        (false, true) => {
            dot.remove(Breath::id());
        }
        _ => {}
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

/// Whether anything breathes (the window then keeps drawing, at a calm rate).
pub fn breathing(w: WorldRef<'_>) -> bool {
    let mut any = false;
    w.new_query::<&Breath>().each_entity(|_, _| any = true);
    any
}

/// A kind's icon: an SF Symbols–like codicon of the bundled Symbols Nerd Font.
pub fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "claude" => "\u{EC10}", // cod-sparkle
        "codex" => "\u{EC81}",  // cod-openai
        "cursor" => "\u{F01BF}", // md-cursor_default_outline
        "shell" => "\u{EA85}",  // cod-terminal
        "stage" => "\u{EC07}",  // cod-circle_small
        _ => "\u{EB8A}",        // cod-circle_small_filled
    }
}

/// A kind's icon tone (a theme role): a small warm colour each.
pub fn kind_tone(kind: &str) -> &'static str {
    match kind {
        "claude" => "kind_claude",
        "codex" => "kind_codex",
        "cursor" => "kind_cursor",
        _ => "kind_shell",
    }
}

/// A run's icon (a small dot, tinted by how it ended), a group's, the disclosure chevron and the Trash's.
pub const RUN_ICON: &str = "\u{EB8A}";
pub const GROUP_ICON: &str = "\u{EA82}";
pub const CHEVRON: &str = "\u{EAB6}";
pub const TRASH_ICON: &str = "\u{EA81}";

/// The selection pill: it springs from the row it lit to the row selected next.
#[derive(Default)]
pub struct Pill {
    y: Spring,
    h: Spring,
    row: Option<u64>,
    pub dt: f32,
}

impl Pill {
    /// Where to draw the pill for selected row `row` laid at `(y, h)`, and whether it is still moving.
    pub fn place(&mut self, row: u64, y: f32, h: f32, tune: Tune) -> (f32, f32, bool) {
        if self.row.is_none() {
            self.y = Spring::at(y);
            self.h = Spring::at(h);
        }
        self.row = Some(row);
        let a = settle(&mut self.y, y, tune, self.dt);
        let b = settle(&mut self.h, h, tune, self.dt);
        // A jump of more than a screenful (a far row, a scrolled tree) lands at once.
        if (self.y.x - y).abs() > PILL_LEAP {
            self.y = Spring::at(y);
        }
        (self.y.x, self.h.x, a || b)
    }

    /// No row lit this frame: the next selection lands where it is.
    pub fn lost(&mut self) {
        self.row = None;
    }
}
