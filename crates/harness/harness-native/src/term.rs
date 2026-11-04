//! The terminal: a libghostty screen (harness-vt) drawn as panels (cell backgrounds, selection, cursor), lines (underlines) and atlas glyphs,
//! on the same pipeline and glyph cache as the rest of the UI; and input: keys through Ghostty's encoder (legacy or the
//! kitty protocol, from the modes [`snapshot`] reads), natural text editing, mouse reports, paste and selection units.
//!
//! Each distinct cell (its grapheme, bold, italic) is shaped once by den's typesetter in the code face, so a character
//! the monospace font lacks (CJK, emoji, symbols) comes from the shaper's fallback, and is placed on the cell grid.
use crate::draw::Draw;
use crate::pack::{GlyphInst, LineInst, PanelInst};
use crate::text::{Face, Fonts, GlyphCache, Layout, SUBPX, Style};
use std::collections::HashMap;

/// A colour as the shaders take it (straight RGBA, sRGB).
pub type Rgba = [f32; 4];

/// The grid's look: default ink and ground, the 16 ANSI colours, cursor and selection.
pub struct Theme {
    pub fg: Rgba,
    pub bg: Rgba,
    pub ansi: [Rgba; 16],
    pub cursor: Rgba,
    pub select: Rgba,
}

const fn hex(v: u32) -> Rgba {
    [((v >> 16) & 0xff) as f32 / 255.0, ((v >> 8) & 0xff) as f32 / 255.0, (v & 0xff) as f32 / 255.0, 1.0]
}

pub const DARK: Theme = Theme {
    fg: hex(0xdcdce0),
    bg: hex(0x121214),
    ansi: [
        hex(0x2a2a2e),
        hex(0xeb6b6b),
        hex(0x4cb782),
        hex(0xf2bb52),
        hex(0x6e8cff),
        hex(0xc48cff),
        hex(0x5ec4d6),
        hex(0xc9c9cf),
        hex(0x5c5c64),
        hex(0xff8a8a),
        hex(0x6fd6a0),
        hex(0xffd27a),
        hex(0x93a9ff),
        hex(0xd7aaff),
        hex(0x86dbe8),
        hex(0xededf0),
    ],
    cursor: hex(0xd0d0ff),
    select: [0.533, 0.553, 1.0, 0.28],
};

/// The 256-colour palette's colour `i` (16..=255: the 6x6x6 cube, then the greys).
fn idx(theme: &Theme, i: u8) -> Rgba {
    match i {
        0..=15 => theme.ansi[usize::from(i)],
        16..=231 => {
            let i = i - 16;
            let step = |c: u8| if c == 0 { 0.0 } else { (55.0 + 40.0 * f32::from(c)) / 255.0 };
            [step(i / 36), step((i / 6) % 6), step(i % 6), 1.0]
        }
        _ => {
            let v = (8.0 + 10.0 * f32::from(i - 232)) / 255.0;
            [v, v, v, 1.0]
        }
    }
}

fn colour(theme: &Theme, c: harness_vt::Color, fg: bool) -> Option<Rgba> {
    match c {
        harness_vt::Color::Default => fg.then_some(theme.fg),
        harness_vt::Color::Idx(i) => Some(idx(theme, i)),
        harness_vt::Color::Rgb(r, g, b) => Some([f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0]),
    }
}

/// One shaped cell: its glyphs, x from the cell's left and y from its baseline (points, y up).
#[derive(Clone)]
struct Shape {
    glyphs: Vec<(u64, u16, f32, f32)>,
}

/// The cell grid's metrics at a font size and display scale, and the cells shaped so far. A cell is a whole number of
/// device pixels each way, so rows and columns tile with no seams (block elements and box lines meet exactly).
pub struct Grid {
    pub size: f32,
    pub scale: f32,
    /// A cell's width and height and the baseline below its top, points (each a whole number of device pixels).
    pub cw: f32,
    pub ch: f32,
    pub base: f32,
    shapes: HashMap<(String, u8), Shape>,
}

/// A selection on the grid, in screen rows (0 the top visible row at `scroll` 0, negative into the scrollback) and columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sel {
    pub a: (i64, u16),
    pub b: (i64, u16),
}

impl Sel {
    /// Start and end, in reading order.
    #[must_use]
    pub fn ordered(self) -> ((i64, u16), (i64, u16)) {
        if self.a <= self.b { (self.a, self.b) } else { (self.b, self.a) }
    }

    fn holds(self, row: i64, col: u16) -> bool {
        let (s, e) = self.ordered();
        (row, col) >= s && (row, col) <= e
    }
}

impl Grid {
    pub fn new(fonts: &mut Fonts, size: f32, scale: f32) -> Self {
        let (adv, ascent, descent) = crate::text::mono_cell(fonts, size);
        let px = |v: f32| (v * scale).round().max(1.0) / scale;
        let cw = px(adv);
        let ch = px(((ascent + descent) * 1.18).max(size));
        let base = px((ch - (ascent + descent)) * 0.5 + ascent);
        Self { size, scale, cw, ch, base, shapes: HashMap::new() }
    }

    /// Columns and rows that fit in `w` x `h` points.
    #[must_use]
    pub fn fit(&self, w: f32, h: f32) -> (u16, u16) {
        (((w / self.cw).floor() as u16).max(2), ((h / self.ch).floor() as u16).max(1))
    }

    fn shape(&mut self, fonts: &mut Fonts, s: &str, bold: bool, italic: bool) -> &Shape {
        let key = (s.to_owned(), u8::from(bold) | u8::from(italic) << 1);
        let size = self.size;
        self.shapes.entry(key).or_insert_with(|| {
            let face = match (bold, italic) {
                (_, true) => Face::CodeItalic,
                (true, false) => Face::CodeWeight(700),
                _ => Face::Code,
            };
            let lay = Layout::in_face(fonts, s, face, Style { size, bold, line: size * 1.3 }, f32::MAX);
            let glyphs = lay.lines.first().map(|l| l.glyphs.iter().map(|g| (g.font, g.id, g.x, g.y)).collect()).unwrap_or_default();
            Shape { glyphs }
        })
    }
}

/// Where the grid is drawn and what goes with it this frame.
pub struct Paint<'a> {
    pub origin: (f32, f32),
    pub scale: f32,
    pub focused: bool,
    pub sel: Option<Sel>,
    /// Rows scrolled back.
    pub scroll: usize,
    pub theme: &'a Theme,
    pub time: f32,
    /// An input method's uncommitted text, drawn underlined at the cursor.
    pub marked: Option<&'a str>,
}

/// One cell as the drawing reads it, colours resolved (inverse applied): the only thing drawing knows of the parser.
#[derive(Clone, Debug, Default)]
pub struct Cell {
    /// Its grapheme cluster ("" for a blank).
    pub text: String,
    pub fg: Rgba,
    /// None: the terminal's own ground shows.
    pub bg: Option<Rgba>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub wide: bool,
    /// The right half of a wide cell.
    pub cont: bool,
}

/// What drawing reads of a screen: its rows of cells, the cursor and whether it shows.
pub struct Snapshot {
    pub rows: Vec<Vec<Cell>>,
    pub cursor: (u16, u16),
    pub cursor_shown: bool,
    /// The cursor's look: DECSCUSR shape and blink, and whether the program set it (`explicit`) or the default applies.
    pub cursor_look: harness_vt::Cursor,
    /// The modes and sizes keyboard and mouse input read.
    pub input: Input,
}

/// Read `term`'s viewport (at its current scrollback offset) into a [`Snapshot`]. The one place drawing touches the
/// terminal: swapping it is swapping this function.
#[must_use]
pub fn snapshot(term: &mut harness_vt::Term, theme: &Theme) -> Snapshot {
    let (rows, cursor) = term.rows();
    let out = rows
        .into_iter()
        .map(|row| {
            row.cells
                .into_iter()
                .map(|cell| {
                    let fg = colour(theme, cell.fg, true).unwrap_or(theme.fg);
                    let bg = colour(theme, cell.bg, false);
                    let (fg, bg) = if cell.inverse { (bg.unwrap_or(theme.bg), Some(fg)) } else { (fg, bg) };
                    Cell {
                        text: if cell.invisible { String::new() } else { cell.text },
                        fg,
                        bg,
                        bold: cell.bold,
                        italic: cell.italic,
                        underline: cell.underline != harness_vt::UnderlineStyle::None,
                        dim: cell.faint,
                        wide: cell.wide,
                        cont: cell.spacer,
                    }
                })
                .collect()
        })
        .collect();
    let look = cursor.unwrap_or_default();
    let at = term.cursor();
    let (cols, nrows) = term.size();
    let input = Input { modes: term.modes(), cursor: (at.y, at.x), size: (cols, nrows), history: term.history() };
    Snapshot { rows: out, cursor: (look.y, look.x), cursor_shown: cursor.is_some_and(|c| c.visible), cursor_look: look, input }
}

/// Draw `snap` at `p.origin`: cell grounds, the selection, the cursor, procedural block and box glyphs, underlines, then
/// the font's glyphs.
pub fn draw(d: &mut Draw, grid: &mut Grid, fonts: &mut Fonts, cache: &mut GlyphCache, snap: &Snapshot, p: &Paint<'_>, upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8])) {
    let sc = p.scale;
    let px = |v: f32| (v * sc).round() / sc;
    let (ox, oy) = (px(p.origin.0), px(p.origin.1));
    let (cw, ch) = (grid.cw, grid.ch);
    let theme = p.theme;
    let cursor = snap.cursor;
    let marked = p.marked.filter(|m| !m.is_empty() && p.scroll == 0);
    // The cells the marked text covers: the grid's own glyphs there give way to it.
    let marked_span = marked.map(|m| (cursor.0, cursor.1, cursor.1.saturating_add(columns(m) as u16)));
    let show_cursor = snap.cursor_shown && p.scroll == 0 && marked.is_none();
    let mut glyph_runs: Vec<(f32, f32, String, bool, bool, Rgba)> = Vec::new();
    let mut boxes: Vec<(char, [f32; 4], Rgba)> = Vec::new();
    let mut lines: Vec<LineInst> = Vec::new();
    for (r, row) in snap.rows.iter().enumerate() {
        let r = r as u16;
        let y = oy + f32::from(r) * ch;
        let srow = i64::from(r) - p.scroll as i64;
        // Backgrounds: runs of one colour along the row, one panel each.
        let mut run: Option<(u16, Rgba)> = None;
        let flush = |d: &mut Draw, run: &mut Option<(u16, Rgba)>, end: u16| {
            if let Some((start, c)) = run.take() {
                let x0 = ox + f32::from(start) * cw;
                d.panel(rect_panel([x0, y, f32::from(end - start) * cw, ch], c));
            }
        };
        for (c, cell) in row.iter().enumerate() {
            let c = c as u16;
            let bg = if p.sel.is_some_and(|s| s.holds(srow, c)) { Some(blend(cell.bg.unwrap_or(theme.bg), theme.select)) } else { cell.bg };
            if run.map(|r| r.1) != bg {
                flush(d, &mut run, c);
                if let Some(bg) = bg {
                    run = Some((c, bg));
                }
            }
            if cell.cont || cell.text.is_empty() || cell.text == " " || marked_span.is_some_and(|(mr, a, b)| mr == r && (a..b).contains(&c)) {
                continue;
            }
            let mut ink = cell.fg;
            if cell.dim {
                ink[3] *= 0.6;
            }
            if show_cursor && p.focused && (r, c) == cursor {
                ink = theme.bg;
            }
            let x = ox + f32::from(c) * cw;
            let w = if cell.wide { 2.0 * cw } else { cw };
            let mut chars = cell.text.chars();
            match (chars.next(), chars.next()) {
                (Some(k), None) if crate::boxdraw::drawn(k) => boxes.push((k, [x, y, w, ch], ink)),
                _ => glyph_runs.push((x, y, cell.text.clone(), cell.bold, cell.italic, ink)),
            }
            if cell.underline {
                let t = px(grid.size / 13.0).max(1.0 / sc);
                let uy = px(y + grid.base + grid.size * 0.12);
                d.panel(rect_panel([x, uy, w, t], cell.fg));
            }
        }
        flush(d, &mut run, row.len() as u16);
    }
    if show_cursor {
        let (r, c) = cursor;
        let wide = snap.rows.get(usize::from(r)).and_then(|row| row.get(usize::from(c))).is_some_and(|cell| cell.wide);
        let x = ox + f32::from(c) * cw;
        let y = oy + f32::from(r) * ch;
        let w = cw * if wide { 2.0 } else { 1.0 };
        let mut panel = rect_panel([x, y, w, ch], theme.cursor);
        if !p.focused {
            // Unfocused: a hollow box.
            panel.fill_d[3] = 0.0;
            panel.fill_l[3] = 0.0;
            panel.edge_d = theme.cursor;
            panel.edge_l = theme.cursor;
            panel.shape = [0.0, 4.0, 1.0, 0.0];
        }
        d.panel(panel);
    }
    if let (Some(m), Some((r, a, b))) = (marked, marked_span) {
        // The preedit: on the terminal's ground, underlined, with a bar caret after it.
        let (x, y, w) = (ox + f32::from(a) * cw, oy + f32::from(r) * ch, f32::from(b - a) * cw);
        d.panel(rect_panel([x, y, w, ch], theme.bg));
        glyph_runs.push((x, y, m.to_owned(), false, false, theme.fg));
        let t = px(grid.size / 13.0).max(1.0 / sc);
        d.panel(rect_panel([x, px(y + grid.base + grid.size * 0.12), w, t], theme.fg));
        d.panel(rect_panel([x + w, y, (2.0 * t).max(1.0 / sc), ch], theme.cursor));
    }
    let thin =(sc * grid.size / 13.0).round().max(1.0) / sc;
    for (k, rect, ink) in boxes {
        crate::boxdraw::draw(d, k, rect, ink, sc, thin);
    }
    for l in lines.drain(..) {
        d.line(l);
    }
    let asz = cache.size as f32;
    for (x, y, s, bold, italic, ink) in glyph_runs {
        let base = y + grid.base;
        let shape = grid.shape(fonts, &s, bold, italic).clone();
        for (font, id, gx, gy) in shape.glyphs {
            let base_px = ((base - gy) * sc).round();
            let xp = (x + gx) * sc;
            let ix = xp.floor();
            let sub = (((xp - ix) * SUBPX as f32).floor() as u32).min(SUBPX - 1);
            let Some(e) = cache.get(fonts, font, id, sc, sub, upload) else { continue };
            if e.w == 0 {
                continue;
            }
            let (x0, y1p) = (ix + e.left as f32, base_px - e.top as f32);
            d.glyph(GlyphInst {
                rect: [x0 / sc, y1p / sc, e.w as f32 / sc, e.h as f32 / sc],
                uv: [e.x as f32 / asz, e.y as f32 / asz, (e.x + e.w) as f32 / asz, (e.y + e.h) as f32 / asz],
                col_d: ink,
                col_l: ink,
                anim: [-1.0e9, 30.0, 0.0, 1.0],
                kind: [f32::from(u8::from(e.colour)), 0.0, 0.0, 0.0],
                ..Default::default()
            });
        }
    }
    let _ = p.time;
}

fn blend(under: Rgba, over: Rgba) -> Rgba {
    let a = over[3];
    [under[0] * (1.0 - a) + over[0] * a, under[1] * (1.0 - a) + over[1] * a, under[2] * (1.0 - a) + over[2] * a, 1.0]
}

/// A plain square plate of colour `c`.
#[must_use]
pub fn rect_panel(rect: [f32; 4], c: Rgba) -> PanelInst {
    PanelInst { rect, fill_d: c, fill_l: c, shape: [0.0, 4.0, 0.0, 0.0], fx2: [0.0, 1.0, 0.0, 0.0], ..Default::default() }
}

// ---- input: keys, mouse, paste, selection units ----

use harness_vt::ghostty::key::{self as gkey, Key, KittyKeyFlags, OptionAsAlt};
pub use harness_vt::KeyAction;

/// The modifiers a key or click came with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub cmd: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// What input needs of the terminal, read in [`snapshot`] with the cells: its modes (cursor keys, kitty flags, mouse,
/// bracketed paste, alternate screen), the cursor in the active area, its size and how much history it holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Input {
    pub modes: harness_vt::Modes,
    /// Row and column.
    pub cursor: (u16, u16),
    /// Columns and rows.
    pub size: (u16, u16),
    pub history: usize,
}

/// One key event as the view saw it.
#[derive(Clone, Copy, Debug)]
pub struct KeyIn<'a> {
    /// The macOS virtual key code.
    pub code: u16,
    /// What it typed (Option as Alt: the key's own character, not Option's symbol). Control and private-use
    /// characters are dropped here.
    pub text: &'a str,
    /// The key's character with no modifiers at all.
    pub unshifted: Option<char>,
    pub mods: Mods,
    pub action: KeyAction,
    /// A dead key or input method is composing.
    pub composing: bool,
}

/// Ghostty's key encoder (legacy xterm and fixterms, or the kitty protocol when the program pushed flags), set from
/// [`Input`] before each key, so it never needs the parser.
pub struct Keys {
    enc: gkey::Encoder<'static>,
    ev: gkey::Event<'static>,
}

impl Default for Keys {
    fn default() -> Self {
        Self::new()
    }
}

impl Keys {
    #[must_use]
    pub fn new() -> Self {
        let mut enc = gkey::Encoder::new().expect("libghostty key encoder");
        // Option is Meta (iTerm2's and Ghostty's "Esc+"): ESC before the key's own character.
        enc.set_macos_option_as_alt(OptionAsAlt::True).set_alt_esc_prefix(true);
        Self { enc, ev: gkey::Event::new().expect("libghostty key event") }
    }

    /// The bytes `k` sends to a program in `modes`.
    pub fn encode(&mut self, k: &KeyIn<'_>, modes: &harness_vt::Modes) -> Vec<u8> {
        let text: String = k.text.chars().filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c)).collect();
        let m = k.mods;
        let mut gm = gkey::Mods::empty();
        gm.set(gkey::Mods::SHIFT, m.shift);
        gm.set(gkey::Mods::CTRL, m.ctrl);
        gm.set(gkey::Mods::ALT, m.alt);
        gm.set(gkey::Mods::SUPER, m.cmd);
        // Shift made the text (A, !): it is consumed, so the kitty protocol sends the text, not a shifted key.
        let consumed = if m.shift && !text.is_empty() { gkey::Mods::SHIFT } else { gkey::Mods::empty() };
        self.enc
            .set_cursor_key_application(modes.app_cursor)
            .set_keypad_key_application(modes.app_keypad)
            .set_kitty_flags(KittyKeyFlags::from_bits_truncate(modes.kitty_flags));
        self.ev
            .set_action(k.action)
            .set_key(key_of(k.code))
            .set_mods(gm)
            .set_consumed_mods(consumed)
            .set_composing(k.composing)
            .set_utf8((!text.is_empty()).then_some(text))
            .set_unshifted_codepoint(k.unshifted.unwrap_or('\0'));
        let mut out = Vec::new();
        let _ = self.enc.encode_to_vec(&self.ev, &mut out);
        out
    }

    /// An unmodified press of key `code` (arrows for option-click and the wheel).
    pub fn press(&mut self, code: u16, modes: &harness_vt::Modes) -> Vec<u8> {
        self.encode(&KeyIn { code, text: "", unshifted: None, mods: Mods::default(), action: KeyAction::Press, composing: false }, modes)
    }
}

/// Key codes the view sends straight to the encoder, around the input method: arrows, function and navigation keys.
#[must_use]
pub fn is_function_key(code: u16) -> bool {
    use Key as K;
    matches!(key_of(code), K::ArrowLeft | K::ArrowRight | K::ArrowUp | K::ArrowDown | K::Home | K::End | K::PageUp | K::PageDown | K::Delete | K::Insert)
        || matches!(code, 96..=101 | 103 | 105..=107 | 109 | 111 | 113 | 118 | 120 | 122 | 64 | 79 | 80 | 90)
}

pub const LEFT: u16 = 123;
pub const RIGHT: u16 = 124;
pub const DOWN: u16 = 125;
pub const UP: u16 = 126;

/// The physical key of a macOS virtual key code (ANSI and ISO layouts).
#[must_use]
pub fn key_of(code: u16) -> Key {
    use Key as K;
    const TABLE: [Key; 127] = {
        let mut t = [K::Unidentified; 127];
        let pairs: &[(usize, Key)] = &[
            (0, K::A), (1, K::S), (2, K::D), (3, K::F), (4, K::H), (5, K::G), (6, K::Z), (7, K::X), (8, K::C), (9, K::V),
            (10, K::IntlBackslash), (11, K::B), (12, K::Q), (13, K::W), (14, K::E), (15, K::R), (16, K::Y), (17, K::T),
            (18, K::Digit1), (19, K::Digit2), (20, K::Digit3), (21, K::Digit4), (22, K::Digit6), (23, K::Digit5),
            (24, K::Equal), (25, K::Digit9), (26, K::Digit7), (27, K::Minus), (28, K::Digit8), (29, K::Digit0),
            (30, K::BracketRight), (31, K::O), (32, K::U), (33, K::BracketLeft), (34, K::I), (35, K::P), (36, K::Enter),
            (37, K::L), (38, K::J), (39, K::Quote), (40, K::K), (41, K::Semicolon), (42, K::Backslash), (43, K::Comma),
            (44, K::Slash), (45, K::N), (46, K::M), (47, K::Period), (48, K::Tab), (49, K::Space), (50, K::Backquote),
            (51, K::Backspace), (53, K::Escape), (54, K::MetaRight), (55, K::MetaLeft), (56, K::ShiftLeft),
            (57, K::CapsLock), (58, K::AltLeft), (59, K::ControlLeft), (60, K::ShiftRight), (61, K::AltRight),
            (62, K::ControlRight), (63, K::Fn), (64, K::F17), (65, K::NumpadDecimal), (67, K::NumpadMultiply),
            (69, K::NumpadAdd), (71, K::NumLock), (72, K::AudioVolumeUp), (73, K::AudioVolumeDown),
            (74, K::AudioVolumeMute), (75, K::NumpadDivide), (76, K::NumpadEnter), (78, K::NumpadSubtract),
            (79, K::F18), (80, K::F19), (81, K::NumpadEqual), (82, K::Numpad0), (83, K::Numpad1), (84, K::Numpad2),
            (85, K::Numpad3), (86, K::Numpad4), (87, K::Numpad5), (88, K::Numpad6), (89, K::Numpad7), (90, K::F20),
            (91, K::Numpad8), (92, K::Numpad9), (93, K::IntlYen), (94, K::IntlRo), (95, K::NumpadComma), (96, K::F5),
            (97, K::F6), (98, K::F7), (99, K::F3), (100, K::F8), (101, K::F9), (103, K::F11), (105, K::F13),
            (106, K::F16), (107, K::F14), (109, K::F10), (110, K::ContextMenu), (111, K::F12), (113, K::F15),
            (114, K::Insert), (115, K::Home), (116, K::PageUp), (117, K::Delete), (118, K::F4), (119, K::End),
            (120, K::F2), (121, K::PageDown), (122, K::F1), (123, K::ArrowLeft), (124, K::ArrowRight),
            (125, K::ArrowDown), (126, K::ArrowUp),
        ];
        let mut i = 0;
        while i < pairs.len() {
            t[pairs[i].0] = pairs[i].1;
            i += 1;
        }
        t
    };
    TABLE.get(usize::from(code)).copied().unwrap_or(K::Unidentified)
}

/// Natural text editing (iTerm2's and Ghostty's preset): what option/command with an arrow, backspace, forward delete
/// or z sends, whatever the program's keyboard mode. `ch` is the key's character without modifiers.
#[must_use]
pub fn natural_edit(code: u16, ch: Option<char>, m: Mods) -> Option<&'static [u8]> {
    if m.ctrl || m.shift {
        return None;
    }
    match (m.cmd, m.alt) {
        (false, true) => match code {
            LEFT => Some(b"\x1bb"),
            RIGHT => Some(b"\x1bf"),
            51 => Some(b"\x1b\x7f"),
            117 => Some(b"\x1bd"),
            _ => None,
        },
        (true, false) => match code {
            LEFT => Some(b"\x01"),
            RIGHT => Some(b"\x05"),
            51 => Some(b"\x15"),
            _ if ch.is_some_and(|c| c.eq_ignore_ascii_case(&'z')) => Some(b"\x1f"),
            _ => None,
        },
        _ => None,
    }
}

/// `text` pasted: bracketed when the program asked for it, newlines as carriage returns. A bracketed paste cannot end
/// its own bracket early.
#[must_use]
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed { format!("\x1b[200~{}\x1b[201~", body.replace("\x1b[201~", "")).into_bytes() } else { body.into_bytes() }
}

/// A mouse button, or none (motion with nothing held), or a wheel step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    None,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAct {
    Press,
    Release,
    Motion,
}

/// The report a mouse event makes in the program's mouse mode (`CSI ? 1000/1002/1003 h`, encoded per 1006 SGR, 1015
/// urxvt, 1005 UTF-8 or the X10 default) at cell `(col, row)` of the viewport; empty when that mode does not report it.
#[must_use]
pub fn mouse(b: Button, act: MouseAct, col: u16, row: u16, m: Mods, modes: &harness_vt::Modes) -> Vec<u8> {
    use harness_vt::{MouseFormat as F, MouseTracking as T};
    let wheel = matches!(b, Button::WheelUp | Button::WheelDown);
    let wanted = match (modes.mouse, act) {
        (T::None, _) => false,
        (T::X10, a) => a == MouseAct::Press,
        (_, MouseAct::Press) => true,
        (_, MouseAct::Release) => !wheel,
        (T::Button, MouseAct::Motion) => b != Button::None && !wheel,
        (T::Any, MouseAct::Motion) => !wheel,
        (T::Normal, MouseAct::Motion) => false,
    };
    if !wanted {
        return Vec::new();
    }
    let mut code: u32 = match b {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
        Button::None => 3,
        Button::WheelUp => 64,
        Button::WheelDown => 65,
    };
    if act == MouseAct::Motion {
        code += 32;
    }
    if modes.mouse != T::X10 {
        code += 4 * u32::from(m.shift) + 8 * u32::from(m.alt) + 16 * u32::from(m.ctrl);
    }
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    if matches!(modes.mouse_format, F::Sgr | F::SgrPixels) {
        return format!("\x1b[<{code};{x};{y}{}", if act == MouseAct::Release { 'm' } else { 'M' }).into_bytes();
    }
    // The older formats have no release button: a release is button 3.
    if act == MouseAct::Release {
        code = (code & !3) | 3;
    }
    match modes.mouse_format {
        F::Urxvt => format!("\x1b[{};{x};{y}M", code + 32).into_bytes(),
        F::Utf8 => {
            let mut s = String::from("\x1b[M");
            for v in [code + 32, x + 32, y + 32] {
                s.push(char::from_u32(v.min(2015)).unwrap_or(' '));
            }
            s.into_bytes()
        }
        _ => {
            let byte = |v: u32| u8::try_from(v.min(255)).unwrap_or(255);
            vec![0x1b, b'[', b'M', byte(code + 32), byte(x + 32), byte(y + 32)]
        }
    }
}

/// Characters that end a word for double-click selection (Ghostty's default `selection-word-chars`).
const WORD_STOPS: &str = " \t'\"\u{2502}`|:;,()[]{}<>$";

/// The columns of the word under column `col` of `cells` (each cell's text; "" a blank or a wide character's right
/// half), inclusive; a stop character or blank is a word of its own.
#[must_use]
pub fn word_span(cells: &[&str], col: usize) -> (usize, usize) {
    if cells.is_empty() {
        return (0, 0);
    }
    // A wide character's right half belongs to its left half.
    let part = |i: usize| i > 0 && cells[i].is_empty() && wide_char(cells[i - 1]);
    let stop = |i: usize| !part(i) && (cells[i].is_empty() || cells[i].chars().all(|c| WORD_STOPS.contains(c)));
    let col = col.min(cells.len() - 1);
    let col = if part(col) { col - 1 } else { col };
    if stop(col) {
        return (col, col);
    }
    let mut a = col;
    while a > 0 && !stop(a - 1) {
        a -= 1;
    }
    let mut b = col;
    while b + 1 < cells.len() && !stop(b + 1) {
        b += 1;
    }
    (a, b)
}

/// Whether `s` (one grapheme) takes two columns: the East Asian wide and fullwidth ranges and emoji.
#[must_use]
pub fn wide_char(s: &str) -> bool {
    s.chars().next().is_some_and(|c| {
        matches!(c as u32,
            0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD)
    })
}

/// How many columns `s` takes on the grid.
#[must_use]
pub fn columns(s: &str) -> usize {
    s.chars().map(|c| if wide_char(c.encode_utf8(&mut [0; 4])) { 2 } else { 1 }).sum()
}

/// The text of the selection from screen row/column `s` to `e` (inclusive, reading order; negative rows are history),
/// read a viewport at a time so it reaches into the scrollback. Rows join with newlines except where one soft-wraps;
/// trailing blanks are trimmed. Leaves the viewport at the live screen.
pub fn selection_text(term: &mut harness_vt::Term, (s, e): ((i64, u16), (i64, u16))) -> String {
    let rows = i64::from(term.size().1);
    let hist = i64::try_from(term.history()).unwrap_or(0);
    let (r0, r1) = (s.0.max(-hist), e.0.min(rows - 1));
    let mut out = String::new();
    let mut at = r0;
    while at <= r1 {
        term.set_scrollback(usize::try_from(-at).unwrap_or(0));
        let n = i64::try_from(term.scrollback()).unwrap_or(0);
        let (vrows, _) = term.rows();
        let (v0, v1) = (at + n, (r1 + n).min(rows - 1));
        if v0 < 0 || v0 > v1 {
            break;
        }
        for v in v0..=v1 {
            let Some(row) = vrows.get(usize::try_from(v).unwrap_or(0)) else { break };
            let sr = v - n;
            let from = if sr == s.0 { usize::from(s.1) } else { 0 };
            let to = if sr == e.0 { (usize::from(e.1) + 1).min(row.cells.len()) } else { row.cells.len() };
            let mut line = String::new();
            for c in row.cells.get(from..to.max(from)).unwrap_or_default() {
                if !c.spacer {
                    line.push_str(if c.text.is_empty() { " " } else { &c.text });
                }
            }
            if sr != r1 && row.wrapped {
                out.push_str(&line);
            } else {
                out.push_str(line.trim_end());
                if sr != r1 {
                    out.push('\n');
                }
            }
        }
        at = v1 - n + 1;
    }
    term.set_scrollback(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_vt::{Modes, MouseFormat, MouseTracking};

    fn key(keys: &mut Keys, modes: &Modes, code: u16, text: &str, unshifted: char, mods: Mods) -> Vec<u8> {
        keys.encode(&KeyIn { code, text, unshifted: Some(unshifted), mods, action: KeyAction::Press, composing: false }, modes)
    }

    const NONE: Mods = Mods { cmd: false, shift: false, ctrl: false, alt: false };
    const CTRL: Mods = Mods { ctrl: true, ..NONE };
    const ALT: Mods = Mods { alt: true, ..NONE };
    const SHIFT: Mods = Mods { shift: true, ..NONE };

    #[test]
    fn legacy_keys() {
        let mut k = Keys::new();
        let m = Modes::default();
        assert_eq!(key(&mut k, &m, 0, "a", 'a', NONE), b"a");
        assert_eq!(key(&mut k, &m, 0, "A", 'a', SHIFT), b"A");
        assert_eq!(key(&mut k, &m, 8, "\u{3}", 'c', CTRL), b"\x03");
        // Option as Meta: the view passes the key's own character.
        assert_eq!(key(&mut k, &m, 11, "b", 'b', ALT), b"\x1bb");
        assert_eq!(key(&mut k, &m, 36, "\r", '\r', NONE), b"\r");
        assert_eq!(key(&mut k, &m, 51, "\u{7f}", '\u{7f}', NONE), b"\x7f");
        assert_eq!(key(&mut k, &m, 53, "\u{1b}", '\u{1b}', NONE), b"\x1b");
        assert_eq!(key(&mut k, &m, 48, "\t", '\t', SHIFT), b"\x1b[Z");
        assert_eq!(k.press(UP, &m), b"\x1b[A");
        assert_eq!(k.press(UP, &Modes { app_cursor: true, ..m }), b"\x1bOA");
        assert_eq!(key(&mut k, &m, LEFT, "\u{F702}", '\u{F702}', CTRL), b"\x1b[1;5D");
        assert_eq!(key(&mut k, &m, 122, "\u{F704}", '\u{F704}', NONE), b"\x1bOP");
        // Command never types text on macOS.
        assert_eq!(key(&mut k, &m, 0, "a", 'a', Mods { cmd: true, ..NONE }), b"");
    }

    #[test]
    fn kitty_keys() {
        let mut k = Keys::new();
        // CSI > 1 u: disambiguate.
        let m = Modes { kitty_flags: 1, ..Modes::default() };
        assert_eq!(key(&mut k, &m, 0, "a", 'a', NONE), b"a");
        assert_eq!(key(&mut k, &m, 0, "A", 'a', SHIFT), b"A");
        assert_eq!(key(&mut k, &m, 53, "\u{1b}", '\u{1b}', NONE), b"\x1b[27u");
        assert_eq!(key(&mut k, &m, 8, "", 'c', CTRL), b"\x1b[99;5u");
        assert_eq!(key(&mut k, &m, 11, "b", 'b', ALT), b"\x1b[98;3u");
        assert_eq!(key(&mut k, &m, 36, "\r", '\r', NONE), b"\r");
        assert_eq!(key(&mut k, &m, 36, "\r", '\r', SHIFT), b"\x1b[13;2u");
        // CSI > 8 u: every key an escape, text included.
        let all = Modes { kitty_flags: 8, ..Modes::default() };
        assert_eq!(key(&mut k, &all, 0, "a", 'a', NONE), b"\x1b[97u");
        assert_eq!(key(&mut k, &all, 36, "\r", '\r', NONE), b"\x1b[13u");
        // Releases only with flag 2 (report event types).
        let rel = KeyIn { code: 0, text: "", unshifted: Some('a'), mods: NONE, action: KeyAction::Release, composing: false };
        assert_eq!(k.encode(&rel, &m), b"");
        assert_eq!(k.encode(&rel, &Modes { kitty_flags: 1 | 2 | 8, ..m }), b"\x1b[97;1:3u");
    }

    #[test]
    fn natural_editing() {
        let cmd = Mods { cmd: true, ..NONE };
        assert_eq!(natural_edit(LEFT, None, ALT), Some(&b"\x1bb"[..]));
        assert_eq!(natural_edit(RIGHT, None, ALT), Some(&b"\x1bf"[..]));
        assert_eq!(natural_edit(LEFT, None, cmd), Some(&b"\x01"[..]));
        assert_eq!(natural_edit(RIGHT, None, cmd), Some(&b"\x05"[..]));
        assert_eq!(natural_edit(51, None, ALT), Some(&b"\x1b\x7f"[..]));
        assert_eq!(natural_edit(51, None, cmd), Some(&b"\x15"[..]));
        assert_eq!(natural_edit(117, None, ALT), Some(&b"\x1bd"[..]));
        assert_eq!(natural_edit(6, Some('z'), cmd), Some(&b"\x1f"[..]));
        assert_eq!(natural_edit(LEFT, None, Mods { shift: true, ..ALT }), None);
        assert_eq!(natural_edit(LEFT, None, NONE), None);
    }

    #[test]
    fn mouse_reports() {
        let sgr = Modes { mouse: MouseTracking::Normal, mouse_format: MouseFormat::Sgr, ..Modes::default() };
        assert_eq!(mouse(Button::Left, MouseAct::Press, 4, 2, NONE, &sgr), b"\x1b[<0;5;3M");
        assert_eq!(mouse(Button::Left, MouseAct::Release, 4, 2, NONE, &sgr), b"\x1b[<0;5;3m");
        assert_eq!(mouse(Button::Right, MouseAct::Press, 0, 0, CTRL, &sgr), b"\x1b[<18;1;1M");
        assert_eq!(mouse(Button::WheelUp, MouseAct::Press, 0, 0, NONE, &sgr), b"\x1b[<64;1;1M");
        assert_eq!(mouse(Button::Left, MouseAct::Motion, 1, 1, NONE, &sgr), b"");
        let drag = Modes { mouse: MouseTracking::Button, ..sgr };
        assert_eq!(mouse(Button::Left, MouseAct::Motion, 1, 1, NONE, &drag), b"\x1b[<32;2;2M");
        assert_eq!(mouse(Button::None, MouseAct::Motion, 1, 1, NONE, &drag), b"");
        let any = Modes { mouse: MouseTracking::Any, ..sgr };
        assert_eq!(mouse(Button::None, MouseAct::Motion, 1, 1, NONE, &any), b"\x1b[<35;2;2M");
        let x10 = Modes { mouse: MouseTracking::Normal, ..Modes::default() };
        assert_eq!(mouse(Button::Left, MouseAct::Press, 0, 0, NONE, &x10), b"\x1b[M !!");
        assert_eq!(mouse(Button::Left, MouseAct::Release, 0, 0, NONE, &x10), b"\x1b[M#!!");
        assert_eq!(mouse(Button::Left, MouseAct::Press, 0, 0, NONE, &Modes::default()), b"");
    }

    #[test]
    fn words() {
        let row = ["l", "s", " ", "/", "t", "m", "p", "/", "a", ".", "t", "x", "t", " ", "(", "x", ")"];
        assert_eq!(word_span(&row, 0), (0, 1));
        assert_eq!(word_span(&row, 5), (3, 12));
        assert_eq!(word_span(&row, 2), (2, 2));
        assert_eq!(word_span(&row, 15), (15, 15));
        let wide = ["a", "\u{4e2d}", "", "b", " "];
        assert_eq!(word_span(&wide, 2), (0, 3));
    }

    #[test]
    fn pastes() {
        assert_eq!(paste("a\nb", false), b"a\rb");
        assert_eq!(paste("a\x1b[201~b", true), b"\x1b[200~ab\x1b[201~");
    }
}

