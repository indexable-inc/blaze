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
    /// Expressive text (textfx.rs): how far past SDR white glowing words go (1 on an SDR display or layer), and how long a
    /// word's motion lasts in seconds (0 under Reduce Motion: nothing moves).
    pub textfx: [f32; 2],
    /// x text gamma dark, y text gamma light, z frosted glass saturation, w unused.
    pub tune: [f32; 4],
    /// Outline glyphs (`OutlineLook`): x emboldening in ems a side, y coverage gamma dark, z light, w the emboldening's
    /// limit in device pixels.
    pub slug: [f32; 4],
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
    /// 4 caret: edge_d the sweep's start rect, edge_l the bar at rest, fx2.z the move time, shape.y the sweep's seconds,
    /// shape.z the seconds it stays solid after it moved, fx.y the blink's cycle in seconds (the scene's CaretLook),
    /// 5 copy light: a band across the row at fx2.z of the way, fx2.w the square-corner mask, 6 image,
    /// 8 chevron, 9 stopwatch: begun at fx2.z (frame time), lasting lens.x seconds, 10 the light on an icon button's glass:
    /// a soft highlight of the fill at lens.xy from the rect's middle, lens.z across, and the rim facing it, 11 invisible
    /// ink, 12 an agent's art (ui/agentgrid.rs): the fill its ink over the edge as its ground, lens its clock, calm,
    /// brightness and density, fx3 which piece (0 threads, 1 dots, 2 contours) and three seeds in 0..1, fx2.w a task's lanes
    /// and which work (lanes + 8 x mask), 13 an agent's art from the art cache, drawn as a `SegKind::Art`: fx3 its uv rect
    /// there, lens.x its brightness, the fill its ink over the edge).
    pub fx: [f32; 4],
    /// shadow dy, opacity, kind parameter (a liquid glass panel's frost), 0.
    pub fx2: [f32; 4],
    /// Liquid glass on a glass panel: bevel (points, 0 for none), magnify, dispersion, rim light.
    pub lens: [f32; 4],
    /// Frosted plate (`fx.z > 0`): sheen (the inner highlight's alpha, 0 for none), saturation (0 for the frame's),
    /// 0, 0. Image (kind 6): 0, 0, an HDR picture's peak (linear, 0 or 1 for SDR), the display's headroom. Meter ring
    /// (kind 3): where its arc starts in turns past twelve (a spinner moves it), 1 for round ends (0 square), 0, 0.
    pub fx3: [f32; 4],
    /// The band it shows in, cut across at the pixel: top, foot (view points; a foot of 0 is none), 0, 0. What it keeps
    /// is drawn as it would be whole (a card going under the title bar keeps its sides, not a new rounded top).
    pub clip: [f32; 4],
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
}

impl LineInst {
    /// The pieces of a line `width` points thick through `points` (in order), in the colours `ink` (dark, light) at
    /// `alpha`: round at its ends and at every bend. Repeated points count once; a single point is a dot that wide.
    #[must_use]
    pub fn stroke(points: &[[f32; 2]], width: f32, ink: ([f32; 4], [f32; 4]), alpha: f32) -> Vec<Self> {
        let mut pts: Vec<[f32; 2]> = Vec::with_capacity(points.len());
        for p in points {
            if pts.last() != Some(p) {
                pts.push(*p);
            }
        }
        let piece = |a: [f32; 2], b: [f32; 2], before: Option<[f32; 2]>, after: Option<[f32; 2]>| {
            let (prev, next) = (before.unwrap_or(a), after.unwrap_or(b));
            Self {
                seg: [a[0], a[1], b[0], b[1]],
                around: [prev[0], prev[1], next[0], next[1]],
                col_d: ink.0,
                col_l: ink.1,
                shape: [width * 0.5, f32::from(u8::from(before.is_some())), f32::from(u8::from(after.is_some())), alpha],
            }
        };
        match pts.as_slice() {
            [] => Vec::new(),
            [only] => vec![piece(*only, *only, None, None)],
            _ => (0..pts.len() - 1).map(|i| piece(pts[i], pts[i + 1], i.checked_sub(1).map(|j| pts[j]), pts.get(i + 2).copied())).collect(),
        }
    }
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
}

