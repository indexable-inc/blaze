//! Quantization, each with its round-trip bound stated and tested.
//!
//! POSITIONS: each coordinate as a `u16` across the mesh's box,
//! `q = roundTiesToEven((p − min) / extent · 65535)`, and the dequantization transform
//! `p' = offset + q · scale` with `offset = min` and `scale = extent / 65535` rounded to the
//! nearest `f32` (glTF's `KHR_mesh_quantization`: the transform is the node's translation and
//! scale). Evaluated as a GPU does, in `f32` with each operation rounded, the round trip is within
//! `bound[a]` on axis `a`:
//!
//! `bound = step/2 + 65535 · 2^-24 · step + 2^-22 · (extent + M) + 2^-40 · (extent + M)`
//!
//! with `step = extent / 65535` and `M = max(|min|, |max|)`: half a step from rounding `q`, the
//! scale's own rounding (relative `2^-24`) times the largest `q`, then the product's and the sum's
//! roundings (each at most `2^-24` times a value below `extent + M`, together `2^-23 · (extent +
//! M)`, written as `2^-22` to spare), and the `f64` arithmetic of the encoder. An axis of zero extent stores `q = 0`, `scale = 0`,
//! and is exact.
//!
//! NORMALS, octahedral (Q. Meyer, J. Süßmuth, G. Sußner, M. Stamminger and G. Greiner, "On
//! floating-point normal vectors", EGSR 2010; Z. H. Cigolle et al., "A survey of efficient
//! representations for independent unit vectors", JCGT 3(2), 2014): the unit normal is projected
//! onto the octahedron `|x| + |y| + |z| = 1`, the lower half folded over the upper, and the square
//! `[−1, 1]²` stored as two signed normalized integers, `q = roundTiesToEven(p · m)` with
//! `m = 32767` (16 bits) or `127` (8 bits). BOUND: each component of `p` moves by at most
//! `1/(2m)`; the octahedron's point moves by at most `sqrt(1.5)/m` (the third coordinate moves by
//! the sum of the other two's moves, the fold changes no lengths); projecting a point at least
//! `1/sqrt(3)` from the centre onto the sphere scales a move by at most `sqrt(3)`; so the decoded
//! unit vector is within `sqrt(4.5)/m ≈ 2.1214/m` of the input's direction, plus `1e-6` for the
//! `f64` arithmetic and the decoded `f32`s: [`OCT16_BOUND`] and [`OCT8_BOUND`]. (glTF's own
//! quantized normals are plain normalized `VEC3`s; octahedral normals are for blaze's own runtime
//! formats.)

#![blaze::sans_io]

use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::geom::{Aabb, D3, P3, narrow, round_even, trunc, unit, wide};
use crate::ids::VertexId;
use crate::measure::bounds;
use crate::mesh::MeshView;
use crate::plant::QuantPlant;
use crate::vecs::{len32, push};

/// The 16-bit octahedral round trip's bound on the distance between unit vectors.
pub const OCT16_BOUND: f64 = 2.1214 / 32767.0 + 1e-6;
/// The 8-bit octahedral round trip's bound.
pub const OCT8_BOUND: f64 = 2.1214 / 127.0 + 1e-6;

/// `p' = offset + q · scale`, per axis, in `f32`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Dequantize {
    pub offset: P3,
    pub scale: P3,
}

impl Dequantize {
    /// The position `q` stands for, computed as a GPU does: `f32` operations, each rounded.
    #[inline]
    pub fn apply(&self, q: [u16; 3]) -> P3 {
        let mut p = [0.0f32; 3];
        for a in 0..3 {
            p[a] = self.offset[a] + f32::from(q[a]) * self.scale[a];
        }
        p
    }
}

#[derive(Debug)]
pub struct QuantizedPositions<'a> {
    pub values: Vec<[u16; 3], Alloc<'a>>,
    pub dequantize: Dequantize,
    /// The round trip's bound per axis (module documentation).
    pub bound: D3,
}

/// The box the 16-bit grid spans.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Within {
    /// The mesh's own bounds: the finest grid for this mesh alone.
    OwnBounds,
    /// A box handed in, which must hold every position: several meshes (a part and its levels
    /// of detail) then share one dequantization transform.
    Box(Aabb),
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct QuantOpts<'a> {
    pub within: Within,
    pub alloc: Alloc<'a>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum QuantError {
    NonFinite { vertex: VertexId, at: P3 },
    /// A position outside the box handed in as [`Within::Box`], or a box that is empty or not
    /// finite.
    OutsideBox { vertex: VertexId, at: P3, within: Aabb },
    /// A normal of zero length (or one whose square underflows): it has no direction.
    ZeroNormal { vertex: VertexId },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for QuantError {
    fn from(e: AllocFailure) -> QuantError {
        QuantError::Alloc(e)
    }
}

pub fn quantize_positions<'a>(view: &MeshView<'_>, opts: QuantOpts<'a>) -> Result<QuantizedPositions<'a>, QuantError> {
    quantize_positions_planted(view, opts, NoPlant)
}

/// [`quantize_positions`] with a planted bug, for the tests' controls.
pub fn quantize_positions_planted<'a>(
    view: &MeshView<'_>,
    opts: QuantOpts<'a>,
    plant: impl Planted<QuantPlant>,
) -> Result<QuantizedPositions<'a>, QuantError> {
    for (v, p) in view.positions().iter().enumerate() {
        if !p.iter().all(|x| x.is_finite()) {
            return Err(QuantError::NonFinite { vertex: VertexId(len32(v)), at: *p });
        }
    }
    let b = match opts.within {
        Within::OwnBounds => bounds(view),
        Within::Box(given) => {
            let finite = given.min.iter().chain(given.max.iter()).all(|x| x.is_finite());
            for (v, &p) in view.positions().iter().enumerate() {
                if !finite || given.is_empty() || !given.contains(p) {
                    return Err(QuantError::OutsideBox { vertex: VertexId(len32(v)), at: p, within: given });
                }
            }
            given
        }
    };
    let extent = b.extent();
    let (min, max) = if b.is_empty() { ([0.0; 3], [0.0; 3]) } else { (wide(b.min), wide(b.max)) };
    let mut scale = [0.0f32; 3];
    let mut bound = [0.0f64; 3];
    for a in 0..3 {
        if extent[a] > 0.0 {
            let step = extent[a] / 65535.0;
            scale[a] = step as f32;
            let m = min[a].abs().max(max[a].abs());
            let ulp24 = 1.0 / 16_777_216.0;
            bound[a] = step * 0.5 + 65535.0 * ulp24 * step + 4.0 * ulp24 * (extent[a] + m) + (extent[a] + m) / 1_099_511_627_776.0;
        }
    }
    let toward_zero = plant.has(QuantPlant::TowardZero);
    let mut values = Vec::try_with_capacity_in(view.positions().len(), opts.alloc)?;
    for p in view.positions() {
        let mut q = [0u16; 3];
        for a in 0..3 {
            if extent[a] > 0.0 {
                let t = (f64::from(p[a]) - min[a]) / extent[a] * 65535.0;
                let r = if toward_zero { trunc(t) } else { round_even(t) };
                // Clamped to the range first, so the conversion is exact.
                q[a] = r.clamp(0.0, 65535.0) as u16;
            }
        }
        push(&mut values, q)?;
    }
    Ok(QuantizedPositions { values, dequantize: Dequantize { offset: if b.is_empty() { [0.0; 3] } else { b.min }, scale }, bound })
}

/// How many bits each octahedral component has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OctBits {
    Eight,
    Sixteen,
}

#[derive(Debug)]
pub enum OctNormals<'a> {
    Eight(Vec<[i8; 2], Alloc<'a>>),
    Sixteen(Vec<[i16; 2], Alloc<'a>>),
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct QuantNormalsOpts<'a> {
    pub bits: OctBits,
    pub alloc: Alloc<'a>,
}

/// The square point of a unit vector (module documentation).
fn oct_square(n: D3) -> [f64; 2] {
    let l1 = n[0].abs() + n[1].abs() + n[2].abs();
    let (x, y) = (n[0] / l1, n[1] / l1);
    if n[2] >= 0.0 {
        [x, y]
    } else {
        [(1.0 - y.abs()) * sign(x), (1.0 - x.abs()) * sign(y)]
    }
}

/// The unit vector of a square point.
fn oct_sphere(p: [f64; 2]) -> P3 {
    let (x, y) = (p[0], p[1]);
    let z = 1.0 - x.abs() - y.abs();
    let u = if z >= 0.0 { [x, y, z] } else { [(1.0 - y.abs()) * sign(x), (1.0 - x.abs()) * sign(y), z] };
    // `|u|₁ = 1`, so `|u|` is at least `1/sqrt(3)` and the unit vector exists.
    match unit(u) {
        Some(n) => narrow(n),
        None => [0.0, 0.0, 1.0],
    }
}

/// `+1` for zero and above, `-1` below (the fold's sign: zero goes up).
fn sign(x: f64) -> f64 {
    if x >= 0.0 { 1.0 } else { -1.0 }
}

fn snorm(x: f64, m: f64) -> f64 {
    round_even(x.clamp(-1.0, 1.0) * m)
}

/// `n`'s 16-bit octahedral code, or `None` for a vector of no direction.
pub fn encode_oct16(n: P3) -> Option<[i16; 2]> {
    let p = oct_square(unit(wide(n))?);
    // Each `snorm` is an integer in `-32767..=32767`.
    Some([snorm(p[0], 32767.0) as i16, snorm(p[1], 32767.0) as i16])
}

pub fn decode_oct16(q: [i16; 2]) -> P3 {
    oct_sphere([(f64::from(q[0]) / 32767.0).max(-1.0), (f64::from(q[1]) / 32767.0).max(-1.0)])
}

/// `n`'s 8-bit octahedral code, or `None` for a vector of no direction.
pub fn encode_oct8(n: P3) -> Option<[i8; 2]> {
    let p = oct_square(unit(wide(n))?);
    // Each `snorm` is an integer in `-127..=127`.
    Some([snorm(p[0], 127.0) as i8, snorm(p[1], 127.0) as i8])
}

pub fn decode_oct8(q: [i8; 2]) -> P3 {
    oct_sphere([(f64::from(q[0]) / 127.0).max(-1.0), (f64::from(q[1]) / 127.0).max(-1.0)])
}

/// Every vertex normal, octahedral.
pub fn quantize_normals<'a>(view: &MeshView<'_>, opts: QuantNormalsOpts<'a>) -> Result<OctNormals<'a>, QuantError> {
    let normals = view.normals();
    match opts.bits {
        OctBits::Sixteen => {
            let mut out = Vec::try_with_capacity_in(normals.len(), opts.alloc)?;
            for (v, &n) in normals.iter().enumerate() {
                let q = encode_oct16(n).ok_or(QuantError::ZeroNormal { vertex: VertexId(len32(v)) })?;
                push(&mut out, q)?;
            }
            Ok(OctNormals::Sixteen(out))
        }
        OctBits::Eight => {
            let mut out = Vec::try_with_capacity_in(normals.len(), opts.alloc)?;
            for (v, &n) in normals.iter().enumerate() {
                let q = encode_oct8(n).ok_or(QuantError::ZeroNormal { vertex: VertexId(len32(v)) })?;
                push(&mut out, q)?;
            }
            Ok(OctNormals::Eight(out))
        }
    }
}
