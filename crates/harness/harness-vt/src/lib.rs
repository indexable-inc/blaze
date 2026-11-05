//! The terminal state machine the harness runs on: Ghostty's terminal core (libghostty-vt, through its C API) behind
//! the few calls the daemon and the native app make.
//!
//! - [`Term::feed`] parses program output; [`Term::query`] parses one terminal query and returns libghostty's reply.
//! - [`Term::resize`] resizes with reflow (the primary screen rewraps; the alternate screen does not).
//! - [`Term::rows`] reads the viewport's cells (grapheme, colours, every attribute, wide/spacer, hyperlink) with each
//!   row's dirty flag; [`Term::cursor`], [`Term::modes`], [`Term::title`] and [`Term::pwd`] the rest of the state.
//! - [`Term::vt_snapshot`] is the redraw sequence a fresh client feeds itself on attach.
//! - [`Term::encode_key`] encodes a key the way the program asked (legacy, or the kitty protocol).
//!
//! libghostty's handles are `!Send` because its C API makes no threading promise. A [`Term`] owns every handle it
//! uses (the terminal, its render state, iterators, the key encoder and the callbacks' shared state) and is only ever
//! touched by one thread at a time when callers keep it behind a `Mutex`, which is how Ghostty itself shares its
//! terminal between its IO and renderer threads. So it is `Send`.
use libghostty_vt::{
    fmt::{Format, Formatter, FormatterOptions},
    key,
    render::{CellIterator, CursorVisualStyle, Dirty, RenderState, RowIterator},
    screen::{CellContentTag, CellWide},
    selection::Selection,
    style::{RgbColor, StyleColor, Underline as GUnderline},
    terminal::{
        ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Point, PointCoordinate,
        PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes, SizeReportSize, TertiaryDeviceAttributes,
    },
    Terminal,
};
use std::cell::RefCell;
use std::rc::Rc;

pub use libghostty_vt as ghostty;
pub use libghostty_vt::key::{Action as KeyAction, Key, KittyKeyFlags, Mods as KeyMods};
pub use libghostty_vt::terminal::Mode;

/// A cell's pixel size as the daemon reports it (CSI 14/16 t). It has no font, so this is a nominal 8x16.
pub const CELL_PX: (u32, u32) = (8, 16);
pub use harness_ghostty_config as ghostty_config;
/// Ghostty's version when no Ghostty is installed to ask.
const GHOSTTY_VERSION_FALLBACK: &str = "1.3.2";

/// The installed Ghostty's version as `ghostty +version` prints it (`1.3.2-main+291fd050b`), which is what Ghostty
/// puts in XTVERSION and TERM_PROGRAM_VERSION. Asked once (about 20 ms); [`GHOSTTY_VERSION_FALLBACK`] without one.
#[must_use]
pub fn ghostty_version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        ["/Applications/Ghostty.app/Contents/MacOS/ghostty", "ghostty"]
            .iter()
            .find_map(|bin| {
                let out = std::process::Command::new(bin).arg("+version").output().ok()?;
                let s = String::from_utf8_lossy(&out.stdout);
                s.lines().next()?.strip_prefix("Ghostty ").map(|v| v.trim().to_owned())
            })
            .unwrap_or_else(|| GHOSTTY_VERSION_FALLBACK.to_owned())
    })
}

/// What XTVERSION answers: Ghostty's own reply (`ghostty <version>`), since this is Ghostty's terminal core and
/// programs (Claude Code among them) pick their Ghostty paths from it.
#[must_use]
pub fn xtversion() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| format!("ghostty {}", ghostty_version()))
}

/// A colour as the program set it: unset, a palette index, or truecolor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    #[default]
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

impl From<StyleColor> for Color {
    fn from(c: StyleColor) -> Self {
        match c {
            StyleColor::None => Self::Default,
            StyleColor::Palette(i) => Self::Idx(i.0),
            StyleColor::Rgb(RgbColor { r, g, b }) => Self::Rgb(r, g, b),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl From<GUnderline> for UnderlineStyle {
    fn from(u: GUnderline) -> Self {
        match u {
            GUnderline::Single => Self::Single,
            GUnderline::Double => Self::Double,
            GUnderline::Curly => Self::Curly,
            GUnderline::Dotted => Self::Dotted,
            GUnderline::Dashed => Self::Dashed,
            _ => Self::None,
        }
    }
}

/// One cell of the grid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Cell {
    /// Its whole grapheme cluster ("" for a blank or a spacer).
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    /// The underline's own colour (SGR 58); `Default` follows the ink.
    pub underline_color: Color,
    pub underline: UnderlineStyle,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    /// The left half of a wide (two-column) character.
    pub wide: bool,
    /// The right half of a wide character, or the pad where one did not fit at a soft-wrapped line's end: draw nothing.
    pub spacer: bool,
    /// The OSC 8 hyperlink it is part of.
    pub link: Option<String>,
}

/// One viewport row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// Changed since the last [`Term::clean`] (conservative: may be set when nothing changed).
    pub dirty: bool,
    /// Soft-wrapped into the next row.
    pub wrapped: bool,
}

/// The cursor's shape as DECSCUSR sets it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

/// The viewport's place in the screen and its history, in rows: `total` rows in all, the viewport `offset` rows from the
/// top of the history, `len` rows long (on the live screen, `offset + len == total`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Column and row (0-based): in the active area from [`Term::cursor`], in the viewport from [`Term::rows`].
    pub x: u16,
    pub y: u16,
    /// Shown (DECTCEM, `CSI ? 25 h/l`).
    pub visible: bool,
    /// Blinks (DECSCUSR's odd styles, or `CSI ? 12 h`).
    pub blinking: bool,
    /// DECSCUSR's shape; `Block` while not `explicit`.
    pub shape: CursorShape,
    /// The program chose the shape with DECSCUSR 1-6. False at start and after `CSI 0 q` or a reset: the embedder's
    /// default cursor applies.
    pub explicit: bool,
}

impl Cursor {
    /// DECSCUSR's parameter for this shape and blink (1-6), or 0 (the default) when not explicit.
    #[must_use]
    pub fn decscusr(self) -> u8 {
        let base = match self.shape {
            CursorShape::Block => 1,
            CursorShape::Underline => 3,
            CursorShape::Bar => 5,
        };
        if self.explicit { base + u8::from(!self.blinking) } else { 0 }
    }
}

/// Shape, blink and explicitness from the render state's cursor. The terminal's default style is block-hollow, which
/// DECSCUSR can never select, so hollow means "not set by the program".
fn cursor_look(rc: &libghostty_vt::render::Cursor) -> (CursorShape, bool, bool) {
    let (shape, explicit) = match rc.visual_style {
        CursorVisualStyle::Bar => (CursorShape::Bar, true),
        CursorVisualStyle::Underline => (CursorShape::Underline, true),
        CursorVisualStyle::Block => (CursorShape::Block, true),
        _ => (CursorShape::Block, false),
    };
    (shape, rc.blinking, explicit)
}

/// Which mouse events the program asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseTracking {
    #[default]
    None,
    /// 9: presses only.
    X10,
    /// 1000: presses and releases.
    Normal,
    /// 1002: and drags.
    Button,
    /// 1003: and every motion.
    Any,
}

/// How mouse reports are encoded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseFormat {
    #[default]
    X10,
    Utf8,
    Sgr,
    Urxvt,
    SgrPixels,
}

/// The modes input and drawing care about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Modes {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseTracking,
    pub mouse_format: MouseFormat,
    pub focus_events: bool,
    pub alt_scroll: bool,
    pub alt_screen: bool,
    pub sync_output: bool,
    pub cursor_visible: bool,
    pub kitty_flags: u8,
}

/// What changed through callbacks since the last [`Term::take_changes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// OSC 0/2.
    pub title: bool,
    /// OSC 7.
    pub pwd: bool,
    /// BELs rung.
    pub bells: u32,
}

#[derive(Default)]
struct Shared {
    replies: Vec<u8>,
    changes: Changes,
}

/// Modes the snapshot re-emits (after the content, so none of them changes how the content lands). Alternate screen,
/// cursor visibility, origin, keypad and grapheme clustering (which changes how the content parses, so goes first) are
/// handled on their own.
const SNAPSHOT_MODES: &[Mode] = &[
    Mode::KAM,
    Mode::INSERT,
    Mode::SRM,
    Mode::LINEFEED,
    Mode::DECCKM,
    Mode::REVERSE_COLORS,
    Mode::WRAPAROUND,
    Mode::AUTOREPEAT,
    Mode::X10_MOUSE,
    Mode::CURSOR_BLINKING,
    Mode::REVERSE_WRAP,
    Mode::NORMAL_MOUSE,
    Mode::BUTTON_MOUSE,
    Mode::ANY_MOUSE,
    Mode::FOCUS_EVENT,
    Mode::UTF8_MOUSE,
    Mode::SGR_MOUSE,
    Mode::ALT_SCROLL,
    Mode::URXVT_MOUSE,
    Mode::SGR_PIXELS_MOUSE,
    Mode::ALT_ESC_PREFIX,
    Mode::ALT_SENDS_ESC,
    Mode::REVERSE_WRAP_EXT,
    Mode::BRACKETED_PASTE,
    Mode::SYNC_OUTPUT,
    Mode::COLOR_SCHEME_REPORT,
    Mode::IN_BAND_RESIZE,
];

/// A terminal: libghostty's state machine plus what reading it needs.
pub struct Term {
    // Field order is drop order: everything reading the terminal goes before it.
    cells_it: CellIterator<'static>,
    rows_it: RowIterator<'static>,
    render: RenderState<'static>,
    encoder: key::Encoder<'static>,
    event: key::Event<'static>,
    term: Terminal<'static, 'static>,
    shared: Rc<RefCell<Shared>>,
    /// Each of [`SNAPSHOT_MODES`] as a fresh terminal has it.
    defaults: Vec<bool>,
}

// SAFETY: see the crate docs. Every libghostty handle and every `Rc` clone (the callbacks hold the others) lives in
// this struct and moves with it, and `&mut self` (or the caller's lock) keeps all use on one thread at a time.
unsafe impl Send for Term {}

/// `t` as `term`'s default colours.
fn apply_theme(term: &mut Terminal<'static, 'static>, t: &harness_ghostty_config::Theme) {
    let rgb = |[r, g, b]: [u8; 3]| RgbColor { r, g, b };
    ok(term.set_default_fg_color(Some(rgb(t.fg))));
    ok(term.set_default_bg_color(Some(rgb(t.bg))));
    ok(term.set_default_cursor_color(t.cursor.map(rgb)));
    ok(term.set_default_color_palette(Some(libghostty_vt::style::Palette(t.palette.map(rgb)))));
}

/// libghostty errors here are invariant breaks (bad handle, out of memory), not input errors: input never fails.
fn ok<T>(r: libghostty_vt::error::Result<T>) -> T {
    r.unwrap_or_else(|e| panic!("libghostty-vt: {e:?}"))
}

impl Term {
    /// A `cols` x `rows` terminal keeping `scrollback` lines of history.
    #[must_use]
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut term = ok(Terminal::new(cols, rows));
        let shared = Rc::new(RefCell::new(Shared::default()));
        ok(term.set_scrollback_max_lines(Some(scrollback)));
        ok(term.resize(cols, rows, CELL_PX.0, CELL_PX.1));
        // The user's Ghostty theme for the current appearance (OSC 4/10/11/12 answer from it; see `query`).
        apply_theme(&mut term, &harness_ghostty_config::current_theme());
        // CSI ? 996 n: the system appearance, which picks the theme's dark or light half.
        ok(term.on_color_scheme(|_| {
            Some(if harness_ghostty_config::system_dark() { libghostty_vt::terminal::ColorScheme::Dark } else { libghostty_vt::terminal::ColorScheme::Light })
        }));
        // The sentinel [`cursor_look`] reads as "no DECSCUSR in effect".
        ok(term.set_default_cursor_style(Some(libghostty_vt::terminal::CursorStyle::BlockHollow)));
        let s = shared.clone();
        ok(term.on_pty_write(move |_, data| s.borrow_mut().replies.extend_from_slice(data)));
        let s = shared.clone();
        ok(term.on_title_changed(move |_| s.borrow_mut().changes.title = true));
        let s = shared.clone();
        ok(term.on_pwd_changed(move |_| s.borrow_mut().changes.pwd = true));
        let s = shared.clone();
        ok(term.on_bell(move |_| s.borrow_mut().changes.bells += 1));
        ok(term.on_xtversion(|_| Some(xtversion())));
        ok(term.on_size(|t| {
            Some(SizeReportSize { rows: t.rows().ok()?, columns: t.cols().ok()?, cell_width: CELL_PX.0, cell_height: CELL_PX.1 })
        }));
        // As Ghostty answers (termio/stream_handler.zig): CSI ? 62 ; 22 c (VT220, ANSI colour; Ghostty adds 52 only
        // when it lets programs write the clipboard, which harness does not) and CSI > 1 ; 10 ; 0 c.
        ok(term.on_device_attributes(|_| {
            Some(DeviceAttributes {
                primary: PrimaryDeviceAttributes::new(ConformanceLevel::LEVEL_2, &[DeviceAttributeFeature::ANSI_COLOR]),
                secondary: SecondaryDeviceAttributes { device_type: DeviceType::VT220, firmware_version: 10, rom_cartridge: 0 },
                tertiary: TertiaryDeviceAttributes::default(),
            })
        }));
        let defaults = SNAPSHOT_MODES.iter().map(|&m| term.mode(m).unwrap_or(false)).collect();
        Self {
            cells_it: ok(CellIterator::new()),
            rows_it: ok(RowIterator::new()),
            render: ok(RenderState::new()),
            encoder: ok(key::Encoder::new()),
            event: ok(key::Event::new()),
            term,
            shared,
            defaults,
        }
    }

    /// The libghostty terminal itself, for anything this API leaves out.
    #[must_use]
    pub fn raw(&self) -> &Terminal<'static, 'static> {
        &self.term
    }

    pub fn raw_mut(&mut self) -> &mut Terminal<'static, 'static> {
        &mut self.term
    }

    /// Parse program output. Any reply libghostty produces for it is dropped (see [`Term::query`]).
    pub fn feed(&mut self, bytes: &[u8]) {
        self.term.vt_write(bytes);
        self.shared.borrow_mut().replies.clear();
    }

    /// Parse `bytes` (normally one terminal query) and return what libghostty wrote back for it. The default colours
    /// are first set again from the user's Ghostty theme for the appearance now, so OSC 4/10/11 follow a theme edit or
    /// a light/dark switch.
    pub fn query(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.set_theme(&harness_ghostty_config::current_theme());
        self.shared.borrow_mut().replies.clear();
        self.term.vt_write(bytes);
        std::mem::take(&mut self.shared.borrow_mut().replies)
    }

    /// Make `t` the default colours (ink, ground, cursor, the 256-colour palette); colours a program set by OSC stay.
    pub fn set_theme(&mut self, t: &harness_ghostty_config::Theme) {
        apply_theme(&mut self.term, t);
    }

    /// Resize to `cols` x `rows`, reflowing the primary screen.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        ok(self.term.resize(cols.max(1), rows.max(1), CELL_PX.0, CELL_PX.1));
        self.shared.borrow_mut().replies.clear();
    }

    /// Columns and rows.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        (ok(self.term.cols()), ok(self.term.rows()))
    }

    #[must_use]
    pub fn title(&self) -> String {
        String::from_utf8_lossy(self.term.title().unwrap_or_default()).into_owned()
    }

    #[must_use]
    pub fn pwd(&self) -> String {
        String::from_utf8_lossy(self.term.pwd().unwrap_or_default()).into_owned()
    }

    /// Title, pwd and bell events since the last call.
    pub fn take_changes(&mut self) -> Changes {
        std::mem::take(&mut self.shared.borrow_mut().changes)
    }

    #[must_use]
    pub fn mode(&self, m: Mode) -> bool {
        self.term.mode(m).unwrap_or(false)
    }

    #[must_use]
    pub fn modes(&self) -> Modes {
        let m = |x| self.mode(x);
        let mouse = if m(Mode::ANY_MOUSE) {
            MouseTracking::Any
        } else if m(Mode::BUTTON_MOUSE) {
            MouseTracking::Button
        } else if m(Mode::NORMAL_MOUSE) {
            MouseTracking::Normal
        } else if m(Mode::X10_MOUSE) {
            MouseTracking::X10
        } else {
            MouseTracking::None
        };
        let mouse_format = if m(Mode::SGR_PIXELS_MOUSE) {
            MouseFormat::SgrPixels
        } else if m(Mode::SGR_MOUSE) {
            MouseFormat::Sgr
        } else if m(Mode::URXVT_MOUSE) {
            MouseFormat::Urxvt
        } else if m(Mode::UTF8_MOUSE) {
            MouseFormat::Utf8
        } else {
            MouseFormat::X10
        };
        Modes {
            app_cursor: m(Mode::DECCKM),
            app_keypad: m(Mode::KEYPAD_KEYS),
            bracketed_paste: m(Mode::BRACKETED_PASTE),
            mouse,
            mouse_format,
            focus_events: m(Mode::FOCUS_EVENT),
            alt_scroll: m(Mode::ALT_SCROLL),
            alt_screen: self.alternate_screen(),
            sync_output: m(Mode::SYNC_OUTPUT),
            cursor_visible: m(Mode::CURSOR_VISIBLE),
            kitty_flags: self.term.kitty_keyboard_flags().map_or(0, |f| f.bits()),
        }
    }

    #[must_use]
    pub fn alternate_screen(&self) -> bool {
        matches!(self.term.active_screen(), Ok(libghostty_vt::screen::Screen::Alternate))
    }

    #[must_use]
    pub fn application_cursor(&self) -> bool {
        self.mode(Mode::DECCKM)
    }

    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.mode(Mode::BRACKETED_PASTE)
    }

    /// The cursor: place in the active area, visibility, and DECSCUSR shape/blink/explicitness.
    pub fn cursor(&mut self) -> Cursor {
        let Self { render, term, .. } = self;
        let rc = ok(ok(render.update(term)).cursor());
        let (shape, blinking, explicit) = cursor_look(&rc);
        Cursor {
            x: self.term.cursor_x().unwrap_or(0),
            y: self.term.cursor_y().unwrap_or(0),
            visible: self.term.is_cursor_visible().unwrap_or(true),
            blinking,
            shape,
            explicit,
        }
    }

    /// Show `n` rows back into the history (0: the live screen), clamped to what there is.
    pub fn set_scrollback(&mut self, n: usize) {
        self.term.scroll_viewport(ScrollViewport::Bottom);
        if n > 0 {
            self.term.scroll_viewport(ScrollViewport::Delta(-isize::try_from(n).unwrap_or(isize::MAX)));
        }
    }

    /// How many rows back the viewport shows.
    #[must_use]
    pub fn scrollback(&self) -> usize {
        self.term.scrollbar().map_or(0, |s| usize::try_from(s.total.saturating_sub(s.offset + s.len)).unwrap_or(0))
    }

    /// Where the viewport sits in the screen and its history, in rows (libghostty's scrollbar): what a scroller shows.
    #[must_use]
    pub fn scrollbar(&self) -> Option<Scrollbar> {
        self.term.scrollbar().ok().map(|s| Scrollbar { total: s.total, offset: s.offset, len: s.len })
    }

    /// Rows of history above the live screen.
    #[must_use]
    pub fn history(&self) -> usize {
        self.term.scrollback_rows().unwrap_or(0)
    }

    /// The viewport: its rows of cells (with dirty flags) and the cursor, if it is within the viewport (check
    /// `visible` before drawing it).
    pub fn rows(&mut self) -> (Vec<Row>, Option<Cursor>) {
        let Self { cells_it, rows_it, render, term, .. } = self;
        let snap = ok(render.update(term));
        let rc = ok(snap.cursor());
        let (shape, blinking, explicit) = cursor_look(&rc);
        let cursor = rc.viewport.map(|v| Cursor { x: v.x, y: v.y, visible: rc.visible, blinking, shape, explicit });
        let mut out = Vec::with_capacity(usize::from(ok(snap.rows())));
        let mut rows = ok(rows_it.update(&snap));
        let mut text = String::new();
        let mut uri = Vec::new();
        let mut y = 0u16;
        while let Some(row) = rows.next() {
            let raw_row = ok(row.raw_row());
            let mut r = Row { cells: Vec::new(), dirty: row.dirty().unwrap_or(true), wrapped: raw_row.is_wrapped().unwrap_or(false) };
            let mut cells = ok(cells_it.update(row));
            let mut x = 0u16;
            while let Some(cell) = cells.next() {
                let raw = ok(cell.raw_cell());
                let style = if cell.has_styling().unwrap_or(false) { ok(cell.style()) } else { Default::default() };
                let wide = raw.wide().unwrap_or(CellWide::Narrow);
                let spacer = matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead);
                if spacer {
                    text.clear();
                } else {
                    ok(cell.graphemes_utf8(&mut text));
                }
                let bg = match raw.content_tag() {
                    Ok(CellContentTag::BgColorPalette) => raw.bg_color_palette().map_or(Color::Default, |i| Color::Idx(i.0)),
                    Ok(CellContentTag::BgColorRgb) => raw.bg_color_rgb().map_or(Color::Default, |c| Color::Rgb(c.r, c.g, c.b)),
                    _ => style.bg_color.into(),
                };
                let link = raw.has_hyperlink().unwrap_or(false).then(|| link_at(term, x, y, &mut uri)).flatten();
                r.cells.push(Cell {
                    text: text.clone(),
                    fg: style.fg_color.into(),
                    bg,
                    underline_color: style.underline_color.into(),
                    underline: style.underline.into(),
                    bold: style.bold,
                    italic: style.italic,
                    faint: style.faint,
                    blink: style.blink,
                    inverse: style.inverse,
                    invisible: style.invisible,
                    strikethrough: style.strikethrough,
                    overline: style.overline,
                    wide: wide == CellWide::Wide,
                    spacer,
                    link,
                });
                x += 1;
            }
            out.push(r);
            y += 1;
        }
        (out, cursor)
    }

    /// Whether anything changed since the last [`Term::clean`].
    pub fn dirty(&mut self) -> Dirty {
        let Self { render, term, .. } = self;
        ok(ok(render.update(term)).dirty())
    }

    /// Mark the frame and every row clean (after drawing them).
    pub fn clean(&mut self) {
        let Self { render, term, .. } = self;
        ok(ok(render.update(term)).clean());
    }

    /// The viewport as text: one line per row, trailing blanks trimmed, trailing empty lines dropped.
    pub fn text(&mut self) -> String {
        let (rows, _) = self.rows();
        let mut lines: Vec<String> = rows.iter().map(|r| row_text(&r.cells, 0, r.cells.len())).collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// The text from viewport row `r0` column `c0` to row `r1` column `c1` (exclusive), rows joined by newlines
    /// except where a row soft-wraps into the next.
    pub fn contents_between(&mut self, r0: u16, c0: u16, r1: u16, c1: u16) -> String {
        let (rows, _) = self.rows();
        let mut s = String::new();
        for (i, row) in rows.iter().enumerate().take(usize::from(r1) + 1).skip(usize::from(r0)) {
            let from = if i == usize::from(r0) { usize::from(c0) } else { 0 };
            let to = if i == usize::from(r1) { usize::from(c1).min(row.cells.len()) } else { row.cells.len() };
            s.push_str(&row_text(&row.cells, from, to));
            if i != usize::from(r1) && !row.wrapped {
                s.push('\n');
            }
        }
        s
    }

    /// Escape codes that rebuild this terminal's visible state on a fresh one: alternate screen, the screen's cells
    /// with their styles and hyperlinks, the pen, charsets, tab stops, kitty keyboard flags, modes, cursor (place,
    /// visibility, shape), title and pwd.
    pub fn vt_snapshot(&mut self) -> Vec<u8> {
        let c = self.cursor();
        let t = &self.term;
        let (cols, rows) = self.size();
        let mut out = Vec::new();
        if self.alternate_screen() {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        out.extend_from_slice(if self.mode(Mode::GRAPHEME_CLUSTER) { b"\x1b[?2027h" } else { b"\x1b[?2027l" });
        out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
        let corner = |x, y| t.grid_ref(Point::Active(PointCoordinate { x, y }));
        if let (Ok(tl), Ok(br)) = (corner(0, 0), corner(cols - 1, u32::from(rows - 1))) {
            let sel = Selection::new(tl, br, false);
            let opts = FormatterOptions::new()
                .with_format(Format::Vt)
                .with_unwrap(false)
                .with_trim(false)
                .with_selection(&sel)
                .with_style(true)
                .with_hyperlink(true)
                .with_protection(true)
                .with_kitty_keyboard(true)
                .with_charsets(true)
                .with_tabstops(true)
                .with_keyboard(true)
                .with_pwd(true);
            if let Ok(bytes) = Formatter::new(t, opts).and_then(|mut f| f.format_alloc(None).map(|b| b.to_vec())) {
                out.extend_from_slice(&bytes);
            }
        }
        for (&m, &default) in SNAPSHOT_MODES.iter().zip(&self.defaults) {
            let on = self.mode(m);
            if on != default {
                let q = if matches!(m.kind(), libghostty_vt::terminal::ModeKind::Dec) { "?" } else { "" };
                out.extend(format!("\x1b[{q}{}{}", m.value(), if on { 'h' } else { 'l' }).into_bytes());
            }
        }
        if self.mode(Mode::KEYPAD_KEYS) {
            out.extend_from_slice(b"\x1b=");
        }
        if self.mode(Mode::ORIGIN) {
            out.extend_from_slice(b"\x1b[?6h");
        }
        out.extend(format!("\x1b[{};{}H", c.y + 1, c.x + 1).into_bytes());
        if !c.visible {
            out.extend_from_slice(b"\x1b[?25l");
        }
        if c.explicit {
            out.extend(format!("\x1b[{} q", c.decscusr()).into_bytes());
        }
        let title = self.title();
        if !title.is_empty() {
            out.extend(format!("\x1b]2;{title}\x1b\\").into_bytes());
        }
        out
    }

    /// The bytes a key event sends, encoded the way the program asked (cursor/keypad modes, kitty flags, ...).
    /// `text` is what the key typed (None for none); `unshifted` the key's character without modifiers.
    pub fn encode_key(&mut self, k: Key, mods: KeyMods, action: KeyAction, text: Option<&str>, unshifted: Option<char>) -> Vec<u8> {
        let Self { encoder, event, term, .. } = self;
        encoder.set_options_from_terminal(term);
        event.set_action(action).set_key(k).set_mods(mods).set_utf8(text);
        if let Some(c) = unshifted {
            event.set_unshifted_codepoint(c);
        }
        let mut out = Vec::new();
        let _ = encoder.encode_to_vec(event, &mut out);
        out
    }
}

/// The OSC 8 URI of viewport cell (x, y).
fn link_at(term: &Terminal<'_, '_>, x: u16, y: u16, buf: &mut Vec<u8>) -> Option<String> {
    let r = term.grid_ref(Point::Viewport(PointCoordinate { x, y: u32::from(y) })).ok()?;
    buf.resize(256, 0);
    let n = match r.hyperlink_uri(buf) {
        Ok(n) => n,
        Err(libghostty_vt::Error::OutOfSpace { required }) => {
            buf.resize(required, 0);
            r.hyperlink_uri(buf).ok()?
        }
        Err(_) => return None,
    };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Cells `from..to` as text, spacers skipped, blanks as spaces, trailing blanks trimmed.
fn row_text(cells: &[Cell], from: usize, to: usize) -> String {
    let mut s = String::new();
    for c in cells.get(from..to.max(from)).unwrap_or_default() {
        if c.spacer {
            continue;
        }
        if c.text.is_empty() { s.push(' ') } else { s.push_str(&c.text) }
    }
    s.truncate(s.trim_end().len());
    s
}
