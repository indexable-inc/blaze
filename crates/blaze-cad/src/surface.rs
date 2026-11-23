//! Surface-authoring & PBR baking systems for CAD game assets:
//!
//! 1. **Surface-conforming vector decals** ([`Decal`], [`DecalShape`], [`DecalMaterial`],
//!    [`DecalProjection`], [`SeamClip`], [`evaluate_decal`]): a built-in `#![no_std]` geometric
//!    technical/stencil vector font (`0-9`, `A-Z`, punctuation) and 2D SVG path parser
//!    ([`parse_svg_path`]) projected onto flat or curved CAD surfaces via exact orthographic or
//!    curvature-compensated / geodesic chord-marching conformal arc-length mapping, clipped cleanly
//!    against material-role boundaries, boxes, normal deviation, and surface depth, with optional
//!    tangent-space relief normal baking and automatic [`FeatureTarget`] sizing
//!    ([`decal_feature_target`]).
//! 2. **Multi-material regions within one continuous shell** ([`MaterialRule`], [`MaterialRegion`],
//!    [`assign_material_regions`]): painter's-order assignment of [`MaterialRole`] per triangle
//!    using spatial regions, CAD solid masks, normal cones, recessed depth from an outer shell,
//!    CAD field curvature rims, or named surface patches — applied before decimation so material
//!    boundaries survive simplification.
//! 3. **Directional finishes & physical-unit surface wear** ([`SurfaceFinish`], [`EdgeWearMask`],
//!    [`FinishAssignment`], [`evaluate_finish`]): deterministic 3D brushed-metal grooves, molded
//!    micro-grain, CNC-turned lathe feeds, and curvature-driven metallic edge wear authored in
//!    physical [`Length`] units with zero ambient RNG.
//! 4. **Deterministic PBR detail baker** ([`bake_surface_maps`], [`BakeOpts`], [`BakedTexture`],
//!    [`BakedMaps`], [`baked_material`]): rasterizes per-role PBR base colour (sRGB), glTF 2.0
//!    metallic-roughness, exact high-res CAD normals, physical finishes, edge wear, and anti-aliased
//!    surface decals with relief into tangent-space textures, dilates UV chart gutters to prevent
//!    mip/bilinear seam bleeding, and encodes deterministic PNGs via `blaze_cad_out::png`.

#![blaze::sans_io]

use blaze_cad_field::{
    Bound, CadPlant, CompileOpts, Detail, Graph, Name, NodeId, Scratch, Solid, Tape, compile, eval_grad, eval_point,
};
use blaze_cad_mesh::{FeatureProtect, FeatureRegion, MaterialRole, Mesh, MeshVecs, MeshView};
use blaze_cad_out::{Image, Limits, Material, PngKind, PngOpts, png};
use blaze_cad_picture::{MaterialMaps, TextureMap};
use blaze_deflate::Level;
use blaze_math::{Aabb3, Vec2, Vec3};
use blaze_own::planted::Planted;
use blaze_own::{Alloc, Vec};
use blaze_units::{Angle, Length};

use crate::region::AttachmentFrame;
use crate::{FeatureKind, FeatureTarget, FeatureTargetOpts, ModelError, STEEL};

// ---- Constants (bounded progress, rule 18) -----------------------------------------------

/// Micro-arcseconds per degree (`3600 * 10^6`), for `const` [`Angle`] defaults.
const UAS_PER_DEGREE_I64: i64 = 3_600_000_000;

/// Maximum flattened 2D line segments accepted from one SVG `d="..."` path (rule 18).
pub const MAX_SVG_SEGMENTS: usize = 2048;

/// Maximum characters evaluated in one [`DecalShape::Text`] string (rule 18).
const MAX_TEXT_CHARS: usize = 256;

/// Chords per cubic Bezier (`C`/`c`/`S`/`s`) segment when flattening SVG paths.
const CUBIC_CHORDS: usize = 8;

/// Chords per quadratic Bezier (`Q`/`q`/`T`/`t`) segment when flattening SVG paths.
const QUAD_CHORDS: usize = 6;

/// Fixed bounded geodesic surface-marching steps for [`DecalProjection::ConformalArcLength`].
const GEODESIC_STEPS: usize = 4;

/// Maximum UV gutter dilation passes in [`bake_surface_maps`] (rule 18).
const MAX_GUTTER_PASSES: u32 = 64;

/// Default glyph box width as a fraction of cap height.
const GLYPH_W: f64 = 0.65;

/// Default inter-glyph sidebearing as a fraction of cap height (added to caller `tracking`).
const GLYPH_SIDEBEARING: f64 = 0.15;

/// Line-to-line vertical pitch for multi-line stencil text as a fraction of cap height.
const TEXT_LINE_PITCH: f64 = 1.35;

// ---- Deterministic math & colour helpers -------------------------------------------------

#[inline]
fn to_v3(p: [f32; 3]) -> Vec3 {
    Vec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2]))
}

#[inline]
fn canonical_tangent(n: Vec3) -> Vec3 {
    let helper = if n.z.abs() < 0.9 { Vec3::new(0.0, 0.0, 1.0) } else { Vec3::new(1.0, 0.0, 0.0) };
    helper.cross(n).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0))
}

#[inline]
fn smoothstep(edge0: f64, edge1: f64, x: f64) -> f64 {
    let span = edge1 - edge0;
    if span.abs() <= 1e-18 {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / span).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
fn smoothstep_deriv(edge0: f64, edge1: f64, x: f64) -> f64 {
    let span = edge1 - edge0;
    if span.abs() <= 1e-18 {
        return 0.0;
    }
    let t = (x - edge0) / span;
    if t <= 0.0 || t >= 1.0 {
        return 0.0;
    }
    (6.0 * t * (1.0 - t)) / span
}

#[inline]
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Deterministic linear `[0, 1]` to sRGB `[0, 1]` using `blaze_math::pow`.
#[inline]
fn linear_to_srgb(c: f64) -> f64 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * blaze_math::pow(c, 1.0 / 2.4) - 0.055
    }
}

/// Quantize a normalized `[0, 1]` channel to `u8`, rounded to nearest-even.
#[inline]
fn quantize_u8(v: f64) -> u8 {
    let scaled = core::f64::math::round_ties_even(v.clamp(0.0, 1.0) * 255.0);
    scaled.max(0.0).min(255.0) as u8
}

/// Exact squared distance from 2D point `p` to line segment `a..=b`.
#[inline]
fn dist_sq_segment_2d(p: Vec2, a: Vec2, b: Vec2) -> f64 {
    let ab = b - a;
    let ap = p - a;
    let denom = ab.dot(ab);
    let t = if denom > 1e-24 { (ap.dot(ab) / denom).clamp(0.0, 1.0) } else { 0.0 };
    let d = ap - ab * t;
    d.dot(d)
}

/// Exact distance from 2D point `p` to line segment `a..=b`.
#[inline]
fn dist_segment_2d(p: Vec2, a: Vec2, b: Vec2) -> f64 {
    blaze_math::sqrt(dist_sq_segment_2d(p, a, b))
}

/// Exact 2D rounded-box signed distance centered at `(0, 0)` with half-extents `(hx, hy)` and
/// corner radius `r`.
#[inline]
fn sd_rounded_box_2d(p: Vec2, hx: f64, hy: f64, r: f64) -> f64 {
    let hx = hx.max(0.0);
    let hy = hy.max(0.0);
    let r = r.clamp(0.0, hx.min(hy));
    let qx = p.x.abs() - (hx - r);
    let qy = p.y.abs() - (hy - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    let out = blaze_math::sqrt(ox * ox + oy * oy);
    let inside = qx.max(qy).min(0.0);
    out + inside - r
}

/// Estimate CAD field divergence `div(n)` (equal to `2/R` on a sphere of radius `R` and `1/R` on a
/// cylinder of radius `R`) at `p` along the tangent plane perpendicular to `n_unit`.
fn estimate_cad_curvature(tape: &Tape<'_>, p: Vec3, n_unit: Vec3, sc: &mut Scratch<'_>) -> f64 {
    let h = 5e-4; // 0.5 mm deterministic finite-difference step
    let e1 = canonical_tangent(n_unit);
    let e2 = n_unit.cross(e1).normalized().unwrap_or_else(|| canonical_tangent(e1));
    let (_, gp1) = eval_grad(tape, p + e1 * h, sc);
    let (_, gm1) = eval_grad(tape, p - e1 * h, sc);
    let (_, gp2) = eval_grad(tape, p + e2 * h, sc);
    let (_, gm2) = eval_grad(tape, p - e2 * h, sc);
    let np1 = gp1.normalized().unwrap_or(n_unit);
    let nm1 = gm1.normalized().unwrap_or(n_unit);
    let np2 = gp2.normalized().unwrap_or(n_unit);
    let nm2 = gm2.normalized().unwrap_or(n_unit);
    let d1 = (np1 - nm1).dot(e1) / (2.0 * h);
    let d2 = (np2 - nm2).dot(e2) / (2.0 * h);
    if d1.is_finite() && d2.is_finite() { d1 + d2 } else { 0.0 }
}

/// Estimate signed curvature on a triangle from its vertex normals when no CAD tape is present.
#[inline]
fn estimate_mesh_tri_curvature(p0: Vec3, p1: Vec3, p2: Vec3, n0: Vec3, n1: Vec3, n2: Vec3) -> f64 {
    let edges = [(p1 - p0, n1 - n0), (p2 - p1, n2 - n1), (p0 - p2, n0 - n2)];
    let mut sum = 0.0f64;
    let mut count = 0.0f64;
    for (dp, dn) in edges {
        let len_sq = dp.dot(dp);
        if len_sq > 1e-18 {
            // Positive for convex edges where normals diverge outward along the edge vector.
            sum += dn.dot(dp) / len_sq;
            count += 1.0;
        }
    }
    if count > 0.0 { (sum / count) * 2.0 } else { 0.0 }
}

// ==========================================================================================
// 1. BUILT-IN GEOMETRIC TECHNICAL / STENCIL VECTOR FONT (`#![no_std]`)
// ==========================================================================================

// Normalized glyph box: X in [0.0, 0.65], Y in [0.0, 1.0] (Y=0 baseline, Y=1 cap height).
// Chamfered industrial corners give crisp technical/aerospace readability at every scale.
const X0: f64 = 0.00;
const X1: f64 = 0.13;
const XC: f64 = 0.325;
const X2: f64 = 0.52;
const X3: f64 = 0.65;

const Y0: f64 = 0.00;
const Y1: f64 = 0.14;
const Y2: f64 = 0.44;
const YM: f64 = 0.50;
const Y3: f64 = 0.56;
const Y4: f64 = 0.86;
const Y5: f64 = 1.00;

/// Each segment is `[x0, y0, x1, y1]` in normalized `[0, 0.65] × [0, 1.0]` glyph space.
type GlyphSegs = &'static [[f64; 4]];

const G_0: GlyphSegs = &[
    [X1, Y0, X2, Y0],
    [X2, Y0, X3, Y1],
    [X3, Y1, X3, Y4],
    [X3, Y4, X2, Y5],
    [X2, Y5, X1, Y5],
    [X1, Y5, X0, Y4],
    [X0, Y4, X0, Y1],
    [X0, Y1, X1, Y0],
];
const G_1: GlyphSegs = &[[0.14, 0.82, 0.36, Y5], [0.36, Y5, 0.36, Y0], [0.12, Y0, 0.58, Y0]];
const G_2: GlyphSegs = &[
    [X0, Y4, X1, Y5],
    [X1, Y5, X2, Y5],
    [X2, Y5, X3, Y4],
    [X3, Y4, X3, 0.64],
    [X3, 0.64, X0, 0.16],
    [X0, 0.16, X0, Y0],
    [X0, Y0, X3, Y0],
];
const G_3: GlyphSegs = &[
    [X0, Y4, X1, Y5],
    [X1, Y5, X2, Y5],
    [X2, Y5, X3, Y4],
    [X3, Y4, X3, Y3],
    [X3, Y3, X2, YM],
    [0.24, YM, X2, YM],
    [X2, YM, X3, Y2],
    [X3, Y2, X3, Y1],
    [X3, Y1, X2, Y0],
    [X2, Y0, X1, Y0],
    [X1, Y0, X0, Y1],
];
const G_4: GlyphSegs = &[[0.48, Y0, 0.48, Y5], [0.48, Y5, X0, 0.34], [X0, 0.34, X3, 0.34]];
const G_5: GlyphSegs = &[
    [X3, Y5, X0, Y5],
    [X0, Y5, X0, Y3],
    [X0, Y3, X2, Y3],
    [X2, Y3, X3, Y2],
    [X3, Y2, X3, Y1],
    [X3, Y1, X2, Y0],
    [X2, Y0, X1, Y0],
    [X1, Y0, X0, Y1],
];
const G_6: GlyphSegs = &[
    [X3, Y4, X2, Y5],
    [X2, Y5, X1, Y5],
    [X1, Y5, X0, Y4],
    [X0, Y4, X0, Y1],
    [X0, Y1, X1, Y0],
    [X1, Y0, X2, Y0],
    [X2, Y0, X3, Y1],
    [X3, Y1, X3, Y2],
    [X3, Y2, X2, 0.54],
    [X2, 0.54, X0, 0.54],
];
const G_7: GlyphSegs = &[[X0, Y5, X3, Y5], [X3, Y5, X3, Y4], [X3, Y4, 0.22, Y0]];
const G_8: GlyphSegs = &[
    [X1, YM, X0, Y3],
    [X0, Y3, X0, Y4],
    [X0, Y4, X1, Y5],
    [X1, Y5, X2, Y5],
    [X2, Y5, X3, Y4],
    [X3, Y4, X3, Y3],
    [X3, Y3, X2, YM],
    [X1, YM, X2, YM],
    [X1, YM, X0, Y2],
    [X0, Y2, X0, Y1],
    [X0, Y1, X1, Y0],
    [X1, Y0, X2, Y0],
    [X2, Y0, X3, Y1],
    [X3, Y1, X3, Y2],
    [X3, Y2, X2, YM],
];
const G_9: GlyphSegs = &[
    [X0, Y1, X1, Y0],
    [X1, Y0, X2, Y0],
    [X2, Y0, X3, Y1],
    [X3, Y1, X3, Y4],
    [X3, Y4, X2, Y5],
    [X2, Y5, X1, Y5],
    [X1, Y5, X0, Y4],
    [X0, Y4, X0, Y3],
    [X0, Y3, X1, 0.46],
    [X1, 0.46, X3, 0.46],
];

const G_A: GlyphSegs = &[[X0, Y0, 0.26, Y5], [0.26, Y5, 0.39, Y5], [0.39, Y5, X3, Y0], [0.11, 0.34, 0.54, 0.34]];
const G_B: GlyphSegs = &[
    [X0, Y0, X0, Y5],
    [X0, Y5, X2, Y5],
    [X2, Y5, X3, Y4],
    [X3, Y4, X3, 0.62],
    [X3, 0.62, X2, 0.52],
    [X0, 0.52, X2, 0.52],
    [X2, 0.52, X3, 0.42],
    [X3, 0.42, X3, Y1],
    [X3, Y1, X2, Y0],
    [X2, Y0, X0, Y0],
];
const G_C: GlyphSegs = &[
    [X3, Y4, X2, Y5],
    [X2, Y5, X1, Y5],
    [X1, Y5, X0, Y4],
    [X0, Y4, X0, Y1],
    [X0, Y1, X1, Y0],
    [X1, Y0, X2, Y0],
    [X2, Y0, X3, Y1],
];
const G_D: GlyphSegs = &[
    [X0, Y0, X0, Y5],
    [X0, Y5, 0.48, Y5],
    [0.48, Y5, X3, 0.82],
    [X3, 0.82, X3, 0.18],
    [X3, 0.18, 0.48, Y0],
    [0.48, Y0, X0, Y0],
];
const G_E: GlyphSegs = &[[X3, Y5, X0, Y5], [X0, Y5, X0, Y0], [X0, Y0, X3, Y0], [X0, YM, 0.52, YM]];
const G_F: GlyphSegs = &[[X3, Y5, X0, Y5], [X0, Y5, X0, Y0], [X0, YM, 0.52, YM]];
const G_G: GlyphSegs = &[
    [X3, Y4, X2, Y5],
    [X2, Y5, X1, Y5],
    [X1, Y5, X0, Y4],
    [X0, Y4, X0, Y1],
    [X0, Y1, X1, Y0],
    [X1, Y0, X2, Y0],
    [X2, Y0, X3, Y1],
    [X3, Y1, X3, YM],
    [X3, YM, 0.34, YM],
];
const G_H: GlyphSegs = &[[X0, Y0, X0, Y5], [X3, Y0, X3, Y5], [X0, YM, X3, YM]];
const G_I: GlyphSegs = &[[0.14, Y5, 0.51, Y5], [XC, Y5, XC, Y0], [0.14, Y0, 0.51, Y0]];
const G_J: GlyphSegs = &[[0.18, Y5, X3, Y5], [0.54, Y5, 0.54, Y1], [0.54, Y1, 0.42, Y0], [0.42, Y0, X1, Y0], [X1, Y0, X0, Y1]];
const G_K: GlyphSegs = &[[X0, Y0, X0, Y5], [X3, Y5, X0, 0.42], [0.18, 0.58, X3, Y0]];
const G_L: GlyphSegs = &[[X0, Y5, X0, Y0], [X0, Y0, X3, Y0]];
const G_M: GlyphSegs = &[[X0, Y0, X0, Y5], [X0, Y5, XC, 0.24], [XC, 0.24, X3, Y5], [X3, Y5, X3, Y0]];
const G_N: GlyphSegs = &[[X0, Y0, X0, Y5], [X0, Y5, X3, Y0], [X3, Y0, X3, Y5]];
const G_O: GlyphSegs = G_0;
const G_P: GlyphSegs = &[
    [X0, Y0, X0, Y5],
    [X0, Y5, X2, Y5],
    [X2, Y5, X3, Y4],
    [X3, Y4, X3, 0.58],
    [X3, 0.58, X2, 0.46],
    [X2, 0.46, X0, 0.46],
];
const G_Q: GlyphSegs = &[
    [X1, Y0, X2, Y0],
    [X2, Y0, X3, Y1],
    [X3, Y1, X3, Y4],
    [X3, Y4, X2, Y5],
    [X2, Y5, X1, Y5],
    [X1, Y5, X0, Y4],
    [X0, Y4, X0, Y1],
    [X0, Y1, X1, Y0],
    [0.38, 0.26, X3, Y0],
];
const G_R: GlyphSegs = &[
    [X0, Y0, X0, Y5],
    [X0, Y5, X2, Y5],
    [X2, Y5, X3, Y4],
    [X3, Y4, X3, 0.58],
    [X3, 0.58, X2, 0.48],
    [X2, 0.48, X0, 0.48],
    [0.36, 0.48, X3, Y0],
];
const G_S: GlyphSegs = &[
    [X3, Y4, X2, Y5],
    [X2, Y5, X1, Y5],
    [X1, Y5, X0, Y4],
    [X0, Y4, X0, 0.62],
    [X0, 0.62, X1, 0.52],
    [X1, 0.52, X2, 0.48],
    [X2, 0.48, X3, 0.38],
    [X3, 0.38, X3, Y1],
    [X3, Y1, X2, Y0],
    [X2, Y0, X1, Y0],
    [X1, Y0, X0, Y1],
];
const G_T: GlyphSegs = &[[X0, Y5, X3, Y5], [XC, Y5, XC, Y0]];
const G_U: GlyphSegs = &[
    [X0, Y5, X0, Y1],
    [X0, Y1, X1, Y0],
    [X1, Y0, X2, Y0],
    [X2, Y0, X3, Y1],
    [X3, Y1, X3, Y5],
];
const G_V: GlyphSegs = &[[X0, Y5, XC, Y0], [XC, Y0, X3, Y5]];
const G_W: GlyphSegs = &[[X0, Y5, 0.14, Y0], [0.14, Y0, XC, 0.56], [XC, 0.56, 0.51, Y0], [0.51, Y0, X3, Y5]];
const G_X: GlyphSegs = &[[X0, Y5, X3, Y0], [X3, Y5, X0, Y0]];
const G_Y: GlyphSegs = &[[X0, Y5, XC, 0.46], [X3, Y5, XC, 0.46], [XC, 0.46, XC, Y0]];
const G_Z: GlyphSegs = &[[X0, Y5, X3, Y5], [X3, Y5, X0, Y0], [X0, Y0, X3, Y0]];

const G_DASH: GlyphSegs = &[[0.10, YM, 0.55, YM]];
const G_DOT: GlyphSegs = &[[XC, 0.03, XC, 0.08]];
const G_SLASH: GlyphSegs = &[[0.08, Y0, 0.57, Y5]];
const G_COLON: GlyphSegs = &[[XC, 0.16, XC, 0.22], [XC, 0.72, XC, 0.78]];
const G_PLUS: GlyphSegs = &[[0.08, YM, 0.57, YM], [XC, 0.22, XC, 0.78]];
const G_UNDER: GlyphSegs = &[[X0, Y0, X3, Y0]];
const G_EMPTY: GlyphSegs = &[];

/// Look up the vector stroke polylines for one stencil glyph (case-insensitive ASCII).
#[inline]
fn glyph_segments(ch: char) -> GlyphSegs {
    match ch.to_ascii_uppercase() {
        '0' => G_0,
        '1' => G_1,
        '2' => G_2,
        '3' => G_3,
        '4' => G_4,
        '5' => G_5,
        '6' => G_6,
        '7' => G_7,
        '8' => G_8,
        '9' => G_9,
        'A' => G_A,
        'B' => G_B,
        'C' => G_C,
        'D' => G_D,
        'E' => G_E,
        'F' => G_F,
        'G' => G_G,
        'H' => G_H,
        'I' => G_I,
        'J' => G_J,
        'K' => G_K,
        'L' => G_L,
        'M' => G_M,
        'N' => G_N,
        'O' => G_O,
        'P' => G_P,
        'Q' => G_Q,
        'R' => G_R,
        'S' => G_S,
        'T' => G_T,
        'U' => G_U,
        'V' => G_V,
        'W' => G_W,
        'X' => G_X,
        'Y' => G_Y,
        'Z' => G_Z,
        '-' => G_DASH,
        '.' => G_DOT,
        '/' => G_SLASH,
        ':' => G_COLON,
        '+' => G_PLUS,
        '_' => G_UNDER,
        _ => G_EMPTY,
    }
}

/// Measure total `(width_m, height_m)` of a centered stencil text block.
fn text_bounds_m(text: &str, height_m: f64, tracking: f64) -> (f64, f64) {
    let advance = (GLYPH_W + GLYPH_SIDEBEARING + tracking).max(0.25) * height_m;
    let glyph_w = GLYPH_W * height_m;
    let mut max_line_chars = 0usize;
    let mut cur_line_chars = 0usize;
    let mut lines = 1usize;
    let mut total_chars = 0usize;
    for ch in text.chars() {
        if total_chars >= MAX_TEXT_CHARS {
            break;
        }
        total_chars += 1;
        if ch == '\n' {
            if cur_line_chars > max_line_chars {
                max_line_chars = cur_line_chars;
            }
            cur_line_chars = 0;
            lines += 1;
        } else {
            cur_line_chars += 1;
        }
    }
    if cur_line_chars > max_line_chars {
        max_line_chars = cur_line_chars;
    }
    let w = if max_line_chars == 0 {
        0.0
    } else {
        glyph_w + (max_line_chars - 1) as f64 * advance
    };
    let h = if lines <= 1 {
        height_m
    } else {
        height_m + (lines - 1) as f64 * (TEXT_LINE_PITCH * height_m)
    };
    (w, h)
}

/// Exact 2D signed distance (metres) to a centered technical stencil text block at `(u, v)`.
fn sd_text_2d(text: &str, height_m: f64, stroke_m: f64, tracking: f64, p: Vec2) -> f64 {
    let h = height_m.max(1e-6);
    let half_stroke = (0.5 * stroke_m).max(1e-6);
    let advance = (GLYPH_W + GLYPH_SIDEBEARING + tracking).max(0.25) * h;
    let glyph_w = GLYPH_W * h;
    let line_step = TEXT_LINE_PITCH * h;

    // Pass 1: count lines (bounded by MAX_TEXT_CHARS).
    let mut line_count = 1usize;
    let mut seen = 0usize;
    for ch in text.chars() {
        if seen >= MAX_TEXT_CHARS {
            break;
        }
        seen += 1;
        if ch == '\n' {
            line_count += 1;
        }
    }
    let total_h = h + (line_count - 1) as f64 * line_step;
    let top_baseline_y = 0.5 * total_h - h;

    // Pass 2: evaluate per line with per-line horizontal centering and fast AABB rejection.
    let mut best_d2 = f64::INFINITY;
    let mut line_idx = 0usize;
    let mut bytes_processed = 0usize;
    let bytes = text.as_bytes();
    while bytes_processed <= bytes.len() && line_idx < line_count {
        let rest = &text[bytes_processed..];
        let nl_off = rest.find('\n');
        let line = match nl_off {
            Some(off) => &rest[..off],
            None => rest,
        };
        let char_count = line.chars().count().min(MAX_TEXT_CHARS);
        let base_y = top_baseline_y - (line_idx as f64) * line_step;

        // Vertical culling against this line's band.
        let dy_band = if p.y < base_y {
            base_y - p.y
        } else if p.y > base_y + h {
            p.y - (base_y + h)
        } else {
            0.0
        };
        if char_count > 0 && dy_band * dy_band < best_d2 {
            let line_w = glyph_w + (char_count - 1) as f64 * advance;
            let x_start = -0.5 * line_w;
            let mut ci = 0usize;
            for ch in line.chars() {
                if ci >= MAX_TEXT_CHARS {
                    break;
                }
                let segs = glyph_segments(ch);
                if !segs.is_empty() {
                    let gx0 = x_start + (ci as f64) * advance;
                    let gx1 = gx0 + glyph_w;
                    let dx_box = if p.x < gx0 {
                        gx0 - p.x
                    } else if p.x > gx1 {
                        p.x - gx1
                    } else {
                        0.0
                    };
                    if dx_box * dx_box + dy_band * dy_band < best_d2 {
                        // Transform query point into normalized glyph space [0, 0.65] x [0, 1].
                        let inv_h = 1.0 / h;
                        let q = Vec2::new((p.x - gx0) * inv_h, (p.y - base_y) * inv_h);
                        let mut g_best = best_d2 * inv_h * inv_h;
                        for s in segs {
                            let a = Vec2::new(s[0], s[1]);
                            let b = Vec2::new(s[2], s[3]);
                            let d2 = dist_sq_segment_2d(q, a, b);
                            if d2 < g_best {
                                g_best = d2;
                            }
                        }
                        let world_d2 = g_best * h * h;
                        if world_d2 < best_d2 {
                            best_d2 = world_d2;
                        }
                    }
                }
                ci += 1;
            }
        }

        line_idx += 1;
        match nl_off {
            Some(off) => bytes_processed += off + 1,
            None => break,
        }
    }

    if best_d2.is_finite() {
        blaze_math::sqrt(best_d2) - half_stroke
    } else {
        1e6
    }
}

// ==========================================================================================
// 2. BUILT-IN `#![no_std]` SVG 2D PATH PARSER & SDF EVALUATOR
// ==========================================================================================

/// One flattened 2D line segment produced by [`parse_svg_path`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SvgSegment {
    pub a: Vec2,
    pub b: Vec2,
    pub subpath: u16,
    pub closed: bool,
}

/// Flattened 2D SVG path geometry and its bounding box in SVG user units.
#[derive(Debug)]
pub struct SvgPath2D<'a> {
    pub segments: Vec<SvgSegment, Alloc<'a>>,
    pub min: Vec2,
    pub max: Vec2,
}

impl SvgPath2D<'_> {
    /// Center of the SVG path's 2D bounding box in SVG user units.
    #[inline]
    pub fn center(&self) -> Vec2 {
        if self.segments.is_empty() {
            Vec2::new(0.0, 0.0)
        } else {
            (self.min + self.max) * 0.5
        }
    }

    /// Size `(width, height)` of the SVG path's 2D bounding box in SVG user units.
    #[inline]
    pub fn extent(&self) -> Vec2 {
        if self.segments.is_empty() {
            Vec2::new(0.0, 0.0)
        } else {
            self.max - self.min
        }
    }
}

/// Skip ASCII whitespace and at most one comma separator.
#[inline]
fn skip_svg_sep(bytes: &[u8], pos: &mut usize) {
    let mut saw_comma = false;
    while *pos < bytes.len() {
        match bytes[*pos] {
            b' ' | b'\t' | b'\n' | b'\r' => *pos += 1,
            b',' if !saw_comma => {
                saw_comma = true;
                *pos += 1;
            }
            _ => break,
        }
    }
}

/// Deterministic `#![no_std]` SVG floating-point number parser.
fn parse_svg_number(bytes: &[u8], pos: &mut usize) -> Option<f64> {
    skip_svg_sep(bytes, pos);
    if *pos >= bytes.len() {
        return None;
    }
    let start = *pos;
    let mut sign = 1.0f64;
    match bytes[*pos] {
        b'+' => *pos += 1,
        b'-' => {
            sign = -1.0;
            *pos += 1;
        }
        _ => {}
    }

    let mut mantissa = 0.0f64;
    let mut digits = 0u32;
    while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
        mantissa = mantissa * 10.0 + f64::from(bytes[*pos] - b'0');
        digits += 1;
        *pos += 1;
    }

    let mut frac_exp = 0i32;
    if *pos < bytes.len() && bytes[*pos] == b'.' {
        *pos += 1;
        while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
            if digits < 18 {
                mantissa = mantissa * 10.0 + f64::from(bytes[*pos] - b'0');
                frac_exp -= 1;
            }
            digits += 1;
            *pos += 1;
        }
    }

    if digits == 0 {
        *pos = start;
        return None;
    }

    let mut exp10 = frac_exp;
    if *pos < bytes.len() && matches!(bytes[*pos], b'e' | b'E') {
        let exp_mark = *pos;
        *pos += 1;
        let mut e_sign = 1i32;
        if *pos < bytes.len() {
            match bytes[*pos] {
                b'+' => *pos += 1,
                b'-' => {
                    e_sign = -1;
                    *pos += 1;
                }
                _ => {}
            }
        }
        let mut e_val = 0i32;
        let mut e_digits = 0u32;
        while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
            if e_val < 10_000 {
                e_val = e_val * 10 + i32::from(bytes[*pos] - b'0');
            }
            e_digits += 1;
            *pos += 1;
        }
        if e_digits == 0 {
            *pos = exp_mark;
        } else {
            exp10 = exp10.saturating_add(e_sign.saturating_mul(e_val));
        }
    }

    let val = if exp10 == 0 {
        mantissa
    } else {
        mantissa * blaze_math::pow(10.0, f64::from(exp10))
    };
    let signed = sign * val;
    if signed.is_finite() { Some(signed) } else { None }
}

/// Stream every flattened segment of SVG path `d` to `emit(a, b, subpath, closed)` and invoke
/// `on_close(subpath)` on every `Z`/`z`, bounded by [`MAX_SVG_SEGMENTS`] (zero allocation, used by
/// both [`parse_svg_path`] and standalone [`evaluate_decal`]).
fn walk_svg_path<F, C>(d: &str, mut emit: F, mut on_close: C) -> Result<usize, ModelError>
where
    F: FnMut(Vec2, Vec2, u16, bool) -> Result<(), ModelError>,
    C: FnMut(u16),
{
    let bytes = d.as_bytes();
    let mut pos = 0usize;
    let mut seg_count = 0usize;
    let mut cur = Vec2::new(0.0, 0.0);
    let mut start = Vec2::new(0.0, 0.0);
    let mut prev_cubic_ctrl: Option<Vec2> = None;
    let mut prev_quad_ctrl: Option<Vec2> = None;
    let mut subpath = 0u16;
    let mut has_subpath = false;
    let mut cmd = 0u8;

    let mut push_seg = |a: Vec2, b: Vec2, sp: u16, closed: bool| -> Result<(), ModelError> {
        if seg_count >= MAX_SVG_SEGMENTS {
            return Err(ModelError::Argument {
                function: "parse_svg_path",
                arg: "d",
                accepted: "at most 2048 flattened path segments",
            });
        }
        seg_count += 1;
        emit(a, b, sp, closed)
    };

    let mut steps = 0usize;
    let max_steps = bytes.len().saturating_add(16);
    while pos < bytes.len() && steps < max_steps {
        steps += 1;
        skip_svg_sep(bytes, &mut pos);
        if pos >= bytes.len() {
            break;
        }
        let b = bytes[pos];
        if b.is_ascii_alphabetic() {
            cmd = b;
            pos += 1;
        } else if cmd == 0 {
            return Err(ModelError::Argument {
                function: "parse_svg_path",
                arg: "d",
                accepted: "valid SVG path data starting with a command letter",
            });
        }

        let rel = cmd.is_ascii_lowercase();
        match cmd {
            b'M' | b'm' => {
                let Some(x) = parse_svg_number(bytes, &mut pos) else {
                    return Err(ModelError::Argument {
                        function: "parse_svg_path",
                        arg: "d",
                        accepted: "x coordinate after M/m",
                    });
                };
                let Some(y) = parse_svg_number(bytes, &mut pos) else {
                    return Err(ModelError::Argument {
                        function: "parse_svg_path",
                        arg: "d",
                        accepted: "y coordinate after M/m",
                    });
                };
                if has_subpath {
                    subpath = subpath.saturating_add(1);
                }
                has_subpath = true;
                cur = if rel { Vec2::new(cur.x + x, cur.y + y) } else { Vec2::new(x, y) };
                start = cur;
                prev_cubic_ctrl = None;
                prev_quad_ctrl = None;
                // Per SVG spec, subsequent coordinate pairs after M/m are implicit L/l.
                cmd = if rel { b'l' } else { b'L' };
            }
            b'L' | b'l' => {
                let Some(x) = parse_svg_number(bytes, &mut pos) else {
                    return Err(ModelError::Argument {
                        function: "parse_svg_path",
                        arg: "d",
                        accepted: "x coordinate after L/l",
                    });
                };
                let Some(y) = parse_svg_number(bytes, &mut pos) else {
                    return Err(ModelError::Argument {
                        function: "parse_svg_path",
                        arg: "d",
                        accepted: "y coordinate after L/l",
                    });
                };
                let next = if rel { Vec2::new(cur.x + x, cur.y + y) } else { Vec2::new(x, y) };
                push_seg(cur, next, subpath, false)?;
                cur = next;
                prev_cubic_ctrl = None;
                prev_quad_ctrl = None;
            }
            b'H' | b'h' => {
                let Some(x) = parse_svg_number(bytes, &mut pos) else {
                    return Err(ModelError::Argument {
                        function: "parse_svg_path",
                        arg: "d",
                        accepted: "x coordinate after H/h",
                    });
                };
                let next = if rel { Vec2::new(cur.x + x, cur.y) } else { Vec2::new(x, cur.y) };
                push_seg(cur, next, subpath, false)?;
                cur = next;
                prev_cubic_ctrl = None;
                prev_quad_ctrl = None;
            }
            b'V' | b'v' => {
                let Some(y) = parse_svg_number(bytes, &mut pos) else {
                    return Err(ModelError::Argument {
                        function: "parse_svg_path",
                        arg: "d",
                        accepted: "y coordinate after V/v",
                    });
                };
                let next = if rel { Vec2::new(cur.x, cur.y + y) } else { Vec2::new(cur.x, y) };
                push_seg(cur, next, subpath, false)?;
                cur = next;
                prev_cubic_ctrl = None;
                prev_quad_ctrl = None;
            }
            b'C' | b'c' => {
                let (Some(x1), Some(y1), Some(x2), Some(y2), Some(x), Some(y)) = (
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                ) else {
                    return Err(ModelError::Argument {
                        function: "parse_svg_path",
                        arg: "d",
                        accepted: "six coordinates after C/c",
                    });
                };
                let p0 = cur;
                let p1 = if rel { p0 + Vec2::new(x1, y1) } else { Vec2::new(x1, y1) };
                let p2 = if rel { p0 + Vec2::new(x2, y2) } else { Vec2::new(x2, y2) };
                let p3 = if rel { p0 + Vec2::new(x, y) } else { Vec2::new(x, y) };
                let mut prev = p0;
                for i in 1..=CUBIC_CHORDS {
                    let t = (i as f64) / (CUBIC_CHORDS as f64);
                    let mt = 1.0 - t;
                    let pt = p0 * (mt * mt * mt)
                        + p1 * (3.0 * mt * mt * t)
                        + p2 * (3.0 * mt * t * t)
                        + p3 * (t * t * t);
                    push_seg(prev, pt, subpath, false)?;
                    prev = pt;
                }
                cur = p3;
                prev_cubic_ctrl = Some(p2);
                prev_quad_ctrl = None;
            }
            b'S' | b's' => {
                let (Some(x2), Some(y2), Some(x), Some(y)) = (
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                ) else {
                    return Err(ModelError::Argument {
                        function: "parse_svg_path",
                        arg: "d",
                        accepted: "four coordinates after S/s",
                    });
                };
                let p0 = cur;
                let p1 = match prev_cubic_ctrl {
                    Some(c) => p0 * 2.0 - c,
                    None => p0,
                };
                let p2 = if rel { p0 + Vec2::new(x2, y2) } else { Vec2::new(x2, y2) };
                let p3 = if rel { p0 + Vec2::new(x, y) } else { Vec2::new(x, y) };
                let mut prev = p0;
                for i in 1..=CUBIC_CHORDS {
                    let t = (i as f64) / (CUBIC_CHORDS as f64);
                    let mt = 1.0 - t;
                    let pt = p0 * (mt * mt * mt)
                        + p1 * (3.0 * mt * mt * t)
                        + p2 * (3.0 * mt * t * t)
                        + p3 * (t * t * t);
                    push_seg(prev, pt, subpath, false)?;
                    prev = pt;
                }
                cur = p3;
                prev_cubic_ctrl = Some(p2);
                prev_quad_ctrl = None;
            }
            b'Q' | b'q' => {
                let (Some(x1), Some(y1), Some(x), Some(y)) = (
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                    parse_svg_number(bytes, &mut pos),
                ) else {
                    return Err(ModelError::Argument {
                        function: "parse_svg_path",
                        arg: "d",
                        accepted: "four coordinates after Q/q",
                    });
                };
                let p0 = cur;
                let p1 = if rel { p0 + Vec2::new(x1, y1) } else { Vec2::new(x1, y1) };
                let p2 = if rel { p0 + Vec2::new(x, y) } else { Vec2::new(x, y) };
                let mut prev = p0;
                for i in 1..=QUAD_CHORDS {
                    let t = (i as f64) / (QUAD_CHORDS as f64);
                    let mt = 1.0 - t;
                    let pt = p0 * (mt * mt) + p1 * (2.0 * mt * t) + p2 * (t * t);
                    push_seg(prev, pt, subpath, false)?;
                    prev = pt;
                }
                cur = p2;
                prev_quad_ctrl = Some(p1);
                prev_cubic_ctrl = None;
            }
            b'T' | b't' => {
                let (Some(x), Some(y)) = (parse_svg_number(bytes, &mut pos), parse_svg_number(bytes, &mut pos)) else {
                    return Err(ModelError::Argument {
                        function: "parse_svg_path",
                        arg: "d",
                        accepted: "two coordinates after T/t",
                    });
                };
                let p0 = cur;
                let p1 = match prev_quad_ctrl {
                    Some(c) => p0 * 2.0 - c,
                    None => p0,
                };
                let p2 = if rel { p0 + Vec2::new(x, y) } else { Vec2::new(x, y) };
                let mut prev = p0;
                for i in 1..=QUAD_CHORDS {
                    let t = (i as f64) / (QUAD_CHORDS as f64);
                    let mt = 1.0 - t;
                    let pt = p0 * (mt * mt) + p1 * (2.0 * mt * t) + p2 * (t * t);
                    push_seg(prev, pt, subpath, false)?;
                    prev = pt;
                }
                cur = p2;
                prev_quad_ctrl = Some(p1);
                prev_cubic_ctrl = None;
            }
            b'Z' | b'z' => {
                let d_close = cur - start;
                if d_close.dot(d_close) > 1e-24 {
                    push_seg(cur, start, subpath, true)?;
                }
                on_close(subpath);
                cur = start;
                prev_cubic_ctrl = None;
                prev_quad_ctrl = None;
                cmd = 0;
            }
            _ => {
                return Err(ModelError::Argument {
                    function: "parse_svg_path",
                    arg: "d",
                    accepted: "SVG path commands M/m, L/l, H/h, V/v, C/c, S/s, Q/q, T/t, Z/z",
                });
            }
        }
    }

    Ok(seg_count)
}

/// Parse an SVG 2D path string `d="..."` into flattened [`SvgSegment`] chords in `alloc`
/// (bounded by [`MAX_SVG_SEGMENTS`] = 2048, rule 18).
pub fn parse_svg_path<'a>(d: &str, alloc: Alloc<'a>) -> Result<SvgPath2D<'a>, ModelError> {
    let mut segments: Vec<SvgSegment, Alloc<'a>> = Vec::new_in(alloc);
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    let mut closed_subpaths = [0u64; 4]; // bitset for first 256 subpaths; overflow handled inline
    walk_svg_path(
        d,
        |a, b, subpath, closed| {
            min_x = min_x.min(a.x).min(b.x);
            min_y = min_y.min(a.y).min(b.y);
            max_x = max_x.max(a.x).max(b.x);
            max_y = max_y.max(a.y).max(b.y);
            segments
                .try_push(SvgSegment { a, b, subpath, closed })
                .map_err(|_| ModelError::Alloc { what: "parsed SVG path segments" })
        },
        |subpath| {
            let idx = usize::from(subpath);
            if idx < 256 {
                closed_subpaths[idx / 64] |= 1u64 << (idx % 64);
            }
        },
    )?;

    for seg in segments.iter_mut() {
        let idx = usize::from(seg.subpath);
        if idx < 256 && (closed_subpaths[idx / 64] & (1u64 << (idx % 64))) != 0 {
            seg.closed = true;
        }
    }

    if segments.is_empty() {
        min_x = 0.0;
        min_y = 0.0;
        max_x = 0.0;
        max_y = 0.0;
    }

    Ok(SvgPath2D {
        segments,
        min: Vec2::new(min_x, min_y),
        max: Vec2::new(max_x, max_y),
    })
}

/// Evaluate exact 2D signed distance (metres) to a pre-parsed [`SvgPath2D`], centered at local
/// `(0, 0)` with `scale_m` metres per SVG unit (Y flipped so upright SVG icons point along `+v`).
pub fn svg_path_sd(path: &SvgPath2D<'_>, p_m: Vec2, scale_m: f64, filled: bool, stroke_m: f64) -> f64 {
    if path.segments.is_empty() || !(scale_m > 0.0 && scale_m.is_finite()) {
        return 1e6;
    }
    let center = path.center();
    let inv_s = 1.0 / scale_m;
    // Map CAD (u, v) -> SVG (x, y) centered, with SVG +Y downward matching -v.
    let q = Vec2::new(center.x + p_m.x * inv_s, center.y - p_m.y * inv_s);

    let mut min_d2 = f64::INFINITY;
    let mut inside = false;
    for seg in path.segments.iter() {
        let d2 = dist_sq_segment_2d(q, seg.a, seg.b);
        if d2 < min_d2 {
            min_d2 = d2;
        }
        if filled && (seg.a.y > q.y) != (seg.b.y > q.y) {
            let x_cross = seg.a.x + (q.y - seg.a.y) * (seg.b.x - seg.a.x) / (seg.b.y - seg.a.y);
            if q.x < x_cross {
                inside = !inside;
            }
        }
    }
    let dist_m = blaze_math::sqrt(min_d2) * scale_m;
    if filled {
        if inside { -dist_m } else { dist_m }
    } else {
        dist_m - (0.5 * stroke_m).max(1e-6)
    }
}

/// Zero-allocation streaming 2D signed distance for an unparsed SVG path `d` (used by standalone
/// [`evaluate_decal`]).
fn svg_str_sd(d: &str, p_m: Vec2, scale_m: f64, filled: bool, stroke_m: f64) -> f64 {
    if !(scale_m > 0.0 && scale_m.is_finite()) {
        return 1e6;
    }
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut count = 0usize;
    if walk_svg_path(
        d,
        |a, b, _, _| {
            min_x = min_x.min(a.x).min(b.x);
            min_y = min_y.min(a.y).min(b.y);
            max_x = max_x.max(a.x).max(b.x);
            max_y = max_y.max(a.y).max(b.y);
            count += 1;
            Ok(())
        },
        |_| {},
    )
    .is_err()
        || count == 0
    {
        return 1e6;
    }

    let center = Vec2::new(0.5 * (min_x + max_x), 0.5 * (min_y + max_y));
    let inv_s = 1.0 / scale_m;
    let q = Vec2::new(center.x + p_m.x * inv_s, center.y - p_m.y * inv_s);

    let mut min_d2 = f64::INFINITY;
    let mut inside = false;
    let _ = walk_svg_path(
        d,
        |a, b, _, _| {
            let d2 = dist_sq_segment_2d(q, a, b);
            if d2 < min_d2 {
                min_d2 = d2;
            }
            if filled && (a.y > q.y) != (b.y > q.y) {
                let x_cross = a.x + (q.y - a.y) * (b.x - a.x) / (b.y - a.y);
                if q.x < x_cross {
                    inside = !inside;
                }
            }
            Ok(())
        },
        |_| {},
    );

    let dist_m = blaze_math::sqrt(min_d2) * scale_m;
    if filled {
        if inside { -dist_m } else { dist_m }
    } else {
        dist_m - (0.5 * stroke_m).max(1e-6)
    }
}

/// Measure `(half_u_m, half_v_m)` bounding half-extents of a [`DecalShape`] in metres.
fn shape_half_extents_m(shape: &DecalShape<'_>, parsed_svg: Option<&SvgPath2D<'_>>) -> (f64, f64) {
    match *shape {
        DecalShape::Text { text, height, stroke_width, tracking } => {
            let (w, h) = text_bounds_m(text, height.as_m().max(1e-6), tracking);
            let pad = stroke_width.as_m().max(0.0);
            (0.5 * w + pad, 0.5 * h + pad)
        }
        DecalShape::SvgPath { d, scale, filled, stroke_width } => {
            let s = scale.as_m().max(0.0);
            let pad = if filled { 0.0 } else { stroke_width.as_m().max(0.0) };
            if let Some(p) = parsed_svg {
                let ext = p.extent();
                (0.5 * ext.x * s + pad, 0.5 * ext.y * s + pad)
            } else {
                let mut min_x = f64::INFINITY;
                let mut min_y = f64::INFINITY;
                let mut max_x = f64::NEG_INFINITY;
                let mut max_y = f64::NEG_INFINITY;
                let mut any = false;
                let _ = walk_svg_path(
                    d,
                    |a, b, _, _| {
                        min_x = min_x.min(a.x).min(b.x);
                        min_y = min_y.min(a.y).min(b.y);
                        max_x = max_x.max(a.x).max(b.x);
                        max_y = max_y.max(a.y).max(b.y);
                        any = true;
                        Ok(())
                    },
                    |_| {},
                );
                if any {
                    (0.5 * (max_x - min_x) * s + pad, 0.5 * (max_y - min_y) * s + pad)
                } else {
                    (pad, pad)
                }
            }
        }
        DecalShape::Stripe { width, length, angle } => {
            let hl = 0.5 * length.as_m().abs();
            let hw = 0.5 * width.as_m().abs();
            let (s, c) = blaze_math::sin_cos(angle.as_radians());
            (hl * c.abs() + hw * s.abs(), hl * s.abs() + hw * c.abs())
        }
        DecalShape::Chevron { width, height, thickness } => {
            let pad = 0.5 * thickness.as_m().abs();
            (0.5 * width.as_m().abs() + pad, 0.5 * height.as_m().abs() + pad)
        }
        DecalShape::Ring { outer_radius, inner_radius } => {
            let r = outer_radius.as_m().abs().max(inner_radius.as_m().abs());
            (r, r)
        }
        DecalShape::Rect { width, height, .. } => (0.5 * width.as_m().abs(), 0.5 * height.as_m().abs()),
        DecalShape::Circle { radius } => {
            let r = radius.as_m().abs();
            (r, r)
        }
    }
}

/// Evaluate exact 2D signed distance (metres) of `shape` at local frame coordinates `uv`.
fn evaluate_shape_sd_2d(shape: &DecalShape<'_>, uv: Vec2, parsed_svg: Option<&SvgPath2D<'_>>) -> f64 {
    match *shape {
        DecalShape::Text { text, height, stroke_width, tracking } => {
            let h = height.as_m().max(1e-6);
            let sw = if stroke_width > Length::ZERO { stroke_width.as_m() } else { h * 0.12 };
            sd_text_2d(text, h, sw, tracking, uv)
        }
        DecalShape::SvgPath { d, scale, filled, stroke_width } => {
            let s = scale.as_m();
            let sw = stroke_width.as_m().max(1e-6);
            if let Some(parsed) = parsed_svg {
                svg_path_sd(parsed, uv, s, filled, sw)
            } else {
                svg_str_sd(d, uv, s, filled, sw)
            }
        }
        DecalShape::Stripe { width, length, angle } => {
            let (s, c) = blaze_math::sin_cos(angle.as_radians());
            let rot = Vec2::new(uv.x * c + uv.y * s, -uv.x * s + uv.y * c);
            sd_rounded_box_2d(rot, 0.5 * length.as_m().abs(), 0.5 * width.as_m().abs(), 0.0)
        }
        DecalShape::Chevron { width, height, thickness } => {
            let hw = 0.5 * width.as_m().abs();
            let hh = 0.5 * height.as_m().abs();
            let left = Vec2::new(-hw, -hh);
            let apex = Vec2::new(0.0, hh);
            let right = Vec2::new(hw, -hh);
            let d = dist_segment_2d(uv, left, apex).min(dist_segment_2d(uv, apex, right));
            d - (0.5 * thickness.as_m().abs()).max(1e-6)
        }
        DecalShape::Ring { outer_radius, inner_radius } => {
            let r_out = outer_radius.as_m().abs().max(inner_radius.as_m().abs());
            let r_in = outer_radius.as_m().abs().min(inner_radius.as_m().abs());
            let r = uv.length();
            (r - r_out).max(r_in - r)
        }
        DecalShape::Rect { width, height, corner_radius } => {
            sd_rounded_box_2d(uv, 0.5 * width.as_m().abs(), 0.5 * height.as_m().abs(), corner_radius.as_m().max(0.0))
        }
        DecalShape::Circle { radius } => uv.length() - radius.as_m().abs(),
    }
}

// ==========================================================================================
// 3. SURFACE-CONFORMING DECALS (Priority 3)
// ==========================================================================================

/// 2D graphic primitive projected onto an [`AttachmentFrame`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DecalShape<'s> {
    /// Crisp geometric technical / stencil vector lettering centered at `(0, 0)`.
    Text { text: &'s str, height: Length, stroke_width: Length, tracking: f64 },
    /// Arbitrary SVG 2D path `d="..."` centered at `(0, 0)`; `scale` maps 1 SVG unit to metres.
    SvgPath { d: &'s str, scale: Length, filled: bool, stroke_width: Length },
    /// Oriented rectangular stripe centered at `(0, 0)`.
    Stripe { width: Length, length: Length, angle: Angle },
    /// Industrial V-chevron pointing along `+tangent_v`, centered at `(0, 0)`.
    Chevron { width: Length, height: Length, thickness: Length },
    /// Annular ring centered at `(0, 0)`.
    Ring { outer_radius: Length, inner_radius: Length },
    /// Rounded rectangle centered at `(0, 0)`.
    Rect { width: Length, height: Length, corner_radius: Length },
    /// Filled disc centered at `(0, 0)`.
    Circle { radius: Length },
}

/// How 3D surface points are mapped into the decal's local 2D `(u, v)` coordinate plane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecalProjection {
    /// Planar orthographic projection via `frame.project_local(p)`.
    Orthographic,
    /// True surface arc-length `(u_arc, v_arc)` via curvature-compensated geodesic mapping and
    /// bounded 4-step surface-marching chord integration (wraps around cylinders, curved housings,
    /// and lofted cells without stretching).
    ConformalArcLength,
}

/// PBR appearance and optional tangent-space relief of a [`Decal`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DecalMaterial {
    /// Linear RGBA (`alpha` controls blend opacity over the underlying material).
    pub base_colour: [f64; 4],
    pub roughness: f64,
    pub metallic: f64,
    /// Optional positive emboss or negative deboss/engrave height baked into the tangent-space
    /// normal map via exact 2D SDF finite-difference gradient.
    pub relief: Option<Length>,
}

/// Seam / boundary clipping policy keeping a decal strictly inside its intended panel or region.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SeamClip {
    /// No extra seam clip beyond `DecalOpts::clip_role`, `clip_box`, `max_angle`, and `max_depth`.
    None,
    /// Clip strictly inside part-local `Aabb3`.
    Box(Aabb3),
    /// Clip strictly to triangles matching this [`MaterialRole`].
    Role(MaterialRole),
    /// Clip strictly to points within `margin` of a named CAD node (evaluated when compiled).
    FieldMargin { solid_id: NodeId, margin: Length },
    /// Clip wherever surface normal deviates past `Angle` from `frame.normal`.
    MaxNormalDeviation(Angle),
}

/// Options for [`Decal::new`] (rule 83: one options-struct constructor).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct DecalOpts<'s> {
    pub name: &'s str,
    pub frame: AttachmentFrame,
    pub shape: DecalShape<'s>,
    pub material: DecalMaterial,
    pub projection: DecalProjection = DecalProjection::ConformalArcLength,
    pub max_angle: Angle = Angle::microarcsec(75 * UAS_PER_DEGREE_I64),
    pub max_depth: Length = Length::nm(30_000_000),
    pub clip_role: Option<MaterialRole> = None,
    pub clip_box: Option<Aabb3> = None,
    pub clip_margin: Length = Length::ZERO,
    pub seam_clip: SeamClip = SeamClip::None,
}

/// A surface-conforming vector or stencil decal anchored to a CAD [`AttachmentFrame`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Decal<'s> {
    pub name: Name,
    pub frame: AttachmentFrame,
    pub shape: DecalShape<'s>,
    pub material: DecalMaterial,
    pub projection: DecalProjection,
    pub max_angle: Angle,
    pub max_depth: Length,
    pub clip_role: Option<MaterialRole>,
    pub clip_box: Option<Aabb3>,
    pub clip_margin: Length,
    pub seam_clip: SeamClip,
}

impl<'s> Decal<'s> {
    /// Construct a [`Decal`] from [`DecalOpts`].
    pub fn new(o: DecalOpts<'s>) -> Decal<'s> {
        Decal {
            name: Name::new(o.name),
            frame: o.frame,
            shape: o.shape,
            material: o.material,
            projection: o.projection,
            max_angle: o.max_angle,
            max_depth: o.max_depth,
            clip_role: o.clip_role,
            clip_box: o.clip_box,
            clip_margin: o.clip_margin,
            seam_clip: o.seam_clip,
        }
    }
}

/// Evaluated decal sample at one 3D surface point.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DecalSample {
    /// Anti-aliased coverage in `0.0..=1.0`.
    pub coverage: f64,
    /// Exact 2D signed distance in metres (`<= 0` inside, `> 0` outside).
    pub sd_m: f64,
    /// Normalized 2D SDF gradient component along `tangent_u` (for relief normal baking).
    pub grad_u: f64,
    /// Normalized 2D SDF gradient component along `tangent_v` (for relief normal baking).
    pub grad_v: f64,
    /// Local surface tangent direction corresponding to `+u` at the sample point.
    pub tangent_u: Vec3,
    /// Local surface tangent direction corresponding to `+v` at the sample point.
    pub tangent_v: Vec3,
}

/// Map 3D point `p` with unit normal `n` onto `(u_m, v_m, normal_dist_m, tangent_u, tangent_v)`
/// using either orthographic or curvature-compensated / geodesic conformal arc-length projection.
#[allow(clippy::too_many_arguments)]
fn project_onto_frame(
    frame: &AttachmentFrame,
    projection: DecalProjection,
    p: Vec3,
    n: Vec3,
    max_depth_m: f64,
    tape: Option<&Tape<'_>>,
    sc: Option<&mut Scratch<'_>>,
) -> (f64, f64, f64, Vec3, Vec3) {
    let local = frame.project_local(p);
    let (u_lin, v_lin, n_lin) = (local.x, local.y, local.z);

    // Project frame tangents onto local surface tangent plane at `n`.
    let local_basis = |surf_n: Vec3, ref_u: Vec3, ref_v: Vec3| -> (Vec3, Vec3) {
        let tu = (ref_u - surf_n * ref_u.dot(surf_n)).normalized().unwrap_or_else(|| canonical_tangent(surf_n));
        let mut tv = (ref_v - surf_n * ref_v.dot(surf_n) - tu * ref_v.dot(tu))
            .normalized()
            .unwrap_or_else(|| surf_n.cross(tu).normalized().unwrap_or(ref_v));
        if tv.dot(ref_v) < 0.0 {
            tv = -tv;
        }
        (tu, tv)
    };

    match projection {
        DecalProjection::Orthographic => {
            let (tu, tv) = local_basis(n, frame.tangent_u, frame.tangent_v);
            (u_lin, v_lin, n_lin.abs(), tu, tv)
        }
        DecalProjection::ConformalArcLength => {
            // Analytic curvature-compensated cylindrical/principal-curvature arc length.
            let arc_axis = |coord: f64, k: f64, n_off: f64| -> (f64, f64) {
                if k.abs() <= 1e-6 {
                    return (coord, 0.0);
                }
                let r = 1.0 / k.abs();
                let nr = if k > 0.0 { r + n_off } else { r - n_off };
                if nr <= 0.05 * r {
                    return (coord, 0.0);
                }
                let theta = blaze_math::atan2(coord, nr);
                let arc = r * theta;
                let sag = r * (1.0 - blaze_math::cos(theta));
                let signed_sag = if k > 0.0 { sag } else { -sag };
                (arc, signed_sag)
            };

            let (u_curv, sag_u) = arc_axis(u_lin, frame.curvature_u, n_lin);
            let (v_curv, sag_v) = arc_axis(v_lin, frame.curvature_v, n_lin);
            let curv_n_dist = (n_lin + sag_u + sag_v).abs();

            // When a CAD tape + scratch are available and we are away from the frame origin,
            // run bounded 4-step geodesic chord marching along the actual CAD surface so
            // non-constant curvature (lofts, superellipsoids, curved housings) has zero stretch.
            if let (Some(t), Some(s)) = (tape, sc) {
                let chord_len = (p - frame.origin).length();
                if chord_len > 1e-5 {
                    let mut q = frame.origin;
                    let mut eu = frame.tangent_u;
                    let mut ev = frame.tangent_v;
                    let mut en = frame.normal;
                    let mut u_arc = 0.0f64;
                    let mut v_arc = 0.0f64;
                    let mut valid = true;

                    for step_i in 0..GEODESIC_STEPS {
                        let rem = p - q;
                        let frac = 1.0 / ((GEODESIC_STEPS - step_i) as f64);
                        let du = rem.dot(eu) * frac;
                        let dv = rem.dot(ev) * frac;
                        let q_try = q + eu * du + ev * dv;
                        let (f_val, g_val) = eval_grad(t, q_try, s);
                        if !f_val.is_finite() {
                            valid = false;
                            break;
                        }
                        let n_new = g_val.normalized().unwrap_or(en);
                        // Clamp Newton projection so a sharp feature cannot throw the march off.
                        let proj_step = f_val.clamp(-max_depth_m, max_depth_m);
                        let q_next = q_try - n_new * proj_step;
                        let delta = q_next - q;
                        let d_u = delta.dot(eu);
                        let d_v = delta.dot(ev);
                        let tan_len = blaze_math::sqrt(d_u * d_u + d_v * d_v);
                        let step_len = delta.length();
                        if tan_len > 1e-12 && step_len.is_finite() {
                            let arc_scale = (step_len / tan_len).min(2.0);
                            u_arc += d_u * arc_scale;
                            v_arc += d_v * arc_scale;
                        } else {
                            u_arc += d_u;
                            v_arc += d_v;
                        }

                        // Parallel-transport (eu, ev) onto the new surface tangent plane.
                        let (next_u, next_v) = local_basis(n_new, eu, ev);
                        q = q_next;
                        eu = next_u;
                        ev = next_v;
                        en = n_new;
                    }

                    if valid {
                        let res = p - q;
                        let n_dist = res.dot(en).abs();
                        u_arc += res.dot(eu);
                        v_arc += res.dot(ev);
                        let (tu, tv) = local_basis(n, eu, ev);
                        return (u_arc, v_arc, n_dist.min(curv_n_dist), tu, tv);
                    }
                }
            }

            let (tu, tv) = local_basis(n, frame.tangent_u, frame.tangent_v);
            (u_curv, v_curv, curv_n_dist, tu, tv)
        }
    }
}

/// Core decal evaluator parameterized by anti-aliasing filter footprint `aa_width_m` and optional
/// pre-parsed [`SvgPath2D`].
#[allow(clippy::too_many_arguments)]
fn evaluate_decal_inner(
    decal: &Decal<'_>,
    p: Vec3,
    n: Vec3,
    role: MaterialRole,
    aa_width_m: f64,
    parsed_svg: Option<&SvgPath2D<'_>>,
    tape: Option<&Tape<'_>>,
    sc: Option<&mut Scratch<'_>>,
) -> Option<DecalSample> {
    // 1. Material role seam clip.
    if let Some(req_role) = decal.clip_role
        && role != req_role
    {
        return None;
    }
    if let SeamClip::Role(req_role) = decal.seam_clip
        && role != req_role
    {
        return None;
    }

    // 2. Box seam clip (shrunk inward by `clip_margin`).
    let margin_m = decal.clip_margin.as_m().max(0.0);
    let check_box = |bx: &Aabb3| -> bool {
        if bx.is_empty() {
            return false;
        }
        p.x >= bx.lo.x + margin_m
            && p.x <= bx.hi.x - margin_m
            && p.y >= bx.lo.y + margin_m
            && p.y <= bx.hi.y - margin_m
            && p.z >= bx.lo.z + margin_m
            && p.z <= bx.hi.z - margin_m
    };
    if let Some(bx) = decal.clip_box
        && !check_box(&bx)
    {
        return None;
    }
    if let SeamClip::Box(bx) = decal.seam_clip
        && !check_box(&bx)
    {
        return None;
    }

    // 3. Normal-deviation clip (prevents back-face bleed and sharp seam wrapping).
    let n_unit = n.normalized().unwrap_or(decal.frame.normal);
    let mut max_ang_rad = decal.max_angle.as_radians().clamp(0.0, core::f64::consts::PI);
    if let SeamClip::MaxNormalDeviation(ang) = decal.seam_clip {
        max_ang_rad = max_ang_rad.min(ang.as_radians().clamp(0.0, core::f64::consts::PI));
    }
    if n_unit.dot(decal.frame.normal) < blaze_math::cos(max_ang_rad) {
        return None;
    }

    // 4. Project onto attachment frame (orthographic or conformal arc-length) & depth check.
    let max_depth_m = decal.max_depth.as_m().max(1e-6);
    let (u, v, n_dist, tangent_u, tangent_v) =
        project_onto_frame(&decal.frame, decal.projection, p, n_unit, max_depth_m, tape, sc);
    if n_dist > max_depth_m {
        return None;
    }

    // Fast 2D AABB reject before evaluating full glyph / SVG SDF.
    let (hu, hv) = shape_half_extents_m(&decal.shape, parsed_svg);
    let relief_pad = decal.material.relief.map_or(0.0, |r| r.as_m().abs() * 2.0);
    let bound_pad = aa_width_m.max(1e-5) * 2.5 + relief_pad;
    if u.abs() > hu + bound_pad || v.abs() > hv + bound_pad {
        return None;
    }

    // 5. Evaluate exact 2D SDF + anti-aliased coverage + finite-difference relief gradient.
    let uv = Vec2::new(u, v);
    let sd_m = evaluate_shape_sd_2d(&decal.shape, uv, parsed_svg);
    let half_aa = (0.5 * aa_width_m).max(1e-6);
    let relief_band = decal.material.relief.map_or(0.0, |r| (r.as_m().abs() * 0.75).max(aa_width_m * 1.5));
    if sd_m > half_aa.max(relief_band) + 1e-6 {
        return None;
    }

    let coverage = smoothstep(half_aa, -half_aa, sd_m);
    let (grad_u, grad_v) = if decal.material.relief.is_some() && sd_m.abs() <= relief_band + half_aa {
        let eps = (0.25 * aa_width_m).clamp(1e-6, 2.5e-4);
        let du = evaluate_shape_sd_2d(&decal.shape, Vec2::new(u + eps, v), parsed_svg)
            - evaluate_shape_sd_2d(&decal.shape, Vec2::new(u - eps, v), parsed_svg);
        let dv = evaluate_shape_sd_2d(&decal.shape, Vec2::new(u, v + eps), parsed_svg)
            - evaluate_shape_sd_2d(&decal.shape, Vec2::new(u, v - eps), parsed_svg);
        let len = blaze_math::sqrt(du * du + dv * dv);
        if len > 1e-18 && len.is_finite() { (du / len, dv / len) } else { (0.0, 0.0) }
    } else {
        (0.0, 0.0)
    };

    Some(DecalSample { coverage, sd_m, grad_u, grad_v, tangent_u, tangent_v })
}

/// Evaluate `decal` at 3D surface point `p` with unit normal `n` and triangle [`MaterialRole`],
/// returning anti-aliased coverage and 2D SDF gradient (or `None` when outside bounds / clipped).
pub fn evaluate_decal(
    decal: &Decal<'_>,
    p: Vec3,
    n: Vec3,
    role: MaterialRole,
    tape: Option<&Tape<'_>>,
    sc: Option<&mut Scratch<'_>>,
) -> Option<DecalSample> {
    let sw_m = decal_stroke_width(decal).as_m();
    let default_aa = (sw_m * 0.12).clamp(1e-5, 5e-4);
    evaluate_decal_inner(decal, p, n, role, default_aa, None, tape, sc)
}

/// Evaluate `decal` with an explicit pixel/texel anti-aliasing footprint `aa_width_m` (metres).
pub fn evaluate_decal_with_footprint(
    decal: &Decal<'_>,
    p: Vec3,
    n: Vec3,
    role: MaterialRole,
    aa_width_m: f64,
    tape: Option<&Tape<'_>>,
    sc: Option<&mut Scratch<'_>>,
) -> Option<DecalSample> {
    evaluate_decal_inner(decal, p, n, role, aa_width_m.max(1e-6), None, tape, sc)
}

/// Characteristic physical stroke width of `decal` (used for automatic mesh/texel sizing).
pub fn decal_stroke_width(decal: &Decal<'_>) -> Length {
    match decal.shape {
        DecalShape::Text { height, stroke_width, .. } => {
            if stroke_width > Length::ZERO {
                stroke_width
            } else {
                Length::m((height.as_m().abs() * 0.12).max(1e-4))
            }
        }
        DecalShape::SvgPath { scale, filled, stroke_width, .. } => {
            if !filled && stroke_width > Length::ZERO {
                stroke_width
            } else {
                Length::m(scale.as_m().abs().max(5e-4))
            }
        }
        DecalShape::Stripe { width, length, .. } => width.abs().min(length.abs()),
        DecalShape::Chevron { thickness, .. } => thickness.abs(),
        DecalShape::Ring { outer_radius, inner_radius } => (outer_radius.abs() - inner_radius.abs()).abs(),
        DecalShape::Rect { width, height, .. } => width.abs().min(height.abs()),
        DecalShape::Circle { radius } => radius.abs() * 2.0,
    }
}

/// Conservative world-space AABB enclosing a decal's footprint on its [`AttachmentFrame`].
fn decal_world_aabb(decal: &Decal<'_>, parsed_svg: Option<&SvgPath2D<'_>>) -> Aabb3 {
    let (hu, hv) = shape_half_extents_m(&decal.shape, parsed_svg);
    let pad = decal_stroke_width(decal).as_m().max(1e-3);
    let u_ext = hu + pad;
    let v_ext = hv + pad;
    // Account for curved surface sagitta when conformal wrapping is enabled.
    let curv_sag = match decal.projection {
        DecalProjection::Orthographic => 0.0,
        DecalProjection::ConformalArcLength => {
            0.5 * (decal.frame.curvature_u.abs() * u_ext * u_ext + decal.frame.curvature_v.abs() * v_ext * v_ext)
        }
    };
    let n_ext = decal.max_depth.as_m().abs().max(pad) + curv_sag;

    let mut lo = Vec3::splat(f64::INFINITY);
    let mut hi = Vec3::splat(f64::NEG_INFINITY);
    for su in [-1.0, 1.0] {
        for sv in [-1.0, 1.0] {
            for sn in [-1.0, 1.0] {
                let pt = decal.frame.unproject_world(su * u_ext, sv * v_ext, sn * n_ext);
                lo = lo.min(pt);
                hi = hi.max(pt);
            }
        }
    }
    let mut aabb = Aabb3::new(lo, hi);
    if let Some(bx) = decal.clip_box {
        let clipped = aabb.intersect(&bx);
        if !clipped.is_empty() {
            aabb = clipped;
        }
    }
    if let SeamClip::Box(bx) = decal.seam_clip {
        let clipped = aabb.intersect(&bx);
        if !clipped.is_empty() {
            aabb = clipped;
        }
    }
    aabb
}

/// Derive a critical [`FeatureTarget`] from `decal` so its stroke width automatically guides local
/// meshing, simplification protection, and texel budgets (Priority 11).
pub fn decal_feature_target(decal: &Decal<'_>) -> FeatureTarget {
    let sw = decal_stroke_width(decal);
    let aabb = decal_world_aabb(decal, None);
    let min_feat = Length::m((sw.as_m() * 0.5).max(2.5e-4));
    let max_err = Length::m((sw.as_m() * 0.125).max(5e-5));
    FeatureTarget::new(FeatureTargetOpts {
        name: decal.name.as_str(),
        kind: FeatureKind::Detail,
        region: FeatureRegion::Box(aabb),
        max_error: Some(max_err),
        max_normal_error: None,
        min_feature: Some(min_feat),
        protect: FeatureProtect::Critical,
    })
}

// ==========================================================================================
// 4. MATERIAL REGIONS WITHIN ONE CONTINUOUS SHELL (Priority 7)
// ==========================================================================================

/// Predicate classifying which triangles of a continuous shell receive a [`MaterialRole`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MaterialRule {
    /// Triangles whose centroid lies inside `FeatureRegion`.
    Region(FeatureRegion),
    /// Triangles whose centroid satisfies `eval_point(solid, centroid) <= margin` (survives
    /// arbitrary booleans).
    SolidMask { solid: Solid<Bound>, margin: Length },
    /// Triangles whose outward unit normal lies within `max_angle` of `dir`.
    NormalCone { dir: Vec3, max_angle: Angle },
    /// Triangles recessed inward from `outer_shell` by at least `min_depth`
    /// (`eval_point(outer_shell, centroid) <= -min_depth`).
    RecessDepth { outer_shell: Solid<Bound>, min_depth: Length },
    /// Narrow high-curvature rims/edges where CAD field mean curvature `|div(n)| >= min_curvature`
    /// (optionally filtered by a normal cone).
    CurvatureRim { min_curvature: f64, max_normal_angle_from: Option<(Vec3, Angle)> },
    /// Named surface patch centered on an [`AttachmentFrame`] within `±half_size`, `max_angle`,
    /// and `max_depth`.
    SurfacePatch {
        frame: AttachmentFrame,
        half_size: [Length; 2],
        max_angle: Angle,
        max_depth: Length,
    },
}

/// One painter's-order material assignment rule for [`assign_material_regions`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MaterialRegion {
    pub role: MaterialRole,
    pub rule: MaterialRule,
}

/// Assign per-triangle [`MaterialRole`] values across one continuous shell by evaluating `regions`
/// in order (later rules override earlier ones), returning a new [`Mesh`] with updated `roles` and
/// all other columns preserved intact. Apply before `decimate` so material seams survive
/// simplification.
pub fn assign_material_regions<'a, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    mesh: &MeshView<'_>,
    base_role: MaterialRole,
    base_tape: Option<&Tape<'_>>,
    regions: &[MaterialRegion],
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Mesh<'a>, ModelError> {
    // Pre-compile tapes for any SolidMask or RecessDepth rules once in `scratch`.
    let mut rule_tapes: Vec<Option<Tape<'_>>, Alloc<'_>> = Vec::new_in(scratch);
    rule_tapes
        .try_reserve(regions.len())
        .map_err(|_| ModelError::Alloc { what: "material region tapes" })?;
    for reg in regions {
        let compiled = match reg.rule {
            MaterialRule::SolidMask { solid, .. } => {
                Some(compile(g, solid.id(), CompileOpts { alloc: scratch, detail: Detail::Full, .. })?)
            }
            MaterialRule::RecessDepth { outer_shell, .. } => {
                Some(compile(g, outer_shell.id(), CompileOpts { alloc: scratch, detail: Detail::Full, .. })?)
            }
            _ => None,
        };
        rule_tapes
            .try_push(compiled)
            .map_err(|_| ModelError::Alloc { what: "material region tapes" })?;
    }

    let c = mesh.columns();
    let n_tris = c.triangles.len();
    let mut roles: Vec<MaterialRole, Alloc<'a>> = Vec::new_in(alloc);
    roles
        .try_reserve(n_tris)
        .map_err(|_| ModelError::Alloc { what: "material region roles" })?;
    let mut sc = Scratch::new(scratch);
    let has_vtx_normals = c.normals.len() == c.positions.len();

    for tri in c.triangles {
        let (i0, i1, i2) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
        let p0 = to_v3(c.positions[i0]);
        let p1 = to_v3(c.positions[i1]);
        let p2 = to_v3(c.positions[i2]);
        let centroid = (p0 + p1 + p2) * (1.0 / 3.0);
        let tri_n = (p1 - p0).cross(p2 - p0).normalized().unwrap_or_else(|| {
            if has_vtx_normals {
                (to_v3(c.normals[i0]) + to_v3(c.normals[i1]) + to_v3(c.normals[i2]))
                    .normalized()
                    .unwrap_or(Vec3::new(0.0, 0.0, 1.0))
            } else {
                Vec3::new(0.0, 0.0, 1.0)
            }
        });

        let mut assigned = base_role;
        for (ri, reg) in regions.iter().enumerate() {
            let matched = match reg.rule {
                MaterialRule::Region(fr) => fr.contains(centroid),
                MaterialRule::SolidMask { margin, .. } => match &rule_tapes[ri] {
                    Some(t) => eval_point(t, centroid, &mut sc) <= margin.as_m(),
                    None => false,
                },
                MaterialRule::NormalCone { dir, max_angle } => {
                    if let Some(d_u) = dir.normalized() {
                        let cos_lim = blaze_math::cos(max_angle.as_radians().clamp(0.0, core::f64::consts::PI));
                        tri_n.dot(d_u) >= cos_lim
                    } else {
                        false
                    }
                }
                MaterialRule::RecessDepth { min_depth, .. } => match &rule_tapes[ri] {
                    Some(t) => eval_point(t, centroid, &mut sc) <= -min_depth.as_m(),
                    None => false,
                },
                MaterialRule::CurvatureRim { min_curvature, max_normal_angle_from } => {
                    let normal_ok = match max_normal_angle_from {
                        Some((dir, ang)) => match dir.normalized() {
                            Some(d_u) => tri_n.dot(d_u) >= blaze_math::cos(ang.as_radians().clamp(0.0, core::f64::consts::PI)),
                            None => false,
                        },
                        None => true,
                    };
                    if !normal_ok {
                        false
                    } else if let Some(bt) = base_tape {
                        let (_, g_c) = eval_grad(bt, centroid, &mut sc);
                        let n_c = g_c.normalized().unwrap_or(tri_n);
                        estimate_cad_curvature(bt, centroid, n_c, &mut sc).abs() >= min_curvature
                    } else if has_vtx_normals {
                        let n0 = to_v3(c.normals[i0]).normalized().unwrap_or(tri_n);
                        let n1 = to_v3(c.normals[i1]).normalized().unwrap_or(tri_n);
                        let n2 = to_v3(c.normals[i2]).normalized().unwrap_or(tri_n);
                        estimate_mesh_tri_curvature(p0, p1, p2, n0, n1, n2).abs() >= min_curvature
                    } else {
                        false
                    }
                }
                MaterialRule::SurfacePatch { frame, half_size, max_angle, max_depth } => {
                    let cos_lim = blaze_math::cos(max_angle.as_radians().clamp(0.0, core::f64::consts::PI));
                    if tri_n.dot(frame.normal) < cos_lim {
                        false
                    } else {
                        let proj = if frame.curvature_u.abs() > 1e-6 || frame.curvature_v.abs() > 1e-6 {
                            DecalProjection::ConformalArcLength
                        } else {
                            DecalProjection::Orthographic
                        };
                        let (u, v, n_dist, _, _) =
                            project_onto_frame(&frame, proj, centroid, tri_n, max_depth.as_m().max(1e-6), base_tape, Some(&mut sc));
                        u.abs() <= half_size[0].as_m().abs()
                            && v.abs() <= half_size[1].as_m().abs()
                            && n_dist <= max_depth.as_m().abs()
                    }
                }
            };
            if matched {
                assigned = reg.role;
            }
        }
        roles
            .try_push(assigned)
            .map_err(|_| ModelError::Alloc { what: "material region roles" })?;
    }

    // Copy all remaining mesh columns untouched into `alloc`.
    let mut positions = Vec::new_in(alloc);
    positions
        .try_extend_from_slice(c.positions)
        .map_err(|_| ModelError::Alloc { what: "material region positions" })?;
    let mut normals = Vec::new_in(alloc);
    normals
        .try_extend_from_slice(c.normals)
        .map_err(|_| ModelError::Alloc { what: "material region normals" })?;
    let mut tangents = Vec::new_in(alloc);
    tangents
        .try_extend_from_slice(c.tangents)
        .map_err(|_| ModelError::Alloc { what: "material region tangents" })?;
    let mut uvs = Vec::new_in(alloc);
    uvs.try_extend_from_slice(c.uvs)
        .map_err(|_| ModelError::Alloc { what: "material region uvs" })?;
    let mut colours = Vec::new_in(alloc);
    colours
        .try_extend_from_slice(c.colours)
        .map_err(|_| ModelError::Alloc { what: "material region colours" })?;
    let mut triangles = Vec::new_in(alloc);
    triangles
        .try_extend_from_slice(c.triangles)
        .map_err(|_| ModelError::Alloc { what: "material region triangles" })?;
    let mut parts = Vec::new_in(alloc);
    parts
        .try_extend_from_slice(c.parts)
        .map_err(|_| ModelError::Alloc { what: "material region parts" })?;
    let mut creases = Vec::new_in(alloc);
    creases
        .try_extend_from_slice(c.creases)
        .map_err(|_| ModelError::Alloc { what: "material region creases" })?;

    Mesh::new(MeshVecs { positions, normals, tangents, uvs, colours, triangles, parts, roles, creases })
        .map_err(ModelError::Shape)
}

// ==========================================================================================
// 5. DIRECTIONAL FINISHES & SURFACE WEAR MASKS (Priority 8)
// ==========================================================================================

/// Physical-unit directional micro-surface finish baked into normals and roughness.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SurfaceFinish {
    /// Anisotropic brushed-metal micro-grooves along `direction` (e.g. `pitch = 0.4 mm`,
    /// `depth = 0.025 mm`).
    BrushedMetal {
        direction: [f64; 3],
        pitch: Length,
        depth: Length,
        roughness_variation: f64,
    },
    /// Isotropic 3D micro-stipple grain in physical `grain_size` (injection-molded / powder-coat).
    MoldedGrain {
        grain_size: Length,
        depth: Length,
        roughness_variation: f64,
    },
    /// Concentric CNC / lathe-turned machining grooves around `axis` through `origin`.
    MachinedTurned {
        axis: [f64; 3],
        origin: [Length; 3],
        pitch: Length,
        depth: Length,
        roughness_variation: f64,
    },
}

/// Curvature-driven metallic/substrate edge wear modulated by physical-unit 3D wear noise.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct EdgeWearMask {
    /// Linear RGB of the exposed underlying substrate (e.g. bright raw aluminum).
    pub substrate_colour: [f64; 3],
    pub substrate_metallic: f64,
    pub substrate_roughness: f64,
    /// Minimum convex curvature (`div(n)` in `1/m`) where wear begins to appear.
    pub curvature_threshold: f64,
    /// Overall wear strength in `0.0..=1.0`.
    pub intensity: f64,
    /// Physical length scale of wear chipping / scuff modulation.
    pub noise_scale: Length,
}

/// Binds an optional [`SurfaceFinish`] and/or [`EdgeWearMask`] to one [`MaterialRole`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FinishAssignment {
    pub role: MaterialRole,
    pub finish: Option<SurfaceFinish>,
    pub wear: Option<EdgeWearMask>,
}

/// Evaluated procedural finish and wear sample at one 3D surface point.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FinishSample {
    /// World-space surface height gradient `∇h` (dimensionless slope) to perturb `N_world`.
    pub height_grad_world: Vec3,
    /// Additive roughness delta (clamped by caller).
    pub roughness_delta: f64,
    /// Exposed substrate blend factor in `0.0..=1.0`.
    pub wear_factor: f64,
}

/// Deterministic integer hash of 3D lattice coordinates `(ix, iy, iz)` -> `[0.0, 1.0)`.
#[inline]
fn hash_lattice_3d(ix: i32, iy: i32, iz: i32) -> f64 {
    let mut h = (ix as i64 as u64).wrapping_mul(0x9E37_79B1_85EB_CA87)
        ^ (iy as i64 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (iz as i64 as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^= h >> 33;
    // Top 53 bits to exact dyadic fraction in [0, 1).
    ((h >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
}

/// Deterministic 3D quintic value noise returning `(value ∈ [0, 1], analytical_grad)`.
fn value_noise_3d_grad(p: Vec3) -> (f64, Vec3) {
    let fx0 = blaze_math::floor(p.x);
    let fy0 = blaze_math::floor(p.y);
    let fz0 = blaze_math::floor(p.z);
    let ix = fx0 as i32;
    let iy = fy0 as i32;
    let iz = fz0 as i32;

    let tx = p.x - fx0;
    let ty = p.y - fy0;
    let tz = p.z - fz0;

    // Quintic C2 Hermite fade u(t) = 6t^5 - 15t^4 + 10t^3, u'(t) = 30t^2(t-1)^2.
    let ux = tx * tx * tx * (tx * (6.0 * tx - 15.0) + 10.0);
    let uy = ty * ty * ty * (ty * (6.0 * ty - 15.0) + 10.0);
    let uz = tz * tz * tz * (tz * (6.0 * tz - 15.0) + 10.0);
    let dux = 30.0 * tx * tx * (tx - 1.0) * (tx - 1.0);
    let duy = 30.0 * ty * ty * (ty - 1.0) * (ty - 1.0);
    let duz = 30.0 * tz * tz * (tz - 1.0) * (tz - 1.0);

    let c000 = hash_lattice_3d(ix, iy, iz);
    let c100 = hash_lattice_3d(ix + 1, iy, iz);
    let c010 = hash_lattice_3d(ix, iy + 1, iz);
    let c110 = hash_lattice_3d(ix + 1, iy + 1, iz);
    let c001 = hash_lattice_3d(ix, iy, iz + 1);
    let c101 = hash_lattice_3d(ix + 1, iy, iz + 1);
    let c011 = hash_lattice_3d(ix, iy + 1, iz + 1);
    let c111 = hash_lattice_3d(ix + 1, iy + 1, iz + 1);

    let k0 = c000;
    let k1 = c100 - c000;
    let k2 = c010 - c000;
    let k3 = c001 - c000;
    let k4 = c000 - c100 - c010 + c110;
    let k5 = c000 - c010 - c001 + c011;
    let k6 = c000 - c100 - c001 + c101;
    let k7 = -c000 + c100 + c010 - c110 + c001 - c101 - c011 + c111;

    let val = k0 + k1 * ux + k2 * uy + k3 * uz + k4 * ux * uy + k5 * uy * uz + k6 * uz * ux + k7 * ux * uy * uz;
    let gx = dux * (k1 + k4 * uy + k6 * uz + k7 * uy * uz);
    let gy = duy * (k2 + k5 * uz + k4 * ux + k7 * uz * ux);
    let gz = duz * (k3 + k6 * ux + k5 * uy + k7 * ux * uy);
    (val, Vec3::new(gx, gy, gz))
}

/// Deterministic 2-octave fBm 3D value noise returning `(val ∈ [0, 1], analytical_grad)`.
fn fbm2_3d_grad(p: Vec3) -> (f64, Vec3) {
    let (v1, g1) = value_noise_3d_grad(p);
    let p2 = p * 2.13 + Vec3::new(17.3, 31.7, 47.1);
    let (v2, g2) = value_noise_3d_grad(p2);
    let val = (v1 * 0.68 + v2 * 0.32).clamp(0.0, 1.0);
    let grad = g1 * 0.68 + g2 * (0.32 * 2.13);
    (val, grad)
}

/// Evaluate directional micro-finish and curvature-driven edge wear at 3D point `p` with unit
/// normal `n` and signed convex curvature `convex_curvature` (`div(n)` in `1/m`).
pub fn evaluate_finish(assignment: &FinishAssignment, p: Vec3, n: Vec3, convex_curvature: f64) -> FinishSample {
    let n_u = n.normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));
    let mut height_grad_world = Vec3::ZERO;
    let mut roughness_delta = 0.0f64;

    if let Some(finish) = assignment.finish {
        match finish {
            SurfaceFinish::BrushedMetal { direction, pitch, depth, roughness_variation } => {
                let pitch_m = pitch.as_m().abs().max(1e-6);
                let depth_m = depth.as_m().abs();
                let d_in = Vec3::from_array(direction);
                let brush_dir = (d_in - n_u * d_in.dot(n_u)).normalized().unwrap_or_else(|| canonical_tangent(n_u));
                let cross_dir = n_u.cross(brush_dir).normalized().unwrap_or_else(|| canonical_tangent(brush_dir));

                let s_across = p.dot(cross_dir);
                let s_along = p.dot(brush_dir);
                let s_norm = p.dot(n_u);

                // Anisotropic brushed-streak lattice: 1× pitch across grooves, 18× stretched along.
                let inv_across = 1.0 / pitch_m;
                let inv_along = 1.0 / (pitch_m * 18.0);
                let inv_norm = 1.0 / (pitch_m * 4.0);
                let q = Vec3::new(s_across * inv_across, s_along * inv_along, s_norm * inv_norm);
                let (nv, ng) = fbm2_3d_grad(q);

                // Subtle periodic micro-groove harmonic modulated by streak noise.
                let phase = 2.0 * core::f64::consts::PI * s_across * inv_across;
                let (sin_p, cos_p) = blaze_math::sin_cos(phase);
                let groove_slope = 0.28 * (2.0 * core::f64::consts::PI * inv_across) * cos_p * (0.4 + 0.6 * nv);

                let grad_across = (ng.x * inv_across + groove_slope) * depth_m;
                let grad_along = ng.y * inv_along * depth_m;
                height_grad_world = cross_dir * grad_across + brush_dir * grad_along;
                roughness_delta = ((nv - 0.5) * 1.6 + 0.2 * sin_p) * roughness_variation;
            }
            SurfaceFinish::MoldedGrain { grain_size, depth, roughness_variation } => {
                let grain_m = grain_size.as_m().abs().max(1e-6);
                let depth_m = depth.as_m().abs();
                let inv_g = 1.0 / grain_m;
                let (nv, ng) = fbm2_3d_grad(p * inv_g);
                let raw_grad = ng * (depth_m * inv_g);
                height_grad_world = raw_grad - n_u * raw_grad.dot(n_u);
                roughness_delta = (nv - 0.5) * 2.0 * roughness_variation;
            }
            SurfaceFinish::MachinedTurned { axis, origin, pitch, depth, roughness_variation } => {
                let pitch_m = pitch.as_m().abs().max(1e-6);
                let depth_m = depth.as_m().abs();
                let ax = Vec3::from_array(axis).normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));
                let orig = Vec3::new(origin[0].as_m(), origin[1].as_m(), origin[2].as_m());
                let rel = p - orig;
                let z_a = rel.dot(ax);
                let rad = rel - ax * z_a;
                let r = rad.length();
                let rad_dir = if r > 1e-12 { rad * (1.0 / r) } else { canonical_tangent(ax) };

                // Concentric tool-feed wave on both end faces (radial) and cylindrical walls (axial).
                let k_r = 2.0 * core::f64::consts::PI / pitch_m;
                let k_z = 2.0 * core::f64::consts::PI / (pitch_m * 1.5);
                let phase = k_r * r + k_z * z_a;
                let (sin1, cos1) = blaze_math::sin_cos(phase);
                let (_, cos2) = blaze_math::sin_cos(2.0 * phase);
                let d_phase = 0.5 * depth_m * (cos1 + 0.35 * 2.0 * cos2);
                let raw_grad = rad_dir * (d_phase * k_r) + ax * (d_phase * k_z);
                height_grad_world = raw_grad - n_u * raw_grad.dot(n_u);
                roughness_delta = (0.5 * sin1) * roughness_variation;
            }
        }
    }

    let wear_factor = if let Some(wear) = assignment.wear {
        let inten = wear.intensity.clamp(0.0, 1.0);
        if inten > 0.0 && convex_curvature > 0.0 && wear.curvature_threshold.is_finite() {
            let scale_m = wear.noise_scale.as_m().abs().max(1e-4);
            let (nv, _) = fbm2_3d_grad(p * (1.0 / scale_m) + Vec3::new(7.1, 13.7, 29.3));
            let thr = wear.curvature_threshold.max(1e-3);
            let lo = thr * (0.65 + 0.70 * nv);
            let hi = thr * (1.65 + 0.70 * nv);
            smoothstep(lo, hi, convex_curvature) * inten
        } else {
            0.0
        }
    } else {
        0.0
    };

    FinishSample { height_grad_world, roughness_delta, wear_factor }
}

// ==========================================================================================
// 6. PBR DETAIL BAKER (Priority 9)
// ==========================================================================================

/// Options for [`bake_surface_maps`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct BakeOpts<'a, 's> {
    pub size: u32 = 512,
    pub gutter_px: u32 = 4,
    pub bake_cad_normals: bool = true,
    pub normal_strength: f64 = 1.0,
    pub limits: Limits = Limits::SHIPPED,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// One baked RGBA8 texture together with its deterministic encoded PNG file bytes.
#[derive(Debug)]
pub struct BakedTexture<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8, Alloc<'a>>,
    pub png: Vec<u8, Alloc<'a>>,
}

impl BakedTexture<'_> {
    /// Borrow the raw RGBA8 pixels as a [`TextureMap`] for `blaze_cad_picture::render_mesh_scene`.
    #[inline]
    pub fn as_texture_map(&self) -> TextureMap<'_> {
        TextureMap { width: self.width, height: self.height, rgba: &self.rgba }
    }
}

/// Baked PBR texture atlas (`base_colour`, `metallic_roughness`, `normal`) ready for glTF 2.0
/// export (`blaze_cad_out::TexturePack`) and direct preview rendering
/// (`blaze_cad_picture::MaterialMaps`).
///
/// # glTF 2.0 factor convention
///
/// In glTF 2.0, `baseColorTexture` is sRGB-encoded and multiplied by `material.baseColorFactor`,
/// and `metallicRoughnessTexture` (linear G = roughness, B = metallic) is multiplied by
/// `material.roughnessFactor` and `material.metallicFactor`. Because [`bake_surface_maps`] bakes
/// each triangle's per-role colour, roughness, metallic, directional finish, edge wear, and decals
/// directly into the atlas, pair the baked maps with [`baked_material`] (`base_colour = [1, 1, 1, 1]`,
/// `metallic = 1.0`, `roughness = 1.0`) so material factors are never double-multiplied.
#[derive(Debug)]
pub struct BakedMaps<'a> {
    pub base_colour: BakedTexture<'a>,
    pub metallic_roughness: BakedTexture<'a>,
    pub normal: BakedTexture<'a>,
    pub covered_texels: u64,
    pub decal_texels: u64,
}

impl BakedMaps<'_> {
    /// Borrow all three baked maps as a [`MaterialMaps`] for `blaze_cad_picture::MeshPictureOpts`.
    #[inline]
    pub fn as_material_maps(&self, normal_scale: f64) -> MaterialMaps<'_> {
        MaterialMaps {
            base_colour: Some(self.base_colour.as_texture_map()),
            metallic_roughness: Some(self.metallic_roughness.as_texture_map()),
            normal: Some(self.normal.as_texture_map()),
            normal_scale,
        }
    }
}

/// Neutral white PBR [`Material`] (`base_colour = [1, 1, 1, 1]`, `metallic = 1.0`,
/// `roughness = 1.0`) for binding a multi-material [`BakedMaps`] atlas in glTF 2.0 or
/// `blaze_cad_picture` without double-multiplying baked colours or roughness/metallic values.
pub fn baked_material(name: &str) -> Material<'_> {
    Material {
        name,
        base_colour: [1.0, 1.0, 1.0, 1.0],
        metallic: 1.0,
        roughness: 1.0,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    }
}

/// Allocate an RGBA8 buffer filled with a constant 4-byte pattern.
fn filled_rgba<'a>(pixels: usize, fill: [u8; 4], alloc: Alloc<'a>, what: &'static str) -> Result<Vec<u8, Alloc<'a>>, ModelError> {
    let total = pixels.checked_mul(4).ok_or(ModelError::Alloc { what })?;
    let mut v: Vec<u8, Alloc<'a>> = Vec::new_in(alloc);
    v.try_reserve(total).map_err(|_| ModelError::Alloc { what })?;
    for _ in 0..pixels {
        v.try_extend_from_slice(&fill).map_err(|_| ModelError::Alloc { what })?;
    }
    Ok(v)
}

/// Allocate a zero-filled `u8` buffer in `alloc`.
fn filled_u8<'a>(len: usize, alloc: Alloc<'a>, what: &'static str) -> Result<Vec<u8, Alloc<'a>>, ModelError> {
    let mut v: Vec<u8, Alloc<'a>> = Vec::new_in(alloc);
    v.try_reserve(len).map_err(|_| ModelError::Alloc { what })?;
    for _ in 0..len {
        v.try_push(0).map_err(|_| ModelError::Alloc { what })?;
    }
    Ok(v)
}

/// Run `passes` deterministic 8-neighbor dilation iterations over uncovered texels so UV chart
/// seams never bleed black under bilinear filtering or mipmapping.
#[allow(clippy::too_many_arguments)]
fn dilate_gutter(
    size: u32,
    passes: u32,
    base_rgba: &mut [u8],
    mr_rgba: &mut [u8],
    norm_rgba: &mut [u8],
    covered: &mut [u8],
    scratch: Alloc<'_>,
) -> Result<(), ModelError> {
    let passes = passes.min(MAX_GUTTER_PASSES);
    if passes == 0 {
        return Ok(());
    }
    let s = size as usize;
    let total_px = s * s;
    let mut next_cov = filled_u8(total_px, scratch, "bake gutter coverage")?;
    let mut next_base = filled_u8(total_px * 4, scratch, "bake gutter base")?;
    let mut next_mr = filled_u8(total_px * 4, scratch, "bake gutter metallic-roughness")?;
    let mut next_norm = filled_u8(total_px * 4, scratch, "bake gutter normal")?;

    const DIRS_4: [(isize, isize); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
    const DIRS_8: [(isize, isize); 4] = [(-1, -1), (1, -1), (-1, 1), (1, 1)];

    for _ in 0..passes {
        next_cov.copy_from_slice(covered);
        next_base.copy_from_slice(base_rgba);
        next_mr.copy_from_slice(mr_rgba);
        next_norm.copy_from_slice(norm_rgba);

        let mut changed = false;
        for y in 0..s {
            for x in 0..s {
                let idx = y * s + x;
                if covered[idx] != 0 {
                    continue;
                }
                let mut sum_b = [0u32; 4];
                let mut sum_m = [0u32; 4];
                let mut sum_n = [0.0f64; 3];
                let mut count = 0u32;

                // Prefer 4-connected cardinal neighbors; fall back to diagonal neighbors if none.
                for dirs in [&DIRS_4[..], &DIRS_8[..]] {
                    for &(dx, dy) in dirs {
                        let nx = x as isize + dx;
                        let ny = y as isize + dy;
                        if nx >= 0 && ny >= 0 && (nx as usize) < s && (ny as usize) < s {
                            let ni = (ny as usize) * s + (nx as usize);
                            if covered[ni] != 0 {
                                let noff = ni * 4;
                                for c in 0..4 {
                                    sum_b[c] += u32::from(base_rgba[noff + c]);
                                    sum_m[c] += u32::from(mr_rgba[noff + c]);
                                }
                                sum_n[0] += f64::from(norm_rgba[noff]) / 255.0 * 2.0 - 1.0;
                                sum_n[1] += f64::from(norm_rgba[noff + 1]) / 255.0 * 2.0 - 1.0;
                                sum_n[2] += f64::from(norm_rgba[noff + 2]) / 255.0 * 2.0 - 1.0;
                                count += 1;
                            }
                        }
                    }
                    if count > 0 {
                        break;
                    }
                }

                if count > 0 {
                    changed = true;
                    next_cov[idx] = 1;
                    let off = idx * 4;
                    let half = count / 2;
                    for c in 0..4 {
                        next_base[off + c] = ((sum_b[c] + half) / count) as u8;
                        next_mr[off + c] = ((sum_m[c] + half) / count) as u8;
                    }
                    let nv = Vec3::new(sum_n[0], sum_n[1], sum_n[2].max(0.05))
                        .normalized()
                        .unwrap_or(Vec3::new(0.0, 0.0, 1.0));
                    next_norm[off] = quantize_u8(nv.x * 0.5 + 0.5);
                    next_norm[off + 1] = quantize_u8(nv.y * 0.5 + 0.5);
                    next_norm[off + 2] = quantize_u8(nv.z * 0.5 + 0.5);
                    next_norm[off + 3] = 255;
                }
            }
        }

        covered.copy_from_slice(&next_cov);
        base_rgba.copy_from_slice(&next_base);
        mr_rgba.copy_from_slice(&next_mr);
        norm_rgba.copy_from_slice(&next_norm);
        if !changed {
            break;
        }
    }

    Ok(())
}

/// Bake per-role PBR materials, high-resolution CAD surface normals, physical-unit directional
/// finishes & edge wear, and anti-aliased surface-conforming decals with optional relief into
/// deterministic RGBA8 + PNG texture maps (`base_colour`, `metallic_roughness`, `normal`).
///
/// Requires `mesh.uvs()` and `mesh.tangents()` (produced by `blaze_cad_mesh::unwrap_uvs`); returns
/// [`ModelError::Argument`] if either column is missing.
pub fn bake_surface_maps<'a>(
    mesh: &MeshView<'_>,
    materials: &[Material<'_>],
    hi_res_tape: Option<&Tape<'_>>,
    decals: &[Decal<'_>],
    finishes: &[FinishAssignment],
    o: BakeOpts<'a, '_>,
) -> Result<BakedMaps<'a>, ModelError> {
    if o.size < 2 {
        return Err(ModelError::Argument {
            function: "bake_surface_maps",
            arg: "size",
            accepted: "a texture size of at least 2",
        });
    }
    let total_px_u64 = u64::from(o.size) * u64::from(o.size);
    if total_px_u64 > o.limits.most_pixels {
        return Err(ModelError::Argument {
            function: "bake_surface_maps",
            arg: "size",
            accepted: "size * size within limits.most_pixels",
        });
    }
    if !(o.normal_strength.is_finite() && o.normal_strength >= 0.0) {
        return Err(ModelError::Argument {
            function: "bake_surface_maps",
            arg: "normal_strength",
            accepted: "a finite number at least zero",
        });
    }
    let Some(uvs) = mesh.uvs() else {
        return Err(ModelError::Argument {
            function: "bake_surface_maps",
            arg: "mesh.uvs",
            accepted: "a UV-unwrapped mesh with a non-empty uvs column",
        });
    };
    let Some(tangents) = mesh.tangents() else {
        return Err(ModelError::Argument {
            function: "bake_surface_maps",
            arg: "mesh.tangents",
            accepted: "a mesh with a non-empty tangents column",
        });
    };

    let s = o.scratch;
    let size_us = o.size as usize;
    let total_px = usize::try_from(total_px_u64).map_err(|_| ModelError::Alloc { what: "baked texture pixels" })?;

    // Pre-parse any SVG decals once in scratch + precompute conservative world AABBs for culling.
    let mut parsed_svgs: Vec<Option<SvgPath2D<'_>>, Alloc<'_>> = Vec::new_in(s);
    parsed_svgs
        .try_reserve(decals.len())
        .map_err(|_| ModelError::Alloc { what: "baked decal SVG paths" })?;
    let mut decal_aabbs: Vec<Aabb3, Alloc<'_>> = Vec::new_in(s);
    decal_aabbs
        .try_reserve(decals.len())
        .map_err(|_| ModelError::Alloc { what: "baked decal bounds" })?;
    for dec in decals {
        let parsed = match dec.shape {
            DecalShape::SvgPath { d, .. } => Some(parse_svg_path(d, s)?),
            _ => None,
        };
        let aabb = decal_world_aabb(dec, parsed.as_ref());
        parsed_svgs
            .try_push(parsed)
            .map_err(|_| ModelError::Alloc { what: "baked decal SVG paths" })?;
        decal_aabbs
            .try_push(aabb)
            .map_err(|_| ModelError::Alloc { what: "baked decal bounds" })?;
    }

    let mut base_rgba = filled_rgba(total_px, [128, 128, 128, 255], o.alloc, "baked base colour")?;
    let mut mr_rgba = filled_rgba(total_px, [255, 128, 0, 255], o.alloc, "baked metallic-roughness")?;
    let mut norm_rgba = filled_rgba(total_px, [128, 128, 255, 255], o.alloc, "baked normal map")?;
    let mut covered = filled_u8(total_px, s, "baked coverage mask")?;
    let mut decal_mask = filled_u8(total_px, s, "baked decal mask")?;

    let mut active_decals: Vec<usize, Alloc<'_>> = Vec::new_in(s);
    active_decals
        .try_reserve(decals.len())
        .map_err(|_| ModelError::Alloc { what: "baked active decals" })?;

    let positions = mesh.positions();
    let normals = mesh.normals();
    let roles = mesh.roles();
    let mut sc = Scratch::new(s);
    let size_f = f64::from(o.size);

    for (ti, tri) in mesh.triangles().iter().enumerate() {
        let (i0, i1, i2) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
        let p0 = to_v3(positions[i0]);
        let p1 = to_v3(positions[i1]);
        let p2 = to_v3(positions[i2]);

        let uv0 = uvs[i0];
        let uv1 = uvs[i1];
        let uv2 = uvs[i2];
        let x0 = f64::from(uv0[0]) * size_f;
        let y0 = f64::from(uv0[1]) * size_f;
        let x1 = f64::from(uv1[0]) * size_f;
        let y1 = f64::from(uv1[1]) * size_f;
        let x2 = f64::from(uv2[0]) * size_f;
        let y2 = f64::from(uv2[1]) * size_f;

        let area2 = (x1 - x0) * (y2 - y0) - (x2 - x0) * (y1 - y0);
        if !(area2.abs() > 1e-12 && area2.is_finite()) {
            continue;
        }
        let inv_area2 = 1.0 / area2;

        let world_cross = (p1 - p0).cross(p2 - p0);
        let world_area2 = world_cross.length();
        let tri_geo_n = if world_area2 > 1e-18 && world_area2.is_finite() {
            world_cross * (1.0 / world_area2)
        } else {
            Vec3::new(0.0, 0.0, 1.0)
        };
        let texel_world_m = if world_area2 > 0.0 && world_area2.is_finite() {
            blaze_math::sqrt(world_area2 / area2.abs()).clamp(1e-6, 0.1)
        } else {
            1e-4
        };

        let n0 = to_v3(normals[i0]).normalized().unwrap_or(tri_geo_n);
        let n1 = to_v3(normals[i1]).normalized().unwrap_or(tri_geo_n);
        let n2 = to_v3(normals[i2]).normalized().unwrap_or(tri_geo_n);

        let t0_raw = tangents[i0];
        let t1_raw = tangents[i1];
        let t2_raw = tangents[i2];
        let t0 = Vec3::new(f64::from(t0_raw[0]), f64::from(t0_raw[1]), f64::from(t0_raw[2]));
        let t1 = Vec3::new(f64::from(t1_raw[0]), f64::from(t1_raw[1]), f64::from(t1_raw[2]));
        let t2 = Vec3::new(f64::from(t2_raw[0]), f64::from(t2_raw[1]), f64::from(t2_raw[2]));
        let w0_tan = f64::from(t0_raw[3]);
        let w1_tan = f64::from(t1_raw[3]);
        let w2_tan = f64::from(t2_raw[3]);

        let role = roles.get(ti).copied().unwrap_or(MaterialRole(0));
        let mat = materials.get(role.0 as usize).copied().unwrap_or(STEEL);

        // Last matching FinishAssignment for this role wins (painter's order).
        let mut finish_opt: Option<&FinishAssignment> = None;
        for fa in finishes {
            if fa.role == role {
                finish_opt = Some(fa);
            }
        }
        let mesh_tri_curv = if finish_opt.is_some_and(|fa| fa.wear.is_some()) && hi_res_tape.is_none() {
            estimate_mesh_tri_curvature(p0, p1, p2, n0, n1, n2)
        } else {
            0.0
        };

        // Broad-phase cull decals against this triangle's 3D AABB.
        active_decals.clear();
        if !decals.is_empty() {
            let tri_aabb = Aabb3::new(p0.min(p1).min(p2), p0.max(p1).max(p2));
            for (di, daabb) in decal_aabbs.iter().enumerate() {
                if tri_aabb.overlaps(daabb) {
                    active_decals
                        .try_push(di)
                        .map_err(|_| ModelError::Alloc { what: "baked active decals" })?;
                }
            }
        }

        let min_xf = x0.min(x1).min(x2);
        let max_xf = x0.max(x1).max(x2);
        let min_yf = y0.min(y1).min(y2);
        let max_yf = y0.max(y1).max(y2);
        let min_x = (blaze_math::floor(min_xf).max(0.0) as u32).min(o.size - 1);
        let max_x = (blaze_math::ceil(max_xf).max(0.0) as u32).min(o.size - 1);
        let min_y = (blaze_math::floor(min_yf).max(0.0) as u32).min(o.size - 1);
        let max_y = (blaze_math::ceil(max_yf).max(0.0) as u32).min(o.size - 1);

        for py in min_y..=max_y {
            let cy = f64::from(py) + 0.5;
            for px in min_x..=max_x {
                let cx = f64::from(px) + 0.5;
                let b0 = ((x1 - cx) * (y2 - cy) - (x2 - cx) * (y1 - cy)) * inv_area2;
                let b1 = ((x2 - cx) * (y0 - cy) - (x0 - cx) * (y2 - cy)) * inv_area2;
                let b2 = 1.0 - b0 - b1;
                if b0 < -1e-6 || b1 < -1e-6 || b2 < -1e-6 {
                    continue;
                }
                let bw0 = b0.max(0.0);
                let bw1 = b1.max(0.0);
                let bw2 = b2.max(0.0);
                let bsum = bw0 + bw1 + bw2;
                if !(bsum > 0.0) {
                    continue;
                }
                let inv_bsum = 1.0 / bsum;
                let (w0, w1, w2) = (bw0 * inv_bsum, bw1 * inv_bsum, bw2 * inv_bsum);

                let p = p0 * w0 + p1 * w1 + p2 * w2;
                let n_shade = (n0 * w0 + n1 * w1 + n2 * w2).normalized().unwrap_or(tri_geo_n);
                let t_interp = t0 * w0 + t1 * w1 + t2 * w2;
                let t_vec = (t_interp - n_shade * t_interp.dot(n_shade))
                    .normalized()
                    .unwrap_or_else(|| canonical_tangent(n_shade));
                let t_hand = if w0_tan * w0 + w1_tan * w1 + w2_tan * w2 < 0.0 { -1.0 } else { 1.0 };
                let b_vec = n_shade.cross(t_vec) * t_hand;

                let mut col = [
                    mat.base_colour[0].clamp(0.0, 1.0),
                    mat.base_colour[1].clamp(0.0, 1.0),
                    mat.base_colour[2].clamp(0.0, 1.0),
                ];
                let alpha = mat.base_colour[3].clamp(0.0, 1.0);
                let mut rough = mat.roughness.clamp(0.04, 1.0);
                let mut metal = mat.metallic.clamp(0.0, 1.0);
                let mut n_world = n_shade;

                // 1. High-resolution CAD normal baking.
                if o.bake_cad_normals
                    && let Some(tape) = hi_res_tape
                {
                    let (_, g_cad) = eval_grad(tape, p, &mut sc);
                    if let Some(n_cad) = g_cad.normalized()
                        && n_cad.dot(n_shade) > 0.2
                    {
                        n_world = n_cad;
                    }
                }

                // 2. Physical-unit directional finish & curvature-driven edge wear.
                if let Some(fa) = finish_opt {
                    let convex_curv = if fa.wear.is_some() {
                        if let Some(tape) = hi_res_tape {
                            estimate_cad_curvature(tape, p, n_world, &mut sc)
                        } else {
                            mesh_tri_curv
                        }
                    } else {
                        0.0
                    };
                    let fs = evaluate_finish(fa, p, n_world, convex_curv);
                    let g_t = fs.height_grad_world - n_world * fs.height_grad_world.dot(n_world);
                    n_world = (n_world - g_t).normalized().unwrap_or(n_world);
                    rough = (rough + fs.roughness_delta).clamp(0.04, 1.0);

                    if fs.wear_factor > 0.0
                        && let Some(wear) = fa.wear
                    {
                        let wf = fs.wear_factor.clamp(0.0, 1.0);
                        for c in 0..3 {
                            col[c] = lerp(col[c], wear.substrate_colour[c].clamp(0.0, 1.0), wf);
                        }
                        metal = lerp(metal, wear.substrate_metallic.clamp(0.0, 1.0), wf);
                        rough = lerp(rough, wear.substrate_roughness.clamp(0.04, 1.0), wf);
                    }
                }

                // 3. Anti-aliased surface decals + optional emboss/deboss relief normals.
                let idx = (py as usize) * size_us + (px as usize);
                for &di in active_decals.iter() {
                    let dec = &decals[di];
                    if let Some(ds) = evaluate_decal_inner(
                        dec,
                        p,
                        n_shade,
                        role,
                        texel_world_m,
                        parsed_svgs[di].as_ref(),
                        hi_res_tape,
                        Some(&mut sc),
                    ) {
                        let eff_a = (ds.coverage * dec.material.base_colour[3].clamp(0.0, 1.0)).clamp(0.0, 1.0);
                        if eff_a > 1e-4 {
                            for c in 0..3 {
                                col[c] = lerp(col[c], dec.material.base_colour[c].clamp(0.0, 1.0), eff_a);
                            }
                            rough = lerp(rough, dec.material.roughness.clamp(0.04, 1.0), eff_a);
                            metal = lerp(metal, dec.material.metallic.clamp(0.0, 1.0), eff_a);
                            decal_mask[idx] = 1;
                        }
                        if let Some(relief) = dec.material.relief {
                            let h_m = relief.as_m();
                            if h_m.abs() > 1e-9 {
                                let bevel_w = (texel_world_m * 1.5).max(h_m.abs() * 0.75);
                                // Height h(sd) = h_m * smoothstep(+bevel_w, -bevel_w, sd).
                                let dh_dsd = h_m * smoothstep_deriv(bevel_w, -bevel_w, ds.sd_m);
                                if dh_dsd.abs() > 1e-12 {
                                    let grad_h = (ds.tangent_u * ds.grad_u + ds.tangent_v * ds.grad_v) * dh_dsd;
                                    let grad_t = grad_h - n_world * grad_h.dot(n_world);
                                    n_world = (n_world - grad_t).normalized().unwrap_or(n_world);
                                }
                            }
                        }
                    }
                }

                // 4. Encode tangent-space normal, sRGB base colour, and glTF metallic-roughness.
                let tx = n_world.dot(t_vec) * o.normal_strength;
                let ty = n_world.dot(b_vec) * o.normal_strength;
                let tz = n_world.dot(n_shade).max(0.05);
                let tn = Vec3::new(tx, ty, tz).normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));

                let off = idx * 4;
                base_rgba[off] = quantize_u8(linear_to_srgb(col[0]));
                base_rgba[off + 1] = quantize_u8(linear_to_srgb(col[1]));
                base_rgba[off + 2] = quantize_u8(linear_to_srgb(col[2]));
                base_rgba[off + 3] = quantize_u8(alpha);

                mr_rgba[off] = 255;
                mr_rgba[off + 1] = quantize_u8(rough);
                mr_rgba[off + 2] = quantize_u8(metal);
                mr_rgba[off + 3] = 255;

                norm_rgba[off] = quantize_u8(tn.x * 0.5 + 0.5);
                norm_rgba[off + 1] = quantize_u8(tn.y * 0.5 + 0.5);
                norm_rgba[off + 2] = quantize_u8(tn.z * 0.5 + 0.5);
                norm_rgba[off + 3] = 255;

                covered[idx] = 1;
            }
        }
    }

    // Count initial rasterized coverage before gutter dilation.
    let mut covered_texels = 0u64;
    let mut decal_texels = 0u64;
    for i in 0..total_px {
        if covered[i] != 0 {
            covered_texels += 1;
        }
        if decal_mask[i] != 0 {
            decal_texels += 1;
        }
    }

    // Iterative gutter dilation prevents dark UV seam halos under bilinear/mipmap sampling.
    dilate_gutter(o.size, o.gutter_px, &mut base_rgba, &mut mr_rgba, &mut norm_rgba, &mut covered, s)?;

    // Encode all three textures to deterministic PNG bytes.
    let png_opts = PngOpts { level: Level::Fast, limits: o.limits, alloc: o.alloc, scratch: s };
    let base_png = png(Image { width: o.size, height: o.size, kind: PngKind::Rgba8, pixels: &base_rgba }, png_opts)
        .map_err(ModelError::Png)?;
    let mr_png = png(Image { width: o.size, height: o.size, kind: PngKind::Rgba8, pixels: &mr_rgba }, png_opts)
        .map_err(ModelError::Png)?;
    let norm_png = png(Image { width: o.size, height: o.size, kind: PngKind::Rgba8, pixels: &norm_rgba }, png_opts)
        .map_err(ModelError::Png)?;

    Ok(BakedMaps {
        base_colour: BakedTexture { width: o.size, height: o.size, rgba: base_rgba, png: base_png },
        metallic_roughness: BakedTexture { width: o.size, height: o.size, rgba: mr_rgba, png: mr_png },
        normal: BakedTexture { width: o.size, height: o.size, rgba: norm_rgba, png: norm_png },
        covered_texels,
        decal_texels,
    })
}
