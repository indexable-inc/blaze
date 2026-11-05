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
use harness_native_ui_look_api::{Edge, Fill, HoverLook, Lift, PressLook, Rgba, Shape, TextStyle};
use harness_native_ui_motion_api::{CopyFlash, Pose};
use harness_native_ui_node_api::Laid;
use harness_native_ui_text_api::{
    BlazeMark, Caret, Frames, Highlight, Mark, MarkdownText, OneLine, Picture, Pixelated, Text,
    TextEdit, Truncate, TruncateMode,
};
use harness_native_ui_timeline_api::GlyphsIn;

use crate::draw::{ATLAS_SIZE, Draw};
use crate::select;
use crate::pack::{GlyphInst, ImageInst, PanelInst};
use crate::text::{Face, Fonts, Glyph, GlyphCache, Layout, SUBPX, Style, TypeLook};

/// The atlas uploader a frame is drawn with: `(x, y, w, h, bytes)` into the glyph atlas.
pub type Upload<'a> = dyn FnMut(u32, u32, u32, u32, &[u8]) + 'a;

/// A picture decoded for drawing as pixel art: its frames, each `w` by `h` straight RGBA8.
struct Sheet {
    w: u32,
    h: u32,
    frames: Vec<Rc<[u8]>>,
}

/// The host's fonts, glyph atlas and layout cache, and the pictures it has decoded.
pub struct Painter {
    pub fonts: Fonts,
    pub cache: GlyphCache,
    layouts: HashMap<(String, u32, u32, bool, u8, u32), Rc<Layout>>,
    /// One-line widths [`Painter::width`] measured, by text and style: cheap entries, kept longer than layouts.
    widths: HashMap<(String, u32, bool, u8), f32>,
    sheets: HashMap<String, Rc<Sheet>>,
    /// Each `BlazeMark` node's fire, by entity and appearance: its automaton runs on from frame to frame.
    marks: HashMap<(u64, bool), harness_native_blaze::mark::Blaze>,
    highlight: crate::highlight::Highlighter,
    /// Where a `Picture`'s handle is a path under (the scene's directory); None: this host draws no pictures.
    pub media: Option<PathBuf>,
}

/// What [`Painter::node`] did with a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Emitted {
    /// The first panel and glyph it added to the `Draw` (what came before it is untouched).
    pub panels: usize,
    pub glyphs: usize,
    pub images: usize,
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
    emit_glyphs(d, fonts, cache, lay, at, sc, upload, &mut |n, _| {
        (n < limit).then_some((ink, alpha, 0.0))
    });
}

/// A layout's glyphs at `(ox, oy)`, each as `look(n, glyph)` says (its ink, opacity and how far below its place it sits,
/// points), stopping at the first it gives None. Glyphs in place snap to the device pixel grid with den's subpixel steps;
/// one still moving is drawn where it is. Where the pen stands after the last one drawn (x, its line's top and height),
/// in points, if any was.
#[allow(clippy::too_many_arguments)]
pub fn emit_glyphs(
    d: &mut Draw,
    fonts: &Fonts,
    cache: &mut GlyphCache,
    lay: &Layout,
    (ox, oy): (f32, f32),
    sc: f32,
    upload: &mut Upload<'_>,
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
            d.glyph(GlyphInst {
                rect: [
                    (ix + e.left as f32) / sc,
                    (base_px - e.top as f32) / sc,
                    e.w as f32 / sc,
                    e.h as f32 / sc,
                ],
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
            layouts: HashMap::new(),
            widths: HashMap::new(),
            sheets: HashMap::new(),
            marks: HashMap::new(),
            highlight: crate::highlight::Highlighter::default(),
            media: None,
        }
    }

    /// `text` in `style`, broken to `width` points, cached by all three.
    pub fn layout(&mut self, text: &str, style: &TextStyle, width: f32) -> Rc<Layout> {
        self.layout_as(text, style, width, false)
    }

    /// [`Painter::layout`], of inline markdown when `md` (`MarkdownText`: the markers out, the faces they ask for in).
    pub fn layout_as(&mut self, text: &str, style: &TextStyle, width: f32, md: bool) -> Rc<Layout> {
        let st = set_of(style);
        let code = style.role == 9;
        let tabular = style.role == 10;
        let medium = !code && !st.bold && style.weight == 500;
        let key = (
            text.to_owned(),
            st.size.to_bits(),
            st.line.to_bits(),
            st.bold,
            u8::from(code) | u8::from(medium) << 1 | u8::from(md) << 2 | u8::from(tabular) << 3,
            width.to_bits(),
        );
        if let Some(l) = self.layouts.get(&key) {
            return l.clone();
        }
        let lay = Rc::new(if md {
            let m = crate::markdown::inline(text);
            Layout::rich(&mut self.fonts, &m.text, &m.runs, &[], st, 0.0, width)
        } else if code {
            Layout::in_face(&mut self.fonts, text, Face::Code, st, width)
        } else if tabular {
            Layout::in_face(&mut self.fonts, text, Face::Tabular, st, width)
        } else if medium {
            Layout::in_face(&mut self.fonts, text, Face::Medium, st, width)
        } else {
            Layout::new(&mut self.fonts, text, st, width)
        });
        self.layouts.insert(key, lay.clone());
        lay
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
        let rich = |fonts: &mut Fonts, text: &str, runs: &[crate::text::Run]| Layout::rich(fonts, text, runs, &[], st, 0.0, 1.0e5);
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
        m.text = cut;
        (Rc::new(lay), m)
    }

    /// Measure every text node layout asked about and hand the sizes back; how many changed (layout runs again while any do).
    pub fn typeset(&mut self, world: WorldRef<'_>) -> usize {
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
        typeset::apply(world, &wanted, &sizes)
    }

    /// After a frame: an overflowed atlas is emptied (true: draw again, its glyphs are gone) and a big layout cache dropped.
    pub fn end_frame(&mut self) -> bool {
        if self.layouts.len() > 4096 {
            self.layouts.clear();
        }
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
        let at = Emitted { panels: d.panels.len(), glyphs: d.glyphs.len(), images: d.images.len() };
        let rect = [laid.x, laid.y, laid.w, laid.h];
        let ink = e
            .try_cloned::<&Fill>()
            .map_or([0.9, 0.9, 0.92, 1.0], |f| rgba(if dark { f.dark } else { f.light }));
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
                // A copied plate (a table's row): the selection's plate over it for as long as the copy's pulse lasts.
                if let Some(f) = e.try_cloned::<&CopyFlash>().filter(|f| f.whole && f.amount() > 0.0) {
                    let look = select::Look::of(e.world());
                    let plate = [select::Plate::of(rect[0], rect[1], rect[2], rect[3])];
                    select::emit(d, &plate, &look, look.colour(dark), Some(f), p.alpha * f.amount(), sc);
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
                let lay = if let (Some(_), Some(_)) = (truncate, md) {
                    // Inline markdown on one line (a table's cell): cut at its end, its faces kept.
                    let (lay, m) = self.layout_md_cut(text.s.as_str(), &style, laid.w.max(1.0) + 0.5);
                    shown_md = Some(m);
                    lay
                } else if let Some(mode) = truncate {
                    // One line, never wrapped: cut with an ellipsis (at its end, middle or start) to the width layout gave it.
                    let room = laid.w.max(1.0) + 0.5;
                    let cut = typeset::elide(text.s.as_str(), mode, room, |t| {
                        self.layout(t, &style, 1.0e5)
                            .lines
                            .iter()
                            .map(|l| l.width)
                            .fold(0.0, f32::max)
                    });
                    self.layout(&cut, &style, 1.0e5)
                } else {
                    self.layout_as(text.s.as_str(), &style, laid.w.max(1.0) + 0.5, md.is_some())
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
                        .unwrap_or(ink);
                    let (a, dy, heat) = arrive.map_or((1.0, 0.0, 0.0), |m| m.of(n, time));
                    let hot = arrive.map_or([0.0; 4], |m| rgba(m.hot));
                    let col = std::array::from_fn(|k| col[k] + (hot[k] - col[k]) * heat);
                    Some((col, p.alpha * a, dy))
                };
                // The selection, under the words: one joined shape over the rows it covers, pulsing after a copy.
                if let Some(t) = edit.filter(|t| t.anchor != t.caret) {
                    let (lo, hi) = (t.anchor.min(t.caret) as usize, t.anchor.max(t.caret) as usize);
                    let plates: Vec<select::Plate> = spans_of(&lay, lo, hi)
                        .into_iter()
                        .map(|r| select::Plate::of(laid.x + r[0], laid.y + r[1], r[2], r[3]))
                        .collect();
                    let look = select::Look::of(e.world());
                    select::emit(d, &plates, &look, look.colour(dark), e.try_cloned::<&CopyFlash>(), p.alpha, sc);
                }
                let pen = emit_glyphs(
                    d,
                    &self.fonts,
                    &mut self.cache,
                    &lay,
                    (laid.x, laid.y),
                    sc,
                    upload,
                    &mut look,
                );
                if let Some(m) = md {
                    // Code spans on a chip, a plate per line they cover.
                    let chip = rgba(if dark { m.chip_dark } else { m.chip_light });
                    let shape = Shape { radius: 3.0, n: 4.0, border: 0.0, feather: 0.0 };
                    for &(lo, hi) in shown_md.as_ref().map_or(&[][..], |m| m.code.as_slice()) {
                        for r in spans_of(&lay, lo, hi) {
                            let rect = [laid.x + r[0] - 3.0, laid.y + r[1] + 1.0, r[2] + 6.0, r[3] - 2.0];
                            d.panel(panel_from(chip, [0.0; 4], shape, rect, p.alpha));
                        }
                    }
                }
                if let Some(t) = edit {
                    emit_edit(d, &lay, &t, laid, &style, dark, sc, p.alpha, time);
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
            PaintKind::Picture => self.picture(d, e, rect, p.alpha, dark, sc)?,
            PaintKind::Ribbon => {
                return Err(format!(
                    "{}: a Ribbon, which the shared painter does not draw yet",
                    e.path()
                ));
            }
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
        }
        Ok(at)
    }

    /// A picture, drawn as pixel art: one texture whose texels are fetched, never filtered, so each is a crisp block with no
    /// seam between neighbours at any size, offset or scale. A `Picture` is a PNG (`Pixelated`; with `Frames`, a sheet of
    /// frames stacked downwards, played from the `Clock` and looped); a `BlazeMark` is drawn live.
    #[allow(clippy::too_many_arguments)]
    fn picture(
        &mut self,
        d: &mut Draw,
        e: EntityView<'_>,
        rect: [f32; 4],
        alpha: f32,
        dark: bool,
        sc: f32,
    ) -> Result<(), String> {
        let path = e.path();
        let time = e.world().try_get::<&Clock>(|c| c.time).unwrap_or(0.0);
        if let Some(m) = e.try_cloned::<&BlazeMark>() {
            let rest = e.try_cloned::<&Laid>().map_or(rect, |l| [l.x, l.y, l.w, l.h]);
            return self.blaze(d, e.id().0, m, rect, rest, alpha, dark, sc, time);
        }
        let Some(pic) = e.try_cloned::<&Picture>() else { return Ok(()) };
        if !e.has(Pixelated::id()) {
            return Err(format!(
                "{path}: Picture {:?} without Pixelated: only pixel art is drawn so far",
                pic.handle.as_str()
            ));
        }
        let frames = e.try_cloned::<&Frames>().unwrap_or(Frames { count: 1, fps: 0.0 });
        let sheet = self
            .sheet(pic.handle.as_str(), frames.count.max(1))
            .map_err(|err| format!("{path}: {err}"))?;
        let frame = ((time * frames.fps).floor().max(0.0) as usize) % sheet.frames.len();
        let img =
            ImageInst { rect, size: [sheet.w as f32, sheet.h as f32, alpha, 0.0], clip: [0.0; 4], clipx: [0.0; 4] };
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
        d.image(ImageInst { rect: place([x, y, img.w as f32 / sc, img.h as f32 / sc]), size, clip: [0.0; 4], clipx: [0.0; 4] }, img.rgba.into());
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
        let sheet = Rc::new(Sheet { w: info.width, h, frames });
        self.sheets.insert(handle.to_owned(), sheet.clone());
        Ok(sheet)
    }
}

/// The rects (x, y from the layout's origin, w, h) of UTF-16 units `lo..hi` of `lay`, one per line they cover.
fn spans_of(lay: &Layout, lo: usize, hi: usize) -> Vec<[f32; 4]> {
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

/// A `TextEdit`'s marked text's underline and caret bar over its words' layout (its selection is drawn under the words).
#[allow(clippy::too_many_arguments)]
fn emit_edit(d: &mut Draw, lay: &Layout, t: &TextEdit, laid: Laid, style: &TextStyle, dark: bool, sc: f32, alpha: f32, time: f32) {
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
        let line = set_of(style).line;
        let (x, top, h) = if lay.lines.is_empty() {
            (0.0, 0.0, line)
        } else {
            let (x, li) = lay.caret(t.caret as usize);
            let l = &lay.lines[li];
            (x, l.top, l.height)
        };
        let w = (2.0 * sc).round().max(1.0) / sc;
        let rect = [snap(laid.x + x), snap(laid.y + top + 1.0), w, snap(h - 2.0)];
        let on = if t.blink { caret_blink(time - t.since) } else { 1.0 };
        if on > 0.004 {
            d.panel(panel_from(ink, [0.0; 4], square, rect, alpha * on));
        }
    }
}

/// Seconds a caret stays solid after it moved, and a whole blink's period (the terminal's cursor and a text area's caret).
pub const BLINK_PAUSE: f32 = 0.5;
pub const BLINK_PERIOD: f32 = 1.2;

/// A blinking caret's opacity `since` seconds after it last moved: solid for [`BLINK_PAUSE`], then a smooth fade in and out
/// every [`BLINK_PERIOD`].
#[must_use]
pub fn caret_blink(since: f32) -> f32 {
    let t = since - BLINK_PAUSE;
    if t <= 0.0 {
        return 1.0;
    }
    let v = (t * std::f32::consts::TAU / BLINK_PERIOD).cos();
    let k = (v + 0.5).clamp(0.0, 1.0);
    k * k * (3.0 - 2.0 * k)
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
