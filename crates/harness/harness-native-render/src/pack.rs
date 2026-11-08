//! Packed instance records, byte for byte the structs in shaders/harness.slang (copied from den-native's pack.rs, github.com/andrewgazelka/den). The scene is flattened into these once;
//! per-frame CPU-written buffers are rings (see gpu.rs).
use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct Frame {
    pub view: [f32; 2],
    pub scale: f32,
    pub time: f32,
    pub dark: f32,
    /// Device pixels to a point this frame. Unlike `scale` (the zoom stepped to) it follows the eased zoom, so an outline
    /// glyph's half-pixel dilation matches the pixels it lands on while the zoom runs.
    pub live: f32,
    /// x: the display's EDR headroom this frame (hdr.rs, the one home of it): the most linear light over SDR reference
    /// white it can show now (1 on an SDR display or target). Everything that draws past white reads it here: HDR pictures
    /// tone-map to it, the blaze mark's fire lifts to it, glowing words (textfx.rs) go up to it. y: how long a word's
    /// motion lasts in seconds (0 under Reduce Motion: nothing moves). Read it through [`Frame::headroom`].
    pub textfx: [f32; 2],
    /// x text gamma dark, y text gamma light, z frosted glass saturation, w unused.
    pub tune: [f32; 4],
    /// Outline glyphs (`OutlineLook`): x emboldening in ems a side, y coverage gamma dark, z light, w the emboldening's
    /// limit in device pixels.
    pub slug: [f32; 4],
    /// x, y: the view point the target's top-left corner shows (0 for the window). A windowed overlay's own window shows
    /// only its part of the view: `view` is then its size and `place` its corner, and the instances stay in the view's
    /// points. z, w unused.
    pub place: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct PanelInst {
    pub rect: [f32; 4],
    pub fill_d: [f32; 4],
    pub fill_l: [f32; 4],
    pub edge_d: [f32; 4],
    pub edge_l: [f32; 4],
    /// radius, squircle exponent, border width, feather.
    pub shape: [f32; 4],
    /// shadow blur, shadow alpha, glass, kind (0 panel, 2 compress glyph, 3 meter ring: a circle inside the square rect, a
    /// track of the edge colour shape.z thick and over it an arc of the fill clockwise from twelve for the share fx2.z,
    /// 4 caret (caret.rs): edge_d the sweep's start rect, edge_l the bar at rest, fx2.z the move time, shape.y the sweep's seconds,
    /// shape.z the seconds it stays solid after it moved, shape.w the share of a blink each turn eases over, fx.y the
    /// blink's cycle in seconds, fx2.w when it last moved (the scene's CaretLook), lens the box it stays in,
    /// 5 copy light (select.rs `Pulse`): a soft band of the fill over the row, its middle through lens.xy (view points),
    /// leaning back lens.w points a point down, lens.z either side (0: the whole row), fx2.z the share of it that adds to
    /// what is under, fx2.w the square-corner mask, 6 image,
    /// 8 chevron, 9 stopwatch: begun at fx2.z (frame time), lasting lens.x seconds, 10 the light on an icon button's glass:
    /// a soft highlight of the fill at lens.xy from the rect's middle, lens.z across, and the rim facing it, 11 invisible
    /// ink, 12 an agent's art (ui/agentgrid.rs): the fill its ink over the edge as its ground, lens its clock, calm,
    /// brightness and density, fx3 which piece (0 threads, 1 dots, 2 contours) and three seeds in 0..1, fx2.w a task's lanes
    /// and which work (lanes + 8 x mask), 13 an agent's art from the art cache, drawn as a `SegKind::Art`: fx3 its uv rect
    /// there, lens.x its brightness, the fill its ink over the edge).
    pub fx: [f32; 4],
    /// shadow dy, opacity, kind parameter (a liquid glass panel's frost), the square-corner mask (1 top-left, 2 top-right,
    /// 4 bottom-right, 8 bottom-left: a selection's rows joined into one shape).
    pub fx2: [f32; 4],
    /// Liquid glass on a glass panel: bevel (points, 0 for none), magnify, dispersion, rim light.
    pub lens: [f32; 4],
    /// Frosted plate (`fx.z > 0`): sheen (the inner highlight's alpha, 0 for none), saturation (0 for the frame's),
    /// 0, 0. Image (kind 6): 0, 0, an HDR picture's peak (linear, 0 or 1 for SDR), the display's headroom. Meter ring
    /// (kind 3): where its arc starts in turns past twelve (a spinner moves it), 1 for round ends (0 square), 0, 0.
    pub fx3: [f32; 4],
    /// The band it shows in, cut across at the pixel: top, foot (view points; a foot of 0 is none), then how far it fades
    /// in from each (points; 0 a hard cut: a scroll's edge with more beyond it fades). What it keeps is drawn as it would
    /// be whole (a card going under the title bar keeps its sides, not a new rounded top).
    pub clip: [f32; 4],
    /// The same across: the band's left and right edges (view points; a right of 0 is none), then how far it fades in from
    /// each (points; 0 a hard cut). A sideways scroll (a table's) cuts its content here.
    pub clipx: [f32; 4],
}

/// One piece of a stroked line: the segment `a` to `b` with round caps, drawn anti-aliased. A polyline is one instance per
/// segment, each naming the ends of its neighbours so the joins are round and blend once (see `line_fs`); a dot is a
/// segment whose ends are the same point.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct LineInst {
    /// ax, ay, bx, by (points, y down).
    pub seg: [f32; 4],
    /// The previous segment's start and the next segment's end: prev.x, prev.y, next.x, next.y.
    pub around: [f32; 4],
    pub col_d: [f32; 4],
    pub col_l: [f32; 4],
    /// Half the line's width (points), 1 when a segment comes before this one, 1 when one comes after, opacity.
    pub shape: [f32; 4],
    /// The band it shows in, as a panel's `clip` (top, foot), then how far it fades in from each (0: a hard cut).
    pub clip: [f32; 4],
    /// The band across, as a panel's `clipx` (left, right, then the fades).
    pub clipx: [f32; 4],
}

/// A filled path (copied from den-native's pack.rs): one instance over its bounding box, reading its run of straight
/// edges from `Draw::edges`; curves are flattened on the CPU (vector.rs). Each pixel takes its winding number and its
/// distance to the nearest edge, so the fill is anti-aliased at any scale.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct PathInst {
    /// The box round the path (points, y down).
    pub rect: [f32; 4],
    pub col_d: [f32; 4],
    pub col_l: [f32; 4],
    /// First edge, edge count, rule (0 nonzero, 1 even-odd), opacity.
    pub shape: [f32; 4],
    /// The band it shows in, as a panel's `clip`.
    pub clip: [f32; 4],
    /// The band across, as a panel's `clipx`.
    pub clipx: [f32; 4],
}

impl PathInst {
    /// The fill of the closed `rings` (each a polygon in points; the last point joins the first), each ring's edges
    /// appended to `edges`. None when there is nothing to fill.
    #[must_use]
    pub fn fill(
        rings: &[Vec<[f32; 2]>],
        even_odd: bool,
        ink: ([f32; 4], [f32; 4]),
        alpha: f32,
        edges: &mut Vec<[f32; 4]>,
    ) -> Option<Self> {
        let first = edges.len();
        let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
        for ring in rings.iter().filter(|r| r.len() >= 3) {
            for (i, a) in ring.iter().enumerate() {
                let b = ring[(i + 1) % ring.len()];
                if *a != b {
                    edges.push([a[0], a[1], b[0], b[1]]);
                }
                lo = [lo[0].min(a[0]), lo[1].min(a[1])];
                hi = [hi[0].max(a[0]), hi[1].max(a[1])];
            }
        }
        let count = edges.len() - first;
        (count > 0).then(|| Self {
            rect: [lo[0], lo[1], hi[0] - lo[0], hi[1] - lo[1]],
            col_d: ink.0,
            col_l: ink.1,
            shape: [first as f32, count as f32, f32::from(u8::from(even_odd)), alpha],
            clip: [0.0; 4],
            clipx: [0.0; 4],
        })
    }
}

impl LineInst {
    /// The pieces of a line `width` points thick through `points` (in order), in the colours `ink` (dark, light) at
    /// `alpha`: round at its ends and at every bend. Repeated points count once; a single point is a dot that wide.
    #[must_use]
    pub fn stroke(
        points: &[[f32; 2]],
        width: f32,
        ink: ([f32; 4], [f32; 4]),
        alpha: f32,
    ) -> Vec<Self> {
        let mut pts: Vec<[f32; 2]> = Vec::with_capacity(points.len());
        for p in points {
            if pts.last() != Some(p) {
                pts.push(*p);
            }
        }
        let piece =
            |a: [f32; 2], b: [f32; 2], before: Option<[f32; 2]>, after: Option<[f32; 2]>| {
                let (prev, next) = (before.unwrap_or(a), after.unwrap_or(b));
                Self {
                    seg: [a[0], a[1], b[0], b[1]],
                    around: [prev[0], prev[1], next[0], next[1]],
                    col_d: ink.0,
                    col_l: ink.1,
                    shape: [
                        width * 0.5,
                        f32::from(u8::from(before.is_some())),
                        f32::from(u8::from(after.is_some())),
                        alpha,
                    ],
                    clip: [0.0; 4],
                    clipx: [0.0; 4],
                }
            };
        match pts.as_slice() {
            [] => Vec::new(),
            [only] => vec![piece(*only, *only, None, None)],
            _ => (0..pts.len() - 1)
                .map(|i| {
                    piece(
                        pts[i],
                        pts[i + 1],
                        i.checked_sub(1).map(|j| pts[j]),
                        pts.get(i + 2).copied(),
                    )
                })
                .collect(),
        }
    }
}

/// One procedural cell (boxdraw.rs, `sprite_fs`): a block element, box-drawing line, braille pattern, sextant or
/// powerline arrow drawn from its geometry on the device-pixel grid, so neighbouring cells meet with no seam.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct SpriteInst {
    /// The cell: x, y, w, h in device pixels (whole pixels, so its edges are exact).
    pub rect: [i32; 4],
    /// The shape's kind and two parameters (`boxdraw::Shape`), then a light line's thickness in device pixels.
    pub shape: [u32; 4],
    /// The ink, straight RGBA (sRGB).
    pub fg: [f32; 4],
    /// The ground under it, straight RGBA (alpha 0: none, what is already drawn shows).
    pub bg: [f32; 4],
}

/// A picture drawn as pixel art: its own texture (bound per draw, not an atlas) stretched over `rect`, each texel a crisp
/// block, never smoothed.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct ImageInst {
    /// x, y, w, h in points.
    pub rect: [f32; 4],
    /// The texture's width and height in texels, the opacity, then 1 to filter it smoothly (a glow) rather than fetch
    /// texels (pixel art).
    pub size: [f32; 4],
    /// The band it shows in, as a panel's `clip` (top, foot), then how far it fades in from each (0: a hard cut).
    pub clip: [f32; 4],
    /// The band across, as a panel's `clipx` (left, right, then the fades).
    pub clipx: [f32; 4],
    /// A light layer (x > 1): the texture is a picture's light (its colour, and in alpha how hot each pixel is), drawn
    /// over that picture to lift each pixel, in linear light, to up to `x` times its colour; it adds and covers nothing.
    /// 0 for a plain picture.
    pub light: [f32; 4],
    /// An HDR picture (x = 1): its texels are premultiplied linear light in half floats (8 bytes a texel; 1.0 SDR
    /// reference white, hdr.rs), tone-mapped to the frame's headroom from its brightest, y (linear; 0: unknown). All 0 for
    /// sRGB-encoded RGBA8 texels.
    pub hdr: [f32; 4],
}

impl ImageInst {
    /// Bytes a texel of its texture: 8 for an HDR picture's half floats, 4 for RGBA8.
    #[must_use]
    pub fn texel_bytes(&self) -> usize {
        if self.hdr[0] > 0.5 { 8 } else { 4 }
    }
}

/// A live picture from the host's Media (a Shader's or a Video's latest finished frame, media/): its texture, premultiplied,
/// sampled smoothly at `uv` over `rect`, cut to the node's rounded (squircle) corners. One instance a draw, as an image's.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct SurfaceInst {
    /// x, y, w, h in points.
    pub rect: [f32; 4],
    /// Corner radius (points), squircle exponent n, opacity, cover (1: cut to the box's shape about its middle; 0: the
    /// whole texture stretched over the box).
    pub shape: [f32; 4],
    /// The band it shows in, as a panel's `clip` (top, foot), then how far it fades in from each (0: a hard cut).
    pub clip: [f32; 4],
    /// The band across, as a panel's `clipx` (left, right, then the fades).
    pub clipx: [f32; 4],
    /// The part of the texture shown: u0, v0, u1, v1. The renderer fills it from the texture's size (gpu.rs).
    pub uv: [f32; 4],
    /// How the texture encodes light, filled by the renderer from Media (gpu.rs): x the transfer (`hdr::Transfer`: 0 sRGB,
    /// drawn as it is; 1 PQ, 2 HLG: decoded to linear and tone-mapped to the frame's headroom), y the picture's brightest
    /// (linear over reference white; 0: unknown), z 1 for BT.2020 primaries, w 0.
    pub hdr: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
pub struct GlyphInst {
    /// x, y, w, h in points: where the bitmap goes, or an outline glyph's ink box.
    pub rect: [f32; 4],
    /// A bitmap's atlas rect (u0, v0, u1, v1); an outline glyph's ink box in ems, its top-left corner then its
    /// bottom-right (x0, y1, x1, y0: y up in ems, so the second is the higher), so a crop of `rect` crops it alike.
    pub uv: [f32; 4],
    pub col_d: [f32; 4],
    pub col_l: [f32; 4],
    /// birth time, settle omega, rise, opacity.
    pub anim: [f32; 4],
    /// colour (1 for a colour bitmap such as an emoji, drawn as it is rather than tinted), how far its top is below its
    /// line's, its line's height (points; both 0 off a layout; the shader reads neither), outline (1 for a glyph drawn
    /// from its Bézier curves: `band` and `gdata` find them).
    pub kind: [f32; 4],
    /// The band it shows in, as a panel's `clip` (top, foot), then how far a bitmap glyph is turned about its middle
    /// (radians, clockwise: an icon button's glyph, ui/iconfx.rs), and for words that swell or shrink (`fx`) how far
    /// below `rect`'s top they grow from (points: their optical middle), else 0.
    pub clip: [f32; 4],
    /// Outline glyphs: band scale x, scale y, offset x, offset y (em space to band indices).
    pub band: [f32; 4],
    /// Outline glyphs: where its band header starts in the band buffer, its greatest vertical (x) and horizontal (y)
    /// band index, 0.
    pub gdata: [u32; 4],
    /// Expressive text (textfx.rs, ui/textfx.rs): when its motion began (frame time), its code (`Fx::code`: the motion, 16
    /// for glow, its place in its words from 32 up), where its words swell from (x, points right of `rect.x`), and its
    /// em (points). All 0 for plain text.
    pub fx: [f32; 4],
    /// How far it fades in from its band's top and foot (points; 0: a hard cut), 0, 0.
    pub fade: [f32; 4],
    /// The band across, as a panel's `clipx` (left, right, then the fades).
    pub clipx: [f32; 4],
}
