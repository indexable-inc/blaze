//! PICTURES OF A FIELD (D183): [`render`] a field tape (blaze-cad-field) as seen from a
//! [`View`] into an RGBA8 [`Picture`] with each pixel's depth, and write it as a PNG
//! (blaze-cad-out's encoder). No mesh is made: the picture is of the solid itself, so a model is
//! looked at the moment it compiles, and an agent can check a change by eye.
//!
//! # The tracer: tiled simplification, distance-bound strides, never past a wall
//!
//! The viewport is partitioned into hierarchical tiles (`64×64` macro-tiles over `16×16` pixel
//! tiles). For each tile, a conservative 3D AABB of the tile's ray frustum clipped to the tape's
//! bounding box is evaluated with `eval_box`:
//! - If `iv.lo() > 0`, the entire tile is empty and skipped in one evaluation.
//! - Otherwise, decided choice sites are pruned with `simplify` into a compact tile-local tape in
//!   `scratch`, so all pixels in the tile trace a much shorter program.
//!
//! Along each ray (clipped to the tape's box), because every field level in `blaze-cad-field`
//! satisfies `|f(p)| <= dist(p, surface)`, empty-space traversal takes safe distance-bound
//! strides (`eval_point` and `iv.lo()` jumps) while never stepping past a wall however thin,
//! seamlessly switching to outward-rounded interval-segment marching (`eval_box` with segment
//! doubling/halving) near grazing silhouettes and slow-converging `Bound` fields. On a hit, the
//! crossing is refined within `[at, end]` using thin-wall-safe distance steps and interval
//! bisection before computing `eval_grad`, eliminating grazing-angle shading artifacts.
//!
//! The step tolerance is a fraction ([`PictureOpts::pixel_fraction`], half by default) of a
//! pixel's footprint at the depth reached ([`Camera::tolerance_at`]): the silhouette is right to
//! half a pixel, so a distant part costs no more steps than it shows.
//!
//! # Shading and render modes: deterministic byte for byte
//!
//! By default ([`Shading::Classic`], [`RenderMode::Shaded`], `supersample: 1`), surfaces use
//! fixed single-light Lambertian shading (`colour × (0.22 + 0.78 max(0, n·l))`, rounded to
//! nearest-even). [`Shading::Studio`] adds three-point CAD studio lighting, Blinn-Phong
//! specular highlights, hemisphere fill, Fresnel rim, multi-scale SDF ambient occlusion, and
//! deterministic soft SDF shadows (with optional sRGB gamma). [`RenderMode`] also supports
//! `Normals`, `Depth`, and procedural `Matcap`, and `supersample: 2` enables deterministic 2×2
//! rotated-grid supersampling (RGSS). Nothing reads a clock, a thread or a hash order: two
//! renders of one tape and view are equal in bytes (rule 25).
//!
//! # Rule 18
//!
//! Every ray takes at most [`PictureOpts::most_steps`] steps, and a picture at most
//! [`PictureOpts::most_pixels`] pixels; passing either is a typed error naming the pixel.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes, core_float_math, default_field_values)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod compare;
mod mesh;
mod shade;

#[cfg(test)]
mod tests;

pub use compare::{PANEL_BORDER_PX, PictureDiff, compose_grid, compose_side_by_side, diff_pictures};
pub use mesh::{
    HighlightStyle, MaterialMaps, MeshHighlight, MeshNormalSource, MeshPictureOpts, MeshPicturePlant, MeshScenePart, TextureMap,
    Wireframe, render_mesh, render_mesh_scene, render_mesh_scene_planted,
};

use blaze_cad_field::{Lanes, Points, Scratch, Tape, eval_box, eval_grad, eval_point, eval_points, simplify};
use blaze_cad_out::{Image, PngError, PngKind, PngOpts};
use blaze_math::{Aabb3, Vec3};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::{Angle, Length};

use crate::shade::{RGSS_2X2, ShadeContext, resolve_samples, shade_classic_u8, shade_hit_sample};

/// The least step tolerance, metres: a nanometre, the grid lengths are kept on.
pub const LEAST_STEP: f64 = 1e-9;

/// Pixel edge length of each fine frustum-culling and tape-simplification tile.
const TILE: u32 = 16;

/// Pixel edge length of each coarse macro-tile (4×4 fine tiles).
const MACRO_TILE: u32 = 64;

/// Minimum tape length worth running `simplify` on for a tile.
const MIN_SIMPLIFY_INSTRS: usize = 16;

/// Coherent ray packet width evaluated simultaneously with `eval_points(..., Lanes::Eight)`.
const PACKET: usize = 8;

/// The renderer's planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PicturePlant {
    None,
    /// A segment the interval does not prove empty is judged by the field at its far end alone:
    /// a ray steps past a wall thinner than the segment, or stops at the segment's start.
    PointStep,
}

impl Plants for PicturePlant {}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Projection {
    /// Rays from the eye; `fov` is the vertical field of view, between 0 and half a turn.
    Perspective { fov: Angle },
    /// Parallel rays along the view; `height` is the picture's height in the model.
    Orthographic { height: Length },
}

/// Where the picture is taken from and how large it is.
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct View {
    pub eye: [Length; 3],
    /// The point at the picture's centre.
    pub target: [Length; 3],
    /// Which way is up in the picture: any direction not along the view (it is made
    /// perpendicular).
    pub up: [f64; 3] = [0.0, 0.0, 1.0],
    pub projection: Projection,
    pub width: u32,
    pub height: u32,
}

/// Rectangular studio softbox / strip light for sweeping crisp grazing highlight ribbons across
/// curved housings and revealing brushed finishes in [`Shading::Studio`].
///
/// `direction` and `tangent` are given in camera space (`+X` right, `+Y` up, `+Z` toward the
/// viewer) so moving a strip light sweeps predictably across the current inspection view.
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct StripLight {
    /// Direction toward the strip centre in camera space (`+X` right, `+Y` up, `+Z` toward viewer).
    pub direction: [f64; 3],
    /// Strip length axis in camera space (orthonormalized perpendicular to `direction`).
    pub tangent: [f64; 3],
    /// Half-length of the rectangular strip at unit distance along `tangent`.
    pub half_length: f64 = 0.55,
    /// Half-width of the rectangular strip at unit distance perpendicular to `tangent`.
    pub half_width: f64 = 0.08,
    /// Radiance / intensity multiplier.
    pub intensity: f64 = 1.1,
    /// Linear RGB colour tint of the strip light.
    pub colour: [f64; 3] = [1.0, 0.99, 0.97],
}

impl StripLight {
    /// Wide horizontal overhead strip light (classic automotive / industrial design studio sweep).
    pub const OVERHEAD_HORIZONTAL: StripLight = StripLight {
        direction: [0.0, 0.78, 0.62],
        tangent: [1.0, 0.0, 0.0],
        half_length: 0.70,
        half_width: 0.07,
        intensity: 1.15,
        colour: [1.0, 0.99, 0.97],
    };

    /// Tall vertical overhead-side strip light for revealing horizontal contour transitions.
    pub const OVERHEAD_VERTICAL: StripLight = StripLight {
        direction: [0.35, 0.65, 0.67],
        tangent: [0.0, 1.0, 0.0],
        half_length: 0.65,
        half_width: 0.07,
        intensity: 1.15,
        colour: [0.97, 0.99, 1.02],
    };

    /// Low grazing side strip light that rakes across surfaces to expose brushed finishes & ripples.
    pub const GRAZING_SIDE: StripLight = StripLight {
        direction: [0.88, 0.22, 0.42],
        tangent: [0.0, 1.0, 0.0],
        half_length: 0.60,
        half_width: 0.06,
        intensity: 1.25,
        colour: [1.02, 0.98, 0.94],
    };
}

/// Studio CAD lighting, material, AO, soft-shadow, and movable strip-light settings for
/// [`Shading::Studio`].
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct StudioOpts {
    /// Key light intensity (warm directional light from above-left of the camera).
    pub key: f64 = 0.82,
    /// Fill light intensity (cool opposite-side directional light).
    pub fill: f64 = 0.28,
    /// Rim / Fresnel edge highlight strength.
    pub rim: f64 = 0.24,
    /// Hemisphere sky/ground ambient strength.
    pub ambient: f64 = 0.26,
    /// Blinn-Phong specular strength (`0.0` disables specular).
    pub specular: f64 = 0.30,
    /// Blinn-Phong specular shininess exponent.
    pub shininess: f64 = 48.0,
    /// SDF ambient occlusion strength in `0.0..=1.0` (`0.0` disables AO).
    pub ao_strength: f64 = 0.65,
    /// Number of SDF ambient occlusion steps along the normal (`0` disables AO).
    pub ao_steps: u8 = 5,
    /// Soft shadow strength in `0.0..=1.0` (`0.0` disables shadows).
    pub shadow_strength: f64 = 0.60,
    /// Soft shadow penumbra sharpness (`k` in `k * d / t`; larger gives crisper shadows).
    pub shadow_softness: f64 = 12.0,
    /// Maximum steps per soft shadow ray (`0` disables shadows).
    pub shadow_steps: u8 = 48,
    /// Encode shaded linear light to sRGB before 8-bit quantization.
    pub srgb: bool = true,
    /// Up to four movable studio strip lights (`None` slots are inactive).
    pub strip_lights: [Option<StripLight>; 4] = [None; 4],
}

impl StudioOpts {
    /// Default balanced CAD studio lighting preset.
    pub const DEFAULT: StudioOpts = StudioOpts {
        key: 0.82,
        fill: 0.28,
        rim: 0.24,
        ambient: 0.26,
        specular: 0.30,
        shininess: 48.0,
        ao_strength: 0.65,
        ao_steps: 5,
        shadow_strength: 0.60,
        shadow_softness: 12.0,
        shadow_steps: 48,
        srgb: true,
        strip_lights: [None; 4],
    };
}

impl Default for StudioOpts {
    fn default() -> StudioOpts {
        StudioOpts::DEFAULT
    }
}

/// Configuration for [`RenderMode::Zebra`] Class-A reflection-line surface inspection.
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ZebraOpts {
    /// Number of full dark-light stripe cycles across the reflection hemisphere.
    pub stripes: f64 = 16.0,
    /// Stripe-band normal axis (camera space by default, or world space when `world_space` is true).
    pub axis: [f64; 3] = [0.0, 0.0, 1.0],
    /// Phase offset in stripe cycles (sweep `0.0..1.0` to roll stripes across a surface).
    pub phase: f64 = 0.0,
    /// When `true`, project the reflected ray onto `axis` in world coordinates instead of camera space.
    pub world_space: bool = false,
    /// Anti-aliased contrast sharpness of stripe edges (larger gives crisper band transitions).
    pub sharpness: f64 = 8.0,
    /// Dark stripe RGB colour.
    pub dark: [u8; 3] = [18, 22, 28],
    /// Light stripe RGB colour.
    pub light: [u8; 3] = [245, 247, 250],
}

impl ZebraOpts {
    /// Default Class-A CAD zebra reflection-stripe settings.
    pub const DEFAULT: ZebraOpts = ZebraOpts {
        stripes: 16.0,
        axis: [0.0, 0.0, 1.0],
        phase: 0.0,
        world_space: false,
        sharpness: 8.0,
        dark: [18, 22, 28],
        light: [245, 247, 250],
    };
}

impl Default for ZebraOpts {
    fn default() -> ZebraOpts {
        ZebraOpts::DEFAULT
    }
}

/// How lit surfaces are shaded when [`RenderMode::Shaded`] is selected.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shading {
    /// Classic single-light Lambertian shading (`0.22 + 0.78 * max(0, n·l)`).
    Classic,
    /// Three-point CAD studio lighting with Blinn-Phong specular, hemisphere fill, Fresnel rim,
    /// SDF ambient occlusion, soft SDF shadows, and optional movable strip lights.
    Studio(StudioOpts),
}

/// What each hit pixel visualizes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RenderMode {
    /// Lit surface shaded according to [`PictureOpts::shading`].
    Shaded,
    /// World-space unit normal mapped from `[-1, 1]` to `[0, 255]`.
    Normals,
    /// Linear camera-forward depth normalized across the tape's bounding box (bright near, dark far).
    Depth,
    /// Analytical studio ceramic/metallic CAD matcap derived from the camera-space normal.
    Matcap,
    /// Class-A reflection-line zebra stripes revealing dents, creases, and C1/C2 curvature breaks.
    Zebra(ZebraOpts),
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct PictureOpts<'a, 's> {
    /// Where the picture's pixels and depths live.
    pub alloc: Alloc<'a>,
    /// Where the evaluators' registers and tile-simplified tapes live.
    pub scratch: Alloc<'s>,
    /// The step tolerance as a fraction of a pixel's footprint at the depth reached.
    pub pixel_fraction: f64 = 0.5,
    /// The most steps one ray may take.
    pub most_steps: u32 = 4096,
    pub most_pixels: u64 = 1 << 26,
    /// What a ray that hits nothing shows, RGBA.
    pub background: [u8; 4] = [246, 246, 244, 255],
    /// The solid's colour before shading, RGB.
    pub colour: [u8; 3] = [168, 178, 192],
    /// Surface shading model used in [`RenderMode::Shaded`].
    pub shading: Shading = Shading::Classic,
    /// Visual output mode.
    pub mode: RenderMode = RenderMode::Shaded,
    /// Deterministic supersampling factor per axis: `1` (pixel centre) or `2` (2×2 RGSS).
    pub supersample: u8 = 1,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PictureError {
    /// A width or height of zero.
    Empty { width: u32, height: u32 },
    TooManyPixels { pixels: u64, most: u64 },
    /// The eye is at the target: there is no view direction.
    EyeAtTarget,
    /// `up` is along the view direction (or zero).
    UpAlongView { up: [f64; 3] },
    BadFov { fov: Angle },
    BadHeight { height: Length },
    /// A pixel fraction that is not positive and finite.
    BadFraction { fraction: f64 },
    /// A supersampling factor other than `1` or `2`.
    BadSupersample { supersample: u8 },
    /// The tape's box is not finite: rays cannot be clipped to it.
    Unbounded,
    /// The ray of pixel (`x`, `y`) took `most` steps without settling.
    TooManySteps { x: u32, y: u32, most: u32 },
    /// Two pictures passed to [`diff_pictures`] have different dimensions.
    DimensionMismatch { left: (u32, u32), right: (u32, u32) },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for PictureError {
    fn from(e: AllocFailure) -> PictureError {
        PictureError::Alloc(e)
    }
}

/// What a render did, in work units.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PictureStats {
    pub rays: u64,
    /// Rays that hit the solid.
    pub hits: u64,
    /// `eval_box` calls: tiles and ray segments looked at.
    pub boxes: u64,
    pub points: u64,
    pub grads: u64,
    /// The most steps one ray took.
    pub most_steps: u32,
    /// Fine tiles skipped without tracing individual pixels.
    pub tiles_culled: u64,
    /// Tiles whose tape was simplified before tracing.
    pub tiles_simplified: u64,
}

/// A rendered picture.
#[derive(Debug)]
pub struct Picture<'a> {
    pub width: u32,
    pub height: u32,
    /// RGBA8, rows from the top, each left to right.
    pub pixels: Vec<u8, Alloc<'a>>,
    /// Each pixel's hit, metres along its ray ([`Camera::ray`]); `+∞` where it hits nothing.
    pub depth: Vec<f64, Alloc<'a>>,
    pub stats: PictureStats,
}

impl Picture<'_> {
    /// The pixels as blaze-cad-out's image.
    pub fn image(&self) -> Image<'_> {
        Image { width: self.width, height: self.height, kind: PngKind::Rgba8, pixels: &self.pixels }
    }

    /// The picture as a PNG file's bytes.
    pub fn png<'b>(&self, o: PngOpts<'b, '_>) -> Result<Vec<u8, Alloc<'b>>, PngError> {
        blaze_cad_out::png(self.image(), o)
    }
}

/// A view made ready for rays: an orthonormal frame and the projection's scale.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Camera {
    pub(crate) eye: Vec3,
    pub(crate) forward: Vec3,
    pub(crate) right: Vec3,
    pub(crate) up: Vec3,
    /// Half the picture's height: at unit distance (perspective) or in metres (orthographic).
    pub(crate) half: f64,
    pub(crate) perspective: bool,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

fn metres(p: [Length; 3]) -> Vec3 {
    Vec3::new(p[0].as_m(), p[1].as_m(), p[2].as_m())
}

impl Camera {
    pub fn new(v: &View) -> Result<Camera, PictureError> {
        if v.width == 0 || v.height == 0 {
            return Err(PictureError::Empty { width: v.width, height: v.height });
        }
        let eye = metres(v.eye);
        let forward = (metres(v.target) - eye).normalized().ok_or(PictureError::EyeAtTarget)?;
        let hint = Vec3::new(v.up[0], v.up[1], v.up[2]);
        let right = forward.cross(hint).normalized().ok_or(PictureError::UpAlongView { up: v.up })?;
        let up = right.cross(forward);
        let (half, perspective) = match v.projection {
            Projection::Perspective { fov } => {
                let r = fov.as_radians();
                if !(r > 0.0 && r < core::f64::consts::PI) {
                    return Err(PictureError::BadFov { fov });
                }
                (blaze_math::tan(r / 2.0), true)
            }
            Projection::Orthographic { height } => {
                if height <= Length::ZERO {
                    return Err(PictureError::BadHeight { height });
                }
                (height.as_m() / 2.0, false)
            }
        };
        Ok(Camera { eye, forward, right, up, half, perspective, width: v.width, height: v.height })
    }

    /// The ray through pixel coordinates `(fx, fy)` (`x + 0.5, y + 0.5` is the centre of `(x, y)`):
    /// its origin and unit direction.
    #[inline]
    pub fn ray_at(&self, fx: f64, fy: f64) -> (Vec3, Vec3) {
        let (w, h) = (f64::from(self.width), f64::from(self.height));
        let sx = (fx / w * 2.0 - 1.0) * (w / h) * self.half;
        let sy = (1.0 - fy / h * 2.0) * self.half;
        if self.perspective {
            let d = self.forward + self.right * sx + self.up * sy;
            (self.eye, d * (1.0 / d.length()))
        } else {
            (self.eye + self.right * sx + self.up * sy, self.forward)
        }
    }

    /// The ray through the centre of pixel (`x`, `y`): its origin and unit direction.
    pub fn ray(&self, x: u32, y: u32) -> (Vec3, Vec3) {
        self.ray_at(f64::from(x) + 0.5, f64::from(y) + 0.5)
    }

    /// The step tolerance `fraction` of a pixel's footprint at `t` metres along a ray, at least
    /// [`LEAST_STEP`].
    pub fn tolerance_at(&self, t: f64, fraction: f64) -> f64 {
        let per = 2.0 * self.half / f64::from(self.height);
        let footprint = if self.perspective { per * t.max(0.0) } else { per };
        (fraction * footprint).max(LEAST_STEP)
    }
}

/// Where the ray `o + t d`, `t ≥ 0`, is inside box `b`: `(t0, t1)`, or `None`.
pub(crate) fn slab(b: &Aabb3, o: Vec3, d: Vec3) -> Option<(f64, f64)> {
    let (lo, hi, oo, dd) = (b.lo.to_array(), b.hi.to_array(), o.to_array(), d.to_array());
    let (mut t0, mut t1) = (0.0f64, f64::INFINITY);
    for k in 0..3 {
        if dd[k] == 0.0 {
            if oo[k] < lo[k] || oo[k] > hi[k] {
                return None;
            }
            continue;
        }
        let inv = 1.0 / dd[k];
        let (a, z) = ((lo[k] - oo[k]) * inv, (hi[k] - oo[k]) * inv);
        let (a, z) = if a <= z { (a, z) } else { (z, a) };
        t0 = t0.max(a);
        t1 = t1.min(z);
    }
    if t0 <= t1 { Some((t0, t1)) } else { None }
}

/// The box of the ray's points from `a` to `b`. Each coordinate `o + t d` is computed with
/// correctly rounded operations, which are monotone in `t`, so every point of the segment as
/// computed anywhere (the tests' samples too) lies in this box.
#[inline]
fn segment(o: Vec3, d: Vec3, a: f64, b: f64) -> Aabb3 {
    let (p, q) = (o + d * a, o + d * b);
    Aabb3::new(p.min(q), p.max(q))
}

/// A conservative outward-padded copy of `b` that absorbs floating-point rounding of `slab` and
/// `o + d * t` at box faces.
fn pad_bounds(b: &Aabb3, eye: Vec3) -> Aabb3 {
    let mut scale = 1.0f64;
    for v in b.lo.to_array().iter().chain(b.hi.to_array().iter()).chain(eye.to_array().iter()) {
        scale = scale.max(v.abs());
    }
    let pad = 1e-7 * scale;
    let delta = Vec3::new(pad, pad, pad);
    Aabb3::new(b.lo - delta, b.hi + delta)
}

/// Intersect two boxes, returning `None` if disjoint.
#[inline]
fn intersect_aabb(a: &Aabb3, b: &Aabb3) -> Option<Aabb3> {
    let lo = a.lo.max(b.lo);
    let hi = a.hi.min(b.hi);
    if lo.x <= hi.x && lo.y <= hi.y && lo.z <= hi.z {
        Some(Aabb3::new(lo, hi))
    } else {
        None
    }
}

/// Conservative 3D AABB containing every point `o + d * t` inside `bounds_pad` for every ray
/// passing through pixel region `[x0, x1] × [y0, y1]` (covering all subpixel offsets in `[0, 1]`).
/// Returns `None` if no ray in the tile can intersect `bounds_pad`.
fn tile_frustum_aabb(cam: &Camera, x0: u32, y0: u32, x1: u32, y1: u32, bounds_pad: &Aabb3) -> Option<Aabb3> {
    let (w, h) = (f64::from(cam.width), f64::from(cam.height));
    let sx_min = (f64::from(x0) / w * 2.0 - 1.0) * (w / h) * cam.half;
    let sx_max = (f64::from(x1) / w * 2.0 - 1.0) * (w / h) * cam.half;
    let sy_max = (1.0 - f64::from(y0) / h * 2.0) * cam.half;
    let sy_min = (1.0 - f64::from(y1) / h * 2.0) * cam.half;

    let blo = bounds_pad.lo.to_array();
    let bhi = bounds_pad.hi.to_array();

    if cam.perspective {
        let vs = [
            cam.forward + cam.right * sx_min + cam.up * sy_min,
            cam.forward + cam.right * sx_max + cam.up * sy_min,
            cam.forward + cam.right * sx_min + cam.up * sy_max,
            cam.forward + cam.right * sx_max + cam.up * sy_max,
        ];
        let v_lo = vs[0].min(vs[1]).min(vs[2]).min(vs[3]).to_array();
        let v_hi = vs[0].max(vs[1]).max(vs[2]).max(vs[3]).to_array();

        // Depth range along `cam.forward` across all 8 corners of `bounds_pad`.
        let mut z_min = f64::INFINITY;
        let mut z_max = f64::NEG_INFINITY;
        for ix in 0..2 {
            let x = if ix == 0 { blo[0] } else { bhi[0] };
            for iy in 0..2 {
                let y = if iy == 0 { blo[1] } else { bhi[1] };
                for iz in 0..2 {
                    let z = if iz == 0 { blo[2] } else { bhi[2] };
                    let proj = (Vec3::new(x, y, z) - cam.eye).dot(cam.forward);
                    z_min = z_min.min(proj);
                    z_max = z_max.max(proj);
                }
            }
        }
        z_min = z_min.max(0.0);
        if z_min > z_max {
            return None;
        }

        // Tighten `[z_min, z_max]` with per-axis interval slab bounds on `eye + v * z ∈ bounds_pad`.
        let eye = cam.eye.to_array();
        for k in 0..3 {
            let dx_lo = blo[k] - eye[k];
            let dx_hi = bhi[k] - eye[k];
            if v_lo[k] > 0.0 {
                if dx_hi < 0.0 {
                    return None;
                }
                if dx_lo > 0.0 {
                    z_min = z_min.max(dx_lo / v_hi[k]);
                }
                z_max = z_max.min(dx_hi / v_lo[k]);
            } else if v_hi[k] < 0.0 {
                if dx_lo > 0.0 {
                    return None;
                }
                if dx_hi < 0.0 {
                    z_min = z_min.max(dx_hi / v_lo[k]);
                }
                z_max = z_max.min(dx_lo / v_hi[k]);
            } else {
                if dx_lo > 0.0 {
                    if v_hi[k] <= 0.0 {
                        return None;
                    }
                    z_min = z_min.max(dx_lo / v_hi[k]);
                }
                if dx_hi < 0.0 {
                    if v_lo[k] >= 0.0 {
                        return None;
                    }
                    z_min = z_min.max(dx_hi / v_lo[k]);
                }
            }
        }
        let z_slack = 64.0 * f64::EPSILON * (z_max.abs() + 1.0);
        if !z_max.is_finite() || z_min - z_slack > z_max + z_slack {
            return None;
        }
        let z0 = (z_min - z_slack).max(0.0);
        let z1 = z_max + z_slack;

        let mut f_lo = Vec3::new(f64::INFINITY, f64::INFINITY, f64::INFINITY);
        let mut f_hi = Vec3::new(f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for v in vs {
            let p0 = cam.eye + v * z0;
            let p1 = cam.eye + v * z1;
            f_lo = f_lo.min(p0).min(p1);
            f_hi = f_hi.max(p0).max(p1);
        }
        intersect_aabb(&Aabb3::new(f_lo, f_hi), bounds_pad)
    } else {
        let os = [
            cam.eye + cam.right * sx_min + cam.up * sy_min,
            cam.eye + cam.right * sx_max + cam.up * sy_min,
            cam.eye + cam.right * sx_min + cam.up * sy_max,
            cam.eye + cam.right * sx_max + cam.up * sy_max,
        ];
        let o_lo = os[0].min(os[1]).min(os[2]).min(os[3]).to_array();
        let o_hi = os[0].max(os[1]).max(os[2]).max(os[3]).to_array();
        let d = cam.forward.to_array();

        let (mut t_min, mut t_max) = (0.0f64, f64::INFINITY);
        for k in 0..3 {
            if d[k] == 0.0 {
                if o_hi[k] < blo[k] || o_lo[k] > bhi[k] {
                    return None;
                }
            } else if d[k] > 0.0 {
                t_min = t_min.max((blo[k] - o_hi[k]) / d[k]);
                t_max = t_max.min((bhi[k] - o_lo[k]) / d[k]);
            } else {
                t_min = t_min.max((bhi[k] - o_lo[k]) / d[k]);
                t_max = t_max.min((blo[k] - o_hi[k]) / d[k]);
            }
        }
        let t_slack = 64.0 * f64::EPSILON * (t_max.abs() + 1.0);
        if !t_max.is_finite() || t_min - t_slack > t_max + t_slack {
            return None;
        }
        let ta = (t_min - t_slack).max(0.0);
        let tb = t_max + t_slack;

        let mut f_lo = Vec3::new(f64::INFINITY, f64::INFINITY, f64::INFINITY);
        let mut f_hi = Vec3::new(f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for o in os {
            let p0 = o + cam.forward * ta;
            let p1 = o + cam.forward * tb;
            f_lo = f_lo.min(p0).min(p1);
            f_hi = f_hi.max(p0).max(p1);
        }
        intersect_aabb(&Aabb3::new(f_lo, f_hi), bounds_pad)
    }
}

/// Safe distance-bound advance along a unit ray from `t` given a positive field value `dist`:
/// because every `blaze-cad-field` level guarantees `|f(p)| <= dist(p, surface)`, the open ball
/// of radius `dist` around `o + d * t` is strictly empty of any surface (even an arbitrarily thin
/// wall). The ULP and `1e-6` guards keep the guarantee sound under IEEE 754 rounding.
#[inline]
fn safe_step(dist: f64, o_mag: f64, t: f64) -> f64 {
    if !(dist > 0.0 && dist.is_finite()) {
        return 0.0;
    }
    let slack = 64.0 * f64::EPSILON * (o_mag + t.abs() + dist);
    ((dist - slack) * (1.0 - 1e-6)).max(0.0)
}

enum Refine {
    Hit(f64),
    Advance(f64),
}

/// Thin-wall-safe sub-pixel hit refinement inside `[at, end]`: advances `lo` ONLY across
/// intervals proven strictly outside (`safe_step(f(lo))` or `eval_box([lo, mid]).lo() > 0`), so
/// no thin wall inside `[at, end]` can ever be skipped while converging accurately to the first
/// surface crossing before `eval_grad`.
#[allow(clippy::too_many_arguments)]
fn refine_hit(
    t: &Tape<'_>,
    o: Vec3,
    d: Vec3,
    at: f64,
    end: f64,
    eps: f64,
    o_mag: f64,
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
) -> Refine {
    let mut lo = at;
    let mut hi = end;
    let target_f = (eps * 1e-3).max(LEAST_STEP);
    let target_w = (eps * 1e-2).max(LEAST_STEP);

    for _ in 0..6 {
        stats.points += 1;
        let fa = eval_point(t, o + d * lo, sc);
        if !fa.is_finite() || fa <= target_f {
            return Refine::Hit(lo);
        }
        let safe = safe_step(fa, o_mag, lo);
        if safe > 0.0 {
            let next = lo + safe;
            if next >= hi {
                if hi == end {
                    // Proved the entire hit segment was empty (grazing interval overestimation).
                    if fa > 0.25 * eps {
                        return Refine::Advance(next);
                    }
                    return Refine::Hit(lo);
                }
                // `[lo, hi]` was actually empty; resume within `[next, end]`.
                if next >= end {
                    if fa > 0.25 * eps {
                        return Refine::Advance(next);
                    }
                    return Refine::Hit(lo);
                }
                lo = next;
                hi = end;
                continue;
            }
            if safe >= (hi - lo) * 0.25 {
                lo = next;
                continue;
            }
        }
        if hi - lo <= target_w {
            return Refine::Hit(lo);
        }
        let mid = 0.5 * (lo + hi);
        if !(mid > lo && mid < hi) {
            return Refine::Hit(lo);
        }
        stats.boxes += 1;
        let (iv, _) = eval_box(t, &segment(o, d, lo, mid), sc);
        if iv.lo() > 0.0 {
            let s2 = safe_step(iv.lo(), o_mag, mid);
            let next = mid + s2;
            if next >= hi {
                if hi == end && iv.lo() > 0.25 * eps {
                    return Refine::Advance(next);
                }
                return Refine::Hit(mid);
            }
            lo = next;
        } else {
            hi = mid;
        }
    }
    Refine::Hit(lo)
}

/// Scalar continuation of ray tracing from `(at, t1)` with `steps` already consumed.
#[allow(clippy::too_many_arguments)]
fn trace_from(
    t: &Tape<'_>,
    cam: &Camera,
    (o, d): (Vec3, Vec3),
    mut at: f64,
    t1: f64,
    mut len: f64,
    mut steps: u32,
    mut try_sphere: bool,
    mut sphere_run: u32,
    mut prev_d: f64,
    fraction: f64,
    most_steps: u32,
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
    point_step: bool,
) -> Result<Option<f64>, u32> {
    let o_mag = o.x.abs() + o.y.abs() + o.z.abs() + 1.0;

    let out = loop {
        if !(at < t1) {
            break None;
        }
        steps += 1;
        if steps > most_steps {
            return Err(steps - 1);
        }

        if try_sphere {
            stats.points += 1;
            let d_at = eval_point(t, o + d * at, sc);
            let eps = cam.tolerance_at(at, fraction);
            let surface_tol = (eps * 1e-4).max(LEAST_STEP);
            if !d_at.is_finite() {
                try_sphere = false;
                len = ((t1 - at) / 8.0).max(LEAST_STEP);
                continue;
            }
            if d_at <= surface_tol {
                break Some(at);
            }
            let safe = safe_step(d_at, o_mag, at);
            let next = at + safe;
            let rapid = d_at < prev_d * 0.5;
            if safe > 0.0 && next > at && (d_at > eps || rapid) && sphere_run < 24 {
                at = next;
                prev_d = d_at;
                sphere_run += 1;
                continue;
            }
            // Grazing or slow-converging `Bound` field: switch to interval segment marching.
            try_sphere = false;
            sphere_run = 0;
            prev_d = f64::INFINITY;
            len = d_at.max(eps).max(LEAST_STEP);
            continue;
        }

        let end = (at + len).min(t1);
        if !(end > at) {
            // No float between: the segment is a point the walk cannot pass.
            break Some(at);
        }
        stats.boxes += 1;
        let (iv, _) = eval_box(t, &segment(o, d, at, end), sc);
        if iv.lo() > 0.0 {
            if point_step {
                at = end;
                len *= 2.0;
            } else {
                let safe = safe_step(iv.lo(), o_mag, end);
                at = end + safe;
                len = (len * 2.0).max(safe).max(LEAST_STEP);
                let eps = cam.tolerance_at(at, fraction);
                if safe > 4.0 * eps {
                    try_sphere = true;
                    sphere_run = 0;
                    prev_d = f64::INFINITY;
                }
            }
            continue;
        }
        let eps = cam.tolerance_at(end, fraction);
        if point_step {
            stats.points += 1;
            if eval_point(t, o + d * end, sc) < 0.0 || end - at <= eps {
                break Some(at);
            }
            at = end;
            continue;
        }
        if end - at <= eps {
            match refine_hit(t, o, d, at, end, eps, o_mag, sc, stats) {
                Refine::Hit(hit) => break Some(hit),
                Refine::Advance(next_at) => {
                    at = next_at;
                    len = (len * 2.0).max(eps).max(LEAST_STEP);
                    continue;
                }
            }
        }
        len = (end - at) / 2.0;
    };
    stats.most_steps = stats.most_steps.max(steps);
    Ok(out)
}

/// One ray queued in a coherent packet.
#[derive(Clone, Copy)]
struct RayJob {
    origin: Vec3,
    dir: Vec3,
    t0: f64,
    t1: f64,
    x: u32,
    y: u32,
    /// Target slot in the caller's batch (`0..PACKET`).
    slot: usize,
}

/// Trace up to [`PACKET`] coherent rays together using `eval_points` (`Lanes::Eight` / `Lanes::Four`)
/// for bit-exact SIMD distance-bound sphere tracing, handing off grazing or `Bound` rays to
/// interval-segment marching in [`trace_from`].
#[allow(clippy::too_many_arguments)]
fn trace_packet(
    t: &Tape<'_>,
    cam: &Camera,
    jobs: &[RayJob],
    fraction: f64,
    most_steps: u32,
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
    point_step: bool,
    hits: &mut [Option<f64>; PACKET],
) -> Result<(), PictureError> {
    let n = jobs.len();
    if n == 0 {
        return Ok(());
    }
    if point_step || n == 1 {
        for job in jobs {
            let len = ((job.t1 - job.t0) / 8.0).max(LEAST_STEP);
            hits[job.slot] = trace_from(
                t,
                cam,
                (job.origin, job.dir),
                job.t0,
                job.t1,
                len,
                0,
                !point_step,
                0,
                f64::INFINITY,
                fraction,
                most_steps,
                sc,
                stats,
                point_step,
            )
            .map_err(|most| PictureError::TooManySteps { x: job.x, y: job.y, most })?;
        }
        return Ok(());
    }

    let mut at = [0.0f64; PACKET];
    let mut o_mag = [0.0f64; PACKET];
    let mut prev_d = [f64::INFINITY; PACKET];
    let mut init_len = [LEAST_STEP; PACKET];
    let mut steps = [0u32; PACKET];
    // 0 = active sphere tracing, 1 = hand off to interval mode, 2 = finished
    let mut state = [0u8; PACKET];

    for i in 0..n {
        at[i] = jobs[i].t0;
        let o = jobs[i].origin;
        o_mag[i] = o.x.abs() + o.y.abs() + o.z.abs() + 1.0;
        init_len[i] = ((jobs[i].t1 - jobs[i].t0) / 8.0).max(LEAST_STEP);
        if !(at[i] < jobs[i].t1) {
            state[i] = 2;
            hits[jobs[i].slot] = None;
        }
    }

    let mut xs = [0.0f64; PACKET];
    let mut ys = [0.0f64; PACKET];
    let mut zs = [0.0f64; PACKET];
    let mut dists = [0.0f64; PACKET];
    let mut active_idx = [0usize; PACKET];

    let mut rounds = 0u32;
    while rounds < 24 {
        let mut active_n = 0usize;
        for i in 0..n {
            if state[i] == 0 {
                if steps[i] >= most_steps {
                    return Err(PictureError::TooManySteps { x: jobs[i].x, y: jobs[i].y, most: steps[i] });
                }
                active_idx[active_n] = i;
                active_n += 1;
            }
        }
        if active_n < 2 {
            break;
        }

        let (lanes, eval_n) = if active_n > 4 { (Lanes::Eight, 8) } else { (Lanes::Four, 4) };
        for j in 0..active_n {
            let i = active_idx[j];
            let p = jobs[i].origin + jobs[i].dir * at[i];
            xs[j] = p.x;
            ys[j] = p.y;
            zs[j] = p.z;
        }
        for j in active_n..eval_n {
            xs[j] = xs[0];
            ys[j] = ys[0];
            zs[j] = zs[0];
        }

        stats.points += active_n as u64;
        let _ = eval_points(
            t,
            Points { x: &xs[..eval_n], y: &ys[..eval_n], z: &zs[..eval_n] },
            &mut dists[..eval_n],
            lanes,
            sc,
        );
        rounds += 1;

        for j in 0..active_n {
            let i = active_idx[j];
            steps[i] += 1;
            let d_at = dists[j];
            let eps = cam.tolerance_at(at[i], fraction);
            let surface_tol = (eps * 1e-4).max(LEAST_STEP);
            if !d_at.is_finite() {
                state[i] = 1;
                init_len[i] = ((jobs[i].t1 - at[i]) / 8.0).max(LEAST_STEP);
            } else if d_at <= surface_tol {
                hits[jobs[i].slot] = Some(at[i]);
                state[i] = 2;
            } else {
                let safe = safe_step(d_at, o_mag[i], at[i]);
                let next = at[i] + safe;
                let rapid = d_at < prev_d[i] * 0.5;
                if safe > 0.0 && next > at[i] && (d_at > eps || rapid) {
                    if !(next < jobs[i].t1) {
                        hits[jobs[i].slot] = None;
                        state[i] = 2;
                    } else {
                        at[i] = next;
                        prev_d[i] = d_at;
                    }
                } else {
                    state[i] = 1;
                    init_len[i] = d_at.max(eps).max(LEAST_STEP);
                }
            }
        }
    }

    for i in 0..n {
        if state[i] == 2 {
            stats.most_steps = stats.most_steps.max(steps[i]);
            continue;
        }
        let job = jobs[i];
        let keep_sphere = state[i] == 0 && rounds < 24;
        let start_len = if keep_sphere {
            init_len[i]
        } else if state[i] == 0 {
            let eps = cam.tolerance_at(at[i], fraction);
            prev_d[i].min(job.t1 - at[i]).max(eps).max(LEAST_STEP)
        } else {
            init_len[i]
        };
        hits[job.slot] = trace_from(
            t,
            cam,
            (job.origin, job.dir),
            at[i],
            job.t1,
            start_len,
            steps[i],
            keep_sphere,
            if keep_sphere { rounds } else { 0 },
            if keep_sphere { prev_d[i] } else { f64::INFINITY },
            fraction,
            most_steps,
            sc,
            stats,
            false,
        )
        .map_err(|most| PictureError::TooManySteps { x: job.x, y: job.y, most })?;
    }
    Ok(())
}

/// Shade a hit (or miss) for a single-sample pixel.
#[allow(clippy::too_many_arguments)]
fn shade_single_pixel(
    full_tape: &Tape<'_>,
    trace_tape: &Tape<'_>,
    cam: &Camera,
    ctx: &ShadeContext,
    o: &PictureOpts<'_, '_>,
    origin: Vec3,
    dir: Vec3,
    hit: Option<f64>,
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
) -> ([u8; 4], f64) {
    let Some(at) = hit else {
        return (o.background, f64::INFINITY);
    };
    stats.hits += 1;
    let p = origin + dir * at;
    let px = match (o.mode, o.shading) {
        (RenderMode::Shaded, Shading::Classic) => {
            stats.grads += 1;
            let (_, g) = eval_grad(trace_tape, p, sc);
            let n = g.normalized().unwrap_or(dir * -1.0);
            shade_classic_u8(ctx, o.colour, n)
        }
        (RenderMode::Depth, _) => {
            let sample = shade_hit_sample(full_tape, cam, ctx, o, p, dir, Vec3::new(0.0, 0.0, 1.0), 0.0, sc, stats);
            resolve_samples(sample, 1.0, false)
        }
        _ => {
            stats.grads += 1;
            let (_, g) = eval_grad(trace_tape, p, sc);
            let n = g.normalized().unwrap_or(dir * -1.0);
            let eps = cam.tolerance_at(at, o.pixel_fraction);
            let sample = shade_hit_sample(full_tape, cam, ctx, o, p, dir, n, eps, sc, stats);
            resolve_samples(sample, 1.0, ctx.linear_blend)
        }
    };
    (px, at)
}

/// Trace all pixels inside a tile `[x0, x1) × [y0, y1)` using 8-wide coherent ray packets
/// against `tile_tape`, and `full_tape` for secondary shading queries (AO, soft shadows).
#[allow(clippy::too_many_arguments)]
fn render_tile_pixels(
    full_tape: &Tape<'_>,
    tile_tape: &Tape<'_>,
    bounds: &Aabb3,
    cam: &Camera,
    ctx: &ShadeContext,
    o: &PictureOpts<'_, '_>,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    rgba: &mut [u8],
    depth: &mut [f64],
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
    point_step: bool,
) -> Result<(), PictureError> {
    let w = cam.width as usize;
    let dummy_job = RayJob {
        origin: Vec3::new(0.0, 0.0, 0.0),
        dir: Vec3::new(0.0, 0.0, 1.0),
        t0: 0.0,
        t1: 0.0,
        x: 0,
        y: 0,
        slot: 0,
    };

    if o.supersample == 1 {
        let mut origins = [Vec3::new(0.0, 0.0, 0.0); PACKET];
        let mut dirs = [Vec3::new(0.0, 0.0, 1.0); PACKET];
        let mut jobs = [dummy_job; PACKET];
        for y in y0..y1 {
            let row = y as usize * w;
            let mut x = x0;
            while x < x1 {
                let chunk = ((x1 - x) as usize).min(PACKET);
                let mut job_n = 0usize;
                let mut hits = [None; PACKET];
                for k in 0..chunk {
                    let px_x = x + k as u32;
                    stats.rays += 1;
                    let (origin, dir) = cam.ray(px_x, y);
                    origins[k] = origin;
                    dirs[k] = dir;
                    if let Some((t0, t1)) = slab(bounds, origin, dir) {
                        jobs[job_n] = RayJob { origin, dir, t0, t1, x: px_x, y, slot: k };
                        job_n += 1;
                    }
                }
                if job_n > 0 {
                    trace_packet(tile_tape, cam, &jobs[..job_n], o.pixel_fraction, o.most_steps, sc, stats, point_step, &mut hits)?;
                }
                for k in 0..chunk {
                    let (px, d) = shade_single_pixel(full_tape, tile_tape, cam, ctx, o, origins[k], dirs[k], hits[k], sc, stats);
                    let idx = row + (x as usize + k);
                    depth[idx] = d;
                    let off = idx * 4;
                    rgba[off] = px[0];
                    rgba[off + 1] = px[1];
                    rgba[off + 2] = px[2];
                    rgba[off + 3] = px[3];
                }
                x += chunk as u32;
            }
        }
        return Ok(());
    }

    // 2×2 deterministic RGSS: batch 2 pixels (8 subpixel rays) per `Lanes::Eight` packet!
    let miss = ctx.miss_sample();
    let mut origins = [Vec3::new(0.0, 0.0, 0.0); PACKET];
    let mut dirs = [Vec3::new(0.0, 0.0, 1.0); PACKET];
    let mut jobs = [dummy_job; PACKET];
    for y in y0..y1 {
        let row = y as usize * w;
        let mut x = x0;
        while x < x1 {
            let px_chunk = ((x1 - x) as usize).min(2);
            let mut job_n = 0usize;
            let mut hits = [None; PACKET];
            for p_idx in 0..px_chunk {
                let px_x = x + p_idx as u32;
                for (s_idx, (ox, oy)) in RGSS_2X2.iter().enumerate() {
                    let slot = p_idx * 4 + s_idx;
                    stats.rays += 1;
                    let (origin, dir) = cam.ray_at(f64::from(px_x) + *ox, f64::from(y) + *oy);
                    origins[slot] = origin;
                    dirs[slot] = dir;
                    if let Some((t0, t1)) = slab(bounds, origin, dir) {
                        jobs[job_n] = RayJob { origin, dir, t0, t1, x: px_x, y, slot };
                        job_n += 1;
                    }
                }
            }
            if job_n > 0 {
                trace_packet(tile_tape, cam, &jobs[..job_n], o.pixel_fraction, o.most_steps, sc, stats, point_step, &mut hits)?;
            }
            for p_idx in 0..px_chunk {
                let mut sum = [0.0f64; 4];
                let mut min_depth = f64::INFINITY;
                for s_idx in 0..4 {
                    let slot = p_idx * 4 + s_idx;
                    let sample = match hits[slot] {
                        None => miss,
                        Some(at) => {
                            stats.hits += 1;
                            min_depth = min_depth.min(at);
                            let origin = origins[slot];
                            let dir = dirs[slot];
                            let p = origin + dir * at;
                            let n = if o.mode == RenderMode::Depth {
                                Vec3::new(0.0, 0.0, 1.0)
                            } else {
                                stats.grads += 1;
                                let (_, g) = eval_grad(tile_tape, p, sc);
                                g.normalized().unwrap_or(dir * -1.0)
                            };
                            let eps = cam.tolerance_at(at, o.pixel_fraction);
                            shade_hit_sample(full_tape, cam, ctx, o, p, dir, n, eps, sc, stats)
                        }
                    };
                    for c in 0..4 {
                        sum[c] += sample[c];
                    }
                }
                let px = resolve_samples(sum, 0.25, ctx.linear_blend);
                let idx = row + (x as usize + p_idx);
                depth[idx] = min_depth;
                let off = idx * 4;
                rgba[off] = px[0];
                rgba[off + 1] = px[1];
                rgba[off + 2] = px[2];
                rgba[off + 3] = px[3];
            }
            x += px_chunk as u32;
        }
    }
    Ok(())
}

/// Render a single fine tile `[x0, x1) × [y0, y1)`, culling via frustum AABB + `eval_box` and
/// simplifying `parent_tape` over the tile box when choices are decided.
#[allow(clippy::too_many_arguments)]
fn render_fine_tile(
    full_tape: &Tape<'_>,
    parent_tape: &Tape<'_>,
    parent_box: &Aabb3,
    bounds: &Aabb3,
    bounds_pad: &Aabb3,
    cam: &Camera,
    ctx: &ShadeContext,
    o: &PictureOpts<'_, '_>,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    rgba: &mut [u8],
    depth: &mut [f64],
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
    point_step: bool,
) -> Result<(), PictureError> {
    let tile_pixels = u64::from(x1 - x0) * u64::from(y1 - y0);
    let spp = u64::from(o.supersample) * u64::from(o.supersample);
    let Some(raw_box) = tile_frustum_aabb(cam, x0, y0, x1, y1, bounds_pad) else {
        stats.tiles_culled += 1;
        stats.rays += tile_pixels * spp;
        return Ok(());
    };
    let Some(tile_box) = intersect_aabb(&raw_box, parent_box) else {
        stats.tiles_culled += 1;
        stats.rays += tile_pixels * spp;
        return Ok(());
    };

    stats.boxes += 1;
    let (iv, choices) = eval_box(parent_tape, &tile_box, sc);
    if iv.lo() > 0.0 {
        stats.tiles_culled += 1;
        stats.rays += tile_pixels * spp;
        return Ok(());
    }

    let simplified = if parent_tape.sites() > 0 && parent_tape.len() >= MIN_SIMPLIFY_INSTRS && tile_pixels >= 4 && choices.decided() > 0 {
        match simplify(parent_tape, choices, o.scratch) {
            Ok(small) => {
                stats.tiles_simplified += 1;
                Some(small)
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let active_tape = simplified.as_ref().unwrap_or(parent_tape);

    render_tile_pixels(full_tape, active_tape, bounds, cam, ctx, o, x0, y0, x1, y1, rgba, depth, sc, stats, point_step)
}

/// Core implementation shared by tiled [`render_planted`] and test-only untiled reference.
fn render_core<'a>(
    t: &Tape<'_>,
    view: View,
    o: PictureOpts<'a, '_>,
    point_step: bool,
    tiled: bool,
) -> Result<Picture<'a>, PictureError> {
    let cam = Camera::new(&view)?;
    let pixels = u64::from(view.width) * u64::from(view.height);
    if pixels > o.most_pixels {
        return Err(PictureError::TooManyPixels { pixels, most: o.most_pixels });
    }
    if !(o.pixel_fraction > 0.0 && o.pixel_fraction.is_finite()) {
        return Err(PictureError::BadFraction { fraction: o.pixel_fraction });
    }
    if o.supersample != 1 && o.supersample != 2 {
        return Err(PictureError::BadSupersample { supersample: o.supersample });
    }
    let b = t.aabb();
    let finite = b.lo.to_array().iter().chain(b.hi.to_array().iter()).all(|x| x.is_finite());
    if !b.is_empty() && !finite {
        return Err(PictureError::Unbounded);
    }

    // Below `most_pixels`, itself below 2^64; a picture that fits in memory fits in `usize`.
    let n = pixels as usize;
    let mut rgba: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(n * 4, o.alloc)?;
    let mut depth: Vec<f64, Alloc<'a>> = Vec::try_with_capacity_in(n, o.alloc)?;
    for _ in 0..n {
        depth.try_push(f64::INFINITY).map_err(|r| r.why)?;
        for c in o.background {
            rgba.try_push(c).map_err(|r| r.why)?;
        }
    }

    let mut stats = PictureStats::default();
    let spp = u64::from(o.supersample) * u64::from(o.supersample);
    if b.is_empty() {
        stats.rays = pixels * spp;
        if tiled {
            let tx = u64::from(view.width.div_ceil(TILE));
            let ty = u64::from(view.height.div_ceil(TILE));
            stats.tiles_culled = tx * ty;
        }
        return Ok(Picture { width: view.width, height: view.height, pixels: rgba, depth, stats });
    }

    let ctx = ShadeContext::new(&cam, b, &o);
    let mut sc = Scratch::new(o.scratch);

    if !tiled {
        render_tile_pixels(t, t, &b, &cam, &ctx, &o, 0, 0, view.width, view.height, &mut rgba, &mut depth, &mut sc, &mut stats, point_step)?;
        return Ok(Picture { width: view.width, height: view.height, pixels: rgba, depth, stats });
    }

    let b_pad = pad_bounds(&b, cam.eye);
    let mut my0 = 0u32;
    while my0 < view.height {
        let my1 = (my0 + MACRO_TILE).min(view.height);
        let mut mx0 = 0u32;
        while mx0 < view.width {
            let mx1 = (mx0 + MACRO_TILE).min(view.width);
            let single_fine = (mx1 - mx0) <= TILE && (my1 - my0) <= TILE;
            if single_fine {
                render_fine_tile(t, t, &b_pad, &b, &b_pad, &cam, &ctx, &o, mx0, my0, mx1, my1, &mut rgba, &mut depth, &mut sc, &mut stats, point_step)?;
                mx0 = mx1;
                continue;
            }

            let fine_x = u64::from((mx1 - mx0).div_ceil(TILE));
            let fine_y = u64::from((my1 - my0).div_ceil(TILE));
            let macro_pixels = u64::from(mx1 - mx0) * u64::from(my1 - my0);

            let Some(macro_box) = tile_frustum_aabb(&cam, mx0, my0, mx1, my1, &b_pad) else {
                stats.tiles_culled += fine_x * fine_y;
                stats.rays += macro_pixels * spp;
                mx0 = mx1;
                continue;
            };

            stats.boxes += 1;
            let (iv, choices) = eval_box(t, &macro_box, &mut sc);
            if iv.lo() > 0.0 {
                stats.tiles_culled += fine_x * fine_y;
                stats.rays += macro_pixels * spp;
                mx0 = mx1;
                continue;
            }

            let macro_simplified = if t.sites() > 0 && t.len() >= MIN_SIMPLIFY_INSTRS && choices.decided() > 0 {
                match simplify(t, choices, o.scratch) {
                    Ok(small) => {
                        stats.tiles_simplified += 1;
                        Some(small)
                    }
                    Err(_) => None,
                }
            } else {
                None
            };
            let macro_tape = macro_simplified.as_ref().unwrap_or(t);

            let mut ty0 = my0;
            while ty0 < my1 {
                let ty1 = (ty0 + TILE).min(my1);
                let mut tx0 = mx0;
                while tx0 < mx1 {
                    let tx1 = (tx0 + TILE).min(mx1);
                    render_fine_tile(
                        t,
                        macro_tape,
                        &macro_box,
                        &b,
                        &b_pad,
                        &cam,
                        &ctx,
                        &o,
                        tx0,
                        ty0,
                        tx1,
                        ty1,
                        &mut rgba,
                        &mut depth,
                        &mut sc,
                        &mut stats,
                        point_step,
                    )?;
                    tx0 = tx1;
                }
                ty0 = ty1;
            }

            mx0 = mx1;
        }
        my0 = my1;
    }

    Ok(Picture { width: view.width, height: view.height, pixels: rgba, depth, stats })
}

/// The picture of `t`'s solid from `view`.
pub fn render<'a>(t: &Tape<'_>, view: View, o: PictureOpts<'a, '_>) -> Result<Picture<'a>, PictureError> {
    render_planted(t, view, o, NoPlant)
}

/// [`render`] with a planted bug, for the tests' controls.
pub fn render_planted<'a>(t: &Tape<'_>, view: View, o: PictureOpts<'a, '_>, plant: impl Planted<PicturePlant>) -> Result<Picture<'a>, PictureError> {
    render_core(t, view, o, plant.has(PicturePlant::PointStep), true)
}

/// Test-only untiled reference renderer for verifying tile-culling & simplification parity.
#[cfg(test)]
pub(crate) fn render_untiled<'a>(t: &Tape<'_>, view: View, o: PictureOpts<'a, '_>) -> Result<Picture<'a>, PictureError> {
    render_core(t, view, o, false, false)
}
