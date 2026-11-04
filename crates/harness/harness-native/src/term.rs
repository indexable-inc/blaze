//! The terminal: a libghostty screen (harness-vt) drawn as panels (cell backgrounds, selection, cursor), lines (underlines) and atlas glyphs,
//! on the same pipeline and glyph cache as the rest of the UI; and the keyboard encoded the way xterm sends it.
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
    Snapshot { rows: out, cursor: (look.y, look.x), cursor_shown: cursor.is_some_and(|c| c.visible), cursor_look: look }
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
    let show_cursor = snap.cursor_shown && p.scroll == 0;
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
            if cell.cont || cell.text.is_empty() || cell.text == " " {
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
    let thin = (sc * grid.size / 13.0).round().max(1.0) / sc;
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

/// The modifiers a key came with.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl Mods {
    /// xterm's modifier parameter: 1 + shift + 2 alt + 4 ctrl.
    fn param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }

    fn any(self) -> bool {
        self.shift || self.ctrl || self.alt
    }
}

/// The bytes xterm sends for a key: `code` the macOS virtual key code, `chars` the characters it typed and `bare` the ones
/// without modifiers. Option is Meta (ESC before the key).
#[must_use]
pub fn encode_key(code: u16, chars: &str, bare: &str, m: Mods, app_cursor: bool) -> Vec<u8> {
    let csi = |n: &str, fin: char| -> Vec<u8> {
        if m.any() { format!("\x1b[{};{}{fin}", if n.is_empty() { "1" } else { n }, m.param()).into_bytes() } else { format!("\x1b[{n}{fin}").into_bytes() }
    };
    let tilde = |n: u8| -> Vec<u8> { if m.any() { format!("\x1b[{n};{}~", m.param()).into_bytes() } else { format!("\x1b[{n}~").into_bytes() } };
    let arrow = |fin: char| -> Vec<u8> { if !m.any() && app_cursor { format!("\x1bO{fin}").into_bytes() } else { csi("", fin) } };
    let meta = |b: Vec<u8>| if m.alt { [vec![0x1b], b].concat() } else { b };
    match code {
        36 | 76 => {
            if m.shift {
                b"\x1b\r".to_vec()
            } else {
                meta(b"\r".to_vec())
            }
        }
        48 => {
            if m.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        51 => meta(if m.ctrl { vec![0x08] } else { vec![0x7f] }),
        53 => b"\x1b".to_vec(),
        123 => arrow('D'),
        124 => arrow('C'),
        125 => arrow('B'),
        126 => arrow('A'),
        115 => csi("", 'H'),
        119 => csi("", 'F'),
        116 => tilde(5),
        121 => tilde(6),
        117 => tilde(3),
        122 => if m.any() { csi("", 'P') } else { b"\x1bOP".to_vec() },
        120 => if m.any() { csi("", 'Q') } else { b"\x1bOQ".to_vec() },
        99 => if m.any() { csi("", 'R') } else { b"\x1bOR".to_vec() },
        118 => if m.any() { csi("", 'S') } else { b"\x1bOS".to_vec() },
        96 => tilde(15),
        97 => tilde(17),
        98 => tilde(18),
        100 => tilde(19),
        101 => tilde(20),
        109 => tilde(21),
        103 => tilde(23),
        111 => tilde(24),
        _ => {
            if m.ctrl {
                let c = bare.chars().next().unwrap_or('\0').to_ascii_lowercase();
                let b = match c {
                    'a'..='z' => Some(c as u8 - b'a' + 1),
                    ' ' | '2' | '@' => Some(0),
                    '[' | '3' => Some(0x1b),
                    '\\' | '4' => Some(0x1c),
                    ']' | '5' => Some(0x1d),
                    '6' | '^' => Some(0x1e),
                    '-' | '_' | '7' | '/' => Some(0x1f),
                    _ => None,
                };
                if let Some(b) = b {
                    return meta(vec![b]);
                }
            }
            // Option as Meta: the key's own character after ESC, not the symbol Option makes.
            if m.alt {
                return meta(bare.as_bytes().to_vec());
            }
            chars.as_bytes().to_vec()
        }
    }
}

/// `text` pasted: bracketed when the program asked for it, newlines as carriage returns.
#[must_use]
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed { format!("\x1b[200~{body}\x1b[201~").into_bytes() } else { body.into_bytes() }
}
