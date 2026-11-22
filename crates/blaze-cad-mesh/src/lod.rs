//! Levels of detail: level 0 is the input; level `k` is the input simplified to `ratio^k` of its
//! triangles. Each level is simplified FROM THE INPUT, not from the level before, so its error is
//! the two-sided distance bound against the input itself (no bounds to add up), and the chain is
//! as deterministic (and as order-free) as [`crate::simplify()`].
//!
//! SWITCH DISTANCE. A perspective camera whose image is `H` pixels tall over a vertical field of
//! view `θ` shows a length `e` at distance `d` (along the view axis) as
//! `e · H / (2 d tan(θ/2))` pixels. With `f = H / (2 tan(θ/2))` (the focal length in pixels, the
//! caller's [`Projection::focal_pixels`]: this crate computes no tangent) and a largest
//! allowed screen error of `τ` pixels, level `k` is fine from `d_k = e_k · f / τ` on. Level 0 is
//! shown from distance 0. The distances only grow: a level whose error is below the one before
//! it gets the one before's distance (it is never needed sooner).

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure, Vec};

use blaze_units::Length;

use crate::MeshLimits;
use crate::check::Verified;
use crate::geom::length_up;
use crate::mesh::{Mesh, vecs_of};
use crate::simplify::{DecimateError, DecimateOpts, Source, Stop, Target, decimate};
use crate::vecs::push;

/// The camera a chain is switched for.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Projection {
    /// `H / (2 tan(θ/2))`: the image height in pixels over twice the tangent of half the vertical
    /// field of view (1080 pixels at 60° is about 935.3).
    pub focal_pixels: f64,
    /// The most screen error, in pixels, a level may show (`τ`).
    pub most_error_pixels: f64,
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct LodOpts<'a, 's> {
    /// Each level's triangle target over the level before's (0.5 halves each time).
    pub ratio: f64,
    /// No level is made with fewer triangles than this.
    pub fewest_triangles: u32,
    pub projection: Projection,
    pub cos_crease: f32,
    pub limits: MeshLimits,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

#[derive(Debug)]
pub struct LodLevel<'a> {
    pub mesh: Mesh<'a>,
    /// The two-sided distance bound against level 0, rounded up to the nanometre.
    pub error: Length,
    /// Show this level from this distance on (module documentation), rounded up.
    pub switch_distance: Length,
}

#[derive(Debug)]
pub struct LodChain<'a> {
    pub levels: Vec<LodLevel<'a>, Alloc<'a>>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LodError {
    /// A ratio not strictly between 0 and 1.
    BadRatio { ratio: f64 },
    /// A projection whose focal length or pixel error is not positive and finite.
    BadProjection { projection: Projection },
    /// More levels than [`MeshLimits::most_lod_levels`] would be made: raise it or the ratio.
    TooManyLevels { most: u32 },
    Decimate { level: u32, error: DecimateError },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for LodError {
    fn from(e: AllocFailure) -> LodError {
        LodError::Alloc(e)
    }
}

/// The chain for a verified mesh: every level is verified too (each is a simplification of a
/// verified input, see [`crate::Decimated::verified`]).
pub fn lod_chain<'a>(mesh: &Verified<'_>, opts: LodOpts<'a, '_>) -> Result<LodChain<'a>, LodError> {
    if !(opts.ratio > 0.0 && opts.ratio < 1.0) {
        return Err(LodError::BadRatio { ratio: opts.ratio });
    }
    let p = opts.projection;
    let good = |x: f64| x > 0.0 && x.is_finite();
    if !good(p.focal_pixels) || !good(p.most_error_pixels) {
        return Err(LodError::BadProjection { projection: p });
    }
    let view = mesh.view();
    let mut levels: Vec<LodLevel<'a>, Alloc<'a>> = Vec::new_in(opts.alloc);
    let level0 = Mesh::new(vecs_of(&view, opts.alloc)?).map_err(|e| LodError::Decimate {
        level: 0,
        error: DecimateError::Built(e),
    })?;
    push(&mut levels, LodLevel { mesh: level0, error: Length::ZERO, switch_distance: Length::ZERO })?;
    let mut last_count = view.triangle_count();
    let mut last_distance = 0.0f64;
    let mut target = f64::from(view.triangle_count());
    loop {
        target *= opts.ratio;
        // `target` is below the input's count, which is a `u32`.
        let want = target as u32;
        if want < opts.fewest_triangles || want >= last_count {
            break;
        }
        let level = crate::vecs::len32(levels.len());
        if level >= opts.limits.most_lod_levels {
            return Err(LodError::TooManyLevels { most: opts.limits.most_lod_levels });
        }
        let out = decimate(
            Source::Verified(*mesh),
            DecimateOpts {
                target: Target::Triangles(want),
                cos_crease: opts.cos_crease,
                limits: opts.limits,
                alloc: opts.alloc,
                scratch: opts.scratch,
                ..
            },
        )
        .map_err(|error| LodError::Decimate { level, error })?;
        let count = out.mesh.view().triangle_count();
        if count >= last_count {
            break;
        }
        let distance = (out.error_bound.as_m() * p.focal_pixels / p.most_error_pixels).max(last_distance);
        last_distance = distance;
        last_count = count;
        let stopped = out.stop == Stop::NoValidCollapse;
        push(&mut levels, LodLevel { error: out.error_bound, switch_distance: length_up(distance), mesh: out.mesh })?;
        if stopped {
            break;
        }
    }
    Ok(LodChain { levels })
}
