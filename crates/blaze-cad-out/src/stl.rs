//! A binary STL writer (3D Systems Stereolithography Interface Specification) for 3D printing,
//! CNC and CAM fabrication.
//!
//! # Layout
//!
//! 1. An 80-byte fixed ASCII header beginning with `"blaze-cad binary stl"` (and the chosen unit
//!    and optional title, zero-padded to 80 bytes). It NEVER begins with `"solid "`, which breaks
//!    ASCII/binary STL auto-detectors in slicers and viewers.
//! 2. A little-endian `u32` triangle count `N`.
//! 3. `N` contiguous 50-byte facets, in triangle order:
//!    - outward unit face normal `[f32; 3]` computed in `f64` from `(v1 − v0) × (v2 − v0)`,
//!      normalized, narrowed to `f32`, and canonicalized (`−0.0 → +0.0`);
//!    - three counter-clockwise vertices `[f32; 3]` scaled into [`StlUnit`] and canonicalized
//!      (`−0.0 → +0.0`);
//!    - two-byte little-endian attribute byte count `0`.
//!
//! # Soundness and determinism
//!
//! Only a [`Verified`] mesh (closed, manifold, outward-oriented, finite, non-degenerate) can be
//! written. Every operation is deterministic IEEE arithmetic in triangle order, with no scratch
//! allocation and no `core::fmt`, so two exports of one mesh are byte-identical on every machine.

#![blaze::sans_io]

use blaze_cad_mesh::Verified;
use blaze_cad_mesh::geom::{P3, narrow, plus_zeros, tri_normal, unit, wide};
use blaze_cad_mesh::ids::TriId;
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::Limits;
use crate::plant::StlPlant;

/// Fixed binary STL header length in bytes.
pub const HEADER_BYTES: usize = 80;

/// Fixed size of one binary STL triangle facet in bytes (4 × 12 + 2).
pub const FACET_BYTES: usize = 50;

const HEADER_PREFIX: &[u8] = b"blaze-cad binary stl";

/// Length unit of the coordinates written into the binary STL.
///
/// STL carries no unit metadata in the binary container; most slicers and CAM tools interpret
/// coordinates as millimetres.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StlUnit {
    /// Millimetres (`m × 1000`): the slicer and CAM default.
    Millimetres,
    /// Metres (`m × 1`): exact stored `f32` coordinates.
    Metres,
}

impl StlUnit {
    /// Multiplier from metres to this unit.
    #[inline]
    pub const fn scale(self) -> f64 {
        match self {
            StlUnit::Millimetres => 1000.0,
            StlUnit::Metres => 1.0,
        }
    }

    fn tag(self) -> &'static [u8] {
        match self {
            StlUnit::Millimetres => b" (mm)",
            StlUnit::Metres => b" (m)",
        }
    }
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct StlOpts<'a, 't> {
    /// Optional ASCII title appended to the 80-byte header (truncated to fit; non-printable bytes
    /// replaced with spaces).
    pub title: &'t str,
    /// Coordinate unit written to each facet's vertices.
    pub unit: StlUnit,
    pub limits: Limits,
    pub alloc: Alloc<'a>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StlError {
    /// Output size exceeds `limits.most_stl_bytes` or the host address space.
    TooLarge { bytes: u64, most: u64 },
    /// Scaling a vertex coordinate into `unit` overflowed `f32`.
    Overflow { triangle: u32, corner: u8 },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for StlError {
    fn from(e: AllocFailure) -> StlError {
        StlError::Alloc(e)
    }
}

/// Write `mesh` as a standard binary STL into `opts.alloc`.
pub fn stl<'a>(mesh: &Verified<'_>, opts: StlOpts<'a, '_>) -> Result<Vec<u8, Alloc<'a>>, StlError> {
    stl_planted(mesh, opts, NoPlant)
}

/// [`stl`] with a planted bug, for the tests' controls.
pub fn stl_planted<'a>(
    mesh: &Verified<'_>,
    opts: StlOpts<'a, '_>,
    plant: impl Planted<StlPlant>,
) -> Result<Vec<u8, Alloc<'a>>, StlError> {
    let view = mesh.view();
    let n_tris = view.triangle_count();
    let total_bytes = (HEADER_BYTES as u64) + 4 + u64::from(n_tris) * (FACET_BYTES as u64);
    if total_bytes > opts.limits.most_stl_bytes {
        return Err(StlError::TooLarge { bytes: total_bytes, most: opts.limits.most_stl_bytes });
    }
    let Ok(cap) = usize::try_from(total_bytes) else {
        return Err(StlError::TooLarge { bytes: total_bytes, most: usize::MAX as u64 });
    };

    let mut out: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(cap, opts.alloc)?;

    // 1. Fixed 80-byte header.
    let mut header = [0u8; HEADER_BYTES];
    let mut pos = 0usize;
    let mut write_hdr = |bytes: &[u8]| {
        for &b in bytes {
            if pos >= HEADER_BYTES {
                break;
            }
            header[pos] = if (0x20..=0x7E).contains(&b) { b } else { b' ' };
            pos += 1;
        }
    };
    write_hdr(HEADER_PREFIX);
    write_hdr(opts.unit.tag());
    if !opts.title.is_empty() {
        write_hdr(b": ");
        write_hdr(opts.title.as_bytes());
    }
    if plant.has(StlPlant::SolidHeader) {
        header[0..6].copy_from_slice(b"solid ");
    }
    out.try_extend_from_slice(&header)?;

    // 2. Little-endian u32 triangle count.
    let declared_tris = if plant.has(StlPlant::WrongTriCount) { n_tris.saturating_sub(1) } else { n_tris };
    out.try_extend_from_slice(&declared_tris.to_le_bytes())?;

    // 3. Facets: 50 bytes each.
    let scale = opts.unit.scale();
    let mut neg_zero_fired = false;

    for t in 0..n_tris {
        let mut corners = view.corners(TriId(t));
        if t == 0 && plant.has(StlPlant::FlipWinding) {
            corners.swap(1, 2);
        }

        let mut verts: [P3; 3] = [[0.0; 3]; 3];
        for (k, &c) in corners.iter().enumerate() {
            let v = match opts.unit {
                StlUnit::Metres => plus_zeros(c),
                StlUnit::Millimetres => {
                    let scaled = narrow([f64::from(c[0]) * scale, f64::from(c[1]) * scale, f64::from(c[2]) * scale]);
                    if !scaled.iter().all(|x| x.is_finite()) {
                        return Err(StlError::Overflow { triangle: t, corner: k as u8 });
                    }
                    plus_zeros(scaled)
                }
            };
            verts[k] = v;
        }

        // Compute outward unit face normal in f64 from the emitted vertices, falling back to
        // unscaled corners if f32 scaling collapsed an extreme aspect ratio.
        let mut n = unit(tri_normal(wide(verts[0]), wide(verts[1]), wide(verts[2])))
            .or_else(|| unit(tri_normal(wide(corners[0]), wide(corners[1]), wide(corners[2]))))
            .unwrap_or([0.0; 3]);
        if t == 0 && plant.has(StlPlant::FlipNormal) {
            n = [-n[0], -n[1], -n[2]];
        }
        let nf = plus_zeros(narrow(n));

        for x in nf {
            put_f32(&mut out, x, plant.has(StlPlant::NegativeZero), &mut neg_zero_fired)?;
        }
        for v in verts {
            for x in v {
                put_f32(&mut out, x, plant.has(StlPlant::NegativeZero), &mut neg_zero_fired)?;
            }
        }
        out.try_extend_from_slice(&0u16.to_le_bytes())?;
    }

    Ok(out)
}

#[inline]
fn put_f32(out: &mut Vec<u8, Alloc<'_>>, x: f32, plant_neg_zero: bool, fired: &mut bool) -> Result<(), AllocFailure> {
    let val = if plant_neg_zero && !*fired && x == 0.0 {
        *fired = true;
        -0.0f32
    } else {
        x
    };
    out.try_extend_from_slice(&val.to_le_bytes())
}
