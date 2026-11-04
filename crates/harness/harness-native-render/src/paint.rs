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
use harness_native_ui_motion_api::Pose;
use harness_native_ui_node_api::Laid;
use harness_native_ui_text_api::{
    BlazeMark, Frames, Mark, OneLine, Picture, Pixelated, Text, Truncate, TruncateMode,
};

use crate::draw::{ATLAS_SIZE, Draw};
use crate::pack::{GlyphInst, ImageInst, PanelInst};
use crate::text::{Face, Fonts, GlyphCache, Layout, SUBPX, Style, TypeLook};

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
    sheets: HashMap<String, Rc<Sheet>>,
    /// Each `BlazeMark` node's fire, by entity and appearance: its automaton runs on from frame to frame.
    marks: HashMap<(u64, bool), harness_native_blaze::mark::Blaze>,
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
        shape: [shape.radius, shape.n, shape.border, shape.feather],
        fx: [0.0; 4],
        fx2: [0.0, alpha, 0.0, 0.0],
        lens: [0.0; 4],
        fx3: [0.0; 4],
        clip: [0.0; 4],
    }
}

/// A layout's first `limit` glyphs at `(ox, oy)` in `ink`, snapped to the device pixel grid with den's subpixel steps.
#[allow(clippy::too_many_arguments)]
pub fn emit_layout(
    d: &mut Draw,
    fonts: &Fonts,
    cache: &mut GlyphCache,
    lay: &Layout,
    (ox, oy): (f32, f32),
    ink: [f32; 4],
    alpha: f32,
    sc: f32,
    limit: usize,
    upload: &mut Upload<'_>,
) {
    let asz = cache.size as f32;
    let mut col = ink;
    col[3] *= alpha;
    let mut n = 0usize;
    for line in &lay.lines {
        for g in &line.glyphs {
            if n >= limit {
                return;
            }
            n += 1;
            let base_px = ((oy + line.baseline - g.y) * sc).round();
            let xp = (ox + g.x) * sc;
            let ix = xp.floor();
            let sub = (((xp - ix) * SUBPX as f32).floor() as u32).min(SUBPX - 1);
            let Some(e) = cache.get(fonts, g.font, g.id, sc, sub, upload) else { continue };
            if e.w == 0 {
                continue;
            }
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
}

impl Painter {
    #[must_use]
    pub fn new(look: TypeLook) -> Self {
        Self {
            fonts: Fonts::new(look),
            cache: GlyphCache::new(ATLAS_SIZE),
            layouts: HashMap::new(),
            sheets: HashMap::new(),
            marks: HashMap::new(),
            media: None,
        }
    }

    /// `text` in `style`, broken to `width` points, cached by all three.
    pub fn layout(&mut self, text: &str, style: &TextStyle, width: f32) -> Rc<Layout> {
        let st = set_of(style);
        let code = style.role == 9;
        let medium = !code && !st.bold && style.weight == 500;
        let key = (
            text.to_owned(),
            st.size.to_bits(),
            st.line.to_bits(),
            st.bold,
            u8::from(code) | u8::from(medium) << 1,
            width.to_bits(),
        );
        if let Some(l) = self.layouts.get(&key) {
            return l.clone();
        }
        let lay = Rc::new(if code {
            Layout::in_face(&mut self.fonts, text, Face::Code, st, width)
        } else if medium {
            Layout::in_face(&mut self.fonts, text, Face::Medium, st, width)
        } else {
            Layout::new(&mut self.fonts, text, st, width)
        });
        self.layouts.insert(key, lay.clone());
        lay
    }

    /// Measure every text node layout asked about and hand the sizes back; how many changed (layout runs again while any do).
    pub fn typeset(&mut self, world: WorldRef<'_>) -> usize {
        let wanted = typeset::wanted(world);
        let sizes: Vec<Intrinsic> = wanted
            .iter()
            .map(|t| {
                let one = self.layout(&t.text, &t.style, 1.0e5);
                let w = one.lines.iter().map(|l| l.width).fold(0.0, f32::max).ceil();
                let at = t.at.unwrap_or(w);
                let lay = if t.one_line || at + 0.5 >= w {
                    one
                } else {
                    self.layout(&t.text, &t.style, at)
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
                    let r = [
                        rect[0] - l.pad,
                        rect[1] - l.pad,
                        rect[2] + 2.0 * l.pad,
                        rect[3] + 2.0 * l.pad,
                    ];
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
            }
            PaintKind::Glyphs => {
                let Some(text) = e.try_cloned::<&Text>() else { return Ok(at) };
                let style = e.try_cloned::<&TextStyle>().unwrap_or_default();
                let truncate = e
                    .try_cloned::<&Truncate>()
                    .map(|t| t.mode)
                    .or_else(|| e.has(OneLine::id()).then_some(TruncateMode::Tail));
                let lay = if let Some(mode) = truncate {
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
                    self.layout(text.s.as_str(), &style, laid.w.max(1.0) + 0.5)
                };
                // A Pose's `cut`: that share of its glyphs, from the end, not drawn yet (words typed on). Layout keeps
                // their room, so nothing moves as they arrive.
                let total: usize = lay.lines.iter().map(|l| l.glyphs.len()).sum();
                let cut = e.try_cloned::<&Pose>().map_or(0.0, |p| p.cut.clamp(0.0, 1.0));
                let limit = ((total as f32) * (1.0 - cut)).round() as usize;
                emit_layout(
                    d,
                    &self.fonts,
                    &mut self.cache,
                    &lay,
                    (laid.x, laid.y),
                    ink,
                    p.alpha,
                    sc,
                    limit,
                    upload,
                );
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
        if p.clipped {
            let band = [p.clip_y, p.clip_y + p.clip_h];
            for panel in &mut d.panels[at.panels..] {
                panel.clip[..2].copy_from_slice(&band);
            }
            for glyph in &mut d.glyphs[at.glyphs..] {
                glyph.clip[..2].copy_from_slice(&band);
            }
            for m in &mut d.images[at.images..] {
                m.clip[..2].copy_from_slice(&band);
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
            return self.blaze(d, e.id().0, m, rect, alpha, dark, sc, time);
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
            ImageInst { rect, size: [sheet.w as f32, sheet.h as f32, alpha, 0.0], clip: [0.0; 4] };
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
        let (cw, ch) = mark.cells();
        let px = ((rect[2] / cw as f32) * sc).floor().max(1.0);
        let cell = px / sc;
        let lead = (rect[1] / cell).ceil().max(0.0) as u32;
        let t = time - m.start;
        let img = mark.render(t, px, lead);
        let (kx, ky) = knock(t);
        let w = cw as f32 * cell;
        let x = ((rect[0] + (rect[2] - w) * 0.5 + kx * cell) * sc).round() / sc;
        let y = ((rect[1] + (rect[3] - ch as f32 * cell) * 0.5 - lead as f32 * cell + ky * cell)
            * sc)
            .round()
            / sc;
        let size = [img.w as f32, img.h as f32, alpha, 0.0];
        d.image(
            ImageInst { rect: [x, y, img.w as f32 / sc, img.h as f32 / sc], size, clip: [0.0; 4] },
            img.rgba.into(),
        );
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
