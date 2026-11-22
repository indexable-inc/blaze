//! PICTURES OF A FIELD (D183): [`render`] a field tape (blaze-cad-field) as seen from a
//! [`View`] into an RGBA8 [`Picture`] with each pixel's depth, and write it as a PNG
//! (blaze-cad-out's encoder). No mesh is made: the picture is of the solid itself, so a model is
//! looked at the moment it compiles, and an agent can check a change by eye.
//!
//! # The tracer: intervals first, never past a wall
//!
//! Each pixel's ray is clipped to the tape's box, then walked in SEGMENTS: the field's interval
//! over a segment's box (`eval_box`) either proves the segment empty (its low end above zero: the
//! walk moves past it and doubles the next segment) or not (the segment is halved). A segment no
//! longer than the STEP TOLERANCE that is not proved empty is the hit. Because an interval bounds
//! the field over the whole segment, no wall is ever stepped past, however thin, and no point
//! before a hit is inside the solid. This is sphere tracing's goal without its assumption: plain
//! sphere tracing trusts the field to be a distance bound, which a `Bound`-level field (a twist,
//! a taper) is not; the interval holds for every level. Where the field is a distance, the walk
//! still takes long strides through empty space (each empty segment doubles the next).
//!
//! The step tolerance is a fraction ([`PictureOpts::pixel_fraction`], half by default) of a
//! pixel's footprint at the depth reached ([`Camera::tolerance_at`]): the silhouette is right to
//! half a pixel, so a distant part costs no more steps than it shows.
//!
//! # Shading: fixed, so the bytes are
//!
//! The normal is `eval_grad`'s gradient at the hit, normalised (a zero gradient, which has no
//! direction, is drawn facing the viewer). One light from above the viewer's left shoulder, in
//! the camera's frame, and a fixed ambient term: `colour × (0.22 + 0.78 max(0, n·l))`, rounded to
//! nearest-even. Nothing reads a clock, a thread or a hash order; pixels are made row by row.
//! Two renders of one tape and view are equal in bytes (rule 25).
//!
//! # Rule 18
//!
//! Every ray takes at most [`PictureOpts::most_steps`] segments, and a picture at most
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

#[cfg(test)]
mod tests;

use blaze_cad_field::{Scratch, Tape, eval_box, eval_grad, eval_point};
use blaze_cad_out::{Image, PngError, PngKind, PngOpts};
use blaze_math::{Aabb3, Vec3};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::{Angle, Length};

/// The least step tolerance, metres: a nanometre, the grid lengths are kept on.
pub const LEAST_STEP: f64 = 1e-9;

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

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct PictureOpts<'a, 's> {
    /// Where the picture's pixels and depths live.
    pub alloc: Alloc<'a>,
    /// Where the evaluators' registers live.
    pub scratch: Alloc<'s>,
    /// The step tolerance as a fraction of a pixel's footprint at the depth reached.
    pub pixel_fraction: f64 = 0.5,
    /// The most segments one ray may take.
    pub most_steps: u32 = 4096,
    pub most_pixels: u64 = 1 << 26,
    /// What a ray that hits nothing shows, RGBA.
    pub background: [u8; 4] = [246, 246, 244, 255],
    /// The solid's colour before shading, RGB.
    pub colour: [u8; 3] = [168, 178, 192],
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
    /// The tape's box is not finite: rays cannot be clipped to it.
    Unbounded,
    /// The ray of pixel (`x`, `y`) took `most` segments without settling.
    TooManySteps { x: u32, y: u32, most: u32 },
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
    /// `eval_box` calls: segments looked at.
    pub boxes: u64,
    pub points: u64,
    pub grads: u64,
    /// The most segments one ray took.
    pub most_steps: u32,
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
    eye: Vec3,
    forward: Vec3,
    right: Vec3,
    up: Vec3,
    /// Half the picture's height: at unit distance (perspective) or in metres (orthographic).
    half: f64,
    perspective: bool,
    width: u32,
    height: u32,
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

    /// The ray through the centre of pixel (`x`, `y`): its origin and unit direction.
    pub fn ray(&self, x: u32, y: u32) -> (Vec3, Vec3) {
        let (w, h) = (f64::from(self.width), f64::from(self.height));
        let sx = ((f64::from(x) + 0.5) / w * 2.0 - 1.0) * (w / h) * self.half;
        let sy = (1.0 - (f64::from(y) + 0.5) / h * 2.0) * self.half;
        if self.perspective {
            let d = self.forward + self.right * sx + self.up * sy;
            (self.eye, d * (1.0 / d.length()))
        } else {
            (self.eye + self.right * sx + self.up * sy, self.forward)
        }
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
fn segment(o: Vec3, d: Vec3, a: f64, b: f64) -> Aabb3 {
    let (p, q) = (o + d * a, o + d * b);
    Aabb3::new(p.min(q), p.max(q))
}

/// The hit along one ray, if any: the start of the first segment no longer than the step
/// tolerance that the interval does not prove empty (module documentation).
#[allow(clippy::too_many_arguments)]
fn trace(
    t: &Tape<'_>,
    cam: &Camera,
    (o, d): (Vec3, Vec3),
    (t0, t1): (f64, f64),
    fraction: f64,
    most_steps: u32,
    sc: &mut Scratch<'_>,
    stats: &mut PictureStats,
    plant: impl Planted<PicturePlant>,
) -> Result<Option<f64>, u32> {
    let mut at = t0;
    let mut len = ((t1 - t0) / 8.0).max(LEAST_STEP);
    let mut steps = 0u32;
    let out = loop {
        if !(at < t1) {
            break None;
        }
        steps += 1;
        if steps > most_steps {
            return Err(steps - 1);
        }
        let end = (at + len).min(t1);
        if !(end > at) {
            // No float between: the segment is a point the walk cannot pass.
            break Some(at);
        }
        stats.boxes += 1;
        let (iv, _) = eval_box(t, &segment(o, d, at, end), sc);
        if iv.lo() > 0.0 {
            at = end;
            len *= 2.0;
            continue;
        }
        let eps = cam.tolerance_at(end, fraction);
        if plant.has(PicturePlant::PointStep) {
            stats.points += 1;
            if eval_point(t, o + d * end, sc) < 0.0 || end - at <= eps {
                break Some(at);
            }
            at = end;
            continue;
        }
        if end - at <= eps {
            break Some(at);
        }
        len = (end - at) / 2.0;
    };
    stats.most_steps = stats.most_steps.max(steps);
    Ok(out)
}

/// One channel shaded: `c × s`, rounded to nearest-even, within `0..=255`.
fn channel(c: u8, s: f64) -> u8 {
    let v = core::f64::math::round_ties_even(f64::from(c) * s);
    // Within `0..=255` after the clamp: exact.
    v.max(0.0).min(255.0) as u8
}

/// The picture of `t`'s solid from `view`.
pub fn render<'a>(t: &Tape<'_>, view: View, o: PictureOpts<'a, '_>) -> Result<Picture<'a>, PictureError> {
    render_planted(t, view, o, NoPlant)
}

/// [`render`] with a planted bug, for the tests' controls.
pub fn render_planted<'a>(t: &Tape<'_>, view: View, o: PictureOpts<'a, '_>, plant: impl Planted<PicturePlant>) -> Result<Picture<'a>, PictureError> {
    let cam = Camera::new(&view)?;
    let pixels = u64::from(view.width) * u64::from(view.height);
    if pixels > o.most_pixels {
        return Err(PictureError::TooManyPixels { pixels, most: o.most_pixels });
    }
    if !(o.pixel_fraction > 0.0 && o.pixel_fraction.is_finite()) {
        return Err(PictureError::BadFraction { fraction: o.pixel_fraction });
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
    let mut sc = Scratch::new(o.scratch);
    let mut stats = PictureStats::default();
    // The light, from above the viewer's left shoulder.
    let light = (cam.up * 0.7 - cam.right * 0.4 - cam.forward * 0.6).normalized().unwrap_or(cam.up);
    for y in 0..view.height {
        for x in 0..view.width {
            stats.rays += 1;
            let (origin, dir) = cam.ray(x, y);
            let span = if b.is_empty() { None } else { slab(&b, origin, dir) };
            let hit = match span {
                None => None,
                Some(span) => trace(t, &cam, (origin, dir), span, o.pixel_fraction, o.most_steps, &mut sc, &mut stats, plant)
                    .map_err(|most| PictureError::TooManySteps { x, y, most })?,
            };
            let px = match hit {
                None => {
                    depth.try_push(f64::INFINITY).map_err(|r| r.why)?;
                    o.background
                }
                Some(at) => {
                    stats.hits += 1;
                    stats.grads += 1;
                    depth.try_push(at).map_err(|r| r.why)?;
                    let (_, g) = eval_grad(t, origin + dir * at, &mut sc);
                    let n = g.normalized().unwrap_or(dir * -1.0);
                    let s = 0.22 + 0.78 * n.dot(light).max(0.0);
                    [channel(o.colour[0], s), channel(o.colour[1], s), channel(o.colour[2], s), 255]
                }
            };
            for c in px {
                rgba.try_push(c).map_err(|r| r.why)?;
            }
        }
    }
    Ok(Picture { width: view.width, height: view.height, pixels: rgba, depth, stats })
}
