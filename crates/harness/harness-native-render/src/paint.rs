//! The paint pass's host half, shared by every host of a flecs UI world (the harness app, harness-video): measure the
//! words layout asks about ([`Painter::typeset`]), then turn each node the draw pass painted
//! (`harness_native_ui_draw::painted`) into panels and glyphs of a [`Draw`] from the node's own look and content
//! components ([`Painter::node`]). What only one host draws (the app's terminals, its sparkline, its tree's fades) stays in
//! that host, which handles those nodes itself and hands the rest here.
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use harness_native_api::{Clock, HoverAmt};
use harness_native_flecs::prelude::*;
use harness_native_ui_draw_api::{PaintKind, Painted};
use harness_native_ui_layout::typeset;
use harness_native_ui_layout_api::Intrinsic;
use harness_native_ui_look_api::{Edge, Fill, HoverInk, HoverLook, Lift, MathLook, PressLook, Rgba, Shape, Sheen, StrikeLook, Stroke, TextStyle};
use harness_native_ui_motion_api::{CopyFlash, Pose};
use harness_native_ui_node_api::Laid;
use harness_native_ui_text_api::{
    BlazeMark, Caret, Lantern, Drawing, Frames, Highlight, Kip, Mark, MarkdownText, OneLine, Picture, PixelBlock, Pixelated, Surface, Text, TextRange,
    TextEdit, Truncate, TruncateMode, Vector, VectorFit, VectorText,
};
use harness_native_ui_timeline_api::{GlyphsArrive, GlyphsIn};

use crate::draw::{ATLAS_SIZE, Draw};
use crate::select;
use crate::pack::{GlyphInst, ImageInst, LineInst, PanelInst, PathInst, SurfaceInst};
use crate::markdown::InlineMath;
use crate::math::Item;
use crate::text::{AtlasGlyph, Face, Fonts, Glyph, GlyphCache, Layout, Run, SUBPX, Shaped, Style, TypeLook};

/// The atlas uploader a frame is drawn with: `(x, y, w, h, bytes)` into the glyph atlas.
pub type Upload<'a> = dyn FnMut(u32, u32, u32, u32, &[u8]) + 'a;

/// A picture decoded for drawing as pixel art: its frames, each `w` by `h` straight RGBA8.
struct Sheet {
    w: u32,
    h: u32,
    frames: Vec<Rc<[u8]>>,
    /// An HDR photo's brightest light (linear over reference white; its frames are premultiplied linear half floats,
    /// photo.rs): None for sRGB RGBA8 texels.
    peak: Option<f32>,
}

/// What a text is shaped by: its text, size, line height, boldness, and its face's say (code, medium, inline markdown,
/// tabular figures, as bits).
type ShapeKey = (String, u32, u32, bool, u8);

/// Shaped texts a painter keeps, and laid-out widths; past either it forgets that cache and starts again.
const SHAPED_KEPT: usize = 4096;
const LAYOUTS_KEPT: usize = 4096;

/// The host's fonts, glyph atlas and layout cache, and the pictures it has decoded.
pub struct Painter {
    pub fonts: Fonts,
    pub cache: GlyphCache,
    /// Each text shaped once, for any width, by its text and how it is set ([`ShapeKey`]): a width it has not been broken
    /// to yet (a resize, a fold's spring, a column growing) only breaks it again ([`Layout::wrap`]), never shapes it again.
    shaped: HashMap<ShapeKey, Rc<Shaped>>,
    /// Each text broken to a width, by its [`ShapeKey`] and the width's bits.
    layouts: HashMap<(ShapeKey, u32), Rc<Layout>>,
    /// One-line widths [`Painter::width`] measured, by text and style: cheap entries, kept longer than layouts.
    widths: HashMap<(String, u32, bool, u8), f32>,
    sheets: HashMap<String, Rc<Sheet>>,
    /// Each `BlazeMark` node's fire, by entity and appearance: its automaton runs on from frame to frame.
    marks: HashMap<(u64, bool), harness_native_blaze::mark::Blaze>,
    /// The `Lantern` nodes' lantern (lantern.rs), and whether one was painted in the last frame
    /// ([`Painter::end_frame`]): its flame moves, so the host keeps drawing, at its calm rate.
    lanterns: crate::lantern::Lanterns,
    pub lantern_live: bool,
    /// Each `PixelBlock` node's item (block.rs).
    blocks: crate::block::Blocks,
    /// A block was painted in the last frame ([`Painter::end_frame`]): its item moves, so the host keeps drawing, at
    /// its calm rate (pixel art at a few steps a second).
    pub blocks_live: bool,
    /// Each `Kip` node's mascot, stepped while it is painted (kips.rs).
    kips: crate::kips::Kips,
    /// What the Kips painted in the last frame want next ([`Painter::end_frame`]).
    pub kip_wake: crate::kips::Wake,
    highlight: crate::highlight::Highlighter,
    /// Where a `Picture`'s handle is a path under (the scene's directory); None: this host draws no pictures.
    pub media: Option<PathBuf>,
    /// Each `Vector`'s path data read once (vector.rs), by its `d`.
    paths: HashMap<String, Rc<[crate::vector::Seg]>>,
    /// When each `GlyphsArrive` node's glyphs came (arrive.rs).
    arrivals: crate::arrive::Arrivals,
    /// A glyph painted in the last frame is still fading in ([`Painter::end_frame`]): the host draws another.
    pub arriving: bool,
    /// Each focused caret's sweep (caret.rs), by its text node.
    carets: crate::caret::Carets,
    /// A caret painted in the last frame is still sweeping ([`Painter::end_frame`]): the host draws another.
    pub gliding: bool,
    /// Words painted this frame carry a moving [`Sheen`].
    sheening: bool,
    /// Words painted in the last frame carried a moving [`Sheen`] ([`Painter::end_frame`]): the host keeps drawing, at its
    /// calm rate (a sheen is slow light, not motion that wants every frame). False under Reduce Motion, where it is still.
    pub shimmering: bool,
    /// LaTeX math's look (`ui.formula`), read from the scene each [`Painter::typeset`]: how large a formula is set, so the
    /// hole markdown leaves for it ([`crate::markdown::Inline::holes`]); a change shapes every text again.
    math: MathLook,
}

/// The most points one `Vector` node flattens to; past it the path is cut and the node's paint fails, named.
pub const MAX_VECTOR_POINTS: usize = 20_000;

/// What [`Painter::node`] did with a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Emitted {
    /// The first panel and glyph it added to the `Draw` (what came before it is untouched).
    pub panels: usize,
    pub glyphs: usize,
    pub images: usize,
    pub surfaces: usize,
    pub lines: usize,
    pub paths: usize,
}

/// The `Style` a `TextStyle` sets (14 points on 1.3 lines unless it says).
#[must_use]
pub fn set_of(s: &TextStyle) -> Style {
    let size = if s.size > 0.0 { s.size } else { 14.0 };
    Style {
        size,
        bold: s.bold != 0,
        line: if s.line > 0.0 { s.line } else { (size * 1.3).round() },
    }
}

#[must_use]
/// Ink `c` of node `e` (its fill's, or with `outline` its edge's or stroke's) mixed toward its `HoverInk` by how hovered
/// it is (its HoverAmt).
fn hovered(e: EntityView<'_>, c: Rgba, outline: bool, dark: bool) -> Rgba {
    let Some(h) = e.try_cloned::<&HoverInk>() else { return c };
    let (bit, to) = if outline { (HoverInk::STROKE, h.stroke) } else { (HoverInk::FILL, h.fill) };
    let t = e.try_cloned::<&HoverAmt>().map_or(0.0, |a| a.spring.x.clamp(0.0, 1.0));
    if h.set & bit == 0 || t <= 0.0 {
        return c;
    }
    let to = if dark { to.dark } else { to.light };
    let m = |a: f32, b: f32| a + (b - a) * t;
    Rgba { r: m(c.r, to.r), g: m(c.g, to.g), b: m(c.b, to.b), a: m(c.a, to.a) }
}

/// How far up from a node a copied row's words sit (a table's row, its cell, the cell's words).
const COPIED_ROW_DEPTH: usize = 6;

/// The copied row (a node with a live `CopyFlash` over its whole rect: a table's row) that words node `e` sits in, with
/// where that row is drawn this frame (view points: its `Painted` frame, as the host draws it, else its `Laid`).
fn copied_row(e: EntityView<'_>) -> Option<([f32; 4], CopyFlash)> {
    std::iter::successors(e.parent(), |n| n.parent()).take(COPIED_ROW_DEPTH).find_map(|n| {
        let f = n.try_cloned::<&CopyFlash>().filter(|f| f.whole && f.live())?;
        let drawn = n.try_cloned::<&Painted>().filter(|p| p.w > 0.0 || p.h > 0.0).map(|p| [p.x, p.y, p.w, p.h]);
        let r = drawn.or_else(|| n.try_cloned::<&Laid>().map(|l| [l.x, l.y, l.w, l.h]))?;
        Some((r, f))
    })
}

pub fn rgba(c: Rgba) -> [f32; 4] {
    [c.r, c.g, c.b, c.a]
}

/// The `code_ink` tokens: each entity under `code_ink` with a `Fill`, by its dotted path below it, in its `dark` or
/// light ink.
fn code_ink(w: WorldRef<'_>, dark: bool) -> Vec<(String, [f32; 4])> {
    fn walk(e: EntityView<'_>, prefix: &str, dark: bool, out: &mut Vec<(String, [f32; 4])>) {
        e.each_child(|c| {
            let name = if prefix.is_empty() { c.name() } else { format!("{prefix}.{}", c.name()) };
            if let Some(f) = c.try_cloned::<&Fill>() {
                out.push((name.clone(), rgba(if dark { f.dark } else { f.light })));
            }
            walk(c, &name, dark, out);
        });
    }
    let mut out = Vec::new();
    if let Some(root) = w.try_lookup("code_ink") {
        walk(root, "", dark, &mut out);
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// `rect` (points: x, y, w, h) with its edges on the device pixel grid at `sc` pixels a point, as glyphs and the terminal's
/// cells already are: at a zoom whose scale is not a whole number (1.1 on a Retina display is 2.2 pixels a point) a plate
/// would otherwise straddle pixels, its edges soft and up to half a pixel off the snapped words it sits behind. Edges, not
/// sizes, are rounded, so plates that meet still meet; a side that has any length keeps at least a pixel.
#[must_use]
pub fn snap_rect(rect: [f32; 4], sc: f32) -> [f32; 4] {
    if sc <= 0.0 {
        return rect;
    }
    let side = |at: f32, len: f32| {
        let (a, b) = ((at * sc).round(), ((at + len) * sc).round());
        let b = if len > 0.0 { b.max(a + 1.0) } else { b };
        (a / sc, (b - a) / sc)
    };
    let (x, w) = side(rect[0], rect[2]);
    let (y, h) = side(rect[1], rect[3]);
    [x, y, w, h]
}

/// den's `panel_from`: a plate of `fill` with `edge` and `shape` at `alpha`.
#[must_use]
pub fn panel_from(
    fill: [f32; 4],
    edge: [f32; 4],
    shape: Shape,
    rect: [f32; 4],
    alpha: f32,
) -> PanelInst {
    PanelInst {
        rect,
        fill_d: fill,
        fill_l: fill,
        edge_d: edge,
        edge_l: edge,
        // No corner rounder than half the short side (a pill: a large radius on a thin bar, a scroller's knob).
        shape: [shape.radius.min(rect[2].min(rect[3]) * 0.5), shape.n, shape.border, shape.feather],
        fx: [0.0; 4],
        fx2: [0.0, alpha, 0.0, 0.0],
        lens: [0.0; 4],
        fx3: [0.0; 4],
        clip: [0.0; 4],
        clipx: [0.0; 4],
    }
}

/// A layout's first `limit` glyphs at `(ox, oy)` in `ink`, snapped to the device pixel grid with den's subpixel steps.
#[allow(clippy::too_many_arguments)]
pub fn emit_layout(
    d: &mut Draw,
    fonts: &Fonts,
    cache: &mut GlyphCache,
    lay: &Layout,
    at: (f32, f32),
    ink: [f32; 4],
    alpha: f32,
    sc: f32,
    limit: usize,
    upload: &mut Upload<'_>,
) {
    emit_glyphs(d, fonts, cache, lay, at, sc, upload, None, &mut |n, _| {
        (n < limit).then_some((ink, alpha, 0.0))
    });
}

/// A layout's glyphs at `(ox, oy)`, each as `look(n, glyph)` says (its ink, opacity and how far below its place it sits,
/// points), stopping at the first it gives None. Glyphs in place snap to the device pixel grid with den's subpixel steps;
/// one still moving is drawn where it is. `grow` grows the glyphs of a UTF-16 range (a copied selection, under its pulse)
/// as their plate grows. Where the pen stands after the last one drawn (x, its line's top and height), in points, if any
/// was.
#[allow(clippy::too_many_arguments)]
pub fn emit_glyphs(
    d: &mut Draw,
    fonts: &Fonts,
    cache: &mut GlyphCache,
    lay: &Layout,
    (ox, oy): (f32, f32),
    sc: f32,
    upload: &mut Upload<'_>,
    grow: Option<(usize, usize, select::Grow)>,
    look: &mut dyn FnMut(usize, &Glyph) -> Option<([f32; 4], f32, f32)>,
) -> Option<(f32, f32, f32)> {
    let asz = cache.size as f32;
    let mut n = 0usize;
    let mut pen = None;
    for line in &lay.lines {
        for (k, g) in line.glyphs.iter().enumerate() {
            let Some((ink, alpha, dy)) = look(n, g) else { return pen };
            n += 1;
            let next_x = line.glyphs.get(k + 1).map_or(line.width, |h| h.x);
            pen = Some((ox + next_x, oy + line.top, line.height));
            if alpha <= 0.002 {
                continue;
            }
            let base = (oy + line.baseline - g.y) * sc;
            let base_px = if dy == 0.0 { base.round() } else { base + dy * sc };
            let xp = (ox + g.x) * sc;
            let ix = xp.floor();
            let sub = (((xp - ix) * SUBPX as f32).floor() as u32).min(SUBPX - 1);
            let Some(e) = cache.get(fonts, g.font, g.id, sc, sub, upload) else { continue };
            if e.w == 0 {
                continue;
            }
            let mut col = ink;
            col[3] *= alpha;
            let rect = [(ix + e.left as f32) / sc, (base_px - e.top as f32) / sc, e.w as f32 / sc, e.h as f32 / sc];
            let rect = match grow {
                Some((lo, hi, gr)) if (lo..hi).contains(&g.idx) => gr.rect(rect),
                _ => rect,
            };
            d.glyph(GlyphInst {
                rect,
                uv: [
                    e.x as f32 / asz,
                    e.y as f32 / asz,
                    (e.x + e.w) as f32 / asz,
                    (e.y + e.h) as f32 / asz,
                ],
                col_d: col,
                col_l: col,
                anim: [-1.0e9, 30.0, 0.0, 1.0],
                kind: [f32::from(u8::from(e.colour)), 0.0, 0.0, 0.0],
                ..Default::default()
            });
        }
    }
    pen
}

impl Painter {
    #[must_use]
    pub fn new(look: TypeLook) -> Self {
        Self {
            fonts: Fonts::new(look),
            cache: GlyphCache::new(ATLAS_SIZE),
            shaped: HashMap::new(),
            layouts: HashMap::new(),
            widths: HashMap::new(),
            sheets: HashMap::new(),
            marks: HashMap::new(),
            blocks: crate::block::Blocks::default(),
            blocks_live: false,
            lanterns: crate::lantern::Lanterns::default(),
            lantern_live: false,
            kips: crate::kips::Kips::default(),
            kip_wake: crate::kips::Wake::None,
            paths: HashMap::new(),
            arrivals: crate::arrive::Arrivals::default(),
            arriving: false,
            carets: crate::caret::Carets::default(),
            gliding: false,
            sheening: false,
            shimmering: false,
            highlight: crate::highlight::Highlighter::default(),
            media: None,
            math: MathLook::default(),
        }
    }

    /// `text` in `style`, broken to `width` points, cached by all three.
    pub fn layout(&mut self, text: &str, style: &TextStyle, width: f32) -> Rc<Layout> {
        self.layout_as(text, style, width, false)
    }

    /// [`Painter::layout`], of inline markdown when `md` (`MarkdownText`: the markers out, the faces they ask for in). A text
    /// is shaped once ([`Painter::shaped`]); a new width only breaks it again.
    pub fn layout_as(&mut self, text: &str, style: &TextStyle, width: f32, md: bool) -> Rc<Layout> {
        let st = set_of(style);
        let code = style.role == 9;
        let tabular = style.role == 10;
        let medium = !code && !st.bold && style.weight == 500;
        let key: ShapeKey = (
            text.to_owned(),
            st.size.to_bits(),
            st.line.to_bits(),
            st.bold,
            u8::from(code) | u8::from(medium) << 1 | u8::from(md) << 2 | u8::from(tabular) << 3,
        );
        let at = (key, width.to_bits());
        if let Some(l) = self.layouts.get(&at) {
            return l.clone();
        }
        let face = if code {
            Some(Face::Code)
        } else if tabular {
            Some(Face::Tabular)
        } else if medium {
            Some(Face::Medium)
        } else {
            None
        };
        let shaped = self.shaped(&at.0, text, st, md, face);
        let lay = Rc::new(Layout::wrap(&mut self.fonts, &shaped, width));
        if self.layouts.len() >= LAYOUTS_KEPT {
            self.layouts.clear();
        }
        self.layouts.insert(at, lay.clone());
        lay
    }

    /// `text` shaped as [`Painter::layout_as`] sets it (inline markdown's faces when `md`, else all of it in `face`, else the
    /// text face), kept by `key`.
    fn shaped(&mut self, key: &ShapeKey, text: &str, st: Style, md: bool, face: Option<Face>) -> Rc<Shaped> {
        if let Some(s) = self.shaped.get(key) {
            return s.clone();
        }
        let shaped = Rc::new(if md {
            let m = crate::markdown::inline(text);
            Shaped::new(&mut self.fonts, &m.text, &m.runs, &[], &m.holes(st.size, &self.math), st, 0.0)
        } else {
            let runs: Vec<Run> = face.map(|face| Run { start: 0, end: text.encode_utf16().count(), face }).into_iter().collect();
            Shaped::new(&mut self.fonts, text, &runs, &[], &[], st, 0.0)
        });
        if self.shaped.len() >= SHAPED_KEPT {
            self.shaped.clear();
        }
        self.shaped.insert(key.clone(), shaped.clone());
        shaped
    }

    /// How wide `text` runs on one line in `style` (inline markdown when `md`: its markers out), points; cached apart from
    /// the layouts, so a table measuring every cell of its columns each frame neither shapes again nor drops layouts.
    pub fn width(&mut self, text: &str, style: &TextStyle, md: bool) -> f32 {
        // The face's say: the role (code, tabular), medium weight, markdown.
        let face = style.role.clamp(0, 63) as u8 | u8::from(style.weight == 500) << 6 | u8::from(md) << 7;
        let key = (text.to_owned(), style.size.to_bits(), style.bold != 0, face);
        if let Some(&w) = self.widths.get(&key) {
            return w;
        }
        let lay = self.layout_as(text, style, 1.0e5, md);
        let w = lay.lines.iter().map(|l| l.width).fold(0.0, f32::max).ceil();
        if self.widths.len() > 1 << 16 {
            self.widths.clear();
        }
        self.widths.insert(key, w);
        w
    }

    /// Inline markdown `src` on one line, cut with an ellipsis at its end to `room` points: the words shown with their
    /// faces, the cut ones gone (a table cell). The links and code spans of what is left, for colour and chips.
    fn layout_md_cut(&mut self, src: &str, style: &TextStyle, room: f32) -> (Rc<Layout>, crate::markdown::Inline) {
        let mut m = crate::markdown::inline(src);
        let st = set_of(style);
        let holes = m.holes(st.size, &self.math);
        let rich = |fonts: &mut Fonts, text: &str, runs: &[crate::text::Run]| {
            let n = text.encode_utf16().count();
            let holes: Vec<crate::text::Hole> = holes.iter().copied().filter(|h| h.end <= n).collect();
            Layout::rich_with(fonts, text, runs, &[], &holes, st, 0.0, 1.0e5)
        };
        let full = rich(&mut self.fonts, &m.text, &m.runs);
        if full.lines.iter().map(|l| l.width).fold(0.0, f32::max) <= room {
            return (Rc::new(full), m);
        }
        let clip = |m: &crate::markdown::Inline, n: usize| -> Vec<crate::text::Run> {
            m.runs.iter().filter(|r| r.start < n).map(|r| crate::text::Run { end: r.end.min(n), ..*r }).collect()
        };
        let fonts = &mut self.fonts;
        let cut = typeset::elide(&m.text, TruncateMode::Tail, room, |t| {
            let n = t.encode_utf16().count();
            rich(fonts, t, &clip(&m, n)).lines.iter().map(|l| l.width).fold(0.0, f32::max)
        });
        let n = cut.encode_utf16().count();
        let lay = rich(&mut self.fonts, &cut, &clip(&m, n));
        let keep = n.saturating_sub(1);
        m.code.retain_mut(|c| {
            c.1 = c.1.min(keep);
            c.0 < c.1
        });
        m.links.retain_mut(|l| {
            l.1 = l.1.min(keep);
            l.0 < l.1
        });
        m.strike.retain_mut(|s| {
            s.1 = s.1.min(keep);
            s.0 < s.1
        });
        m.tags.retain_mut(|t| {
            t.end = t.end.min(keep);
            t.start < t.end
        });
        m.math.retain(|f| f.at < keep);
        m.text = cut;
        (Rc::new(lay), m)
    }

    /// Measure every text node layout asked about and hand the sizes back; how many changed (layout runs again while any do).
    pub fn typeset(&mut self, world: WorldRef<'_>) -> usize {
        let math = math_look(world);
        if math != self.math {
            // Every formula's hole is another size: shape and break the words again.
            self.math = math;
            self.shaped.clear();
            self.layouts.clear();
            self.widths.clear();
        }
        let wanted = typeset::wanted(world);
        let sizes: Vec<Intrinsic> = wanted
            .iter()
            .map(|t| {
                let one = self.layout_as(&t.text, &t.style, 1.0e5, t.markdown);
                let w = one.lines.iter().map(|l| l.width).fold(0.0, f32::max).ceil();
                let at = t.at.unwrap_or(w);
                let lay = if t.one_line || at + 0.5 >= w {
                    one
                } else {
                    self.layout_as(&t.text, &t.style, at, t.markdown)
                };
                let st = set_of(&t.style);
                let h = lay.lines.last().map_or(st.line, |l| l.top + l.height).max(st.line);
                let [base, last] =
                    [lay.lines.first(), lay.lines.last()].map(|l| l.map_or(0.0, |l| l.baseline));
                Intrinsic { w, h, at, base, last }
            })
            .collect();
        typeset::apply(world, &wanted, &sizes) + Self::measure_kips(world)
    }

    /// Each `Kip` node's box as its `Intrinsic` (kips.rs `measure`); how many changed.
    fn measure_kips(world: WorldRef<'_>) -> usize {
        let mut changed = Vec::new();
        world.new_query::<&Kip>().each_entity(|e, k| {
            let Some([w, h]) = crate::kips::measure(e, k) else { return };
            let now = e.try_cloned::<&Intrinsic>();
            if now.is_none_or(|i| (i.w - w).abs() > 0.01 || (i.h - h).abs() > 0.01) {
                changed.push((*e.id(), Intrinsic { w, h, at: w, base: 0.0, last: 0.0 }));
            }
        });
        for (id, i) in &changed {
            world.entity_from_id(*id).set(*i);
        }
        changed.len()
    }

    /// While nothing was drawn, `secs` went by: the Kips last in view move their clocks on (kips.rs). Whether one is due
    /// to blink or fidget now.
    pub fn rest_kips(&mut self, secs: f64) -> bool {
        self.kips.rest(secs)
    }

    /// After a frame: an overflowed atlas is emptied (true: draw again, its glyphs are gone) and a big layout cache dropped.
    pub fn end_frame(&mut self) -> bool {
        self.kip_wake = self.kips.end_frame();
        self.blocks_live = self.blocks.end_frame();
        self.lantern_live = self.lanterns.end_frame();
        self.arriving = self.arrivals.end_frame();
        self.gliding = self.carets.end_frame();
        self.shimmering = std::mem::take(&mut self.sheening);
        if self.cache.overflowed() {
            self.cache.clear();
            return true;
        }
        false
    }

    /// Draw node `e` as the draw pass painted it (`p`, at `laid`, with `held` its pressed state): a plate, words, a vector
    /// mark or a pixel-art picture, then the transition's scale and the clip it sits in. Err names the node and what this
    /// painter cannot draw of it.
    #[allow(clippy::too_many_arguments)]
    pub fn node(
        &mut self,
        d: &mut Draw,
        e: EntityView<'_>,
        p: &Painted,
        laid: Laid,
        dark: bool,
        held: bool,
        sc: f32,
        upload: &mut Upload<'_>,
    ) -> Result<Emitted, String> {
        let at = Emitted {
            panels: d.panels.len(),
            glyphs: d.glyphs.len(),
            images: d.images.len(),
            surfaces: d.surfaces.len(),
            lines: d.lines.len(),
            paths: d.paths.len(),
        };
        let rect = [laid.x, laid.y, laid.w, laid.h];
        let ink = e
            .try_cloned::<&Fill>()
            .map_or([0.9, 0.9, 0.92, 1.0], |f| rgba(hovered(e, if dark { f.dark } else { f.light }, false, dark)));
        match p.kind {
            PaintKind::Plate => {
                // At rest, on whole device pixels at this frame's scale (the zoom's): crisp, and lined up with the words
                // on it. One still moving (mid-slide, posed, in a transition) is drawn where it is, as moving glyphs are.
                // (Within a thousandth of a point: a settled spring is only that close to its target.)
                let near = |a: f32, b: f32| (a - b).abs() <= 1e-3;
                let rest = near(p.scale, 1.0)
                    && e.try_cloned::<&Laid>().is_some_and(|l| {
                        near(l.x, laid.x) && near(l.y, laid.y) && near(l.w, laid.w) && near(l.h, laid.h)
                    });
                let grid = |r: [f32; 4]| if rest { snap_rect(r, sc) } else { r };
                let rect = grid(rect);
                let fill = e.try_cloned::<&Fill>().unwrap_or_default();
                let edge = e.try_cloned::<&Edge>().unwrap_or_default();
                let mut shape = e.try_cloned::<&Shape>().unwrap_or_default();
                if shape.n <= 0.0 {
                    shape.n = 4.0;
                }
                let (fc, ec) = if dark { (fill.dark, edge.dark) } else { (fill.light, edge.light) };
                let (fc, ec) = (hovered(e, fc, false, dark), hovered(e, ec, true, dark));
                let mut panel = panel_from(rgba(fc), rgba(ec), shape, rect, p.alpha);
                if let Some(l) = e.try_cloned::<&Lift>() {
                    panel.fx = [l.blur, l.alpha, 0.0, 0.0];
                    panel.fx2[0] = l.dy;
                }
                if fc.a > 0.0 || ec.a > 0.0 || panel.fx[1] > 0.0 {
                    d.panel(panel);
                }
                let wash = |c: Rgba, r: [f32; 4], radius: f32, a: f32| {
                    panel_from(
                        rgba(c),
                        [0.0; 4],
                        Shape { radius, n: shape.n, border: 0.0, feather: 0.0 },
                        r,
                        p.alpha * a,
                    )
                };
                let hover = e
                    .try_cloned::<&HoverLook>()
                    .zip(e.try_cloned::<&HoverAmt>().map(|h| h.spring.x.clamp(0.0, 1.0)));
                if let Some((l, h)) = hover.filter(|(_, h)| *h > 0.004) {
                    let r = grid([
                        rect[0] - l.pad,
                        rect[1] - l.pad,
                        rect[2] + 2.0 * l.pad,
                        rect[3] + 2.0 * l.pad,
                    ]);
                    d.panel(wash(
                        if dark { l.dark } else { l.light },
                        r,
                        if l.radius < 0.0 { shape.radius + l.pad } else { l.radius },
                        h,
                    ));
                }
                if let Some(l) = e.try_cloned::<&PressLook>().filter(|_| held) {
                    d.panel(wash(if dark { l.dark } else { l.light }, rect, shape.radius, 1.0));
                }
                // A copied plate (a table's row): the selection's plate over it for as long as the copy's pulse lasts, its
                // light crossing it (its words catch that light as they are drawn: `copied_row`).
                if let Some(f) = e.try_cloned::<&CopyFlash>().filter(|f| f.whole && f.live()) {
                    let look = select::Look::of(e.world());
                    let plate = [select::Plate::of(rect[0], rect[1], rect[2], rect[3])];
                    let pulse = select::Pulse::of(&plate, &look, Some(f), dark);
                    select::emit(d, &plate, &look, look.colour(dark), &pulse, p.alpha * look.shown(f), sc);
                }
            }
            PaintKind::Glyphs => {
                let Some(text) = e.try_cloned::<&Text>() else { return Ok(at) };
                let style = e.try_cloned::<&TextStyle>().unwrap_or_default();
                let md = e.try_cloned::<&MarkdownText>();
                let edit = e.try_cloned::<&TextEdit>();
                let truncate = e
                    .try_cloned::<&Truncate>()
                    .map(|t| t.mode)
                    .or_else(|| e.has(OneLine::id()).then_some(TruncateMode::Tail));
                let mut shown_md = None;
                // Whether the words were cut to fit (their glyphs are then not the text's).
                let mut elided = false;
                // The width its lines break (or its one line is cut) at: the one its box has this frame, so words re-wrap
                // live as a fold or a resize moves it (only broken again: `layout_as` keeps them shaped).
                let room = laid.w.max(1.0) + 0.5;
                let lay = if let (Some(_), Some(_)) = (truncate, md) {
                    // Inline markdown on one line (a table's cell): its first line (`one_line`), cut at its
                    // end, its faces kept.
                    let (lay, m) = self.layout_md_cut(&harness_native_ui_text_api::one_line(text.s.as_str()), &style, room);
                    shown_md = Some(m);
                    lay
                } else if let Some(mode) = truncate {
                    // One line, never wrapped: its first line (`one_line`, as layout measured it), cut with an
                    // ellipsis (at its end, middle or start) to that width.
                    let cut = typeset::elide(&harness_native_ui_text_api::one_line(text.s.as_str()), mode, room, |t| {
                        self.layout(t, &style, 1.0e5)
                            .lines
                            .iter()
                            .map(|l| l.width)
                            .fold(0.0, f32::max)
                    });
                    elided = cut.as_str() != text.s.as_str();
                    self.layout(&cut, &style, 1.0e5)
                } else {
                    self.layout_as(text.s.as_str(), &style, room, md.is_some())
                };
                // Inline markdown's links, in the link ink (the text's own when MarkdownText says none).
                let shown_md = match (md, shown_md) {
                    (Some(_), Some(m)) => Some(m),
                    (Some(_), None) => Some(crate::markdown::inline(text.s.as_str())),
                    _ => None,
                };
                let link_ink = md.map(|m| if dark { m.link_dark } else { m.link_light }).filter(|c| c.a > 0.0).map(rgba);
                let links: Vec<(usize, usize)> = match (&shown_md, link_ink) {
                    (Some(m), Some(_)) => m.links.iter().map(|l| (l.0, l.1)).collect(),
                    _ => Vec::new(),
                };
                // Inline code's glyphs in the code ink, bold's in the strong ink (each the text's own when unset).
                let pick = |dark_c: Rgba, light_c: Rgba| {
                    let c = if dark { dark_c } else { light_c };
                    (c.a > 0.0).then(|| rgba(c))
                };
                let inline_code_ink = md.and_then(|m| pick(m.code_dark, m.code_light));
                let strong_ink = md.and_then(|m| pick(m.strong_dark, m.strong_light));
                // An inline component's quiet words after its label (a link's host), in `ui.inline.meta`'s ink.
                let meta_ink = shown_md
                    .as_ref()
                    .filter(|m| m.tags.iter().any(|t| t.meta.is_some()))
                    .map(|_| crate::inline::meta(e.world(), dark));
                let faced: Vec<(usize, usize, [f32; 4])> = shown_md
                    .as_ref()
                    .map(|m| {
                        m.runs
                            .iter()
                            .filter_map(|r| match r.face {
                                Face::InlineCode => inline_code_ink.map(|c| (r.start, r.end, c)),
                                Face::Bold | Face::BoldItalic => strong_ink.map(|c| (r.start, r.end, c)),
                                _ => None,
                            })
                            .chain(m.tags.iter().filter_map(|t| t.meta.zip(meta_ink).map(|((a, b), c)| (a, b, c))))
                            .collect()
                    })
                    .unwrap_or_default();
                // A Pose's `cut`: that share of its glyphs, from the end, not drawn yet (words typed on). Layout keeps
                // their room, so nothing moves as they arrive.
                let total: usize = lay.lines.iter().map(|l| l.glyphs.len()).sum();
                let cut = e.try_cloned::<&Pose>().map_or(0.0, |p| p.cut.clamp(0.0, 1.0));
                let limit = ((total as f32) * (1.0 - cut)).round() as usize;
                let time = e.world().try_get::<&Clock>(|c| c.time).unwrap_or(0.0);
                let inks = match e.try_cloned::<&Highlight>() {
                    Some(h) => {
                        let tokens = code_ink(e.world(), dark);
                        let names: Vec<String> = tokens.iter().map(|(n, _)| n.clone()).collect();
                        let spans = self
                            .highlight
                            .spans(h.file.as_str(), text.s.as_str(), &names)
                            .map_err(|err| format!("{}: {err}", e.path()))?;
                        Some((tokens, spans))
                    }
                    None => None,
                };
                let arrive = e.try_cloned::<&GlyphsIn>();
                // Words growing: each glyph fades in from the frame it came (GlyphsArrive), where it already lies.
                let came = e.try_cloned::<&GlyphsArrive>().map(|g| (g, self.arrivals.note(e.id().0, total, time, &g)));
                let mut look = |n: usize, g: &Glyph| {
                    if n >= limit {
                        return None;
                    }
                    let col = inks
                        .as_ref()
                        .and_then(|(tokens, spans)| {
                            spans
                                .iter()
                                .find(|s| (s.start..s.end).contains(&g.idx))
                                .map(|s| tokens[s.name].1)
                        })
                        .or_else(|| link_ink.filter(|_| links.iter().any(|l| (l.0..l.1).contains(&g.idx))))
                        .or_else(|| faced.iter().find(|f| (f.0..f.1).contains(&g.idx)).map(|f| f.2))
                        .unwrap_or(ink);
                    let (a, dy, heat) = arrive.map_or((1.0, 0.0, 0.0), |m| m.of(n, time));
                    let (a, dy) = match came.as_ref().and_then(|(g, c)| c.of(n).map(|at| g.of(at, time))) {
                        Some((fa, fdy)) => (a * fa, dy + fdy),
                        None => (a, dy),
                    };
                    let hot = arrive.map_or([0.0; 4], |m| rgba(m.hot));
                    let col = std::array::from_fn(|k| col[k] + (hot[k] - col[k]) * heat);
                    Some((col, p.alpha * a, dy))
                };
                // The selection, under the words: one joined shape over the rows it covers, pulsing after a copy, the
                // words it holds growing with it and catching its light (below).
                let mut grow = None;
                let mut lit = None;
                if let Some(t) = edit.filter(|t| t.anchor != t.caret) {
                    let (lo, hi) = (t.anchor.min(t.caret) as usize, t.anchor.max(t.caret) as usize);
                    let plates: Vec<select::Plate> = spans_of(&lay, lo, hi)
                        .into_iter()
                        .map(|r| select::Plate::of(laid.x + r[0], laid.y + r[1], r[2], r[3]))
                        .collect();
                    let look = select::Look::of(e.world());
                    let pulse = select::Pulse::of(&plates, &look, e.try_cloned::<&CopyFlash>(), dark);
                    grow = pulse.grow.map(|g| (lo, hi, g));
                    select::emit(d, &plates, &look, look.colour(dark), &pulse, p.alpha, sc);
                    lit = Some((pulse, plates));
                }
                // Words in a copied row (a table's) grow with its plate and catch its light.
                let row = copied_row(e).map(|(r, f)| {
                    let plate = [select::Plate::of(r[0], r[1], r[2], r[3])];
                    select::Pulse::of(&plate, &select::Look::of(e.world()), Some(f), dark)
                });
                let first_glyph = d.glyphs.len();
                // The stretches of its words a child marks (a `TextRange`: a table's match), a plate in the child's Fill and
                // Shape over each row they cover, under the words. Not on words cut to fit or set from markdown: their glyphs
                // are not the text's.
                if md.is_none() && !elided {
                    let mut marks = Vec::new();
                    e.each_child(|c| {
                        if let (Some(r), Some(f)) = (c.try_cloned::<&TextRange>(), c.try_cloned::<&Fill>()) {
                            marks.push((r, f, c.try_cloned::<&Shape>().unwrap_or_default()));
                        }
                    });
                    for (r, f, shape) in marks {
                        let ink = rgba(if dark { f.dark } else { f.light });
                        for s in spans_of(&lay, r.from as usize, r.to as usize) {
                            d.panel(panel_from(ink, [0.0; 4], shape, [laid.x + s[0], laid.y + s[1], s[2], s[3]], p.alpha));
                        }
                    }
                }
                // Plates under words set from markdown, drawn before the words so the words lie on them (a plate drawn after
                // would hide its words, as soon as its fill is opaque): code spans on their chip, a plate per line they
                // cover; inline components on `ui.inline.rest`'s plate (clear), the one the pointer is over (InlineHot) on
                // the hover or pressed plate with its label underlined. Their icons and marks are drawn over, below.
                if let Some(m) = md {
                    let size = set_of(&style).size;
                    let inset = size * crate::markdown::CHIP_INSET_EM;
                    let units: Vec<u16> = shown_md.as_ref().map_or_else(Vec::new, |m| m.text.encode_utf16().collect());
                    let chip = rgba(if dark { m.chip_dark } else { m.chip_light });
                    let radius = if m.chip_radius > 0.0 { m.chip_radius } else { crate::markdown::CHIP_RADIUS };
                    let code_shape = Shape { radius, n: 4.0, border: 0.0, feather: 0.0 };
                    // A stretch's plate, a rect per line it covers: its carets are half the room the typesetter left
                    // (chip_room) outside its glyphs, and the plate reaches `inset` past the glyphs either side, into that
                    // room.
                    let mut plate = |d: &mut Draw, lo: usize, hi: usize, fill: [f32; 4], shape: Shape| {
                        let (before, after) = crate::markdown::chip_room(&units, lo, hi, size);
                        let rects = spans_of(&lay, lo, hi);
                        let last = rects.len().saturating_sub(1);
                        for (k, r) in rects.into_iter().enumerate() {
                            let x0 = r[0] + if k == 0 { before * 0.5 } else { 0.0 } - inset;
                            let x1 = r[0] + r[2] - if k == last { after * 0.5 } else { 0.0 } + inset;
                            let rect = [laid.x + x0, laid.y + r[1] + 1.0, x1 - x0, r[3] - 2.0];
                            d.panel(panel_from(fill, [0.0; 4], shape, rect, p.alpha));
                        }
                    };
                    for &(lo, hi) in shown_md.as_ref().map_or(&[][..], |m| m.code.as_slice()) {
                        plate(d, lo, hi, chip, code_shape);
                    }
                    let tags = shown_md.as_ref().map_or(&[][..], |m| m.tags.as_slice());
                    if !tags.is_empty() {
                        let w = e.world();
                        let (_, ref_shape) = crate::inline::look(w, dark);
                        let hot = e.try_cloned::<&harness_native_ui_text_api::InlineHot>();
                        let rest = crate::inline::rest(w, dark);
                        for (n, t) in tags.iter().enumerate() {
                            let here = hot.filter(|h| h.tag as usize == n);
                            let fill = here.map_or(rest, |h| crate::inline::hot(w, h.pressed, dark));
                            if fill[3] > 0.0 && !t.tag.plain {
                                plate(d, t.start, t.end, fill, ref_shape);
                            }
                            if t.tag.plain && here.is_none() {
                                // A cited phrase at rest: a dotted underline under its words (a dot a device pixel
                                // tall, one on and two off), the sign that the pointer shows where they came from.
                                let (label_lo, label_hi) = t.label();
                                let under = crate::inline::underline(w, dark);
                                let px = 1.0 / sc.max(1.0);
                                let (dot, step) = ((1.5 * sc).round().max(1.0) * px, (4.0 * sc).round().max(2.0) * px);
                                let flat = Shape { radius: 0.0, n: 4.0, border: 0.0, feather: 0.0 };
                                for r in spans_of(&lay, label_lo, label_hi) {
                                    let y = ((laid.y + r[1] + r[3] - 3.0) * sc).round() / sc;
                                    let mut x = 0.0;
                                    while x + dot <= r[2] {
                                        d.panel(panel_from(under, [0.0; 4], flat, [laid.x + r[0] + x, y, dot, px], p.alpha));
                                        x += step;
                                    }
                                }
                            }
                            if here.is_some() {
                                // The label's underline: a hairline under its words, past the icon's hole, before its host
                                // and its mark.
                                let (label_lo, label_hi) = t.label();
                                let under = crate::inline::underline(w, dark);
                                for r in spans_of(&lay, label_lo, label_hi) {
                                    let y = laid.y + r[1] + r[3] - 3.0;
                                    let rect = [laid.x + r[0], (y * sc).round() / sc, r[2], 1.0 / sc.max(1.0)];
                                    let flat = Shape { radius: 0.0, n: 4.0, border: 0.0, feather: 0.0 };
                                    d.panel(panel_from(under, [0.0; 4], flat, rect, p.alpha));
                                }
                            }
                        }
                    }
                }
                let pen = emit_glyphs(
                    d,
                    &self.fonts,
                    &mut self.cache,
                    &lay,
                    (laid.x, laid.y),
                    sc,
                    upload,
                    grow,
                    &mut look,
                );
                // Formulas, over their holes: each in the ink, fade and arrival of the words it stands in (the hole's own
                // glyph's, as `look` gives it: a link's, bold's, a heading's), not drawn while its hole is cut (words typed on).
                let formulas = shown_md.as_ref().map_or(&[][..], |m| m.math.as_slice());
                if !formulas.is_empty() {
                    let size = set_of(&style).size;
                    let stroke = self.math.stroke;
                    let mut n = 0usize;
                    let mut stood: Vec<Option<(usize, Glyph)>> = vec![None; formulas.len()];
                    for g in lay.lines.iter().flat_map(|l| &l.glyphs) {
                        if let Ok(k) = formulas.binary_search_by_key(&g.idx, |f| f.at) {
                            stood[k] = Some((n, *g));
                        }
                        n += 1;
                    }
                    for (f, at) in formulas.iter().zip(stood) {
                        let shown = match at {
                            Some((n, g)) => look(n, &g),
                            None => Some((ink, p.alpha, 0.0)),
                        };
                        let Some((col, alpha, dy)) = shown else { continue };
                        let em = f.em(size, &self.math);
                        emit_math(d, &mut self.fonts, &mut self.cache, f, em, &lay, (laid.x, laid.y + dy), col, alpha, stroke, sc, upload);
                    }
                }
                if let Some((pulse, plates)) = &lit {
                    pulse.light_glyphs(&mut d.glyphs[first_glyph..], plates);
                }
                if let Some(pulse) = &row {
                    for g in &mut d.glyphs[first_glyph..] {
                        if let Some(gr) = pulse.grow {
                            g.rect = gr.rect(g.rect);
                        }
                        pulse.light_glyph(g);
                    }
                }
                // A sheen: its band of light through the words, each glyph lit by where its middle is (the widest line
                // the span it crosses). Words at rest (none, or one of strength 0) cost one lookup.
                if let Some(s) = e.try_cloned::<&Sheen>().filter(Sheen::live) {
                    let still = harness_native_api::reduced(e.world());
                    let span = lay.lines.iter().map(|l| l.width).fold(0.0, f32::max);
                    let lift = rgba(if dark { s.dark } else { s.light });
                    let t = time + s.offset(e.id().0);
                    for g in &mut d.glyphs[first_glyph..] {
                        // Colour glyphs (emoji) keep their own colours.
                        if g.kind[0] > 0.5 {
                            continue;
                        }
                        let k = s.lit(t, span, g.rect[0] + g.rect[2] * 0.5 - laid.x, still);
                        if k <= 0.0 {
                            continue;
                        }
                        for col in [&mut g.col_d, &mut g.col_l] {
                            for (c, to) in col.iter_mut().zip(lift).take(3) {
                                *c += (to - *c) * k;
                            }
                        }
                    }
                    self.sheening |= !still;
                }
                if md.is_some() {
                    let size = set_of(&style).size;
                    let units: Vec<u16> = shown_md.as_ref().map_or_else(Vec::new, |m| m.text.encode_utf16().collect());
                    // Struck-through words: a hairline over the glyphs of each line they cover, `ui.strike`'s look.
                    let struck = shown_md.as_ref().map_or(&[][..], |m| m.strike.as_slice());
                    if !struck.is_empty() {
                        let look = strike_look(e.world());
                        let thick = (look.w * sc).round().max(1.0) / sc;
                        let flat = Shape { radius: 0.0, n: 4.0, border: 0.0, feather: 0.0 };
                        let line_ink = [ink[0], ink[1], ink[2], ink[3] * look.alpha];
                        for &(lo, hi) in struck {
                            let ((x0, l0), (x1, l1)) = (lay.caret(lo), lay.caret(hi));
                            for (li, l) in lay.lines.iter().enumerate().take(l1 + 1).skip(l0) {
                                let a = if li == l0 { x0 } else { 0.0 };
                                let b = if li == l1 { x1 } else { l.width };
                                let y = laid.y + l.baseline - size * look.rise - thick * 0.5;
                                let rect = [laid.x + a, (y * sc).round() / sc, b - a, thick];
                                if b > a {
                                    d.panel(panel_from(line_ink, [0.0; 4], flat, rect, p.alpha));
                                }
                            }
                        }
                    }
                    // Inline components, over their plates (drawn under the words, above): the icon drawn in its hole in its
                    // ink (the look's, quiet, or its live state's: a merged pull request's purple), or its live picture
                    // there (a link's favicon); the mark in its own ink.
                    let tags = shown_md.as_ref().map_or(&[][..], |m| m.tags.as_slice());
                    if !tags.is_empty() {
                        let w = e.world();
                        let icon_style = TextStyle {
                            size: size * crate::markdown::REF_ICON_SIZE_EM,
                            line: set_of(&style).line,
                            ..TextStyle::default()
                        };
                        let mark_style = TextStyle { size: size * crate::markdown::MARK_SIZE_EM, ..icon_style };
                        for t in tags {
                            let (before, _) = crate::markdown::chip_room(&units, t.start, t.end, size);
                            // The hole is REF_ICON_EM wide: the icon at its start, the rest the gap before the label.
                            let Some(hole) = spans_of(&lay, t.start, t.start + 1).into_iter().next() else { continue };
                            // A cited phrase has no icon.
                            if t.tag.plain {
                                continue;
                            }
                            if let Some(pic) = crate::inline::picture(w, &t.tag) {
                                // Its live picture (a favicon), square, the icon's size, where the icon goes.
                                let s = icon_style.size;
                                let sheet = self.photo(&pic, 1000).map_err(|err| format!("{}: {}'s picture: {err}", e.path(), t.tag.name))?;
                                let rect = [laid.x + hole[0] + before * 0.5, laid.y + hole[1] + (hole[3] - s) * 0.5, s, s];
                                let hdr = sheet.peak.map_or([0.0; 4], |peak| [1.0, peak, 0.0, 0.0]);
                                let size = [sheet.w as f32, sheet.h as f32, p.alpha, 1.0];
                                d.image(ImageInst { rect, size, clip: [0.0; 4], clipx: [0.0; 4], light: [0.0; 4], hdr }, sheet.frames[0].clone());
                            } else {
                                match crate::inline::icon(w, &t.tag, dark) {
                                    crate::inline::ChipIcon::Glyph(glyph, icon_ink) => {
                                        let icon = self.layout(&glyph, &icon_style, 1.0e5);
                                        let gw = icon.lines.iter().map(|l| l.width).fold(0.0, f32::max);
                                        let gh = icon.lines.first().map_or(icon_style.line, |l| l.height);
                                        let x = laid.x + hole[0] + before * 0.5 + (icon_style.size - gw) * 0.5;
                                        let y = laid.y + hole[1] + (hole[3] - gh) * 0.5;
                                        emit_layout(d, &self.fonts, &mut self.cache, &icon, (x, y), icon_ink, p.alpha, sc, usize::MAX, upload);
                                    }
                                    crate::inline::ChipIcon::Drawing(name) => {
                                        // A file kind's drawing in its own colours, centred where the glyph would be. The
                                        // path pass is not scaled after (below), so a transition's scale is folded in here.
                                        let art = crate::drawing::get(name, dark).ok_or_else(|| format!("{}: file icon {name:?} is not one of drawing.rs's", e.path()))?;
                                        let k = if p.scale > 0.0 { p.scale } else { 1.0 };
                                        let s = size * crate::markdown::REF_DRAWING_EM;
                                        let cx = hole[0] + before * 0.5 + icon_style.size * 0.5;
                                        let cy = hole[1] + hole[3] * 0.5;
                                        let rect = [laid.x + (cx - s * 0.5) * k, laid.y + (cy - s * 0.5) * k, s * k, s * k];
                                        art.emit(d, rect, p.alpha, sc).map_err(|err| format!("{}: file icon {name:?}: {err}", e.path()))?;
                                    }
                                }
                            }
                            // The mark's hole is MARK_EM wide: the gap after the label, then the mark at its end. Nothing
                            // while its live state has none (unknown, or not asked yet).
                            let (Some(at), Some((mglyph, mink))) = (t.mark(), crate::inline::mark(w, &t.tag, dark)) else { continue };
                            let Some(mhole) = spans_of(&lay, at, at + 1).into_iter().next() else { continue };
                            let mlay = self.layout(&mglyph, &mark_style, 1.0e5);
                            let mw = mlay.lines.iter().map(|l| l.width).fold(0.0, f32::max);
                            let mh = mlay.lines.first().map_or(mark_style.line, |l| l.height);
                            let x = laid.x + mhole[0] + mhole[2] - mw;
                            let y = laid.y + mhole[1] + (mhole[3] - mh) * 0.5;
                            emit_layout(d, &self.fonts, &mut self.cache, &mlay, (x, y), mink, p.alpha, sc, usize::MAX, upload);
                        }
                    }
                }
                if let Some(t) = edit {
                    let look = crate::caret::look(e.world());
                    emit_edit(d, &mut self.carets, (e.id().0, &look), &lay, &t, laid, &style, dark, sc, p.alpha, time);
                }
                if let Some(c) =
                    e.try_cloned::<&Caret>().filter(|c| (c.at..c.until).contains(&time))
                {
                    // Solid while glyphs arrive (a cue on Pose.cut is running), then blinking.
                    let typing = limit < total;
                    let on = typing || c.blink <= 0.0 || ((time - c.at) / c.blink).fract() < 0.5;
                    if on {
                        let line = lay.lines.first().map_or(set_of(&style).line, |l| l.height);
                        let (x, top, h) = pen.unwrap_or((laid.x, laid.y, line));
                        let w = (style.size * 0.6 * sc).round() / sc;
                        let hh = (h * 0.82 * sc).round() / sc;
                        let y = ((top + (h - hh) * 0.5) * sc).round() / sc;
                        let square = Shape { radius: 0.0, n: 4.0, border: 0.0, feather: 0.0 };
                        d.panel(panel_from(
                            rgba(c.ink),
                            [0.0; 4],
                            square,
                            [(x * sc).round() / sc, y, w, hh],
                            p.alpha,
                        ));
                    }
                }
            }
            PaintKind::Icon => {
                // A ui.Symbol: its glyph (a Nerd Font icon) centred in its rect, 14 in a 16 box as SF Symbols sit in a row.
                let Some(icon) = e.try_cloned::<&harness_native_ui_text_api::Icon>() else {
                    return Ok(at);
                };
                let style = TextStyle {
                    size: laid.h.min(laid.w + 2.0) * 0.875,
                    line: laid.h,
                    ..TextStyle::default()
                };
                let lay = self.layout(icon.glyph.as_str(), &style, 1.0e5);
                let gw = lay.lines.iter().map(|l| l.width).fold(0.0, f32::max);
                emit_layout(
                    d,
                    &self.fonts,
                    &mut self.cache,
                    &lay,
                    (laid.x + (laid.w - gw) * 0.5, laid.y),
                    ink,
                    p.alpha,
                    sc,
                    usize::MAX,
                    upload,
                );
            }
            PaintKind::Mark => {
                // A ui.Icon: its vector mark (icons.rs) drawn from its outline, optically sized in its box.
                let Some(name) = e.try_cloned::<&Mark>() else { return Ok(at) };
                let Some(mark) = crate::icons::mark(name.name.as_str()) else {
                    return Err(format!(
                        "{}: Mark {:?} is not one of the marks icons.rs has",
                        e.path(),
                        name.name.as_str()
                    ));
                };
                let side = laid.w.min(laid.h);
                let (r, uv) = crate::icons::place(
                    mark,
                    laid.x + (laid.w - side) * 0.5,
                    laid.y + (laid.h - side) * 0.5,
                    side,
                );
                let col = [ink[0], ink[1], ink[2], ink[3] * p.alpha];
                d.glyph(GlyphInst {
                    rect: r,
                    uv,
                    col_d: col,
                    col_l: col,
                    anim: [-1.0e9, 30.0, 0.0, 1.0],
                    kind: [0.0, 0.0, 0.0, 1.0],
                    band: mark.band,
                    gdata: mark.gdata,
                    ..GlyphInst::default()
                });
            }
            PaintKind::Drawing => {
                // A ui.Art: its drawing (drawing.rs) in its own colours, the appearance's variant, square in its box. The
                // path pass is not scaled after (below): a transition's scale is folded into where it goes.
                // No name yet (a ui.Art the host has not filled) draws nothing, as an empty glyph does.
                let Some(name) = e.try_cloned::<&Drawing>().filter(|d| !d.name.as_str().is_empty()) else { return Ok(at) };
                let art = crate::drawing::get(name.name.as_str(), dark).ok_or_else(|| {
                    let names: Vec<&str> = crate::drawing::names().collect(); // alloc: the error's list
                    format!("{}: Drawing {:?} is not one of drawing.rs's ({})", e.path(), name.name.as_str(), names.join(", "))
                })?;
                let k = if p.scale > 0.0 { p.scale } else { 1.0 };
                let side = laid.w.min(laid.h) * k;
                let (cx, cy) = (laid.x + laid.w * 0.5 * k, laid.y + laid.h * 0.5 * k);
                art.emit(d, [cx - side * 0.5, cy - side * 0.5, side, side], p.alpha, sc)
                    .map_err(|err| format!("{}: Drawing {:?}: {err}", e.path(), name.name.as_str()))?;
            }
            PaintKind::Picture => self.picture(d, e, rect, p.alpha, dark, sc)?,
            PaintKind::Ribbon => {
                return Err(format!(
                    "{}: a Ribbon, which the shared painter does not draw yet",
                    e.path()
                ));
            }
            PaintKind::Vector => self.vector(d, e, p, laid, dark, sc, upload)?,
        }
        // A transition's scale: what this node drew, scaled about its corner.
        if (p.scale - 1.0).abs() > 1e-4 && p.scale > 0.0 {
            let (ox, oy, k) = (laid.x, laid.y, p.scale);
            for panel in &mut d.panels[at.panels..] {
                panel.rect = [
                    ox + (panel.rect[0] - ox) * k,
                    oy + (panel.rect[1] - oy) * k,
                    panel.rect[2] * k,
                    panel.rect[3] * k,
                ];
            }
            for g in &mut d.glyphs[at.glyphs..] {
                g.rect = [
                    ox + (g.rect[0] - ox) * k,
                    oy + (g.rect[1] - oy) * k,
                    g.rect[2] * k,
                    g.rect[3] * k,
                ];
            }
            for m in &mut d.images[at.images..] {
                m.rect = [
                    ox + (m.rect[0] - ox) * k,
                    oy + (m.rect[1] - oy) * k,
                    m.rect[2] * k,
                    m.rect[3] * k,
                ];
            }
            for m in &mut d.surfaces[at.surfaces..] {
                m.rect = [
                    ox + (m.rect[0] - ox) * k,
                    oy + (m.rect[1] - oy) * k,
                    m.rect[2] * k,
                    m.rect[3] * k,
                ];
                m.shape[0] *= k;
            }
        }
        // The clip it sits in, both ways: a scroll cuts its content at its top and foot and at its sides (a table that
        // scrolls sideways). A far edge of 0 reads as none in the shader, so a clip with nothing left in it keeps a sliver.
        if p.clipped {
            let far = |at: f32, len: f32| (at + len.max(0.0)).max(f32::MIN_POSITIVE);
            let band = [p.clip_y, far(p.clip_y, p.clip_h)];
            let across = [p.clip_x, far(p.clip_x, p.clip_w)];
            for panel in &mut d.panels[at.panels..] {
                panel.clip[..2].copy_from_slice(&band);
                panel.clipx[..2].copy_from_slice(&across);
            }
            for glyph in &mut d.glyphs[at.glyphs..] {
                glyph.clip[..2].copy_from_slice(&band);
                glyph.clipx[..2].copy_from_slice(&across);
            }
            for m in &mut d.images[at.images..] {
                m.clip[..2].copy_from_slice(&band);
                m.clipx[..2].copy_from_slice(&across);
            }
            for m in &mut d.surfaces[at.surfaces..] {
                m.clip[..2].copy_from_slice(&band);
                m.clipx[..2].copy_from_slice(&across);
            }
            for l in &mut d.lines[at.lines..] {
                l.clip[..2].copy_from_slice(&band);
                l.clipx[..2].copy_from_slice(&across);
            }
            for f in &mut d.paths[at.paths..] {
                f.clip[..2].copy_from_slice(&band);
                f.clipx[..2].copy_from_slice(&across);
            }
        }
        Ok(at)
    }

    /// A `Vector` node: its path data through its transform and view box into its box (less its insets), filled in its
    /// `Fill` (none when that is clear) and stroked in its `Stroke`, then its `VectorText` at its point. Strokes and fills
    /// are cut to device-pixel accuracy; a transition's scale is folded into the mapping (the caller scales only panels,
    /// glyphs and images).
    #[allow(clippy::too_many_arguments)]
    fn vector(
        &mut self,
        d: &mut Draw,
        e: EntityView<'_>,
        p: &Painted,
        laid: Laid,
        dark: bool,
        sc: f32,
        upload: &mut Upload<'_>,
    ) -> Result<(), String> {
        let Some(v) = e.try_cloned::<&Vector>() else { return Ok(()) };
        let pick = |c: (Rgba, Rgba), outline: bool| rgba(hovered(e, if dark { c.0 } else { c.1 }, outline, dark));
        let fill = e.try_cloned::<&Fill>().map(|f| pick((f.dark, f.light), false));
        let stroke = e.try_cloned::<&Stroke>().map(|s| pick((s.dark, s.light), true));
        let map = VectorMap::new(&v, laid, p.scale);
        let segs = if v.d.as_str().trim().is_empty() {
            None
        } else if let Some(s) = self.paths.get(v.d.as_str()) {
            Some(s.clone())
        } else {
            let segs: Rc<[crate::vector::Seg]> = crate::vector::segments(v.d.as_str())
                .map_err(|err| format!("{}: Vector path data: {err}", e.path()))?
                .into();
            if self.paths.len() > 4096 {
                self.paths.clear();
            }
            self.paths.insert(v.d.as_str().to_owned(), segs.clone()); // alloc: the parse cache, cleared past 4096 paths
            Some(segs)
        };
        if let Some(segs) = segs {
            // A quarter of a device pixel off the true curve at most.
            let tol = 0.25 / sc.max(0.1);
            let (rings, cut) = crate::vector::flatten(&segs, |q| map.at(q), tol, MAX_VECTOR_POINTS);
            if cut {
                return Err(format!(
                    "{}: Vector path flattens to more than {MAX_VECTOR_POINTS} points; draw it with fewer",
                    e.path()
                ));
            }
            if let Some(ink) = fill.filter(|c| c[3] > 0.0) {
                // alloc: this node's rings as plain polygons, for one frame
                let polys: Vec<Vec<[f32; 2]>> = rings.iter().map(|r| r.pts.clone()).collect();
                if let Some(inst) = PathInst::fill(&polys, v.even_odd, (ink, ink), p.alpha, &mut d.edges) {
                    d.path(inst);
                }
            }
            if let Some(ink) = stroke.filter(|c| c[3] > 0.0) {
                let w = if v.stroke_w > 0.0 { v.stroke_w } else { 1.0 };
                let w = if v.stroke_fixed { w * p.scale.max(0.0) } else { w * map.unit };
                for r in &rings {
                    let mut pts = r.pts.clone(); // alloc: one ring's stroke points, for one frame
                    if r.closed && pts.first() != pts.last() {
                        pts.push(pts[0]);
                    }
                    for l in LineInst::stroke(&pts, w, (ink, ink), p.alpha) {
                        d.line(l);
                    }
                }
            }
        }
        if let Some(t) = e.try_cloned::<&VectorText>() {
            let ink = fill.unwrap_or([0.9, 0.9, 0.92, 1.0]);
            let size = if t.size > 0.0 { t.size } else { 12.0 };
            // The text is drawn at the node's own scale (the caller scales its glyphs by a transition's).
            let size = if v.stroke_fixed { size } else { size * map.unit / p.scale.max(1e-3) };
            let style = TextStyle {
                size,
                line: (size * 1.25).ceil(),
                bold: i32::from(t.bold),
                ..TextStyle::default()
            };
            let lay = self.layout(t.s.as_str(), &style, 1.0e5);
            let w = lay.lines.iter().map(|l| l.width).fold(0.0, f32::max);
            let base = lay.lines.first().map_or(size, |l| l.baseline);
            let k = p.scale.max(1e-3);
            let [x, y] = map.at([f64::from(t.x), f64::from(t.y)]);
            // Undo the transition's scale here; the caller applies it to the glyphs about the node's corner.
            let (x, y) = (laid.x + (x - laid.x) / k, laid.y + (y - laid.y) / k);
            let x = x - w * match t.anchor {
                1 => 0.5,
                2 => 1.0,
                _ => 0.0,
            };
            emit_layout(d, &self.fonts, &mut self.cache, &lay, (x, y - base), ink, p.alpha, sc, usize::MAX, upload);
        }
        Ok(())
    }

    /// A picture, drawn as pixel art: one texture whose texels are fetched, never filtered, so each is a crisp block with no
    /// seam between neighbours at any size, offset or scale. A `Picture` is a PNG (`Pixelated`; with `Frames`, a sheet of
    /// frames stacked downwards, played from the `Clock` and looped); a `BlazeMark` is drawn live.
    #[allow(clippy::too_many_arguments)]
    /// The photo at absolute path `handle` (a PNG or JPEG) as a sheet, cut to the shape `milli` (width over height, in
    /// thousandths) about its middle, decoded once per path and shape.
    fn photo(&mut self, handle: &str, milli: u32) -> Result<Rc<Sheet>, String> {
        let key = format!("photo:{milli}:{handle}");
        if let Some(s) = self.sheets.get(&key) {
            return Ok(s.clone());
        }
        let ph = crate::photo::decode(std::path::Path::new(handle), Some(milli as f32 / 1000.0), true)?;
        let s = Rc::new(Sheet { w: ph.w, h: ph.h, frames: vec![ph.rgba], peak: ph.peak });
        self.sheets.insert(key, s.clone()); // alloc: the sheet cache, kept with the painter
        Ok(s)
    }

    fn picture(
        &mut self,
        d: &mut Draw,
        e: EntityView<'_>,
        rect: [f32; 4],
        alpha: f32,
        dark: bool,
        sc: f32,
    ) -> Result<(), String> {
        // A live picture from Media (a Shader's or a Video's frames): drawn by key; the renderer binds Media's latest
        // finished frame for it and skips the draw while there is none.
        if let Some(sf) = e.try_cloned::<&Surface>() {
            let mut shape = e.try_cloned::<&Shape>().unwrap_or_default();
            if shape.n <= 0.0 {
                shape.n = 4.0;
            }
            let inst = SurfaceInst {
                rect,
                shape: [shape.radius.max(0.0), shape.n, alpha, f32::from(u8::from(sf.cover != 0))],
                clip: [0.0; 4],
                clipx: [0.0; 4],
                uv: [0.0, 0.0, 1.0, 1.0],
                hdr: [0.0; 4],
            };
            d.surface(inst, sf.key);
            return Ok(());
        }
        let path = e.path();
        let time = e.world().try_get::<&Clock>(|c| c.time).unwrap_or(0.0);
        if let Some(k) = e.try_cloned::<&Kip>() {
            let laid = e.try_cloned::<&Laid>().map_or(rect, |l| [l.x, l.y, l.w, l.h]);
            return self.kips.paint(d, e, &k, rect, laid, alpha, dark, sc, time);
        }
        if let Some(s) = e.try_cloned::<&Lantern>() {
            self.lanterns.paint(d, e.id().0, s.on, s.lit, rect, alpha, dark, sc, time);
            return Ok(());
        }
        if let Some(b) = e.try_cloned::<&PixelBlock>() {
            self.blocks.paint(d, e.id().0, &b, rect, alpha, sc, time);
            return Ok(());
        }
        if let Some(m) = e.try_cloned::<&BlazeMark>() {
            let rest = e.try_cloned::<&Laid>().map_or(rect, |l| [l.x, l.y, l.w, l.h]);
            return self.blaze(d, e.id().0, m, rect, rest, alpha, dark, sc, time);
        }
        let Some(pic) = e.try_cloned::<&Picture>() else { return Ok(()) };
        if !e.has(Pixelated::id()) {
            // A photo (an absolute path to a PNG or JPEG): filtered, covering its box, cut to the box's shape about its
            // middle (layout gives a contained photo a box of its own shape).
            let handle = pic.handle.as_str();
            if !handle.starts_with('/') {
                return Err(format!(
                    "{path}: Picture {handle:?} without Pixelated: a photo is an absolute path, pixel art is Pixelated"
                ));
            }
            let rest = e.try_cloned::<&Laid>().map_or([rect[2], rect[3]], |l| [l.w, l.h]);
            if rest[0] <= 0.0 || rest[1] <= 0.0 {
                return Ok(());
            }
            // The shape to a thousandth, so a box that wobbles by a fraction of a point decodes once.
            let milli = (rest[0] / rest[1] * 1000.0).round().max(1.0) as u32;
            let sheet = self.photo(handle, milli).map_err(|err| format!("{path}: {err}"))?;
            // An HDR photo is drawn in its own range, tone-mapped to the display's headroom from its brightest.
            let hdr = sheet.peak.map_or([0.0; 4], |peak| [1.0, peak, 0.0, 0.0]);
            let img = ImageInst { rect, size: [sheet.w as f32, sheet.h as f32, alpha, 1.0], clip: [0.0; 4], clipx: [0.0; 4], light: [0.0; 4], hdr };
            d.image(img, sheet.frames[0].clone());
            return Ok(());
        }
        let frames = e.try_cloned::<&Frames>().unwrap_or(Frames { count: 1, fps: 0.0 });
        let sheet = self
            .sheet(pic.handle.as_str(), frames.count.max(1))
            .map_err(|err| format!("{path}: {err}"))?;
        let frame = ((time * frames.fps).floor().max(0.0) as usize) % sheet.frames.len();
        let img =
            ImageInst { rect, size: [sheet.w as f32, sheet.h as f32, alpha, 0.0], clip: [0.0; 4], clipx: [0.0; 4], light: [0.0; 4], hdr: [0.0; 4] };
        d.image(img, sheet.frames[frame].clone());
        Ok(())
    }

    /// A `BlazeMark` at frame time `time`: its cells as big as the node's width allows on whole device pixels, centred, with
    /// room above it up to the top of the view for the letters to fall through, shaken as they land.
    #[allow(clippy::too_many_arguments)]
    fn blaze(
        &mut self,
        d: &mut Draw,
        id: u64,
        m: BlazeMark,
        rect: [f32; 4],
        rest: [f32; 4],
        alpha: f32,
        dark: bool,
        sc: f32,
        time: f32,
    ) -> Result<(), String> {
        use harness_native_blaze::mark::{Blaze, knock};
        let mark = self.marks.entry((id, dark)).or_insert_with(|| Blaze::new(dark));
        if m.exit > m.start && !mark.burning() {
            mark.burn(m.exit - m.start);
        }
        // Drawn at its laid size, cells on whole device pixels, then scaled with the node as a whole (its Pose or a
        // transition), so it grows and shrinks smoothly and is exactly crisp at rest.
        let (cw, ch) = mark.cells();
        let px = ((rest[2] / cw as f32) * sc).floor().max(1.0);
        let cell = px / sc;
        let lead = (rest[1] / cell).ceil().max(0.0) as u32;
        let t = time - m.start;
        let img = mark.render(t, px, lead);
        let (kx, ky) = knock(t);
        let w = cw as f32 * cell;
        let x = ((rest[0] + (rest[2] - w) * 0.5 + kx * cell) * sc).round() / sc;
        let y = ((rest[1] + (rest[3] - ch as f32 * cell) * 0.5 - lead as f32 * cell + ky * cell) * sc).round() / sc;
        let k = if rest[2] > 0.0 { rect[2] / rest[2] } else { 1.0 };
        let place = |r: [f32; 4]| [rect[0] + (r[0] - rest[0]) * k, rect[1] + (r[1] - rest[1]) * k, r[2] * k, r[3] * k];
        let size = [img.w as f32, img.h as f32, alpha, 0.0];
        let at = place([x, y, img.w as f32 / sc, img.h as f32 / sc]);
        d.image(ImageInst { rect: at, size, clip: [0.0; 4], clipx: [0.0; 4], light: [0.0; 4], hdr: [0.0; 4] }, img.rgba.into());
        // Past white: the hot pixels again, as light over the picture, lifting them by heat up to `glow` whites. A scene
        // that sets its glow (a video's HDR10 master) gets that; otherwise the fire burns up to the display's headroom this
        // frame (1 on an SDR display: no light layer at all, the picture exactly as before).
        let glow = if m.glow > 0.0 { m.glow } else { d.frame.headroom() };
        if glow > 1.0 {
            let light = [glow, 0.0, 0.0, 0.0];
            d.image(ImageInst { rect: at, size, clip: [0.0; 4], clipx: [0.0; 4], light, hdr: [0.0; 4] }, img.light.into());
        }
        Ok(())
    }

    /// A picture by handle, a PNG under `media` of `count` frames stacked downwards, decoded and cut up once.
    fn sheet(&mut self, handle: &str, count: u32) -> Result<Rc<Sheet>, String> {
        if let Some(s) = self.sheets.get(handle) {
            return Ok(s.clone());
        }
        let Some(media) = &self.media else {
            return Err(format!("Picture {handle:?}: this host has no media directory"));
        };
        let path = media.join(handle);
        let file = std::fs::File::open(&path)
            .map_err(|e| format!("Picture {handle:?}: {}: {e}", path.display()))?;
        let mut dec = png::Decoder::new(std::io::BufReader::new(file));
        dec.set_transformations(
            png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
        );
        let mut reader =
            dec.read_info().map_err(|e| format!("Picture {handle:?}: {}: {e}", path.display()))?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| format!("Picture {handle:?}: {}: {e}", path.display()))?;
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
            png::ColorType::GrayscaleAlpha => buf[..info.buffer_size()]
                .chunks_exact(2)
                .flat_map(|c| [c[0], c[0], c[0], c[1]])
                .collect(),
            other => {
                return Err(format!(
                    "Picture {handle:?}: {}: {other:?} pixels after expanding to 8-bit with alpha",
                    path.display()
                ));
            }
        };
        if info.height % count != 0 {
            return Err(format!(
                "Picture {handle:?}: {} pixels tall, not a whole number of its {count} Frames",
                info.height
            ));
        }
        let h = info.height / count;
        let per = (info.width * h * 4) as usize;
        let frames = rgba.chunks_exact(per).map(Rc::from).collect();
        let sheet = Rc::new(Sheet { w: info.width, h, frames, peak: None });
        self.sheets.insert(handle.to_owned(), sheet.clone());
        Ok(sheet)
    }
}

/// `ui.formula`'s look (prims.flecs): LaTeX math's sizes and strokes. The scene is compiled in, so one without it is a bug:
/// this panics naming it.
fn math_look(w: WorldRef<'_>) -> MathLook {
    w.try_lookup("ui::formula")
        .unwrap_or_else(|| panic!("the scene has no ui::formula (prims.flecs)"))
        .try_cloned::<&MathLook>()
        .unwrap_or_else(|| panic!("ui::formula has no MathLook"))
}

/// Formula `f` of words laid out in `lay` with their origin at `(ox, oy)`, drawn over its hole at `em` points to its em, in
/// `ink` (a `\color` it chose wins) at `alpha`: its glyphs from the bundled KaTeX fonts through the atlas, on the quarter-
/// pixel grid text is placed on; its rules as plates, at least a device pixel thick and on the pixel grid while thin; its
/// outlines (radicals, tall brackets, stretchy arrows) rasterised into the atlas, strokes `stroke` ems wide.
#[allow(clippy::too_many_arguments)]
fn emit_math(
    d: &mut Draw,
    fonts: &mut Fonts,
    cache: &mut GlyphCache,
    f: &InlineMath,
    em: f32,
    lay: &Layout,
    (ox, oy): (f32, f32),
    ink: [f32; 4],
    alpha: f32,
    stroke: f32,
    sc: f32,
    upload: &mut Upload<'_>,
) {
    let (left, line) = lay.caret(f.at);
    let Some(row) = lay.lines.get(line) else { return };
    let set = &f.set;
    let (x0, top) = (ox + left, oy + row.baseline - set.height * em);
    let asz = cache.size as f32;
    let own = |c: Option<[f32; 4]>| c.unwrap_or(ink);
    // The pixel an origin at `x` points falls in, and the quarter of it: as text is placed.
    let across = |x: f32| {
        let xp = x * sc;
        let ix = xp.floor();
        (ix, (((xp - ix) * SUBPX as f32).floor() as u32).min(SUBPX - 1))
    };
    let put = |d: &mut Draw, e: AtlasGlyph, ix: f32, base: f32, c: [f32; 4]| {
        let col = [c[0], c[1], c[2], c[3] * alpha];
        d.glyph(GlyphInst {
            rect: [(ix + e.left as f32) / sc, (base - e.top as f32) / sc, e.w as f32 / sc, e.h as f32 / sc],
            uv: [e.x as f32 / asz, e.y as f32 / asz, (e.x + e.w) as f32 / asz, (e.y + e.h) as f32 / asz],
            col_d: col,
            col_l: col,
            anim: [-1.0e9, 30.0, 0.0, 1.0],
            kind: [0.0; 4],
            ..Default::default()
        });
    };
    // Rules first (a box's ground under what it holds, a bar beside its numerator), then the marks over them: one run of
    // plates and one of glyphs, not a draw call for each.
    let rules = set.items.iter().filter(|i| matches!(i, Item::Rule { .. }));
    let marks = set.items.iter().filter(|i| !matches!(i, Item::Rule { .. }));
    for item in rules.chain(marks) {
        match item {
            Item::Glyph { font, ch, at, em: k, ink: c } => {
                // A character the bundled font lacks (CJK or emoji in `\text{}`) draws nothing, as RaTeX's own fonts do.
                let Some(id) = fonts.math_font(*font) else { continue };
                let Some(glyph) = fonts.math_glyph(id, *ch) else { continue };
                let (ix, sub) = across(x0 + at[0] * em);
                let base = ((top + at[1] * em) * sc).round();
                let Some(e) = cache.get(fonts, id, glyph, em * k * sc, sub, upload) else { continue };
                if e.w > 0 {
                    put(d, e, ix, base, own(*c));
                }
            }
            Item::Rule { rect: [rx, ry, rw, rh], ink: c } => {
                let (xl, xr) = (x0 + rx * em, x0 + (rx + rw) * em);
                let (yt, yb) = (top + ry * em, top + (ry + rh) * em);
                // A rule is at least a device pixel thick and, while it is thin, sits on the pixel grid so it is as sharp as
                // a stem; a box's ground is left where it is.
                let thick = ((yb - yt) * sc).max(1.0);
                let rect = if thick < 3.0 {
                    let t = thick.round().max(1.0);
                    let y = ((yt + yb) * 0.5 * sc - t * 0.5).round();
                    let (a, b) = ((xl * sc).round(), (xr * sc).round());
                    [a / sc, y / sc, (b - a).max(1.0) / sc, t / sc]
                } else {
                    [xl, yt, xr - xl, yb - yt]
                };
                let flat = Shape { radius: 0.0, n: 4.0, border: 0.0, feather: 0.0 };
                d.panel(panel_from(own(*c), [0.0; 4], flat, rect, alpha));
            }
            Item::Path { at, segs, fill, ink: c, key } => {
                let (ix, sub) = across(x0 + at[0] * em);
                let base = ((top + at[1] * em) * sc).round();
                let Some(e) = cache.outline(*key, segs, *fill, stroke, em * sc, sub, upload) else { continue };
                if e.w > 0 {
                    put(d, e, ix, base, own(*c));
                }
            }
        }
    }
}

/// `ui.strike`'s look (prims.flecs): struck-through words' hairline. The scene is compiled in, so one without it is a bug:
/// this panics naming it.
fn strike_look(w: WorldRef<'_>) -> StrikeLook {
    w.try_lookup("ui::strike")
        .unwrap_or_else(|| panic!("the scene has no ui::strike (prims.flecs)"))
        .try_cloned::<&StrikeLook>()
        .unwrap_or_else(|| panic!("ui::strike has no StrikeLook"))
}

/// The rects (x, y from the layout's origin, w, h) of UTF-16 units `lo..hi` of `lay`, one per line they cover.
pub fn spans_of(lay: &Layout, lo: usize, hi: usize) -> Vec<[f32; 4]> {
    let mut out = Vec::new();
    if lo >= hi || lay.lines.is_empty() {
        return out;
    }
    let (x0, l0) = lay.caret(lo);
    let (x1, l1) = lay.caret(hi);
    for li in l0..=l1.min(lay.lines.len() - 1) {
        let l = &lay.lines[li];
        let a = if li == l0 { x0 } else { 0.0 };
        let b = if li == l1 { x1 } else { l.width };
        if b > a {
            out.push([a, l.top, b - a, l.height]);
        }
    }
    out
}

/// A `TextEdit`'s marked text's underline and caret over its words' layout (its selection is drawn under the words). The
/// caret is den's (caret.rs, `ui.caret`'s look): a bar centred on the caret's place, swept when it jumps, its blink the
/// shader's from when it last moved (`TextEdit::since`), node `node`'s glide kept in `carets`.
#[allow(clippy::too_many_arguments)]
fn emit_edit(
    d: &mut Draw, carets: &mut crate::caret::Carets, (node, look): (u64, &harness_native_ui_look_api::CaretLook), lay: &Layout,
    t: &TextEdit, laid: Laid, style: &TextStyle, dark: bool, sc: f32, alpha: f32, time: f32,
) {
    let square = Shape { radius: 0.0, n: 4.0, border: 0.0, feather: 0.0 };
    let snap = |v: f32| (v * sc).round() / sc;
    let (lo, hi) = (t.anchor.min(t.caret) as usize, t.anchor.max(t.caret) as usize);
    let ink = rgba(if dark { t.caret_dark } else { t.caret_light });
    if t.mark_hi > t.mark_lo {
        for r in spans_of(lay, t.mark_lo as usize, t.mark_hi as usize) {
            let y = snap(laid.y + r[1] + r[3] - 2.0);
            d.panel(panel_from(ink, [0.0; 4], square, [laid.x + r[0], y, r[2], 1.0], alpha));
        }
    }
    if t.focused && lo == hi {
        let set = set_of(style);
        let (x, top, line) = if lay.lines.is_empty() {
            (0.0, 0.0, set.line)
        } else {
            let (x, li) = lay.caret(t.caret as usize);
            let l = &lay.lines[li];
            (x, l.top, l.height)
        };
        // Whole device pixels wide (3 on a Retina display at 1.5 points) and on the pixel grid at rest: crisp, never a
        // smear between two; `h` of the font's size about its line's middle, as den's.
        let w = (look.w * sc).round().max(1.0) / sc;
        let h = snap(set.size * look.h).min(line);
        let rest = [snap(laid.x + x - w * 0.5) - laid.x, snap(laid.y + top + (line - h) * 0.5) - laid.y, w, h];
        let bar = crate::caret::Bar {
            rest,
            at: (laid.x, laid.y),
            // The bar and its sweep stay in the text's box (the caret's width past either side, for one at an edge).
            clip: [laid.x - w, laid.y, laid.w + 2.0 * w, laid.h.max(lay.height())],
            ink,
            alpha,
            moved: t.since,
            blink: t.blink,
        };
        d.panel(carets.panel(node, &bar, look, time));
    }
}

/// Seconds the terminal's cursor stays solid after it moved, and a whole blink's period (a text field's caret is
/// caret.rs's, its timings `ui.caret`'s).
pub const BLINK_PAUSE: f32 = 0.5;
pub const BLINK_PERIOD: f32 = 1.2;

/// How long a blink's turn on or off takes, seconds: short, so the caret reads as on or off, not as pulsing.
pub const BLINK_FADE: f32 = 0.08;

/// A blinking caret's opacity `since` seconds after it last moved: solid for [`BLINK_PAUSE`], then every [`BLINK_PERIOD`]
/// on for half of it and off for the other half, each turn a [`BLINK_FADE`] ease (macOS's caret, softened by a frame).
#[must_use]
pub fn caret_blink(since: f32) -> f32 {
    let t = since - BLINK_PAUSE;
    if t <= 0.0 {
        return 1.0;
    }
    let half = BLINK_PERIOD * 0.5;
    let phase = t % BLINK_PERIOD;
    // Off from `half`, back on from the period's end: the distance into each turn, as a share of its fade.
    let k = if phase < half { 1.0 - ((phase - half + BLINK_FADE) / BLINK_FADE).clamp(0.0, 1.0) } else {
        ((phase - BLINK_PERIOD + BLINK_FADE) / BLINK_FADE).clamp(0.0, 1.0)
    };
    k * k * (3.0 - 2.0 * k)
}

/// A `Vector`'s user units into points: its transform, then its view box fitted into its node's box less its insets,
/// then a transition's scale about the node's corner.
struct VectorMap {
    m: [f64; 6],
    sx: f64,
    sy: f64,
    tx: f64,
    ty: f64,
    /// Points per user unit (the geometric mean of both axes and the transform): what a scaling stroke is multiplied by.
    unit: f32,
}

impl VectorMap {
    fn new(v: &Vector, laid: Laid, scale: f32) -> Self {
        let m = if v.ma == 0.0 && v.mb == 0.0 && v.mc == 0.0 && v.md == 0.0 {
            [1.0, 0.0, 0.0, 1.0, f64::from(v.me), f64::from(v.mf)]
        } else {
            [v.ma, v.mb, v.mc, v.md, v.me, v.mf].map(f64::from)
        };
        let k = f64::from(if scale > 0.0 { scale } else { 1.0 });
        let (bx, by) = (f64::from(laid.x + v.inset_l), f64::from(laid.y + v.inset_t));
        let bw = f64::from((laid.w - v.inset_l - v.inset_r).max(0.0));
        let bh = f64::from((laid.h - v.inset_t - v.inset_b).max(0.0));
        let (vx, vy, vw, vh) = if v.view_w > 0.0 && v.view_h > 0.0 {
            (f64::from(v.view_x), f64::from(v.view_y), f64::from(v.view_w), f64::from(v.view_h))
        } else {
            (0.0, 0.0, bw.max(1e-6), bh.max(1e-6))
        };
        let (mut sx, mut sy) = (bw / vw, bh / vh);
        let (mut ox, mut oy) = (bx, by);
        if v.fit == VectorFit::Meet {
            let s = sx.min(sy);
            ox += (bw - vw * s) * 0.5;
            oy += (bh - vh * s) * 0.5;
            (sx, sy) = (s, s);
        }
        let (lx, ly) = (f64::from(laid.x), f64::from(laid.y));
        // Into the box, then about the node's corner by the transition's scale.
        let tx = lx + (ox - vx * sx - lx) * k;
        let ty = ly + (oy - vy * sy - ly) * k;
        let det = (m[0] * m[3] - m[1] * m[2]).abs();
        let unit = ((sx * sy).abs().sqrt() * det.sqrt() * k) as f32;
        Self { m, sx: sx * k, sy: sy * k, tx, ty, unit }
    }

    fn at(&self, p: [f64; 2]) -> [f32; 2] {
        let m = &self.m;
        let (ux, uy) = (m[0] * p[0] + m[2] * p[1] + m[4], m[1] * p[0] + m[3] * p[1] + m[5]);
        [(self.tx + ux * self.sx) as f32, (self.ty + uy * self.sy) as f32]
    }
}

#[cfg(test)]
mod tests {
    use super::snap_rect;

    /// At 2 pixels a point a half-point plate is already on the grid and stays; at a zoom of 1.1 (2.2) a selection pill
    /// on a row at 137.5 lands on whole pixels, a plate under it still meets it, and a thin hairline keeps a pixel.
    #[test]
    fn plates_snap_to_device_pixels_at_any_zoom() {
        assert_eq!(snap_rect([10.5, 137.5, 228.0, 24.0], 2.0), [10.5, 137.5, 228.0, 24.0]);
        let sc = 2.2;
        let pill = snap_rect([10.5, 137.5, 228.0, 24.0], sc);
        for v in pill {
            assert!(((v * sc) - (v * sc).round()).abs() < 1e-3, "{pill:?} off the grid");
        }
        let under = snap_rect([10.5, 161.5, 228.0, 24.0], sc);
        assert!((pill[1] + pill[3] - under[1]).abs() < 1e-4, "{pill:?} and {under:?} no longer meet");
        let hair = snap_rect([0.0, 100.3, 50.0, 0.3], 1.34);
        assert!(hair[3] * 1.34 >= 1.0 - 1e-4, "a hairline vanished: {hair:?}");
    }
}
