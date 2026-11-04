//! The terminal: a libghostty screen (harness-vt) drawn as panels (cell backgrounds, selection, cursor) and atlas glyphs,
//! on the same pipeline and glyph cache as the rest of the UI; and the keyboard encoded the way xterm sends it.
//!
//! Text is set the way Ghostty sets it: cells sized from the primary face's metrics in whole device pixels; each row cut
//! into runs of one style and colour, shaped by HarfRust with ligatures; what the primary face lacks taken from Symbols
//! Nerd Font Mono (fitted to its cells by Ghostty's constraint table), Apple Color Emoji or CoreText's cascade.
use crate::draw::Draw;
use crate::pack::{GlyphInst, PanelInst};
use crate::text::nerd_rules::{Constraint, Metrics, Rect as GRect};
use crate::text::{AtlasGlyph, FontId, Fonts, GlyphCache, RunGlyph, SUBPX};
use std::collections::HashMap;
use std::rc::Rc;

/// A colour as the shaders take it (straight RGBA, sRGB).
pub type Rgba = [f32; 4];

/// The grid's look: default ink and ground, the 16 ANSI colours, cursor, the default cursor's accent and selection.
pub struct Theme {
    pub fg: Rgba,
    pub bg: Rgba,
    pub ansi: [Rgba; 16],
    pub cursor: Rgba,
    /// The default cursor (a thin bar) when the program has not set one.
    pub accent: Rgba,
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
    accent: hex(0x8b9cff),
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

/// How a cell's grapheme is drawn: which font, and how it is fitted to its cells.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pick {
    /// The primary face, shaped with the rest of its row run (ligatures).
    Primary,
    /// Symbols Nerd Font Mono, constrained by Ghostty's Nerd Font table.
    Nerd(FontId, Constraint),
    /// A colour emoji, covering its cells.
    Emoji(FontId),
    /// CoreText's fallback for the primary face (CJK, symbols), centred in its cells.
    Fallback(FontId),
}

/// The cursor's blink clock: where the cursor was, a hash of its row, and when either last changed (typing), app
/// seconds. The blink waits [`BLINK_PAUSE`] after that.
#[derive(Clone, Copy, Debug, Default)]
struct Blink {
    at: (u16, u16),
    row: u64,
    since: f32,
}

/// Seconds the cursor stays solid after it moved or its row changed, and a full blink's period.
const BLINK_PAUSE: f32 = 0.5;
const BLINK_PERIOD: f32 = 1.2;

/// The cell grid's metrics at a font size and display scale (Ghostty's: a cell is the primary face's advance by its
/// ascent + descent + line gap, each rounded to whole device pixels, the baseline on a pixel), and the runs shaped so far.
/// Rows and columns tile with no seams: every cell starts on a whole device pixel.
pub struct Grid {
    pub size: f32,
    pub scale: f32,
    /// A cell's width and height and the baseline below its top, points (each a whole number of device pixels).
    pub cw: f32,
    pub ch: f32,
    pub base: f32,
    /// The same, device pixels.
    cwp: i32,
    chp: i32,
    basep: i32,
    /// The underline's top below the cell's top and its thickness, device pixels (Ghostty's, from the `post` table).
    ulp: i32,
    ult: i32,
    /// What glyph constraints measure against, device pixels.
    m: Metrics,
    /// The primary face by style (bit 0 bold, bit 1 italic), Nerd Font symbols and colour emoji.
    faces: [FontId; 4],
    nerd: Option<FontId>,
    emoji: Option<FontId>,
    /// Each grapheme's font, by a hash of it and its style.
    picks: HashMap<u64, Pick>,
    /// Shaped runs by a hash of their font and cells.
    runs: HashMap<u64, Rc<[RunGlyph]>>,
    blink: Blink,
    /// The cursor is blinking: the window has to keep drawing.
    pub animating: bool,
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

fn fnv_mix(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

const FNV0: u64 = 0xcbf2_9ce4_8422_2325;

impl Grid {
    pub fn new(fonts: &mut Fonts, size: f32, scale: f32) -> Self {
        let faces = [(false, false), (true, false), (false, true), (true, true)].map(|(b, i)| fonts.term_font(size, b, i));
        let fm = fonts.term_metrics(faces[0]);
        let s = f64::from(scale);
        let face_w = f64::from(fm.advance) * s;
        let face_h = f64::from(fm.ascent + fm.descent + fm.leading) * s;
        let (cwp, chp) = (face_w.round().max(1.0), face_h.round().max(1.0));
        // Ghostty's Metrics.calc: half the line gap above and below; the face centred in the rounded cell, its
        // baseline (from the cell's bottom) rounded to a pixel.
        let face_base = (f64::from(fm.leading) / 2.0 + f64::from(fm.descent)) * s;
        let cell_base = (face_base - (chp - face_h) / 2.0).round();
        let cap = f64::from(fm.cap) * s;
        let m = Metrics { cell_w: cwp, cell_h: chp, face_w, face_h, face_y: cell_base - face_base, icon_h: face_h, icon_h1: (2.0 * cap + face_h) / 3.0 };
        let (cwp, chp) = (cwp as i32, chp as i32);
        let basep = chp - cell_base as i32;
        // Ghostty: the underline's position rounded from the baseline, its thickness at least a pixel, rounded up.
        let ul_pos = fm.underline_pos.map_or(-f64::from(size) * 0.12 * s, |v| f64::from(v) * s);
        let ult = fm.underline_thick.map_or((f64::from(size) / 13.0 * s).round(), |v| (f64::from(v) * s).ceil()).max(1.0) as i32;
        let ulp = (f64::from(basep) - ul_pos).round() as i32;
        Self {
            size,
            scale,
            cw: cwp as f32 / scale,
            ch: chp as f32 / scale,
            base: basep as f32 / scale,
            cwp,
            chp,
            basep,
            ulp,
            ult,
            m,
            faces,
            nerd: fonts.nerd_font(size),
            emoji: fonts.emoji_font(size),
            picks: HashMap::new(),
            runs: HashMap::new(),
            blink: Blink::default(),
            animating: false,
        }
    }

    /// Columns and rows that fit in `w` x `h` points.
    #[must_use]
    pub fn fit(&self, w: f32, h: f32) -> (u16, u16) {
        (((w / self.cw).floor() as u16).max(2), ((h / self.ch).floor() as u16).max(1))
    }

    /// The font chain for `text` in `style`: primary, Nerd symbols, Apple Color Emoji for emoji presentation, then
    /// CoreText's own fallback (a colour font from it drawn as an emoji).
    fn pick(&mut self, fonts: &mut Fonts, text: &str, style: u8) -> Pick {
        let key = fnv_mix(fnv_mix(FNV0, &[style]), text.as_bytes());
        if let Some(p) = self.picks.get(&key) {
            return *p;
        }
        let primary = self.faces[usize::from(style)];
        let emoji = crate::text::emoji_presentation(text);
        let mut chars = text.chars();
        let single = match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        };
        let p = if !emoji && fonts.covers(primary, text) {
            Pick::Primary
        } else if let (Some(n), Some(c), false) = (self.nerd, single, emoji)
            && fonts.covers(n, text)
        {
            Pick::Nerd(n, crate::text::nerd_rules::nerd(u32::from(c)).unwrap_or(Constraint::FIT))
        } else if let Some(e) = self.emoji.filter(|&e| emoji && fonts.covers(e, text)) {
            Pick::Emoji(e)
        } else {
            match fonts.fallback_font(primary, text) {
                Some(f) if fonts.is_colour(f) => Pick::Emoji(f),
                Some(f) if f != primary => Pick::Fallback(f),
                _ => Pick::Primary,
            }
        };
        if self.picks.len() > 16384 {
            self.picks.clear();
        }
        self.picks.insert(key, p);
        p
    }

    /// `cells` shaped in `font`, from the cache when it has them (by content hash).
    fn shaped(&mut self, fonts: &mut Fonts, font: FontId, cells: &[&str]) -> Rc<[RunGlyph]> {
        let mut key = fnv_mix(FNV0, &font.to_le_bytes());
        for c in cells {
            key = fnv_mix(fnv_mix(key, c.as_bytes()), &[0xff]);
        }
        if let Some(r) = self.runs.get(&key) {
            return r.clone();
        }
        if self.runs.len() > 8192 {
            self.runs.clear();
        }
        let r: Rc<[RunGlyph]> = fonts.shape_cells(font, cells).into();
        self.runs.insert(key, r.clone());
        r
    }

    /// The cursor's opacity this frame (1 solid, 0 hidden): a smooth fade on the app clock when it blinks and the pane
    /// is focused, held solid for [`BLINK_PAUSE`] after the cursor moved or its row changed.
    fn cursor_alpha(&mut self, snap: &Snapshot, p: &Paint<'_>, blinking: bool, shown: bool) -> f32 {
        let row = snap.rows.get(usize::from(snap.cursor.0)).map_or(FNV0, |r| r.iter().fold(FNV0, |h, c| fnv_mix(fnv_mix(h, c.text.as_bytes()), &[0])));
        if (snap.cursor, row) != (self.blink.at, self.blink.row) {
            self.blink = Blink { at: snap.cursor, row, since: p.time };
        }
        self.animating = blinking && p.focused && shown;
        if !self.animating {
            return 1.0;
        }
        let t = p.time - self.blink.since - BLINK_PAUSE;
        if t <= 0.0 {
            return 1.0;
        }
        let v = (t * std::f32::consts::TAU / BLINK_PERIOD).cos();
        let k = ((v + 0.5) / 1.0).clamp(0.0, 1.0);
        k * k * (3.0 - 2.0 * k)
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

impl Cell {
    fn blank(&self) -> bool {
        self.text.is_empty() || self.text == " "
    }

    fn style(&self) -> u8 {
        u8::from(self.bold) | u8::from(self.italic) << 1
    }

    /// A box-drawing or block character drawn procedurally ([`crate::boxdraw`]).
    fn procedural(&self) -> Option<char> {
        let mut chars = self.text.chars();
        match (chars.next(), chars.next()) {
            (Some(k), None) if crate::boxdraw::drawn(k) => Some(k),
            _ => None,
        }
    }
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

/// Glyphs to draw once every plate is down: a shaped run, a glyph constrained into its cells, or a run centred in them.
enum Job {
    /// A run of the primary face from cell `x` (device pixels, the run's left), each glyph at its cluster's cell.
    Run { x: i32, base: i32, font: FontId, glyphs: Rc<[RunGlyph]>, ink: Rgba },
    /// One glyph fitted to the cells from `x` with Ghostty's constraint, `width` cells free for it.
    Placed { x: i32, bottom: i32, font: FontId, glyph: u16, con: Constraint, width: u8, ink: Rgba },
    /// A fallback face's glyphs centred in `span` pixels from `x`.
    Centred { x: i32, base: i32, span: i32, font: FontId, glyphs: Rc<[RunGlyph]>, ink: Rgba },
}

/// Draw `snap` at `p.origin`: cell grounds, underlines, the cursor, procedural block and box glyphs, then the text: row
/// runs of one style and colour shaped with ligatures, Nerd icons and emoji fitted to their cells, fallback glyphs centred.
pub fn draw(d: &mut Draw, grid: &mut Grid, fonts: &mut Fonts, cache: &mut GlyphCache, snap: &Snapshot, p: &Paint<'_>, upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8])) {
    let sc = p.scale;
    let pt = |v: i32| v as f32 / sc;
    // Every cell sits on a whole device pixel: the origin is snapped, cells are whole pixels.
    let (ox, oy) = ((p.origin.0 * sc).round() as i32, (p.origin.1 * sc).round() as i32);
    let (cw, ch) = (grid.cwp, grid.chp);
    let theme = p.theme;
    let cursor = snap.cursor;
    let show_cursor = snap.cursor_shown && p.scroll == 0;
    let look = snap.cursor_look;
    // The program's DECSCUSR shape and blink, else the default: a thin blinking bar in the accent colour.
    let (shape, blinking, cursor_ink) = if look.explicit { (look.shape, look.blinking, theme.cursor) } else { (harness_vt::CursorShape::Bar, true, theme.accent) };
    let alpha = grid.cursor_alpha(snap, p, blinking, show_cursor);
    let inverts = show_cursor && p.focused && shape == harness_vt::CursorShape::Block;
    let mut boxes: Vec<(char, [f32; 4], Rgba)> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    let ink_at = |cell: &Cell, r: u16, c: u16| {
        let mut ink = cell.fg;
        if cell.dim {
            ink[3] *= 0.6;
        }
        if inverts && (r, c) == cursor {
            // The glyph under a block cursor in the ground's colour, as solid as the cursor is.
            for k in 0..3 {
                ink[k] += (theme.bg[k] - ink[k]) * alpha;
            }
        }
        ink
    };
    for (r, row) in snap.rows.iter().enumerate() {
        let r = r as u16;
        let y = oy + i32::from(r) * ch;
        let srow = i64::from(r) - p.scroll as i64;
        // Backgrounds: runs of one colour along the row, one panel each.
        let mut run: Option<(u16, Rgba)> = None;
        let flush = |d: &mut Draw, run: &mut Option<(u16, Rgba)>, end: u16| {
            if let Some((start, c)) = run.take() {
                let x0 = ox + i32::from(start) * cw;
                d.panel(rect_panel([pt(x0), pt(y), pt(i32::from(end - start) * cw), pt(ch)], c));
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
            if cell.underline && !cell.cont {
                let w = if cell.wide { 2 * cw } else { cw };
                d.panel(rect_panel([pt(ox + i32::from(c) * cw), pt(y + grid.ulp), pt(w), pt(grid.ult)], cell.fg));
            }
        }
        flush(d, &mut run, row.len() as u16);

        // Text: runs of one style and ink, broken at the cursor, at wide cells and at anything not in the primary face.
        let mut c = 0usize;
        while c < row.len() {
            let cell = &row[c];
            let cu = c as u16;
            if cell.cont || cell.blank() {
                c += 1;
                continue;
            }
            let x = ox + c as i32 * cw;
            let w = if cell.wide { 2 * cw } else { cw };
            let ink = ink_at(cell, r, cu);
            if let Some(k) = cell.procedural() {
                boxes.push((k, [pt(x), pt(y), pt(w), pt(ch)], ink));
                c += 1;
                continue;
            }
            let style = cell.style();
            let at_cursor = show_cursor && (r, cu) == cursor;
            match grid.pick(fonts, &cell.text, style) {
                Pick::Primary => {
                    let start = c;
                    let mut texts = vec![cell.text.as_str()];
                    c += 1;
                    if !cell.wide && !at_cursor {
                        while c < row.len() {
                            let n = &row[c];
                            let nu = c as u16;
                            if n.cont || n.wide || (show_cursor && (r, nu) == cursor) || n.procedural().is_some() {
                                break;
                            }
                            if !n.blank() && (n.style() != style || ink_at(n, r, nu) != ink || grid.pick(fonts, &n.text, style) != Pick::Primary) {
                                break;
                            }
                            texts.push(if n.blank() { " " } else { n.text.as_str() });
                            c += 1;
                        }
                    }
                    while texts.last() == Some(&" ") {
                        texts.pop();
                    }
                    let font = grid.faces[usize::from(style)];
                    let glyphs = grid.shaped(fonts, font, &texts);
                    jobs.push(Job::Run { x: ox + start as i32 * cw, base: y + grid.basep, font, glyphs, ink });
                }
                Pick::Nerd(font, con) => {
                    // Two cells free when the next one is blank (Ghostty's constraint width).
                    let width = if row.get(c + 1).is_some_and(|n| n.blank() && !n.cont) { 2 } else { 1 };
                    if let Some(g) = grid.shaped(fonts, font, &[cell.text.as_str()]).first() {
                        jobs.push(Job::Placed { x, bottom: y + ch, font, glyph: g.id, con, width, ink });
                    }
                    c += 1;
                }
                Pick::Emoji(font) => {
                    // CoreText picks the emoji's glyph: Apple Color Emoji joins flags and ZWJ sequences there.
                    if let Some(glyph) = fonts.ct_glyph(font, &cell.text) {
                        jobs.push(Job::Placed { x, bottom: y + ch, font, glyph, con: Constraint::EMOJI, width: if cell.wide { 2 } else { 1 }, ink });
                    }
                    c += 1;
                }
                Pick::Fallback(font) => {
                    let glyphs = grid.shaped(fonts, font, &[cell.text.as_str()]);
                    jobs.push(Job::Centred { x, base: y + grid.basep, span: w, font, glyphs, ink });
                    c += 1;
                }
            }
        }
    }
    if show_cursor && alpha > 0.0 {
        let (r, c) = cursor;
        let wide = snap.rows.get(usize::from(r)).and_then(|row| row.get(usize::from(c))).is_some_and(|cell| cell.wide);
        let x = ox + i32::from(c) * cw;
        let y = oy + i32::from(r) * ch;
        let w = cw * if wide { 2 } else { 1 };
        // A bar or underline about 1.5 points thick, whole pixels.
        let thick = ((1.5 * sc).round() as i32).max(1);
        let hair = (sc.round() as i32).max(1);
        let mut ink = cursor_ink;
        ink[3] *= alpha;
        if !p.focused && shape != harness_vt::CursorShape::Block {
            ink[3] *= 0.45;
        }
        let rects: Vec<[i32; 4]> = match (shape, p.focused) {
            (harness_vt::CursorShape::Block, true) => vec![[x, y, w, ch]],
            // Unfocused: a hollow box.
            (harness_vt::CursorShape::Block, false) => vec![[x, y, w, hair], [x, y + ch - hair, w, hair], [x, y + hair, hair, ch - 2 * hair], [x + w - hair, y + hair, hair, ch - 2 * hair]],
            (harness_vt::CursorShape::Bar, _) => vec![[x, y, thick, ch]],
            (harness_vt::CursorShape::Underline, _) => vec![[x, y + ch - thick, w, thick]],
        };
        for [rx, ry, rw, rh] in rects {
            d.panel(rect_panel([pt(rx), pt(ry), pt(rw), pt(rh)], ink));
        }
    }
    let thin = (sc * grid.size / 13.0).round().max(1.0) / sc;
    for (k, rect, ink) in boxes {
        crate::boxdraw::draw(d, k, rect, ink, sc, thin);
    }
    let asz = cache.size as f32;
    let put = |d: &mut Draw, e: AtlasGlyph, left: f32, top: f32, ink: Rgba| {
        if e.w == 0 {
            return;
        }
        d.glyph(GlyphInst {
            rect: [left / sc, top / sc, e.w as f32 / sc, e.h as f32 / sc],
            uv: [e.x as f32 / asz, e.y as f32 / asz, (e.x + e.w) as f32 / asz, (e.y + e.h) as f32 / asz],
            col_d: ink,
            col_l: ink,
            anim: [-1.0e9, 30.0, 0.0, 1.0],
            kind: [f32::from(u8::from(e.colour)), 0.0, 0.0, 0.0],
            ..Default::default()
        });
    };
    // A shaped run from `x0` pixels: each glyph at its cluster's cell, moved by the advances before it in the cluster.
    let set_run = |d: &mut Draw, cache: &mut GlyphCache, upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]), fonts: &Fonts, x0: f32, base: i32, font: FontId, glyphs: &[RunGlyph], ink: Rgba, cells: bool| {
        let (mut pen, mut cluster, mut cluster_pen) = (0.0_f32, u32::MAX, 0.0_f32);
        for g in glyphs {
            if g.cell != cluster {
                cluster = g.cell;
                cluster_pen = pen;
            }
            let cell_x = if cells { g.cell as f32 * cw as f32 } else { cluster_pen * sc };
            let xp = x0 + cell_x + (pen - cluster_pen + g.dx) * sc;
            pen += g.adv;
            let ix = xp.floor();
            let sub = (((xp - ix) * SUBPX as f32).floor() as u32).min(SUBPX - 1);
            let Some(e) = cache.get(fonts, font, g.id, sc, sub, upload) else { continue };
            let base_px = base as f32 - (g.dy * sc).round();
            put(d, e, ix + e.left as f32, base_px - e.top as f32, ink);
        }
    };
    for job in jobs {
        match job {
            Job::Run { x, base, font, glyphs, ink } => set_run(d, cache, upload, fonts, x as f32, base, font, &glyphs, ink, true),
            Job::Centred { x, base, span, font, glyphs, ink } => {
                let adv: f32 = glyphs.iter().map(|g| g.adv).sum::<f32>() * sc;
                let off = ((span as f32 - adv) / 2.0).max(0.0).round();
                set_run(d, cache, upload, fonts, x as f32 + off, base, font, &glyphs, ink, false);
            }
            Job::Placed { x, bottom, font, glyph, con, width, ink } => {
                let [bx, by, bw, bh] = fonts.glyph_bounds(font, glyph);
                if bw <= 0.0 || bh <= 0.0 {
                    continue;
                }
                let s = f64::from(sc);
                let base_up = f64::from(grid.chp - grid.basep);
                let out = con.constrain(GRect { w: bw * s, h: bh * s, x: bx * s, y: by * s + base_up }, &grid.m, width);
                let (sx, sy) = (out.w / bw, out.h / bh);
                let Some(e) = cache.get_placed(fonts, font, glyph, sx, sy, out.x - bx * sx, out.y - by * sy, upload) else { continue };
                put(d, e, (x + e.left) as f32, (bottom - e.top) as f32, ink);
            }
        }
    }
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
