//! shaders/harness.slang in Rust, function for function.
//!
//! harness-native: the panel, line and glyph passes. Copied from den-native's shaders/den.slang
//! (github.com/andrewgazelka/den) and stripped to what harness draws: plates (base, meter ring,
//! caret, disclosure chevron), hairlines, filled paths, pictures, live surfaces, terminal sprites,
//! atlas glyphs and outline glyphs (the brand marks). The structs are den's, unchanged, so
//! harness-native-render's pack.rs matches them byte for byte (its tests check it).
//!
//! Units: points in the instance buffers; `Frame::view` is the window size in points,
//! `Frame::scale` the backing scale. Every per-pass buffer and texture is an entry parameter, never
//! a global, so each entry's binding table is its own.
//!
//! Outline glyphs (`glyph_vs` / `glyph_fs`, `slug_*`: the brand marks of icons.rs) are the Slug
//! algorithm: Eric Lengyel, "GPU-Centered Font Rendering Directly from Glyph Outlines", JCGT 6(2),
//! 2017. The patent is dedicated to the public domain; the reference shaders
//! (https://github.com/EricLengyel/Slug) are MIT OR Apache-2.0, Copyright 2017 Eric Lengyel, and
//! `slug_dilate`, `slug_root_code`, `slug_solve_*` and `slug_render` are ports of them, copied from
//! den-native's den.slang.
//!
//! Where the port differs from the Slang in spelling, not meaning: Slang's `?:` is `if`, its
//! `select(bool3, ..)` a per-component helper (the scalar inner `fn one`), its `out` parameter a
//! `&mut`, `(uint)f` and `int(f)` are `as` (toward zero, as there), and integer division is Rust's,
//! which ends the invocation on a division by zero where Metal returns an undefined value (no
//! caller here divides by a value that can be 0: dash counts are 2 to 4, boxdraw.rs). Three private
//! helpers name what Slang wrote out at each use: `clip_pos` (`float4(to_ndc(pt - frame.place.xy,
//! frame.view), 0, 1)`), `corner_bit` (the square-corner mask's bit, twice in `panel_shade`) and
//! `turned` (a turn about a middle, twice in `glyph_vs`). The chevron's quarter turn is `FRAC_PI_2`,
//! one ulp above Slang's literal 1.5707963, which clippy refuses as an approximate constant.
// Slang's names, kept so the two read side by side: `W` and `H` are a cell's size in whole pixels
// (Ghostty's), `R` the meter ring's radius.
#![allow(non_snake_case, reason = "the Slang source's names, kept for reading side by side")]

use core::f32::consts::{FRAC_PI_2, TAU};
use gpu::prelude::*;

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, gpu::Pod)]
pub struct Frame {
    pub view: Vec2,
    pub scale: f32,
    pub time: f32,
    /// 1 in the dark appearance, 0 in the light one.
    pub dark: f32,
    /// Device pixels to a point this frame (the eased zoom's; `scale` is the zoom stepped to), 0: scale.
    pub live: f32,
    /// x the display's EDR headroom (linear over SDR white, 1: SDR; hdr.rs `Frame::headroom`, read by
    /// [`frame_headroom`]): how far past white HDR pictures, the mark's fire and glowing words go; y a
    /// motion's seconds (0: nothing moves).
    pub textfx: Vec2,
    /// x: text gamma dark, y: text gamma light, z: frosted glass saturation, w: unused.
    pub tune: Vec4,
    /// Outline glyphs: x emboldening (ems a side), y coverage gamma dark, z light, w the emboldening's
    /// limit (device pixels).
    pub slug: Vec4,
    /// xy: the point (view points) the target's top-left corner shows, 0 for the window; a windowed
    /// overlay's own window shows only its part of the view (view = its size). zw unused. Instances
    /// stay in the view's points; every vertex subtracts xy, so clip bands stay where they were. Not
    /// applied to sprites' pixel grid (terminal cells, never in an overlay's window).
    pub place: Vec4,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, gpu::Pod)]
pub struct PanelInst {
    /// x, y, w, h (points, y down).
    pub rect: Vec4,
    pub fill_d: Vec4,
    pub fill_l: Vec4,
    pub edge_d: Vec4,
    pub edge_l: Vec4,
    /// radius, squircle exponent n, border width, feather.
    pub shape: Vec4,
    /// shadow blur, shadow alpha, glass (0 or 1), kind (0 panel, 2 compress glyph, 4 caret, 11 ink, ...;
    /// see pack.rs).
    pub fx: Vec4,
    /// shadow dy, opacity, kind parameter, corner mask (bits 1 TL, 2 TR, 4 BR, 8 BL: square corner).
    pub fx2: Vec4,
    /// liquid glass: bevel (points, 0 none), magnify, dispersion, rim light; a caret's clip rect.
    pub lens: Vec4,
    /// frosted plate: sheen (the inner highlight's alpha), saturation (0: the frame's); image: 0, 0,
    /// peak, headroom.
    pub fx3: Vec4,
    /// The band it shows in: top, foot (points, y down; a foot of 0 is none), then how far it fades in
    /// from each (points; 0: a hard cut).
    pub clip: Vec4,
    /// The band across: left, right (points; a right of 0 is none), then how far it fades in from each.
    pub clipx: Vec4,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, gpu::Pod)]
pub struct LineInst {
    /// ax, ay, bx, by (points, y down).
    pub seg: Vec4,
    /// The previous segment's start and the next segment's end: prev.x, prev.y, next.x, next.y.
    pub around: Vec4,
    pub col_d: Vec4,
    pub col_l: Vec4,
    /// half width (points), 1 when a segment comes before this one, 1 when one comes after, opacity.
    pub shape: Vec4,
    /// The band it shows in, as a panel's.
    pub clip: Vec4,
    /// The band across, as a panel's.
    pub clipx: Vec4,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, gpu::Pod)]
pub struct GlyphInst {
    /// x, y, w, h (points): a bitmap on the device-pixel grid, or an outline glyph's ink box.
    pub rect: Vec4,
    /// bitmap: u0, v0, u1, v1 in the atlas; outline: its ink box in ems, top-left then bottom-right
    /// (x0, y1, x1, y0).
    pub uv: Vec4,
    pub col_d: Vec4,
    pub col_l: Vec4,
    /// birth time, settle omega, rise (points), opacity.
    pub anim: Vec4,
    /// colour (1: a colour bitmap such as an emoji, drawn as it is, not tinted), drop below its line's
    /// top, its line's height (CPU only), outline (1: drawn from its curves).
    pub kind: Vec4,
    /// The band it shows in: top, foot (points, y down; a foot of 0 is none); a bitmap's turn about its
    /// middle (radians), where swelling words grow from (points below rect.y).
    pub clip: Vec4,
    /// outline: band scale x, scale y, offset x, offset y.
    pub band: Vec4,
    /// outline: band header start in `bands`, greatest vertical (x) band, greatest horizontal (y) band, 0.
    pub gdata: UVec4,
    /// expressive text: its motion's start (frame time), code (motion | 16 glow | place << 5), swell x
    /// (points right of rect.x), em (points).
    pub fx: Vec4,
    /// how far it fades in from its band's top and foot (points; 0: a hard cut), 0, 0.
    pub fade: Vec4,
    /// The band across, as a panel's.
    pub clipx: Vec4,
}

pub const CORNERS: [Vec2; 4] = [vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0), vec2(1.0, 1.0)];

pub fn to_ndc(pt: Vec2, view: Vec2) -> Vec2 {
    vec2(pt.x / view.x * 2.0 - 1.0, 1.0 - pt.y / view.y * 2.0)
}

/// Continuous-corner (squircle) rounded-rect distance: a superellipse of exponent n in the corner region.
pub fn sd_squircle(p: Vec2, half_size: Vec2, r: f32, n: f32) -> f32 {
    let q = p.abs() - (half_size - r);
    let qp = q.max(Vec2::splat(0.0));
    let corner = pow(pow(qp.x, n) + pow(qp.y, n), 1.0 / n);
    corner + q.x.max(q.y).min(0.0) - r
}

pub fn sd_seg(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let h = saturate(dot(pa, ba) / dot(ba, ba));
    length(pa - ba * h)
}

/// Signed distance to the quad v0 v1 v2 v3 (Inigo Quilez's polygon SDF); degenerate edges are skipped.
pub fn sd_quad(p: Vec2, v0: Vec2, v1: Vec2, v2: Vec2, v3: Vec2) -> f32 {
    let v = [v0, v1, v2, v3];
    let mut d = dot(p - v0, p - v0);
    let mut s = 1.0;
    for i in 0..4 {
        let a = v[i];
        let e = v[(i + 3) % 4] - a;
        let w = p - a;
        let ee = dot(e, e);
        if ee > 1e-8 {
            let b = w - e * saturate(dot(w, e) / ee);
            d = d.min(dot(b, b));
        }
        let c0 = p.y >= a.y;
        let c1 = p.y < v[(i + 3) % 4].y;
        let c2 = e.x * w.y > e.y * w.x;
        if (c0 && c1 && c2) || (!c0 && !c1 && !c2) {
            s = -s;
        }
    }
    s * d.sqrt()
}

/// The caret's blink, `t` seconds after it last moved: solid for `hold` seconds, then a cycle of
/// `period` seconds (the os text field's), on for half, its edges a short eased fade (`fade`, a share
/// of the cycle) rather than a cut. 1 shown, 0 hidden. `hold`, `period` and `fade` come from the
/// scene's CaretLook (the caret panel's shape.z, fx.y and shape.w).
pub fn caret_blink(t: f32, hold: f32, period: f32, fade: f32) -> f32 {
    if t < hold {
        return 1.0;
    }
    let ph = frac((t - hold) / period);
    let off = smoothstep(0.5 - fade, 0.5, ph) * (1.0 - smoothstep(1.0 - fade, 1.0, ph));
    1.0 - off
}

pub fn pick(dark: f32, d: Vec4, l: Vec4) -> Vec4 {
    if dark > 0.5 { d } else { l }
}

/// How much of a pixel at `y` (points) is inside the band `clip` (top, foot; a foot of 0 is none): a
/// cut across, its edge antialiased on the pixel grid, that leaves what it keeps as it was. `fade`
/// (top, foot; points) softens each cut into a ramp that long, where a scroll has more beyond that
/// edge. The same along x (`clipx`: left, right) cuts the sides.
pub fn in_band(y: f32, clip: Vec4, fade: Vec2, scale: f32) -> f32 {
    let mut a = saturate((y - clip.x) * scale + 0.5);
    if fade.x > 0.0 {
        a *= smoothstep(0.0, 1.0, saturate((y - clip.x) / fade.x));
    }
    if clip.y > 0.0 {
        a *= saturate((clip.y - y) * scale + 0.5);
        if fade.y > 0.0 {
            a *= smoothstep(0.0, 1.0, saturate((clip.y - y) / fade.y));
        }
    }
    a
}

/// The clip position of the view point `pt`: Slang's `float4(to_ndc(pt - frame.place.xy, frame.view), 0, 1)`.
fn clip_pos(pt: Vec2, frame: &Frame) -> Position {
    Position(to_ndc(pt - frame.place.xy(), frame.view).extend(0.0).extend(1.0))
}

// ---- panels --------------------------------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, gpu::Varyings)]
pub struct PanelV {
    pub pos: Position,
    pub local: Vec2,
    pub inst: Flat<u32>,
}

#[gpu::vertex]
pub fn panel_vs(vid: VertexIndex, iid: InstanceIndex, frame: &Frame, insts: &[PanelInst]) -> PanelV {
    let p = insts[iid.get() as usize];
    let pad = p.fx.x * 2.0 + p.shape.w + p.fx2.x.abs() + 3.0;
    let half_size = p.rect.zw() * 0.5;
    let center = p.rect.xy() + half_size;
    let c = CORNERS[vid.get() as usize];
    let local = (c * 2.0 - 1.0) * (half_size + pad);
    PanelV { pos: clip_pos(center + local, frame), local, inst: Flat(iid.get()) }
}

#[gpu::fragment]
pub fn panel_fs(v: PanelV, frame: &Frame, insts: &[PanelInst]) -> Vec4 {
    let p = insts[*v.inst as usize];
    let y = p.rect.y + p.rect.w * 0.5 + v.local.y;
    let x = p.rect.x + p.rect.z * 0.5 + v.local.x;
    panel_shade(v, frame, p) * in_band(y, p.clip, p.clip.zw(), frame.scale) * in_band(x, p.clipx, p.clipx.zw(), frame.scale)
}

/// The corner `local` is in, as the square-corner mask's bit: 1 TL, 2 TR, 8 BL, 4 BR.
fn corner_bit(local: Vec2) -> u32 {
    let quad: u32 = (if local.x > 0.0 { 1 } else { 0 }) + (if local.y > 0.0 { 2 } else { 0 });
    if quad == 0 {
        1
    } else if quad == 1 {
        2
    } else if quad == 2 {
        8
    } else {
        4
    }
}

pub fn panel_shade(v: PanelV, frame: &Frame, p: PanelInst) -> Vec4 {
    let opacity = p.fx2.y;
    let fill = pick(frame.dark, p.fill_d, p.fill_l);
    let edge = pick(frame.dark, p.edge_d, p.edge_l);
    let half_size = p.rect.zw() * 0.5;
    let aa = 1.0 / frame.scale;

    if p.fx.w > 2.5 && p.fx.w < 3.5 {
        // Meter ring: a track of the edge colour, shape.z thick, just inside the square rect, and over it
        // an arc of the fill clockwise for the share fx2.z, starting fx3.x of a turn past twelve o'clock (a
        // spinner turns it), its ends cut square across the stroke, or round when fx3.y is 1. Coverage is
        // the distance to the band and, along it, to the arc's nearer end, so it is crisp at any scale.
        let R = half_size.x;
        let sw = p.shape.z.max(aa);
        let mid = R - sw * 0.5;
        let r = length(v.local);
        let band = saturate(0.5 - ((r - mid).abs() - sw * 0.5) / aa);
        let share = p.fx2.z;
        let start = p.fx3.x;
        let turn = frac(v.local.x.atan2(-v.local.y) / TAU + 2.0 - start);
        let outside = if turn <= share { -turn.min(share - turn) } else { (turn - share).min(1.0 - turn) };
        let lit = if share <= 0.0 {
            0.0
        } else if share >= 1.0 {
            1.0
        } else {
            saturate(0.5 - outside * TAU * mid / aa)
        };
        let mut arc = band * lit;
        if p.fx3.y > 0.5 && share > 0.0 && share < 1.0 {
            // Each round end is a disc the stroke wide on the arc's end, which also hides the hard cut under it.
            let a0 = start * TAU;
            let a1 = (start + share) * TAU;
            let e0 = mid * vec2(a0.sin(), -a0.cos());
            let e1 = mid * vec2(a1.sin(), -a1.cos());
            let cap = saturate(0.5 - (length(v.local - e0).min(length(v.local - e1)) - sw * 0.5) / aa);
            arc = (if turn <= share { band } else { 0.0 }).max(cap);
        }
        let ta = edge.w * band;
        let fa = fill.w * arc;
        let acc = (fill.rgb() * fa).extend(fa) + (edge.rgb() * ta).extend(ta) * (1.0 - fa);
        return acc * opacity;
    }
    if p.fx.w > 7.5 {
        // Disclosure chevron: a right angle of round-capped stroke (shape.z points) filling the square
        // rect, pointing down at fx2.z 0 and turned a quarter to point right at 1. It spans the rect's
        // width and half its height, centred on the rect, so it sits centred at every turn.
        let turn = p.fx2.z * FRAC_PI_2;
        let mut q = v.local / half_size.x;
        q = vec2(q.x * turn.cos() - q.y * turn.sin(), q.x * turn.sin() + q.y * turn.cos());
        let apex = vec2(0.0, 0.5);
        let dd = sd_seg(q, vec2(-1.0, -0.5), apex).min(sd_seg(q, vec2(1.0, -0.5), apex)) * half_size.x;
        let cov = saturate(0.5 - (dd - p.shape.z * 0.5) / aa);
        let a = fill.w * cov * opacity;
        return (fill.rgb() * a).extend(a);
    }
    if p.fx.w > 4.5 && p.fx.w < 5.5 {
        // Copy light (den's selection flash, harness-native-render select.rs): a soft band of the fill
        // across the row, its centre going left to right as fx2.z goes 0 to 1; the row's corners as the
        // selection's (fx2.w mask).
        let lmask = p.fx2.w as u32;
        let lbit = corner_bit(v.local);
        let ld = sd_squircle(v.local, half_size, if (lmask & lbit) != 0 { 0.0 } else { p.shape.x }, p.shape.y);
        let band = 12.0f32.max(p.rect.z * 0.26);
        let centre = -band + (p.rect.z + 2.0 * band) * p.fx2.z;
        let lit = saturate(1.0 - (v.local.x + half_size.x - centre).abs() / band);
        let a = fill.w * saturate(0.5 - ld / aa) * lit * opacity;
        return (fill.rgb() * a).extend(a);
    }
    if p.fx.w > 3.5 {
        // Caret: a thin bar at rest in edge_l, blinking from fx2.w, when it last moved, so it is solid
        // while typed with; over a sweep from where it was (edge_d, none when its width is 0) that shrinks
        // into it over shape.y seconds (CaretLook::glide) with an ease-out cubic, as Ghostty's
        // cursor_sweep. fx2.z is when the sweep began; shape.x is the bar's corner radius, shape.z how long
        // it stays solid after it moved, fx.y the blink's cycle, shape.w the share of it each turn of the
        // blink eases over.
        let pt = p.rect.xy() + half_size + v.local;
        let to = p.edge_l;
        let from = p.edge_d;
        let hb = to.zw() * 0.5;
        let db = sd_squircle(pt - (to.xy() + to.zw() * 0.5), hb, p.shape.x.min(hb.x), 2.0);
        let bar = saturate(0.5 - db / aa) * caret_blink((frame.time - p.fx2.w).max(0.0), p.shape.z, p.fx.y, p.shape.w);
        let mut trail = 0.0;
        let prog = (frame.time - p.fx2.z) / p.shape.y;
        if from.z > 0.0 && prog < 1.0 {
            let q = 1.0 - saturate(prog);
            let tail = lerp(from, to, 1.0 - q * q * q);
            // The leading edge follows the direction of travel, so a vertical move still has area.
            let fwd = to.x >= tail.x;
            let tx = if fwd { tail.x } else { tail.x + tail.z };
            let hx = if fwd { to.x + to.z } else { to.x };
            let dt = sd_quad(pt, vec2(tx, tail.y), vec2(hx, to.y), vec2(hx, to.y + to.w), vec2(tx, tail.y + tail.w));
            trail = saturate(0.5 - dt / aa) * 0.5 * q;
        }
        // lens is the text box (x, y, w, h): the bar and its sweep never leave it.
        let lo = p.lens.xy();
        let hi = p.lens.xy() + p.lens.zw();
        let inside = if p.lens.z > 0.0 {
            saturate((pt.x - lo.x).min(hi.x - pt.x).min((pt.y - lo.y).min(hi.y - pt.y)) / aa + 0.5)
        } else {
            1.0
        };
        let a = fill.w * (bar + trail * (1.0 - bar)) * opacity * inside;
        return (fill.rgb() * a).extend(a);
    }
    // Corners flagged in the mask are square (stacked selection plates read as one shape).
    let mut radius = p.shape.x;
    let mask = p.fx2.w as u32;
    let bit = corner_bit(v.local);
    if (mask & bit) != 0 {
        radius = 0.0;
    }
    let d = sd_squircle(v.local, half_size, radius, p.shape.y);
    let w = aa.max(p.shape.w);
    let mut cov = saturate(0.5 - d / w);
    if p.shape.w > 0.0 {
        cov = cov * cov * (3.0 - 2.0 * cov);
    }

    // Soft shadow: a Gaussian-like falloff of the distance to the offset shape, hidden under the panel itself.
    let mut outc = Vec4::splat(0.0);
    if p.fx.y > 0.0 {
        let ds = sd_squircle(v.local - vec2(0.0, p.fx2.x), half_size, radius, p.shape.y);
        let mut s = saturate(0.5 - ds / (2.0 * p.fx.x.max(0.001)));
        s = s * s * (3.0 - 2.0 * s);
        let sa = s * p.fx.y * opacity;
        outc = vec4(0.0, 0.0, 0.0, sa);
    }

    // The plate, premultiplied: the fill over nothing. (Slang computes the backdrop's uv here too, for
    // the glass this copy no longer draws, and never reads it.)
    let base = fill.rgb() * fill.w;
    let base_a = fill.w;
    // Glass amount 0..1: the fill is a tint laid over the blurred backdrop (its alpha is how much of the
    // page's own colour the plate keeps, so what is under shows through by the rest), the plain fill at
    // 0. The backdrop is premultiplied: where it is transparent (the card's margin over the desktop) the
    // plate is too.
    let ia = base_a * cov * opacity;
    outc = (base * cov * opacity).extend(ia) + outc * (1.0 - ia);

    if p.shape.z > 0.0 {
        let inner = saturate(0.5 - (d + p.shape.z) / aa);
        let ring = saturate(cov - inner) * edge.w * opacity;
        outc = (edge.rgb() * ring).extend(ring) + outc * (1.0 - ring);
    }
    outc
}

// ---- lines ----------------------------------------------------------------------------------------------------
//
// A stroked polyline is one instance per segment (a dot is a segment of no length). Each instance is a
// capsule: the distance to its segment, less the half width, is the signed distance to the stroke, and
// one device pixel of it is the edge's ramp, so the line is anti-aliased at any backing scale. Two
// capsules that meet overlap, and blending both would show the join (a translucent line gets darker
// where its segments meet), so the join is split on its bisector: each segment draws only the side of
// it that its own end lies on. Beyond the vertex both capsules are the same disc, so the split is
// invisible and the join is round.

#[derive(Copy, Clone, Debug, gpu::Varyings)]
pub struct LineV {
    pub pos: Position,
    pub pt: Vec2,
    pub inst: Flat<u32>,
}

pub fn unit2(v: Vec2) -> Vec2 {
    let l = length(v);
    if l > 1e-5 { v / l } else { vec2(0.0, 0.0) }
}

/// Distance to the segment a b; a segment of no length is a point.
pub fn sd_stub(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let h = saturate(dot(pa, ba) / dot(ba, ba).max(1e-8));
    length(pa - ba * h)
}

#[gpu::vertex]
pub fn line_vs(vid: VertexIndex, iid: InstanceIndex, frame: &Frame, insts: &[LineInst]) -> LineV {
    let l = insts[iid.get() as usize];
    let a = l.seg.xy();
    let ab = l.seg.zw() - a;
    let len = length(ab);
    let along = if len > 1e-4 { ab / len } else { vec2(1.0, 0.0) };
    let across = vec2(-along.y, along.x);
    // The box round the capsule: its half width and two device pixels of edge past it.
    let reach = l.shape.x + 2.0 / frame.scale;
    let c = CORNERS[vid.get() as usize];
    let pt = a + along * lerp(-reach, len + reach, c.x) + across * (c.y * 2.0 - 1.0) * reach;
    LineV { pos: clip_pos(pt, frame), pt, inst: Flat(iid.get()) }
}

#[gpu::fragment]
pub fn line_fs(v: LineV, frame: &Frame, insts: &[LineInst]) -> Vec4 {
    let l = insts[*v.inst as usize];
    let a = l.seg.xy();
    let b = l.seg.zw();
    let dir = unit2(b - a);
    // A pixel on the other side of a join's bisector belongs to the neighbour. The margin keeps a pixel on
    // the line itself drawn by both (not by neither) however the two instances round it.
    if l.shape.y > 0.5 {
        let m = unit2(unit2(a - l.around.xy()) + dir);
        if dot(v.pt - a, m) < -1e-3 {
            return Vec4::splat(0.0);
        }
    }
    if l.shape.z > 0.5 {
        let m = unit2(dir + unit2(l.around.zw() - b));
        if dot(v.pt - b, m) > 1e-3 {
            return Vec4::splat(0.0);
        }
    }
    let aa = 1.0 / frame.scale;
    let mut cov = saturate(0.5 - (sd_stub(v.pt, a, b) - l.shape.x) / aa);
    cov *= in_band(v.pt.y, l.clip, l.clip.zw(), frame.scale) * in_band(v.pt.x, l.clipx, l.clipx.zw(), frame.scale);
    let col = pick(frame.dark, l.col_d, l.col_l);
    let alpha = col.w * cov * l.shape.w;
    (col.rgb() * alpha).extend(alpha)
}

// ---- filled paths ---------------------------------------------------------------------------------------------
//
// Copied from den-native's den.slang. A filled path is one instance (its bounding box) over a run of
// straight edges in their own buffer; curves are flattened on the CPU (vector.rs). Each pixel takes its
// winding number and its distance to the nearest edge: signed by the fill rule, one device pixel of that
// distance is the edge's ramp, so a path is as crisp as a panel at any scale. It is cut to its clip bands
// as a panel is.

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, gpu::Pod)]
pub struct PathInst {
    /// The box round the path (points, y down).
    pub rect: Vec4,
    pub col_d: Vec4,
    pub col_l: Vec4,
    /// first edge, edge count, rule (0 nonzero, 1 even-odd), opacity.
    pub shape: Vec4,
    /// The band it shows in, as a panel's.
    pub clip: Vec4,
    /// The band across, as a panel's.
    pub clipx: Vec4,
}

#[gpu::vertex]
pub fn path_vs(vid: VertexIndex, iid: InstanceIndex, frame: &Frame, insts: &[PathInst]) -> LineV {
    let p = insts[iid.get() as usize];
    let reach = 2.0 / frame.scale;
    let pt = p.rect.xy() - reach + CORNERS[vid.get() as usize] * (p.rect.zw() + reach * 2.0);
    LineV { pos: clip_pos(pt, frame), pt, inst: Flat(iid.get()) }
}

#[gpu::fragment]
pub fn path_fs(v: LineV, frame: &Frame, insts: &[PathInst], edges: &[Vec4]) -> Vec4 {
    let p = insts[*v.inst as usize];
    let first = p.shape.x as u32;
    let count = p.shape.y as u32;
    let q = v.pt;
    let mut winding: i32 = 0;
    let mut d: f32 = 1e9;
    for i in 0..count {
        let e = edges[(first + i) as usize];
        let a = e.xy();
        let b = e.zw();
        d = d.min(sd_stub(q, a, b));
        let side = (b.x - a.x) * (q.y - a.y) - (q.x - a.x) * (b.y - a.y);
        if a.y <= q.y {
            if b.y > q.y && side > 0.0 {
                winding += 1;
            }
        } else if b.y <= q.y && side < 0.0 {
            winding -= 1;
        }
    }
    let inside = if p.shape.z > 0.5 { (winding & 1) != 0 } else { winding != 0 };
    let sd = if inside { -d } else { d };
    let mut cov = saturate(0.5 - sd * frame.scale);
    cov *= in_band(q.y, p.clip, p.clip.zw(), frame.scale) * in_band(q.x, p.clipx, p.clipx.zw(), frame.scale);
    let col = pick(frame.dark, p.col_d, p.col_l);
    let alpha = col.w * cov * p.shape.w;
    (col.rgb() * alpha).extend(alpha)
}

// ---- pictures -------------------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, gpu::Pod)]
pub struct ImageInst {
    /// x, y, w, h (points).
    pub rect: Vec4,
    /// the texture's width and height (texels), opacity, 1 to filter it smoothly (a glow) rather than
    /// fetch texels.
    pub size: Vec4,
    /// The band it shows in, as a panel's.
    pub clip: Vec4,
    /// The band across, as a panel's.
    pub clipx: Vec4,
    /// a light layer when x > 1: lift the picture under it to up to x times its colour (linear), by heat.
    pub light: Vec4,
    /// an HDR picture when x = 1: premultiplied linear half floats, y its brightest (0: unknown);
    /// tone-mapped to the headroom.
    pub hdr: Vec4,
}

#[derive(Copy, Clone, Debug, gpu::Varyings)]
pub struct ImageV {
    pub pos: Position,
    pub uv: Vec2,
    pub y: f32,
    pub inst: Flat<u32>,
    pub x: f32,
}

#[gpu::vertex]
pub fn image_vs(vid: VertexIndex, iid: InstanceIndex, frame: &Frame, insts: &[ImageInst]) -> ImageV {
    let m = insts[iid.get() as usize];
    let c = CORNERS[vid.get() as usize];
    let pt = m.rect.xy() + c * m.rect.zw();
    ImageV { pos: clip_pos(pt, frame), uv: c, y: pt.y, inst: Flat(iid.get()), x: pt.x }
}

/// Pixel art: the texel under the fragment, fetched (never filtered), so each one is a crisp block with
/// no seam between neighbours at any size or offset.
#[gpu::fragment]
pub fn image_fs(v: ImageV, frame: &Frame, insts: &[ImageInst], tex: &Texture2d, smp: &Sampler) -> Vec4 {
    let m = insts[*v.inst as usize];
    let at = (v.uv * m.size.xy()).floor().as_ivec2().clamp(ivec2(0, 0), m.size.xy().as_ivec2() - 1);
    let t = if m.size.w > 0.5 { tex.sample_level(smp, v.uv, 0.0) } else { tex.load(at, 0) };
    let cover = m.size.z * in_band(v.y, m.clip, m.clip.zw(), frame.scale) * in_band(v.x, m.clipx, m.clipx.zw(), frame.scale);
    if m.hdr.x > 0.5 {
        // An HDR photo: premultiplied linear light, 1.0 SDR white. Straight, rolled off to the display,
        // then encoded as the target is; SDR colours under the knee come out as an SDR photo's would.
        let lin = if t.w > 0.0 { t.rgb() / t.w } else { vec3(0.0, 0.0, 0.0) };
        let c = srgb_encode_ext(tone_map_rgb(lin, m.hdr.y, frame_headroom(frame)));
        let ah = t.w * cover;
        return (c * ah).extend(ah);
    }
    if m.light.x > 1.0 {
        // A light layer over the same picture: its colour c is already drawn here; add what takes it, in
        // linear light, to c times 1 + (gain - 1) * heat. Alpha 0, so over-blending adds and covers nothing.
        let k = 1.0 + (m.light.x - 1.0) * t.w;
        let lifted = srgb_encode_ext(srgb_decode_ext(t.rgb()) * k);
        return ((lifted - t.rgb()) * cover).extend(0.0);
    }
    let a = t.w * cover;
    (t.rgb() * a).extend(a)
}

/// sRGB's curve, extended past 1 (the target is extended-range sRGB-encoded half floats).
pub fn srgb_decode_ext(c: Vec3) -> Vec3 {
    // Slang's `select(a <= 0.04045, ..)`, per component.
    fn one(c: f32) -> f32 {
        let a = c.abs();
        let l = if a <= 0.04045 { a / 12.92 } else { pow((a + 0.055) / 1.055, 2.4) };
        l * sign(c)
    }
    vec3(one(c.x), one(c.y), one(c.z))
}

pub fn srgb_encode_ext(l: Vec3) -> Vec3 {
    fn one(l: f32) -> f32 {
        let a = l.abs();
        let c = if a <= 0.0031308 { a * 12.92 } else { 1.055 * pow(a, 1.0 / 2.4) - 0.055 };
        c * sign(l)
    }
    vec3(one(l.x), one(l.y), one(l.z))
}

// ---- HDR: the display's headroom and the tone map (ports of hdr.rs; its tests pin the curve) ------------------------

/// hdr.rs WHITE_NITS: reference white (BT.2408).
pub const HDR_WHITE_NITS: f32 = 203.0;
pub const HDR_HLG_PEAK_NITS: f32 = 1000.0;
pub const HDR_KNEE: f32 = 0.75;

/// The display's headroom this frame (hdr.rs `Frame::headroom`): linear over SDR white, at least 1.
pub fn frame_headroom(f: &Frame) -> f32 {
    let h = f.textfx.x;
    if h.is_finite() { h.max(1.0) } else { 1.0 }
}

pub fn hdr_knee(h: f32) -> f32 {
    (HDR_KNEE * h.max(1.0)).min(1.0)
}

/// hdr.rs `tone_map`: identity to the knee, then rolled off so `peak` (0: unknown) lands on the headroom.
pub fn tone_map(x: f32, peak: f32, headroom: f32) -> f32 {
    let h = headroom.max(1.0);
    if !(x > 0.0) {
        return 0.0;
    }
    let known = peak.is_finite() && peak > 0.0;
    if known && peak <= h {
        return x.min(h);
    }
    let k = hdr_knee(h);
    if x <= k {
        return x;
    }
    let span = h - k;
    let c = if known { 1.0 / span - 1.0 / (peak - k) } else { 1.0 / span };
    let a = x - k;
    if a.is_infinite() {
        return h;
    }
    (k + a / (1.0 + a * c)).min(h)
}

/// hdr.rs `tone_map_rgb`: by the brightest channel, so hue and saturation hold.
pub fn tone_map_rgb(rgb: Vec3, peak: f32, headroom: f32) -> Vec3 {
    let m = rgb.x.max(rgb.y.max(rgb.z));
    if !(m > 0.0) {
        return vec3(0.0, 0.0, 0.0);
    }
    rgb * (tone_map(m, peak, headroom) / m)
}

/// hdr.rs `pq_linear`: a PQ signal as linear light over reference white.
pub fn pq_linear(e: Vec3) -> Vec3 {
    fn one(e: f32) -> f32 {
        const M1: f32 = 0.1593017578125;
        const M2: f32 = 78.84375;
        const C1: f32 = 0.8359375;
        const C2: f32 = 18.8515625;
        const C3: f32 = 18.6875;
        let p = pow(saturate(e), 1.0 / M2);
        let nits = 10000.0 * pow((p - C1).max(0.0) / (C2 - C3 * p), 1.0 / M1);
        nits / HDR_WHITE_NITS
    }
    vec3(one(e.x), one(e.y), one(e.z))
}

/// hdr.rs `hlg_scene`, per channel.
pub fn hlg_scene(e: Vec3) -> Vec3 {
    fn one(e: f32) -> f32 {
        const A: f32 = 0.17883277;
        const B: f32 = 0.28466892;
        const C: f32 = 0.55991073;
        let e = saturate(e);
        if e <= 0.5 { e * e / 3.0 } else { (((e - C) / A).exp() + B) / 12.0 }
    }
    vec3(one(e.x), one(e.y), one(e.z))
}

/// hdr.rs `hlg_linear`: BT.2100's OOTF for a 1000-nit display (gamma 1.2), on BT.2020 primaries.
pub fn hlg_linear(e: Vec3) -> Vec3 {
    let s = hlg_scene(e);
    let y = dot(s, vec3(0.2627, 0.6780, 0.0593));
    s * (HDR_HLG_PEAK_NITS * pow(y.max(0.0), 0.2) / HDR_WHITE_NITS)
}

/// hdr.rs `bt2020_to_709` (BT.2087).
pub fn bt2020_to_709(c: Vec3) -> Vec3 {
    vec3(
        1.660491 * c.x - 0.5876411 * c.y - 0.0728499 * c.z,
        -0.1245505 * c.x + 1.1329 * c.y - 0.0083494 * c.z,
        -0.0181508 * c.x - 0.1005789 * c.y + 1.1187297 * c.z,
    )
}

// ---- live pictures (media/: a Shader's or a Video's latest finished frame) ------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, gpu::Pod)]
pub struct SurfaceInst {
    /// x, y, w, h (points).
    pub rect: Vec4,
    /// corner radius (points), squircle exponent n, opacity, cover (1: cut to the box's shape; the host's
    /// uv says how).
    pub shape: Vec4,
    /// The band it shows in, as a panel's.
    pub clip: Vec4,
    /// The band across, as a panel's.
    pub clipx: Vec4,
    /// the part of the texture shown: u0, v0, u1, v1.
    pub uv: Vec4,
    /// how the texture encodes light: x 0 sRGB (as it is), 1 PQ, 2 HLG; y its brightest (0: unknown);
    /// z 1 BT.2020.
    pub hdr: Vec4,
}

#[derive(Copy, Clone, Debug, gpu::Varyings)]
pub struct SurfaceV {
    pub pos: Position,
    pub local: Vec2,
    pub uv: Vec2,
    pub y: f32,
    pub x: f32,
    pub inst: Flat<u32>,
}

#[gpu::vertex]
pub fn surface_vs(vid: VertexIndex, iid: InstanceIndex, frame: &Frame, insts: &[SurfaceInst]) -> SurfaceV {
    let m = insts[iid.get() as usize];
    let c = CORNERS[vid.get() as usize];
    let pt = m.rect.xy() + c * m.rect.zw();
    SurfaceV {
        pos: clip_pos(pt, frame),
        local: (c * 2.0 - 1.0) * m.rect.zw() * 0.5,
        uv: lerp(m.uv.xy(), m.uv.zw(), c),
        y: pt.y,
        x: pt.x,
        inst: Flat(iid.get()),
    }
}

/// The texture is premultiplied (a shader's picture, stored as its prelude premultiplied it) or opaque (a
/// video frame), and may be brighter than white (EDR): only the corners' coverage, the clip bands and the
/// opacity scale it. An HDR video's frame holds its PQ or HLG signal: decoded to linear light, brought to
/// BT.709 primaries, rolled off to the display's headroom and encoded as the target is.
#[gpu::fragment]
pub fn surface_fs(v: SurfaceV, frame: &Frame, insts: &[SurfaceInst], tex: &Texture2d, smp: &Sampler) -> Vec4 {
    let m = insts[*v.inst as usize];
    let half_size = m.rect.zw() * 0.5;
    let r = m.shape.x.min(half_size.x.min(half_size.y));
    let d = sd_squircle(v.local, half_size, r.max(1e-3), m.shape.y.max(2.0));
    let cover = saturate(0.5 - d * frame.scale);
    let a = cover * m.shape.z * in_band(v.y, m.clip, m.clip.zw(), frame.scale) * in_band(v.x, m.clipx, m.clipx.zw(), frame.scale);
    let mut t = tex.sample_level(smp, v.uv, 0.0);
    if m.hdr.x > 0.5 {
        let mut lin = if m.hdr.x > 1.5 { hlg_linear(t.rgb()) } else { pq_linear(t.rgb()) };
        if m.hdr.z > 0.5 {
            lin = bt2020_to_709(lin);
        }
        t = (srgb_encode_ext(tone_map_rgb(lin, m.hdr.y, frame_headroom(frame))) * t.w).extend(t.w);
    }
    t * a
}

// ---- sprites --------------------------------------------------------------------------------------------------
//
// Box drawing, block elements, braille, sextants and powerline arrows (boxdraw.rs), one instance a cell,
// drawn from their geometry. The geometry is Ghostty's (src/font/sprite/draw: box.zig, block.zig,
// braille.zig, common.zig, symbols_for_legacy_computing.zig, powerline.zig; github.com/ghostty-org/ghostty,
// MIT), ported from its CPU canvas. Everything Ghostty puts on whole pixels is computed here in integers
// from the cell's whole-pixel size and tested against the pixel, so its coverage is exactly 0 or 1 and a
// line that leaves one cell meets the next with no seam. Arcs, diagonals and the powerline curves are
// distances (in pixels) antialiased over the distance's own screen derivative.

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, gpu::Pod)]
pub struct SpriteInst {
    /// the cell: x, y, w, h (device pixels).
    pub rect: IVec4,
    /// kind, a, b (boxdraw.rs `Shape`), a light line's thickness (device pixels).
    pub shape: UVec4,
    /// ink, straight RGBA.
    pub fg: Vec4,
    /// the ground under it, straight RGBA (alpha 0: none).
    pub bg: Vec4,
}

pub const SPRITE_BLOCK: u32 = 1;
pub const SPRITE_SHADE: u32 = 2;
pub const SPRITE_MOSAIC: u32 = 3;
pub const SPRITE_LINES: u32 = 4;
pub const SPRITE_DASH: u32 = 5;
pub const SPRITE_ARC: u32 = 6;
pub const SPRITE_DIAGONAL: u32 = 7;
pub const SPRITE_BRAILLE: u32 = 8;
pub const SPRITE_POWERLINE: u32 = 9;

#[derive(Copy, Clone, Debug, gpu::Varyings)]
pub struct SpriteV {
    pub pos: Position,
    pub inst: Flat<u32>,
}

#[gpu::vertex]
pub fn sprite_vs(vid: VertexIndex, iid: InstanceIndex, frame: &Frame, insts: &[SpriteInst]) -> SpriteV {
    let s = insts[iid.get() as usize];
    let px = s.rect.xy().as_vec2() + CORNERS[vid.get() as usize] * s.rect.zw().as_vec2();
    SpriteV { pos: clip_pos(px / frame.scale, frame), inst: Flat(iid.get()) }
}

/// Whether the pixel `p` (whole pixels from the cell's top left) is in the box between corners (x0, y0)
/// and (x1, y1), as Ghostty's canvas.box fills it (its corners in either order).
pub fn sp_box(p: IVec2, x0: i32, y0: i32, x1: i32, y1: i32) -> bool {
    p.x >= x0.min(x1) && p.x < x0.max(x1) && p.y >= y0.min(y1) && p.y < y0.max(y1)
}

/// Ghostty's Fraction.min and .max (common.zig): where the fraction `f` of `size` falls as a block's start
/// and as its end, rounded so the two halves of an odd size meet (start of the second half = end of the
/// first). `round` is Metal's, half away from zero, as `f32::round`.
pub fn frac_lo(f: f32, size: i32) -> i32 {
    size - ((1.0 - f) * size as f32).round() as i32
}

pub fn frac_hi(f: f32, size: i32) -> i32 {
    (f * size as f32).round() as i32
}

/// Coverage of a signed distance `d` (pixels, negative inside), its edge a pixel wide on screen.
pub fn sp_aa(d: f32) -> f32 {
    let w = length(vec2(dpdx(d), dpdy(d)));
    saturate(0.5 - d / w.max(1e-4))
}

/// box.zig linesChar: up to four arms from the cell's middle, each none, light, heavy or double, joined as
/// Ghostty joins them.
pub fn sp_lines(p: IVec2, W: i32, H: i32, t: i32, arms: u32) -> bool {
    let up = arms & 3;
    let right = (arms >> 2) & 3;
    let down = (arms >> 4) & 3;
    let left = (arms >> 6) & 3;
    let lt = t;
    let hv = 2 * t;
    let h_light_top = (H - lt).max(0) / 2;
    let h_light_bottom = h_light_top + lt;
    let h_heavy_top = (H - hv).max(0) / 2;
    let h_heavy_bottom = h_heavy_top + hv;
    let h_double_top = (h_light_top - lt).max(0);
    let h_double_bottom = h_light_bottom + lt;
    let v_light_left = (W - lt).max(0) / 2;
    let v_light_right = v_light_left + lt;
    let v_heavy_left = (W - hv).max(0) / 2;
    let v_heavy_right = v_heavy_left + hv;
    let v_double_left = (v_light_left - lt).max(0);
    let v_double_right = v_light_right + lt;

    let up_bottom = if left == 2 || right == 2 {
        h_heavy_bottom
    } else if left != right || down == up {
        if left == 3 || right == 3 { h_double_bottom } else { h_light_bottom }
    } else if left == 0 && right == 0 {
        h_light_bottom
    } else {
        h_light_top
    };
    let down_top = if left == 2 || right == 2 {
        h_heavy_top
    } else if left != right || up == down {
        if left == 3 || right == 3 { h_double_top } else { h_light_top }
    } else if left == 0 && right == 0 {
        h_light_top
    } else {
        h_light_bottom
    };
    let left_right = if up == 2 || down == 2 {
        v_heavy_right
    } else if up != down || left == right {
        if up == 3 || down == 3 { v_double_right } else { v_light_right }
    } else if up == 0 && down == 0 {
        v_light_right
    } else {
        v_light_left
    };
    let right_left = if up == 2 || down == 2 {
        v_heavy_left
    } else if up != down || right == left {
        if up == 3 || down == 3 { v_double_left } else { v_light_left }
    } else if up == 0 && down == 0 {
        v_light_left
    } else {
        v_light_right
    };

    let mut on = false;
    if up == 1 {
        on = on || sp_box(p, v_light_left, 0, v_light_right, up_bottom);
    }
    if up == 2 {
        on = on || sp_box(p, v_heavy_left, 0, v_heavy_right, up_bottom);
    }
    if up == 3 {
        let lb = if left == 3 { h_light_top } else { up_bottom };
        let rb = if right == 3 { h_light_top } else { up_bottom };
        on = on || sp_box(p, v_double_left, 0, v_light_left, lb) || sp_box(p, v_light_right, 0, v_double_right, rb);
    }
    if right == 1 {
        on = on || sp_box(p, right_left, h_light_top, W, h_light_bottom);
    }
    if right == 2 {
        on = on || sp_box(p, right_left, h_heavy_top, W, h_heavy_bottom);
    }
    if right == 3 {
        let tl = if up == 3 { v_light_right } else { right_left };
        let bl = if down == 3 { v_light_right } else { right_left };
        on = on || sp_box(p, tl, h_double_top, W, h_light_top) || sp_box(p, bl, h_light_bottom, W, h_double_bottom);
    }
    if down == 1 {
        on = on || sp_box(p, v_light_left, down_top, v_light_right, H);
    }
    if down == 2 {
        on = on || sp_box(p, v_heavy_left, down_top, v_heavy_right, H);
    }
    if down == 3 {
        let lt2 = if left == 3 { h_light_bottom } else { down_top };
        let rt2 = if right == 3 { h_light_bottom } else { down_top };
        on = on || sp_box(p, v_double_left, lt2, v_light_left, H) || sp_box(p, v_light_right, rt2, v_double_right, H);
    }
    if left == 1 {
        on = on || sp_box(p, 0, h_light_top, left_right, h_light_bottom);
    }
    if left == 2 {
        on = on || sp_box(p, 0, h_heavy_top, left_right, h_heavy_bottom);
    }
    if left == 3 {
        let tr = if up == 3 { v_light_left } else { left_right };
        let br = if down == 3 { v_light_left } else { left_right };
        on = on || sp_box(p, 0, h_double_top, tr, h_light_top) || sp_box(p, 0, h_light_bottom, br, h_double_bottom);
    }
    on
}

/// box.zig dashHorizontal / dashVertical: `n` dashes, a horizontal row centred with half gaps at both
/// ends, a vertical column from the top with a whole gap at the foot; spare pixels widen the first dashes.
pub fn sp_dash(p: IVec2, W: i32, H: i32, t: i32, n: u32, b: u32) -> bool {
    let vert = (b & 1) != 0;
    let thick = if (b & 2) != 0 { 2 * t } else { t };
    let g = b >> 2;
    let want = if g == 0 {
        t.max(4)
    } else if g == 1 {
        t
    } else {
        2 * t
    };
    let len = if vert { H } else { W };
    let across = if vert { W } else { H };
    let q = if vert { p.yx() } else { p };
    let count = n as i32;
    if len < 2 * count {
        let y0 = (across - t).max(0) / 2;
        return sp_box(q, 0, y0, len, y0 + t);
    }
    let gap = want.min(len / (2 * count));
    let total = len - count * gap;
    let dash = total / count;
    let extra = total - dash * count;
    let y0 = (across - thick).max(0) / 2;
    let mut x = if vert { 0 } else { gap / 2 };
    let mut on = false;
    for i in 0..4 {
        if i < count {
            let x1 = x + dash + if i < extra { 1 } else { 0 };
            on = on || sp_box(q, x, y0, x1, y0 + thick);
            x = x1 + gap;
        }
    }
    on
}

/// braille.zig: two columns of four square dots, Ghostty's whole-pixel layout of dot, spacing and margins.
pub fn sp_braille(p: IVec2, W: i32, H: i32, bits: u32) -> bool {
    let mut w = (W / 4).min(H / 8);
    let mut xs = W / 4;
    let mut ys = H / 8;
    let mut xm = xs / 2;
    let mut ym = ys / 2;
    let mut xl = W - 2 * xm - xs - 2 * w;
    let mut yl = H - 2 * ym - 3 * ys - 4 * w;
    if xl >= 2 && yl >= 4 && w == 0 {
        w += 1;
        xl -= 2;
        yl -= 4;
    }
    if xl >= 2 && xm == 0 {
        xm = 1;
        xl -= 2;
    }
    if yl >= 2 && ym == 0 {
        ym = 1;
        yl -= 2;
    }
    if xl >= 1 {
        xs += 1;
        xl -= 1;
    }
    if yl >= 3 {
        ys += 1;
        yl -= 3;
    }
    if xl >= 2 {
        xm += 1;
        xl -= 2;
    }
    if yl >= 2 {
        ym += 1;
        yl -= 2;
    }
    if xl >= 2 && yl >= 4 {
        w += 1;
        xl -= 2;
        yl -= 4;
    }
    // Slang leaves the remainders unread from here on, as Ghostty does.
    let _ = (xl, yl);
    let x0 = xm;
    let x1 = xm + w + xs;
    // Dots in bit order: tl, ul, ll, tr, ur, lr, bl, br (the code point's own).
    let cols = [x0, x0, x0, x1, x1, x1, x0, x1];
    let rows = [0, 1, 2, 0, 1, 2, 3, 3];
    let mut on = false;
    for i in 0..8 {
        let y = ym + rows[i] * (w + ys);
        on = on || (((bits >> i as u32) & 1) != 0 && sp_box(p, cols[i], y, cols[i] + w, y + w));
    }
    on
}

/// box.zig arc: a light line from the middle of one edge, down a straight piece, round a cubic (control
/// points a quarter of the radius from the centre) and out to the middle of the next edge. `corner`: 0 tl,
/// 1 tr, 2 bl, 3 br, the corner it bends toward (Ghostty's names: ╭ is .br). Returns the distance to the
/// stroke's middle, pixels.
pub fn sp_arc(p: Vec2, W: i32, H: i32, t: i32, corner: u32) -> f32 {
    let fw = W as f32;
    let fh = H as f32;
    let ft = t as f32;
    let cx = ((W - t).max(0) / 2) as f32 + ft * 0.5;
    let cy = ((H - t).max(0) / 2) as f32 + ft * 0.5;
    let r = fw.min(fh) * 0.5;
    let s = 0.25;
    let sx = if (corner & 1) != 0 { 1.0 } else { -1.0 };
    let sy = if (corner & 2) != 0 { 1.0 } else { -1.0 };
    let ey = if sy > 0.0 { fh } else { 0.0 };
    let ex = if sx > 0.0 { fw } else { 0.0 };
    let p0 = vec2(cx, cy + sy * r);
    let p1 = vec2(cx, cy + sy * s * r);
    let p2 = vec2(cx + sx * s * r, cy);
    let p3 = vec2(cx + sx * r, cy);
    let mut d = sd_stub(p, vec2(cx, ey), p0).min(sd_stub(p, p3, vec2(ex, cy)));
    let mut prev = p0;
    // Slang's `for (i = 1; i <= 24; i++)`.
    for i in 1..25 {
        let u = i as f32 / 24.0;
        let v = 1.0 - u;
        let q = v * v * v * p0 + 3.0 * v * v * u * p1 + 3.0 * v * u * u * p2 + u * u * u * p3;
        d = d.min(sd_stub(p, prev, q));
        prev = q;
    }
    d
}

pub fn sprite_cov(s: SpriteInst, ip: IVec2) -> f32 {
    let kind = s.shape.x;
    let a = s.shape.y;
    let b = s.shape.z;
    let t = (s.shape.w as i32).max(1);
    let W = s.rect.z;
    let H = s.rect.w;
    let p = ip.as_vec2() + 0.5;
    let fw = W as f32;
    let fh = H as f32;
    let ft = t as f32;
    if kind == SPRITE_SHADE {
        return a as f32 / 255.0;
    }
    if kind == SPRITE_BLOCK {
        // block.zig blockShade: a side's share rounded to whole pixels, flush to that side.
        let f = b as f32 / 8.0;
        let w = (fw * f).round() as i32;
        let h = (fh * f).round() as i32;
        let on = if a == 0 {
            sp_box(ip, 0, 0, w, H)
        } else if a == 1 {
            sp_box(ip, W - w, 0, W, H)
        } else if a == 2 {
            sp_box(ip, 0, 0, W, h)
        } else {
            sp_box(ip, 0, H - h, W, H)
        };
        return if on { 1.0 } else { 0.0 };
    }
    if kind == SPRITE_MOSAIC {
        // Quadrants (block.zig) and sextants (symbols_for_legacy_computing.zig): common.zig `fill` of halves
        // and thirds.
        let rows = b as f32;
        let mut on = false;
        for i in 0..6 {
            let c = (i % 2) as f32;
            let r = (i / 2) as f32;
            on = on
                || (((a >> i as u32) & 1) != 0
                    && sp_box(ip, frac_lo(c * 0.5, W), frac_lo(r / rows, H), frac_hi((c + 1.0) * 0.5, W), frac_hi((r + 1.0) / rows, H)));
        }
        return if on { 1.0 } else { 0.0 };
    }
    if kind == SPRITE_LINES {
        return if sp_lines(ip, W, H, t, a) { 1.0 } else { 0.0 };
    }
    if kind == SPRITE_DASH {
        return if sp_dash(ip, W, H, t, a, b) { 1.0 } else { 0.0 };
    }
    if kind == SPRITE_BRAILLE {
        return if sp_braille(ip, W, H, a) { 1.0 } else { 0.0 };
    }
    if kind == SPRITE_ARC {
        return sp_aa(sp_arc(p, W, H, t, a) - ft * 0.5);
    }
    if kind == SPRITE_DIAGONAL {
        // box.zig lightDiagonal*: corner to corner, overshooting each by half a pixel along the slope.
        let slx = 1.0f32.min(fw / fh);
        let sly = 1.0f32.min(fh / fw);
        let d1 = sd_stub(p, vec2(fw + 0.5 * slx, -0.5 * sly), vec2(-0.5 * slx, fh + 0.5 * sly));
        let d2 = sd_stub(p, vec2(-0.5 * slx, -0.5 * sly), vec2(fw + 0.5 * slx, fh + 0.5 * sly));
        let d = (if (a & 1) != 0 { d1 } else { 1e9 }).min(if (a & 2) != 0 { d2 } else { 1e9 });
        return sp_aa(d - ft * 0.5);
    }
    if kind == SPRITE_POWERLINE {
        // powerline.zig: E0B0 a solid triangle, E0B1 its outline as a light chevron, E0B4 a solid half disc
        // (a capsule against the left edge), E0B5 the same stroked inside its edge; b mirrors them (E0B2,
        // E0B3, E0B6, E0B7).
        let q = if b != 0 { vec2(fw - p.x, p.y) } else { p };
        if a == 0 {
            return sp_aa(sd_quad(q, vec2(0.0, 0.0), vec2(fw, fh * 0.5), vec2(0.0, fh), vec2(0.0, 0.0)));
        }
        if a == 1 {
            return sp_aa(sd_stub(q, vec2(0.0, 0.0), vec2(fw, fh * 0.5)).min(sd_stub(q, vec2(fw, fh * 0.5), vec2(0.0, fh))) - ft * 0.5);
        }
        let r = fw.min(fh * 0.5);
        let d = sd_stub(q, vec2(0.0, r), vec2(0.0, fh - r)) - r;
        if a == 4 {
            return sp_aa(d);
        }
        return sp_aa(d) * sp_aa(-(d + ft));
    }
    0.0
}

#[gpu::fragment]
pub fn sprite_fs(v: SpriteV, insts: &[SpriteInst]) -> Vec4 {
    let s = insts[*v.inst as usize];
    // The pixel this fragment is, whole pixels from the cell's top left (the position is the pixel's centre).
    let ip = v.pos.0.xy().floor().as_ivec2() - s.rect.xy();
    let cov = sprite_cov(s, ip);
    let fa = s.fg.w * cov;
    let ga = s.bg.w * (1.0 - fa);
    (s.fg.rgb() * fa + s.bg.rgb() * ga).extend(fa + ga)
}

// ---- text -----------------------------------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, gpu::Varyings)]
pub struct GlyphV {
    pub pos: Position,
    pub uv: Vec2,
    pub color: Vec4,
    pub dark: f32,
    pub colour: f32,
    pub clip: Flat<Vec4>,
    pub y: f32,
    pub fade: Flat<Vec2>,
    pub outline: Flat<f32>,
    pub banding: Flat<Vec4>,
    pub gdata: Flat<UVec4>,
    pub x: f32,
    pub clipx: Flat<Vec4>,
}

/// Dynamic dilation (SlugDilate): moves the vertex `pos.xy` along its outward normal `pos.zw` so every edge
/// of the quad is half a pixel further out than the glyph's, whatever the transform `m0 m1 m3` (rows of the
/// object-to-clip matrix) and the viewport `dim` (pixels). Returns the em-space sample coordinates for the
/// moved vertex and writes the moved vertex to `vpos` (Slang's `out`); `jac` is the inverse Jacobian from
/// object space to em space.
pub fn slug_dilate(pos: Vec4, tex: Vec2, jac: Vec4, m0: Vec4, m1: Vec4, m3: Vec4, dim: Vec2, vpos: &mut Vec2) -> Vec2 {
    let n = normalize(pos.zw());
    let s = dot(m3.xy(), pos.xy()) + m3.w;
    let t = dot(m3.xy(), n);

    let u = (s * dot(m0.xy(), n) - t * (dot(m0.xy(), pos.xy()) + m0.w)) * dim.x;
    let v = (s * dot(m1.xy(), n) - t * (dot(m1.xy(), pos.xy()) + m1.w)) * dim.y;

    let s2 = s * s;
    let st = s * t;
    let uv = u * u + v * v;
    let d = pos.zw() * (s2 * (st + uv.sqrt()) / (uv - st * st));

    *vpos = pos.xy() + d;
    vec2(tex.x + dot(d, jac.xy()), tex.y + dot(d, jac.zw()))
}

/// Device pixels to a point for an outline glyph: the eased zoom's, so its edges are half a pixel out at
/// every step.
pub fn live_px(frame: &Frame) -> f32 {
    if frame.live > 0.0 { frame.live } else { frame.scale }
}

/// `p` turned by `angle` radians about `m`, clockwise (y down).
fn turned(p: Vec2, m: Vec2, angle: f32) -> Vec2 {
    let d = p - m;
    let sn = angle.sin();
    let cs = angle.cos();
    m + vec2(d.x * cs - d.y * sn, d.x * sn + d.y * cs)
}

#[gpu::vertex]
pub fn glyph_vs(vid: VertexIndex, iid: InstanceIndex, frame: &Frame, insts: &[GlyphInst]) -> GlyphV {
    let g = insts[iid.get() as usize];
    let s = frame.time - g.anim.x;
    let mut a = 0.0;
    let mut rise = 0.0;
    if s >= 0.0 {
        // Step response of a critically damped spring: 0 at birth, settling to 1 with no overshoot.
        let x = g.anim.y * s;
        let k = 1.0 - (1.0 + x) * (-x).exp();
        a = k;
        rise = g.anim.z * (1.0 - k);
    }
    let c = CORNERS[vid.get() as usize];
    let col = pick(frame.dark, g.col_d, g.col_l);
    let mut o = GlyphV {
        pos: Position::default(),
        uv: Vec2::default(),
        color: col.rgb().extend(col.w * a * g.anim.w),
        dark: frame.dark,
        colour: g.kind.x,
        clip: Flat(g.clip),
        y: 0.0,
        fade: Flat(g.fade.xy()),
        outline: Flat(g.kind.w),
        banding: Flat(g.band),
        gdata: Flat(g.gdata),
        x: 0.0,
        clipx: Flat(g.clipx),
    };
    if g.kind.w > 0.5 {
        // Outline glyph (a brand mark, icons.rs): the quad is its em box (uv, the top-left corner then the
        // bottom-right) shown on rect. Object space is em space; the rows take it to clip space (points to
        // ndc, y flipped), and each corner's normal points out of the box (`span` is negative in y: ems go
        // up, points down).
        let em = lerp(g.uv.xy(), g.uv.zw(), c);
        let span = g.uv.zw() - g.uv.xy();
        let n = span.sign() * (c * 2.0 - 1.0);
        let sc = g.rect.zw() / (span.sign() * span.abs().max(vec2(1e-6, 1e-6)));
        let top = g.rect.xy() + vec2(0.0, rise) - g.uv.xy() * sc;
        let m0 = vec4(2.0 * sc.x / frame.view.x, 0.0, 0.0, 2.0 * (top.x - frame.place.x) / frame.view.x - 1.0);
        let m1 = vec4(0.0, -2.0 * sc.y / frame.view.y, 0.0, 1.0 - 2.0 * (top.y - frame.place.y) / frame.view.y);
        let m3 = vec4(0.0, 0.0, 0.0, 1.0);
        let dim = frame.view * live_px(frame) / (1.0 + 2.0 * frame.slug.w);
        let mut vpos = Vec2::default();
        o.uv = slug_dilate(vec4(em.x, em.y, n.x, n.y), em, vec4(1.0, 0.0, 0.0, 1.0), m0, m1, m3, dim, &mut vpos);
        o.pos = Position(vec4(vpos.x * m0.x + m0.w, vpos.y * m1.y + m1.w, 0.0, 1.0));
        o.y = top.y + vpos.y * sc.y;
        o.x = top.x + vpos.x * sc.x;
        // A turned mark (the tree's disclosure chevron): its dilated corners turned about its middle,
        // clockwise (y down). The em coordinates ride along, so the outline turns whole.
        if g.clip.z != 0.0 {
            let m = g.rect.xy() + g.rect.zw() * 0.5 + vec2(0.0, rise);
            let q = turned(vec2(o.x, o.y), m, g.clip.z);
            o.pos = clip_pos(q, frame);
            o.x = q.x;
            o.y = q.y;
        }
        return o;
    }
    let mut pt = g.rect.xy() + c * g.rect.zw() + vec2(0.0, rise);
    // A turned bitmap (a disclosure chevron): its corners turned about its middle, clockwise (y down).
    if g.clip.z != 0.0 {
        let m = g.rect.xy() + g.rect.zw() * 0.5 + vec2(0.0, rise);
        pt = turned(pt, m, g.clip.z);
    }
    o.pos = clip_pos(pt, frame);
    o.uv = lerp(g.uv.xy(), g.uv.zw(), c);
    o.y = pt.y;
    o.x = pt.x;
    o
}

/// The root eligibility code of a sample-relative quadratic Bézier from the signs of its three y (or x)
/// coordinates: bit 0 set: the first root counts, bit 8: the second does (SlugPixelShader.hlsl
/// CalcRootCode).
pub fn slug_root_code(y1: f32, y2: f32, y3: f32) -> u32 {
    let i1 = asuint(y1) >> 31;
    let i2 = asuint(y2) >> 30;
    let i3 = asuint(y3) >> 29;

    let mut shift = (i2 & 2) | (i1 & !2);
    shift = (i3 & 4) | (shift & !4);

    (0x2E74 >> shift) & 0x0101
}

/// Where the curve crosses y = 0 (SolveHorizPoly; its quadratic is a t^2 - 2 b t + c): the x coordinates
/// at the two roots. A nearly linear polynomial solves -2 b t + c = 0 instead; the divisions are guarded so
/// no infinity is made.
pub fn slug_solve_horiz(p12: Vec4, p3: Vec2) -> Vec2 {
    let a = p12.xy() - p12.zw() * 2.0 + p3;
    let b = p12.xy() - p12.zw();
    let lin = a.y.abs() < 1.0 / 65536.0;
    let ra = 1.0 / if lin { 1.0 } else { a.y };
    let rb = 0.5 / if b.y == 0.0 { 1.0 } else { b.y };

    let d = (b.y * b.y - a.y * p12.y).max(0.0).sqrt();
    let t1 = if lin { p12.y * rb } else { (b.y - d) * ra };
    let t2 = if lin { t1 } else { (b.y + d) * ra };

    vec2((a.x * t1 - b.x * 2.0) * t1 + p12.x, (a.x * t2 - b.x * 2.0) * t2 + p12.x)
}

/// Where the curve crosses x = 0 (SolveVertPoly): the y coordinates at the two roots.
pub fn slug_solve_vert(p12: Vec4, p3: Vec2) -> Vec2 {
    let a = p12.xy() - p12.zw() * 2.0 + p3;
    let b = p12.xy() - p12.zw();
    let lin = a.x.abs() < 1.0 / 65536.0;
    let ra = 1.0 / if lin { 1.0 } else { a.x };
    let rb = 0.5 / if b.x == 0.0 { 1.0 } else { b.x };

    let d = (b.x * b.x - a.x * p12.x).max(0.0).sqrt();
    let t1 = if lin { p12.x * rb } else { (b.x - d) * ra };
    let t2 = if lin { t1 } else { (b.x + d) * ra };

    vec2((a.y * t1 - b.y * 2.0) * t1 + p12.y, (a.y * t2 - b.y * 2.0) * t2 + p12.y)
}

/// The coverage of the pixel centred at em-space `render` (SlugRender): casts a horizontal and a vertical
/// ray through the glyph's curves, reading only the curves of the band the pixel is in, and blends the two
/// estimates by how near each ray's nearest edge is. `ppem` is the pixels to the em on each axis; `gd` is
/// the glyph's `gdata`. The band buffer holds, from gd.x, a (count, list offset) pair per horizontal band
/// then per vertical one, then the lists of curve indices into `curves` (two Vec4s a curve: p1 p2, then
/// p3), sorted so a ray can stop at the first curve left of it. The outline winds counter-clockwise (ink on
/// the left of every edge), so a root of the first kind is a left edge going down (horizontal ray) or a top
/// edge going left (vertical ray), the second kind the opposite sides: `embolden` pixels moves each edge
/// that far out, as the font smoothing of the system's bitmaps does at small sizes.
pub fn slug_render(curves: &[Vec4], bands: &[u32], render: Vec2, band_tf: Vec4, gd: UVec4, ppem: Vec2, embolden: f32) -> f32 {
    let e = embolden;
    let band_max = ivec2(gd.y as i32, gd.z as i32);
    let band_ix = (render * band_tf.xy() + band_tf.zw()).as_ivec2().clamp(ivec2(0, 0), band_max);
    let at = gd.x;
    let rr = vec4(render.x, render.y, render.x, render.y);

    let mut xcov = 0.0;
    let mut xwgt: f32 = 0.0;
    let hb = at + 2 * band_ix.y as u32;
    let hcount = bands[hb as usize];
    let hlist = at + bands[(hb + 1) as usize];
    for i in 0..hcount {
        let slot = bands[(hlist + i) as usize];
        let p12 = curves[slot as usize] - rr;
        let p3 = curves[(slot + 1) as usize].xy() - render;

        if p12.x.max(p12.z).max(p3.x) * ppem.x < -0.5 - e {
            break;
        }

        let code = slug_root_code(p12.y, p12.w, p3.y);
        if code != 0 {
            let r = slug_solve_horiz(p12, p3) * ppem.x;
            if (code & 1) != 0 {
                let x = r.x - e;
                xcov += saturate(x + 0.5);
                xwgt = xwgt.max(saturate(1.0 - x.abs() * 2.0));
            }
            if code > 1 {
                let x = r.y + e;
                xcov -= saturate(x + 0.5);
                xwgt = xwgt.max(saturate(1.0 - x.abs() * 2.0));
            }
        }
    }

    let mut ycov = 0.0;
    let mut ywgt: f32 = 0.0;
    let vb = at + 2 * (band_max.y as u32 + 1 + band_ix.x as u32);
    let vcount = bands[vb as usize];
    let vlist = at + bands[(vb + 1) as usize];
    for j in 0..vcount {
        let slot = bands[(vlist + j) as usize];
        let p12 = curves[slot as usize] - rr;
        let p3 = curves[(slot + 1) as usize].xy() - render;

        if p12.y.max(p12.w).max(p3.y) * ppem.y < -0.5 - e {
            break;
        }

        let code = slug_root_code(p12.x, p12.z, p3.x);
        if code != 0 {
            let r = slug_solve_vert(p12, p3) * ppem.y;
            if (code & 1) != 0 {
                let y = r.x + e;
                ycov -= saturate(y + 0.5);
                ywgt = ywgt.max(saturate(1.0 - y.abs() * 2.0));
            }
            if code > 1 {
                let y = r.y - e;
                ycov += saturate(y + 0.5);
                ywgt = ywgt.max(saturate(1.0 - y.abs() * 2.0));
            }
        }
    }

    // Absolute values make either winding convention work (CalcCoverage; nonzero fill).
    let coverage = ((xcov * xwgt + ycov * ywgt).abs() / (xwgt + ywgt).max(1.0 / 65536.0)).max(xcov.abs().min(ycov.abs()));
    saturate(coverage)
}

#[gpu::fragment]
pub fn glyph_fs(v: GlyphV, frame: &Frame, atlas: &Texture2d, smp: &Sampler, curves: &[Vec4], bands: &[u32]) -> Vec4 {
    // The em box's size in pixels from the sample coordinates' derivatives (uniform control flow: before any branch).
    let fw = dpdx(v.uv).abs() + dpdy(v.uv).abs();
    if *v.outline > 0.5 {
        let ppem = 1.0 / fw.max(vec2(1e-9, 1e-9));
        let embolden = (0.5 * (ppem.x + ppem.y) * frame.slug.x).min(frame.slug.w);
        let cov = slug_render(curves, bands, v.uv, *v.banding, *v.gdata, ppem, embolden);
        // Gamma on the coverage per appearance, its own: this is exact geometric coverage, not the platform's smoothing.
        let go = if v.dark > 0.5 { frame.slug.y } else { frame.slug.z };
        let ao = pow(cov * in_band(v.y, *v.clip, *v.fade, live_px(frame)) * in_band(v.x, *v.clipx, v.clipx.zw(), live_px(frame)), go)
            * v.color.w;
        return (v.color.rgb() * ao).extend(ao);
    }
    // The atlas is premultiplied RGBA: a colour glyph's own pixels, or a plain glyph's coverage as white.
    let t = atlas.sample_level(smp, v.uv, 0.0) * in_band(v.y, *v.clip, *v.fade, frame.scale) * in_band(v.x, *v.clipx, v.clipx.zw(), frame.scale);
    if v.colour > 0.5 {
        return t * v.color.w;
    }
    // Coverage is gamma-adjusted per appearance: light text on dark reads thin and is boosted, dark on light is thinned.
    let g = if v.dark > 0.5 { frame.tune.x } else { frame.tune.y };
    let a = pow(t.w, g) * v.color.w;
    (v.color.rgb() * a).extend(a)
}
