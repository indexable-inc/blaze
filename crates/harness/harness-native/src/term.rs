//! The terminal: a libghostty screen (harness-vt) drawn as panels (cell backgrounds, selection, cursor) and atlas glyphs,
//! on the same pipeline and glyph cache as the rest of the UI; and input: keys through Ghostty's encoder (legacy or the
//! kitty protocol, from the modes [`snapshot`] reads), natural text editing, mouse reports, paste and selection units.
//!
//! Text is set the way Ghostty sets it: cells sized from the primary face's metrics in whole device pixels; each row cut
//! into runs of one style and colour, shaped by HarfRust with ligatures; what the primary face lacks taken from Symbols
//! Nerd Font Mono (fitted to its cells by Ghostty's constraint table), Apple Color Emoji or CoreText's cascade.
use crate::draw::Draw;
use crate::pack::{GlyphInst, PanelInst, SpriteInst};
use crate::text::nerd_rules::{Constraint, Metrics, Rect as GRect};
use crate::text::{AtlasGlyph, FontId, Fonts, GlyphCache, RunGlyph, SUBPX};
use std::collections::HashMap;
use std::rc::Rc;

/// A colour as the shaders take it (straight RGBA, sRGB).
pub type Rgba = [f32; 4];

/// The grid's look: default ink and ground, the 256-colour palette, cursor (and the glyph under a block cursor), the
/// default cursor's accent and blink, and selection. From the user's Ghostty theme ([`Theme::from_ghostty`]), else the
/// built-in dark one.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub fg: Rgba,
    pub bg: Rgba,
    pub palette: [Rgba; 256],
    pub cursor: Rgba,
    /// The glyph's ink under a block cursor (Ghostty's cursor-text); None: the ground.
    pub cursor_text: Option<Rgba>,
    /// The default cursor (a thin bar) when the program has not set one.
    pub accent: Rgba,
    /// Whether that default cursor blinks (Ghostty's cursor-style-blink = true).
    pub blink: bool,
    /// Ghostty's selection-background, when the theme sets one: it retints the app's one selection plate (`ui.selection`,
    /// src/app/ghostty.rs); None: the scene's.
    pub select: Option<Rgba>,
    /// A selected cell's ink (Ghostty's selection-foreground); None: its own.
    pub select_fg: Option<Rgba>,
}

const fn hex(v: u32) -> Rgba {
    [
        ((v >> 16) & 0xff) as f32 / 255.0,
        ((v >> 8) & 0xff) as f32 / 255.0,
        (v & 0xff) as f32 / 255.0,
        1.0,
    ]
}

/// A Ghostty colour as the shaders take it.
#[must_use]
pub fn rgb([r, g, b]: [u8; 3]) -> Rgba {
    [f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0]
}

impl Theme {
    /// harness's own dark theme (with no Ghostty config, in both appearances).
    #[must_use]
    pub fn builtin() -> Self {
        let b = harness_ghostty_config::builtin();
        Self {
            fg: rgb(b.fg),
            bg: rgb(b.bg),
            palette: b.palette.map(rgb),
            cursor: hex(0xd0d0ff),
            cursor_text: None,
            accent: hex(0x8b9cff),
            blink: false,
            select: None,
            select_fg: None,
        }
    }

    /// One appearance's Ghostty theme; what it leaves unset (cursor, selection) stays the built-in theme's.
    #[must_use]
    pub fn from_ghostty(t: &harness_ghostty_config::Theme, blink: bool) -> Self {
        let b = Self::builtin();
        let cursor = t.cursor.map(rgb);
        Self {
            fg: rgb(t.fg),
            bg: rgb(t.bg),
            palette: t.palette.map(rgb),
            cursor: cursor.unwrap_or(b.cursor),
            cursor_text: t.cursor_text.map(rgb),
            accent: cursor.unwrap_or(b.accent),
            blink,
            select: t.selection_bg.map(rgb),
            select_fg: t.selection_fg.map(rgb),
        }
    }
}

/// The 256-colour palette's colour `i`.
fn idx(theme: &Theme, i: u8) -> Rgba {
    theme.palette[usize::from(i)]
}

fn colour(theme: &Theme, c: harness_vt::Color, fg: bool) -> Option<Rgba> {
    match c {
        harness_vt::Color::Default => fg.then_some(theme.fg),
        harness_vt::Color::Idx(i) => Some(idx(theme, i)),
        harness_vt::Color::Rgb(r, g, b) => {
            Some([f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0])
        }
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
/// seconds. The blink waits `paint::BLINK_PAUSE` after that.
#[derive(Clone, Copy, Debug, Default)]
struct Blink {
    at: (u16, u16),
    row: u64,
    since: f32,
}


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
    /// Shaped runs found in `runs` and shaped anew, since this grid was made (F3's overlay, src/app/stats.rs).
    pub shape_hits: u64,
    pub shape_misses: u64,
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
        let faces = [(false, false), (true, false), (false, true), (true, true)]
            .map(|(b, i)| fonts.term_font(size, b, i));
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
        let m = Metrics {
            cell_w: cwp,
            cell_h: chp,
            face_w,
            face_h,
            face_y: cell_base - face_base,
            icon_h: face_h,
            icon_h1: (2.0 * cap + face_h) / 3.0,
        };
        let (cwp, chp) = (cwp as i32, chp as i32);
        let basep = chp - cell_base as i32;
        // Ghostty: the underline's position rounded from the baseline, its thickness at least a pixel, rounded up.
        let ul_pos = fm.underline_pos.map_or(-f64::from(size) * 0.12 * s, |v| f64::from(v) * s);
        let ult = fm
            .underline_thick
            .map_or((f64::from(size) / 13.0 * s).round(), |v| (f64::from(v) * s).ceil())
            .max(1.0) as i32;
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
            shape_hits: 0,
            shape_misses: 0,
            blink: Blink::default(),
            animating: false,
        }
    }

    /// A cell's width and height, device pixels.
    #[must_use]
    pub fn cwp(&self) -> i32 {
        self.cwp
    }

    #[must_use]
    pub fn chp(&self) -> i32 {
        self.chp
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
            self.shape_hits += 1;
            return r.clone();
        }
        self.shape_misses += 1;
        if self.runs.len() > 8192 {
            self.runs.clear();
        }
        let r: Rc<[RunGlyph]> = fonts.shape_cells(font, cells).into();
        self.runs.insert(key, r.clone());
        r
    }

    /// The cursor's opacity this frame (1 solid, 0 hidden): a smooth fade on the app clock when it blinks and the pane
    /// is focused, held solid for `paint::BLINK_PAUSE` after the cursor moved or its row changed.
    fn cursor_alpha(&mut self, snap: &Snapshot, p: &Paint<'_>, blinking: bool, shown: bool) -> f32 {
        let row = snap.rows.get(usize::from(snap.cursor.0)).map_or(FNV0, |r| {
            r.iter().fold(FNV0, |h, c| fnv_mix(fnv_mix(h, c.text.as_bytes()), &[0]))
        });
        if (snap.cursor, row) != (self.blink.at, self.blink.row) {
            self.blink = Blink { at: snap.cursor, row, since: p.time };
        }
        self.animating = blinking && p.focused && shown;
        if !self.animating {
            return 1.0;
        }
        harness_native_render::paint::caret_blink(p.time - self.blink.since)
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
    /// The selection's look (the app's one, harness-native-render select.rs), its colour on this ground, whether that
    /// ground is dark (a copy's light adds to a dark one), and a copy's pulse.
    pub select: &'a harness_native_render::select::Look,
    pub select_ink: Rgba,
    pub ground_dark: bool,
    pub flash: Option<harness_native_ui_motion_api::CopyFlash>,
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

    /// A box-drawing, block, braille, sextant or powerline character, drawn as a GPU sprite ([`crate::boxdraw`]).
    fn procedural(&self) -> Option<crate::boxdraw::Shape> {
        let mut chars = self.text.chars();
        match (chars.next(), chars.next()) {
            (Some(k), None) => crate::boxdraw::shape(k),
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
                    let (fg, bg) =
                        if cell.inverse { (bg.unwrap_or(theme.bg), Some(fg)) } else { (fg, bg) };
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
    let input = Input {
        modes: term.modes(),
        cursor: (at.y, at.x),
        size: (cols, nrows),
        history: term.history(),
    };
    Snapshot {
        rows: out,
        cursor: (look.y, look.x),
        cursor_shown: cursor.is_some_and(|c| c.visible),
        cursor_look: look,
        input,
    }
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
pub fn draw(
    d: &mut Draw,
    grid: &mut Grid,
    fonts: &mut Fonts,
    cache: &mut GlyphCache,
    snap: &Snapshot,
    p: &Paint<'_>,
    upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]),
) {
    let sc = p.scale;
    let pt = |v: i32| v as f32 / sc;
    // Every cell sits on a whole device pixel: the origin is snapped, cells are whole pixels.
    let (ox, oy) = ((p.origin.0 * sc).round() as i32, (p.origin.1 * sc).round() as i32);
    let (cw, ch) = (grid.cwp, grid.chp);
    let theme = p.theme;
    let cursor = snap.cursor;
    let marked = p.marked.filter(|m| !m.is_empty() && p.scroll == 0);
    // The cells the marked (IME) text covers: the grid's own glyphs there give way to it.
    let marked_span =
        marked.map(|m| (cursor.0, cursor.1, cursor.1.saturating_add(columns(m) as u16)));
    let in_marked =
        |r: u16, c: u16| marked_span.is_some_and(|(mr, a, b)| mr == r && (a..b).contains(&c));
    let show_cursor = snap.cursor_shown && p.scroll == 0 && marked.is_none();
    let look = snap.cursor_look;
    // The program's DECSCUSR shape and blink (1, 3, 5 blink), else the default (no DECSCUSR, or 0, which Ghostty reads as
    // "the configured default"): a steady thin bar in the accent colour, so an idle pane draws no frames.
    let (shape, blinking, cursor_ink) = if look.explicit {
        (look.shape, look.blinking, theme.cursor)
    } else {
        (harness_vt::CursorShape::Bar, theme.blink, theme.accent)
    };
    let alpha = grid.cursor_alpha(snap, p, blinking, show_cursor);
    let inverts = show_cursor && p.focused && shape == harness_vt::CursorShape::Block;
    let mut sprites: Vec<SpriteInst> = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    // A copy's pulse is playing: text runs break at the selection's edges, so the selected glyphs can grow with their plate.
    let flashing = p.sel.is_some() && p.flash.is_some_and(|f| f.live());
    let selected = |r: u16, c: u16| p.sel.is_some_and(|s| s.holds(i64::from(r) - p.scroll as i64, c));
    let ink_at = |cell: &Cell, r: u16, c: u16| {
        let mut ink = match theme.select_fg {
            Some(f) if p.sel.is_some_and(|s| s.holds(i64::from(r) - p.scroll as i64, c)) => f,
            _ => cell.fg,
        };
        if cell.dim {
            ink[3] *= 0.6;
        }
        if inverts && (r, c) == cursor {
            // The glyph under a block cursor in the cursor-text colour (else the ground's), as solid as the cursor is.
            let under = theme.cursor_text.unwrap_or(theme.bg);
            for k in 0..3 {
                ink[k] += (under[k] - ink[k]) * alpha;
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
            // The selection is a plate over the grounds (after the rows), not a part of them.
            let bg = cell.bg;
            if run.map(|r| r.1) != bg {
                flush(d, &mut run, c);
                if let Some(bg) = bg {
                    run = Some((c, bg));
                }
            }
            if cell.underline && !cell.cont {
                let w = if cell.wide { 2 * cw } else { cw };
                d.panel(rect_panel(
                    [pt(ox + i32::from(c) * cw), pt(y + grid.ulp), pt(w), pt(grid.ult)],
                    cell.fg,
                ));
            }
        }
        flush(d, &mut run, row.len() as u16);

        // Text: runs of one style and ink, broken at the cursor, at wide cells and at anything not in the primary face.
        let mut c = 0usize;
        while c < row.len() {
            let cell = &row[c];
            let cu = c as u16;
            if cell.cont || cell.blank() || in_marked(r, cu) {
                c += 1;
                continue;
            }
            let x = ox + c as i32 * cw;
            let w = if cell.wide { 2 * cw } else { cw };
            let ink = ink_at(cell, r, cu);
            if let Some(sh) = cell.procedural() {
                // Its own ground under it, except under the cursor, whose plate shows instead. A ground of its own hides the
                // selection's plate drawn before it, so a selected one takes the plate's colour over it.
                let bg = match cell.bg {
                    Some(g) if p.sel.is_some_and(|s| s.holds(srow, cu)) => Some(blend(g, p.select_ink)),
                    g => g,
                };
                let under_cursor = show_cursor && (r, cu) == cursor;
                sprites.push(SpriteInst {
                    rect: [x, y, w, ch],
                    shape: [sh.kind, sh.a, sh.b, 0],
                    fg: ink,
                    bg: if under_cursor { [0.0; 4] } else { bg.unwrap_or([0.0; 4]) },
                });
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
                            if n.cont
                                || n.wide
                                || (show_cursor && (r, nu) == cursor)
                                || n.procedural().is_some()
                                || in_marked(r, nu)
                                || (flashing && selected(r, nu) != selected(r, start as u16))
                            {
                                break;
                            }
                            if !n.blank()
                                && (n.style() != style
                                    || ink_at(n, r, nu) != ink
                                    || grid.pick(fonts, &n.text, style) != Pick::Primary)
                            {
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
                    jobs.push(Job::Run {
                        x: ox + start as i32 * cw,
                        base: y + grid.basep,
                        font,
                        glyphs,
                        ink,
                    });
                }
                Pick::Nerd(font, con) => {
                    // Two cells free when the next one is blank (Ghostty's constraint width).
                    let width =
                        if row.get(c + 1).is_some_and(|n| n.blank() && !n.cont) { 2 } else { 1 };
                    if let Some(g) = grid.shaped(fonts, font, &[cell.text.as_str()]).first() {
                        jobs.push(Job::Placed {
                            x,
                            bottom: y + ch,
                            font,
                            glyph: g.id,
                            con,
                            width,
                            ink,
                        });
                    }
                    c += 1;
                }
                Pick::Emoji(font) => {
                    // CoreText picks the emoji's glyph: Apple Color Emoji joins flags and ZWJ sequences there.
                    if let Some(glyph) = fonts.ct_glyph(font, &cell.text) {
                        jobs.push(Job::Placed {
                            x,
                            bottom: y + ch,
                            font,
                            glyph,
                            con: Constraint::EMOJI,
                            width: if cell.wide { 2 } else { 1 },
                            ink,
                        });
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
    // The selection: one joined plate over the grounds and under the text, each row from its first selected cell to its
    // last (a row it runs through whole, the grid's width), pulsing after a copy (its glyphs growing with it and catching
    // its light, below).
    let mut pulse = harness_native_render::select::Pulse::default();
    if let Some(sel) = p.sel {
        let ((r0, c0), (r1, c1)) = sel.ordered();
        let cols = snap.rows.first().map_or(0, Vec::len) as i32;
        let plates: Vec<harness_native_render::select::Plate> = (0..snap.rows.len() as i64)
            .filter_map(|r| {
                let srow = r - p.scroll as i64;
                if srow < r0 || srow > r1 {
                    return None;
                }
                let a = if srow == r0 { i32::from(c0) } else { 0 };
                let b = if srow == r1 { i32::from(c1) + 1 } else { cols };
                let y = oy + r as i32 * ch;
                (b > a).then(|| harness_native_render::select::Plate::of(pt(ox + a * cw), pt(y), pt((b - a) * cw), pt(ch)))
            })
            .collect();
        pulse = harness_native_render::select::Pulse::of(&plates, p.select, p.flash, p.ground_dark);
        harness_native_render::select::emit(d, &plates, p.select, p.select_ink, &pulse, 1.0, sc);
    }
    if let (Some(m), Some((r, a, b))) = (marked, marked_span) {
        // Marked text inline at the cursor on the ground colour, underlined, with a bar caret after it.
        let (x, y, w) = (ox + i32::from(a) * cw, oy + i32::from(r) * ch, i32::from(b - a) * cw);
        d.panel(rect_panel([pt(x), pt(y), pt(w), pt(ch)], theme.bg));
        let font = grid.faces[0];
        let mut cx = x;
        let mut buf = [0u8; 4];
        for chr in m.chars() {
            let t: &str = chr.encode_utf8(&mut buf);
            let span = cw * columns(t) as i32;
            let glyphs = grid.shaped(fonts, font, &[t]);
            jobs.push(Job::Centred {
                x: cx,
                base: y + grid.basep,
                span,
                font,
                glyphs,
                ink: theme.fg,
            });
            cx += span;
        }
        d.panel(rect_panel([pt(x), pt(y + grid.ulp), pt(w), pt(grid.ult)], theme.fg));
        let thick = ((1.5 * sc).round() as i32).max(1);
        d.panel(rect_panel([pt(x + w), pt(y), pt(thick), pt(ch)], theme.cursor));
    }
    if show_cursor && alpha > 0.0 {
        let (r, c) = cursor;
        let wide = snap
            .rows
            .get(usize::from(r))
            .and_then(|row| row.get(usize::from(c)))
            .is_some_and(|cell| cell.wide);
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
            (harness_vt::CursorShape::Block, false) => vec![
                [x, y, w, hair],
                [x, y + ch - hair, w, hair],
                [x, y + hair, hair, ch - 2 * hair],
                [x + w - hair, y + hair, hair, ch - 2 * hair],
            ],
            (harness_vt::CursorShape::Bar, _) => vec![[x, y, thick, ch]],
            (harness_vt::CursorShape::Underline, _) => vec![[x, y + ch - thick, w, thick]],
        };
        for [rx, ry, rw, rh] in rects {
            d.panel(rect_panel([pt(rx), pt(ry), pt(rw), pt(rh)], ink));
        }
    }
    let thick = crate::boxdraw::thickness(fonts, grid.size, sc);
    for mut s in sprites {
        s.shape[3] = thick;
        d.sprite(s);
    }
    let asz = cache.size as f32;
    let put = |d: &mut Draw, e: AtlasGlyph, left: f32, top: f32, ink: Rgba| {
        if e.w == 0 {
            return;
        }
        d.glyph(GlyphInst {
            rect: [left / sc, top / sc, e.w as f32 / sc, e.h as f32 / sc],
            uv: [
                e.x as f32 / asz,
                e.y as f32 / asz,
                (e.x + e.w) as f32 / asz,
                (e.y + e.h) as f32 / asz,
            ],
            col_d: ink,
            col_l: ink,
            anim: [-1.0e9, 30.0, 0.0, 1.0],
            kind: [f32::from(u8::from(e.colour)), 0.0, 0.0, 0.0],
            ..Default::default()
        });
    };
    // A shaped run from `x0` pixels: each glyph at its cluster's cell, moved by the advances before it in the cluster.
    let set_run = |d: &mut Draw,
                   cache: &mut GlyphCache,
                   upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]),
                   fonts: &Fonts,
                   x0: f32,
                   base: i32,
                   font: FontId,
                   glyphs: &[RunGlyph],
                   ink: Rgba,
                   cells: bool| {
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
    let first_glyph = d.glyphs.len();
    for job in jobs {
        match job {
            Job::Run { x, base, font, glyphs, ink } => {
                set_run(d, cache, upload, fonts, x as f32, base, font, &glyphs, ink, true)
            }
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
                let out = con.constrain(
                    GRect { w: bw * s, h: bh * s, x: bx * s, y: by * s + base_up },
                    &grid.m,
                    width,
                );
                let (sx, sy) = (out.w / bw, out.h / bh);
                let Some(e) = cache.get_placed(
                    fonts,
                    font,
                    glyph,
                    sx,
                    sy,
                    out.x - bx * sx,
                    out.y - by * sy,
                    upload,
                ) else {
                    continue;
                };
                put(d, e, (x + e.left) as f32, (bottom - e.top) as f32, ink);
            }
        }
    }
    // Under a copy's pulse, the glyphs whose cell (the one under their middle) is selected grow with the plate and catch
    // its light.
    if pulse.live() {
        let cell = |v: f32, o: i32, step: i32| ((v * sc - o as f32) / step as f32).floor();
        for gi in &mut d.glyphs[first_glyph..] {
            let (col, row) = (cell(gi.rect[0] + gi.rect[2] * 0.5, ox, cw), cell(gi.rect[1] + gi.rect[3] * 0.5, oy, ch));
            if row >= 0.0 && col >= 0.0 && selected(row as u16, col as u16) {
                if let Some(g) = pulse.grow {
                    gi.rect = g.rect(gi.rect);
                }
                pulse.light_glyph(gi);
            }
        }
    }
}

fn blend(under: Rgba, over: Rgba) -> Rgba {
    let a = over[3];
    [
        under[0] * (1.0 - a) + over[0] * a,
        under[1] * (1.0 - a) + over[1] * a,
        under[2] * (1.0 - a) + over[2] * a,
        1.0,
    ]
}

/// A plain square plate of colour `c`.
#[must_use]
pub fn rect_panel(rect: [f32; 4], c: Rgba) -> PanelInst {
    PanelInst {
        rect,
        fill_d: c,
        fill_l: c,
        shape: [0.0, 4.0, 0.0, 0.0],
        fx2: [0.0, 1.0, 0.0, 0.0],
        ..Default::default()
    }
}

// ---- input: keys, mouse, paste, selection units ----

pub use harness_vt::KeyAction;
use harness_vt::ghostty::key::{self as gkey, Key, KittyKeyFlags, OptionAsAlt};

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
        let text: String = k
            .text
            .chars()
            .filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c))
            .collect();
        let m = k.mods;
        let mut gm = gkey::Mods::empty();
        gm.set(gkey::Mods::SHIFT, m.shift);
        gm.set(gkey::Mods::CTRL, m.ctrl);
        gm.set(gkey::Mods::ALT, m.alt);
        gm.set(gkey::Mods::SUPER, m.cmd);
        // Shift made the text (A, !): it is consumed, so the kitty protocol sends the text, not a shifted key.
        let consumed =
            if m.shift && !text.is_empty() { gkey::Mods::SHIFT } else { gkey::Mods::empty() };
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
        self.encode(
            &KeyIn {
                code,
                text: "",
                unshifted: None,
                mods: Mods::default(),
                action: KeyAction::Press,
                composing: false,
            },
            modes,
        )
    }
}

/// Key codes the view sends straight to the encoder, around the input method: arrows, function and navigation keys.
#[must_use]
pub fn is_function_key(code: u16) -> bool {
    use Key as K;
    matches!(
        key_of(code),
        K::ArrowLeft
            | K::ArrowRight
            | K::ArrowUp
            | K::ArrowDown
            | K::Home
            | K::End
            | K::PageUp
            | K::PageDown
            | K::Delete
            | K::Insert
    ) || matches!(code, 96..=101 | 103 | 105..=107 | 109 | 111 | 113 | 118 | 120 | 122 | 64 | 79 | 80 | 90)
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
            (0, K::A),
            (1, K::S),
            (2, K::D),
            (3, K::F),
            (4, K::H),
            (5, K::G),
            (6, K::Z),
            (7, K::X),
            (8, K::C),
            (9, K::V),
            (10, K::IntlBackslash),
            (11, K::B),
            (12, K::Q),
            (13, K::W),
            (14, K::E),
            (15, K::R),
            (16, K::Y),
            (17, K::T),
            (18, K::Digit1),
            (19, K::Digit2),
            (20, K::Digit3),
            (21, K::Digit4),
            (22, K::Digit6),
            (23, K::Digit5),
            (24, K::Equal),
            (25, K::Digit9),
            (26, K::Digit7),
            (27, K::Minus),
            (28, K::Digit8),
            (29, K::Digit0),
            (30, K::BracketRight),
            (31, K::O),
            (32, K::U),
            (33, K::BracketLeft),
            (34, K::I),
            (35, K::P),
            (36, K::Enter),
            (37, K::L),
            (38, K::J),
            (39, K::Quote),
            (40, K::K),
            (41, K::Semicolon),
            (42, K::Backslash),
            (43, K::Comma),
            (44, K::Slash),
            (45, K::N),
            (46, K::M),
            (47, K::Period),
            (48, K::Tab),
            (49, K::Space),
            (50, K::Backquote),
            (51, K::Backspace),
            (53, K::Escape),
            (54, K::MetaRight),
            (55, K::MetaLeft),
            (56, K::ShiftLeft),
            (57, K::CapsLock),
            (58, K::AltLeft),
            (59, K::ControlLeft),
            (60, K::ShiftRight),
            (61, K::AltRight),
            (62, K::ControlRight),
            (63, K::Fn),
            (64, K::F17),
            (65, K::NumpadDecimal),
            (67, K::NumpadMultiply),
            (69, K::NumpadAdd),
            (71, K::NumLock),
            (72, K::AudioVolumeUp),
            (73, K::AudioVolumeDown),
            (74, K::AudioVolumeMute),
            (75, K::NumpadDivide),
            (76, K::NumpadEnter),
            (78, K::NumpadSubtract),
            (79, K::F18),
            (80, K::F19),
            (81, K::NumpadEqual),
            (82, K::Numpad0),
            (83, K::Numpad1),
            (84, K::Numpad2),
            (85, K::Numpad3),
            (86, K::Numpad4),
            (87, K::Numpad5),
            (88, K::Numpad6),
            (89, K::Numpad7),
            (90, K::F20),
            (91, K::Numpad8),
            (92, K::Numpad9),
            (93, K::IntlYen),
            (94, K::IntlRo),
            (95, K::NumpadComma),
            (96, K::F5),
            (97, K::F6),
            (98, K::F7),
            (99, K::F3),
            (100, K::F8),
            (101, K::F9),
            (103, K::F11),
            (105, K::F13),
            (106, K::F16),
            (107, K::F14),
            (109, K::F10),
            (110, K::ContextMenu),
            (111, K::F12),
            (113, K::F15),
            (114, K::Insert),
            (115, K::Home),
            (116, K::PageUp),
            (117, K::Delete),
            (118, K::F4),
            (119, K::End),
            (120, K::F2),
            (121, K::PageDown),
            (122, K::F1),
            (123, K::ArrowLeft),
            (124, K::ArrowRight),
            (125, K::ArrowDown),
            (126, K::ArrowUp),
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

/// `text` pasted: bracketed when the program asked for it, newlines as carriage returns. A bracketed paste cannot end
/// its own bracket early.
#[must_use]
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        format!("\x1b[200~{}\x1b[201~", body.replace("\x1b[201~", "")).into_bytes()
    } else {
        body.into_bytes()
    }
}

/// `path` as one shell word: as it is when it holds only characters no shell treats specially, else in single quotes (a
/// quote inside closed, escaped and reopened), as Ghostty and Terminal paste a dropped file.
#[must_use]
pub fn shell_quote(path: &str) -> String {
    let plain = |c: char| c.is_alphanumeric() || "/._-+,:@%=".contains(c);
    if !path.is_empty() && path.chars().all(plain) {
        path.to_owned()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
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
pub fn mouse(
    b: Button,
    act: MouseAct,
    col: u16,
    row: u16,
    m: Mods,
    modes: &harness_vt::Modes,
) -> Vec<u8> {
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
        return format!("\x1b[<{code};{x};{y}{}", if act == MouseAct::Release { 'm' } else { 'M' })
            .into_bytes();
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
    let stop = |i: usize| {
        !part(i) && (cells[i].is_empty() || cells[i].chars().all(|c| WORD_STOPS.contains(c)))
    };
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
            let to = if sr == e.0 {
                (usize::from(e.1) + 1).min(row.cells.len())
            } else {
                row.cells.len()
            };
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

    fn key(
        keys: &mut Keys,
        modes: &Modes,
        code: u16,
        text: &str,
        unshifted: char,
        mods: Mods,
    ) -> Vec<u8> {
        keys.encode(
            &KeyIn {
                code,
                text,
                unshifted: Some(unshifted),
                mods,
                action: KeyAction::Press,
                composing: false,
            },
            modes,
        )
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
        let rel = KeyIn {
            code: 0,
            text: "",
            unshifted: Some('a'),
            mods: NONE,
            action: KeyAction::Release,
            composing: false,
        };
        assert_eq!(k.encode(&rel, &m), b"");
        assert_eq!(k.encode(&rel, &Modes { kitty_flags: 1 | 2 | 8, ..m }), b"\x1b[97;1:3u");
    }

    #[test]
    fn mouse_reports() {
        let sgr = Modes {
            mouse: MouseTracking::Normal,
            mouse_format: MouseFormat::Sgr,
            ..Modes::default()
        };
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
        let row =
            ["l", "s", " ", "/", "t", "m", "p", "/", "a", ".", "t", "x", "t", " ", "(", "x", ")"];
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
