//! Panes and the terminal primitive, as flecs components.
//!
//! A pane is a node under `hn.Main` holding `(Shows, object)` (harness-native-model): it draws that object with the
//! prefab its Kind picks (`hn.TermPane`, `hn.RunPane`). One pane has [`PaneFocus`]; it follows the
//! app's `(Selected, object)`. Nothing assumes there is only one: a split is a second pane node.
//!
//! `hn.Terminal` is the terminal primitive inside a term or run pane. Its cells stay in the GPU grid (term.rs draws a
//! `Snapshot`, which `term::snapshot` reads, the only reader of the parser). The node holds what input and drawing share:
//! - [`TermGrid`]: where the grid sits and how many cells it has (written by the sync, read by hit tests and drawing)
//! - [`TermCursor`] and [`TermInput`]: the cursor, modes and sizes from the last snapshot (written by drawing, read by input)
//! - [`TermSelection`]: the selection and the drag making it
//! - [`TermScroll`]: row-snapped scrolling and the wheel's remainder; the whole rows back stay on the object's `Term`
//! - [`TermMouse`]: the program's mouse button held down and the cell last reported
//! - [`MarkedText`]: an input method's uncommitted text (only on the focused terminal)
//! - [`TermFocus`]: the terminal keys go to (the focused pane's)
use crate::term::{Button, Input, Sel};
use harness_native_flecs::prelude::*;
use harness_vt::{MouseFormat, MouseTracking};

/// A pane node.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct PaneNode;

/// `(Back, view)` on a pane node: what its back affordance (cmd+[, the title bar's chevron) returns it to (the Archive,
/// after an object was opened from it). Exclusive; it goes when the pane shows something else.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Back;

/// The pane that follows the selection and takes keys (one).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct PaneFocus;

/// On an `hn.Terminal` node (named apart from the prefab: inside `prefab hn.Terminal` a bare `Terminal` is the prefab).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct TermNode;

/// The terminal keys go to.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct TermFocus;

/// The grid's rectangle (layout points, snapped to device pixels) and its size in cells.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct TermGrid {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub cols: u16,
    pub rows: u16,
}

impl TermGrid {
    /// Whether `(x, y)` is on the grid, `slack` points around it counted in.
    #[must_use]
    pub fn holds(&self, x: f32, y: f32, slack: f32) -> bool {
        self.w > 0.0
            && x >= self.x - slack
            && y >= self.y - slack
            && x <= self.x + self.w + slack
            && y <= self.y + self.h + slack
    }
}

/// The cursor in the active area (row, column), from the last snapshot.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct TermCursor {
    pub row: u16,
    pub col: u16,
}

/// The modes and sizes input reads, from the last snapshot ([`Input`], its modes packed into plain fields).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct TermInput {
    /// Bit 0 app cursor, 1 app keypad, 2 bracketed paste, 3 focus events, 4 alt scroll, 5 alt screen, 6 sync output,
    /// 7 cursor visible.
    pub flags: u8,
    pub mouse: u8,
    pub format: u8,
    pub kitty: u8,
    pub cols: u16,
    pub rows: u16,
    pub history: usize,
}

impl TermInput {
    #[must_use]
    pub fn pack(i: &Input) -> Self {
        let m = &i.modes;
        let bits = [
            m.app_cursor,
            m.app_keypad,
            m.bracketed_paste,
            m.focus_events,
            m.alt_scroll,
            m.alt_screen,
            m.sync_output,
            m.cursor_visible,
        ];
        let flags = bits.iter().enumerate().fold(0u8, |f, (k, &b)| f | (u8::from(b) << k));
        let mouse = match m.mouse {
            MouseTracking::None => 0,
            MouseTracking::X10 => 1,
            MouseTracking::Normal => 2,
            MouseTracking::Button => 3,
            MouseTracking::Any => 4,
        };
        let format = match m.mouse_format {
            MouseFormat::X10 => 0,
            MouseFormat::Utf8 => 1,
            MouseFormat::Sgr => 2,
            MouseFormat::Urxvt => 3,
            MouseFormat::SgrPixels => 4,
        };
        Self {
            flags,
            mouse,
            format,
            kitty: m.kitty_flags,
            cols: i.size.0,
            rows: i.size.1,
            history: i.history,
        }
    }

    /// The [`Input`] with `cursor`.
    #[must_use]
    pub fn unpack(&self, cursor: TermCursor) -> Input {
        let f = |k: u8| self.flags & (1 << k) != 0;
        let modes = harness_vt::Modes {
            app_cursor: f(0),
            app_keypad: f(1),
            bracketed_paste: f(2),
            focus_events: f(3),
            alt_scroll: f(4),
            alt_screen: f(5),
            sync_output: f(6),
            cursor_visible: f(7),
            mouse: match self.mouse {
                1 => MouseTracking::X10,
                2 => MouseTracking::Normal,
                3 => MouseTracking::Button,
                4 => MouseTracking::Any,
                _ => MouseTracking::None,
            },
            mouse_format: match self.format {
                1 => MouseFormat::Utf8,
                2 => MouseFormat::Sgr,
                3 => MouseFormat::Urxvt,
                4 => MouseFormat::SgrPixels,
                _ => MouseFormat::X10,
            },
            kitty_flags: self.kitty,
        };
        Input {
            modes,
            cursor: (cursor.row, cursor.col),
            size: (self.cols, self.rows),
            history: self.history,
        }
    }
}

/// What a press-and-drag selects by: cells, words (double click) or lines (triple click).
#[derive(Layout, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Unit {
    #[default]
    Cell,
    Word,
    Line,
}

impl Unit {
    /// What the `clicks`th press in a row selects by: a single press cells, a double words, a triple or more lines.
    pub fn of_clicks(clicks: isize) -> Self {
        match clicks {
            ..=1 => Unit::Cell,
            2 => Unit::Word,
            _ => Unit::Line,
        }
    }
}

/// The selection, in screen rows (0 the top visible row at scroll 0, negative into the history) and columns; the drag
/// making it; the unit it selects by and the span the press selected (a drag keeps it whole).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct TermSelection {
    pub sel: Option<((i64, u16), (i64, u16))>,
    pub selecting: bool,
    pub unit: Unit,
    pub anchor: ((i64, u16), (i64, u16)),
}

impl TermSelection {
    #[must_use]
    pub fn get(&self) -> Option<Sel> {
        self.sel.map(|(a, b)| Sel { a, b })
    }

    pub fn put(&mut self, s: Option<Sel>) {
        self.sel = s.map(|s| (s.a, s.b));
    }
}

/// Row-snapped scrolling: the wheel's fraction of a row not yet scrolled (the grid only moves by whole rows), wheel
/// lines not yet sent as keys or reports, and where the last frame drew the grid's first row and how many rows back
/// that viewport was. `total` and `len` are the rows in all and in view as the last frame fed them to the node's overlay
/// scroller (0: nothing to scroll, as on the alternate screen), and `row` how many of the scroller's points a row was.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct TermScroll {
    pub frac: f32,
    pub wheel: f32,
    pub top: f32,
    pub shift: i64,
    pub total: u64,
    pub len: u64,
    pub row: f32,
}

/// The program's mouse button held down (its mouse mode is on), and the cell last reported.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct TermMouse {
    pub report: Option<u8>,
    pub reported: Option<(u16, u16)>,
}

impl TermMouse {
    #[must_use]
    pub fn button(&self) -> Option<Button> {
        self.report.map(|b| match b {
            1 => Button::Middle,
            2 => Button::Right,
            _ => Button::Left,
        })
    }

    #[must_use]
    pub fn code(b: Button) -> u8 {
        match b {
            Button::Middle => 1,
            Button::Right => 2,
            _ => 0,
        }
    }
}

/// An input method's uncommitted text, drawn at the cursor.
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct MarkedText {
    pub s: ScriptString,
}

pub fn register(world: &World) {
    // The scene puts these tags on hn.Pane and hn.Terminal.
    world.component::<PaneNode>().writable();
    world.component::<PaneFocus>();
    world
        .component::<Back>()
        .add_trait::<flecs::Exclusive>()
        .on_delete_target(flecs::Cleanup::Remove);
    world.component::<TermNode>().writable();
    world.component::<TermFocus>();
    world.component::<TermGrid>();
    world.component::<TermCursor>();
    world.component::<TermInput>();
    world.component::<TermSelection>();
    world.component::<TermScroll>();
    world.component::<TermMouse>();
    world.component::<MarkedText>();
}
