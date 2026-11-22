//! Sharper geometric & topological fidelity checks between an exact CAD solid ([`Graph`] +
//! [`Part`]) and its exported/decimated triangle mesh ([`MeshView`]).
//!
//! Position-only Hausdorff bounds miss two failure modes that break game assets:
//!
//! 1. **Lost / bridged holes**: a coarse octree cell or aggressive collapse bridges flat across a
//!    window, doorway, slot, or bolt hole — even inside a huge 2-triangle wall where vertex and
//!    centroid probes miss the opening completely. [`check_mesh_fidelity`] certifies every exported
//!    triangle with adaptive 4-way midpoint subdivision guided by `blaze_cad_field::eval_box`: a
//!    sub-triangle whose interval stays inside `[-bevel_tol, bevel_tol]` is *proved* clean in one
//!    box; any sub-triangle whose interval reaches empty air zooms straight onto the bridged void
//!    and reports [`FidelityFinding::LostHole`] with its exact 3D coordinate and void depth.
//!    When a pre-decimation reference mesh is provided, exact Euler-characteristic genus
//!    (`blaze_cad_mesh::topology`) also flags any lost through-hole
//!    ([`FidelityFinding::TopologyHoleLost`]) or vanished shell component.
//! 2. **Lost / flattened bevels & chamfers**: deleting a 2–4 mm chamfer moves vertices by only
//!    `0.29 × width` (often below a game asset's position tolerance), yet bends the surface normal
//!    by 30°–45° and destroys specular edge highlights. Evaluating `eval_grad` at triangle
//!    centroids and interval-flagged edge sub-triangles catches normal collapse directly
//!    ([`FidelityFinding::LostBevel`]), complemented by dihedral bevel-strip length retention
//!    (`blaze_cad_mesh::crease_profile`) against the pre-decimation reference mesh.

#![blaze::sans_io]

use blaze_cad_field::{CadPlant, Detail, Ex, Graph, Scratch, eval_box, eval_grad, eval_point};
use blaze_cad_mesh::{CreaseProfileOpts, MeshView, crease_profile, topology};
use blaze_math::{Aabb3, Vec3};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, Vec};
use blaze_text::{Put, Show, Writes};
use blaze_units::{Angle, Length};

use crate::{ModelError, Part, Unsettled, Verdict, part_tape};

/// Planted bugs for [`check_mesh_fidelity_planted`], caught by the test controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FidelityPlant {
    None,
    /// Skips interval-guided triangle subdivision and probes only each triangle's single centroid
    /// (misses a small bridged hole hidden off-center inside a large wall triangle).
    SkipVoidSubdivision,
    /// Ignores `eval_grad` normal deviation and bevel-strip retention (misses a flattened bevel
    /// whose positional error stays under `hole_tolerance`).
    IgnoreNormalDeviation,
}

impl Plants for FidelityPlant {}

const UAS_PER_DEGREE_I64: i64 = 3_600_000_000;

/// Options for [`check_mesh_fidelity`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct FidelityOpts<'s> {
    /// Maximum positive CAD field depth `f(p) > 0` (empty air) a mesh triangle may span before
    /// flagging [`FidelityFinding::LostHole`].
    pub hole_tolerance: Length = Length::nm(2_500_000),
    /// Maximum negative CAD field depth `f(p) < 0` (cutting inside the solid) a mesh triangle may
    /// penetrate before flagging [`FidelityFinding::SurfaceClipped`].
    pub surface_tolerance: Length = Length::nm(2_500_000),
    /// Positional probe threshold above which interval subdivision zooms in to test bevel normal
    /// fidelity (defaults to `0.25 × min(hole_tolerance, surface_tolerance)` when `None`).
    pub bevel_probe_tolerance: Option<Length> = None,
    /// Maximum allowed angular deviation between the exported triangle normal and the exact CAD
    /// field gradient `eval_grad` before flagging [`FidelityFinding::LostBevel`].
    pub max_normal_error: Angle = Angle::microarcsec(18 * UAS_PER_DEGREE_I64),
    /// Smallest hole or bevel feature size to resolve during triangle subdivision (holes wider
    /// than this are guaranteed to be sampled).
    pub min_hole_feature: Length = Length::nm(4_000_000),
    /// Minimum triangle or sub-triangle area required before a normal-angle mismatch triggers
    /// [`FidelityFinding::LostBevel`] (`None` derives a tight area floor from `min_hole_feature`).
    pub min_bevel_area_m2: Option<f64> = None,
    /// Minimum edge length of a bevel strip in the reference mesh before bevel-retention loss is
    /// enforced (prevents noise on tiny parts with no real bevels).
    pub min_reference_bevel_length: Length = Length::nm(20_000_000),
    /// Maximum allowed fraction (`0.0..=1.0`) of reference bevel edge length lost during
    /// decimation before flagging [`FidelityFinding::LostBevel`].
    pub max_bevel_loss: f64 = 0.45,
    /// Hard cap on total triangle subdivisions per part (rule 18: bounded progress).
    pub most_subdivisions: u64 = 1 << 18,
    /// Scratch allocator for the compiled tape, subdivision stack, and topology tables.
    pub scratch: Alloc<'s>,
}

/// First / worst fidelity defect discovered between the exact CAD solid and its exported mesh.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FidelityFinding {
    /// An exported triangle spans across empty space (`f(p) > tolerance`) — a bridged or missed
    /// hole, window, doorway, or recess.
    LostHole {
        at: [Length; 3],
        void_depth: Length,
        tolerance: Length,
    },
    /// Exact Euler-characteristic genus dropped relative to the pre-decimation mesh: at least one
    /// through-hole was lost.
    TopologyHoleLost {
        expected_holes: u32,
        actual_holes: u32,
    },
    /// Connected shell component count changed relative to the pre-decimation mesh (e.g. a small
    /// detached trim piece or internal cavity vanished).
    ComponentCountChanged {
        expected: u32,
        actual: u32,
    },
    /// A bevel, chamfer, or curved highlight transition was flattened or lost (either by exact
    /// CAD normal deviation `eval_grad`, or by dihedral bevel-strip length collapse vs reference).
    LostBevel {
        at: [Length; 3],
        normal_error: Angle,
        max_normal_error: Angle,
        surface_offset: Length,
        bevel_retention: Option<f64>,
    },
    /// An exported triangle cuts into solid material deeper than `surface_tolerance`.
    SurfaceClipped {
        at: [Length; 3],
        depth: Length,
        tolerance: Length,
    },
}

/// Quantitative fidelity metrics gathered across the whole exported part mesh.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FidelityMetrics {
    /// Worst positive field value (`f(p) > 0`, bridged void depth) measured on the mesh.
    pub max_outside_error: Length,
    /// Worst negative field value (`-f(p) > 0`, shaved solid depth) measured on the mesh.
    pub max_inside_error: Length,
    /// Worst angular deviation between triangle normal and CAD field gradient.
    pub max_normal_error: Angle,
    /// Area-weighted mean normal deviation across sampled triangles.
    pub mean_normal_error: Angle,
    /// Exact topological through-hole count (genus) of the exported mesh (`None` if non-manifold/open).
    pub holes: Option<u32>,
    /// Exact topological through-hole count of the pre-decimation reference mesh, if supplied.
    pub reference_holes: Option<u32>,
    /// Connected shell component count of the exported mesh.
    pub components: u32,
    /// Retained fraction (`0.0..=1.0+`) of dihedral bevel edge length vs reference mesh, if
    /// reference had meaningful bevels.
    pub bevel_retention: Option<f64>,
    /// Number of sampled triangles/sub-triangles exceeding `hole_tolerance`.
    pub bridged_void_samples: u32,
    /// Number of sampled triangles/sub-triangles exceeding `max_normal_error`.
    pub bevel_violation_samples: u32,
    /// Total adaptive triangle subdivisions performed.
    pub subdivisions: u64,
}

/// Complete fidelity check outcome: three-way [`Verdict`] plus full quantitative metrics.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FidelityChecked {
    pub verdict: Verdict<FidelityFinding>,
    pub metrics: FidelityMetrics,
}

// ---- blaze_text::Show formatting ---------------------------------------------------------

fn put_mm<P: Put + ?Sized>(out: &mut P, nm: i64, places: u32) {
    let places = places.min(6);
    let unit = 10u128.pow(6 - places);
    let scale = 10u128.pow(places);
    let q = (i128::from(nm).unsigned_abs() + unit / 2) / unit;
    let (whole, mut frac) = (q / scale, q % scale);
    if nm < 0 && q != 0 {
        out.put_byte(b'-');
    }
    out.int(whole);
    if frac != 0 {
        let mut digits = places as usize;
        while frac % 10 == 0 {
            frac /= 10;
            digits -= 1;
        }
        out.put_byte(b'.');
        out.int_zeros(frac, digits);
    }
}

fn put_deg<P: Put + ?Sized>(out: &mut P, uas: i64, places: u32) {
    const UAS_PER_DEGREE: u128 = 3_600_000_000;
    let places = places.min(6);
    let scale = 10u128.pow(places);
    let mag = i128::from(uas).unsigned_abs();
    let q = (mag * scale + UAS_PER_DEGREE / 2) / UAS_PER_DEGREE;
    let (whole, mut frac) = (q / scale, q % scale);
    if uas < 0 && q != 0 {
        out.put_byte(b'-');
    }
    out.int(whole);
    if frac != 0 {
        let mut digits = places as usize;
        while frac % 10 == 0 {
            frac /= 10;
            digits -= 1;
        }
        out.put_byte(b'.');
        out.int_zeros(frac, digits);
    }
}

fn put_point<P: Put + ?Sized>(out: &mut P, at: &[Length; 3]) {
    out.put_byte(b'(');
    for (k, c) in at.iter().enumerate() {
        if k > 0 {
            out.put_str(", ");
        }
        put_mm(out, c.as_nm(), 2);
    }
    out.put_str(") mm");
}

impl Show for FidelityFinding {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match *self {
            FidelityFinding::LostHole { at, void_depth, tolerance } => {
                out.put_str("lost/bridged hole: mesh spans ");
                put_mm(out, void_depth.as_nm(), 2);
                out.put_str(" mm into empty space (limit ");
                put_mm(out, tolerance.as_nm(), 2);
                out.put_str(" mm) at ");
                put_point(out, &at);
            }
            FidelityFinding::TopologyHoleLost { expected_holes, actual_holes } => {
                out.put_str("lost through-hole: expected ");
                out.int(expected_holes);
                out.put_str(" holes (genus), exported mesh has ");
                out.int(actual_holes);
            }
            FidelityFinding::ComponentCountChanged { expected, actual } => {
                out.put_str("component count changed: expected ");
                out.int(expected);
                out.put_str(" shells, exported mesh has ");
                out.int(actual);
            }
            FidelityFinding::LostBevel { at, normal_error, max_normal_error, surface_offset, bevel_retention } => {
                out.put_str("lost/flattened bevel: normal off by ");
                put_deg(out, normal_error.as_microarcsec(), 1);
                out.put_str(" deg (limit ");
                put_deg(out, max_normal_error.as_microarcsec(), 1);
                out.put_str(" deg, offset ");
                put_mm(out, surface_offset.as_nm(), 2);
                out.put_str(" mm)");
                if let Some(ret) = bevel_retention {
                    let pct = (ret.clamp(0.0, 2.0) * 100.0 + 0.5) as u64;
                    out.put_str(", bevel retention ");
                    out.int(pct);
                    out.put_byte(b'%');
                }
                out.put_str(" at ");
                put_point(out, &at);
            }
            FidelityFinding::SurfaceClipped { at, depth, tolerance } => {
                out.put_str("surface clipped ");
                put_mm(out, depth.as_nm(), 2);
                out.put_str(" mm inside solid (limit ");
                put_mm(out, tolerance.as_nm(), 2);
                out.put_str(" mm) at ");
                put_point(out, &at);
            }
        }
    }
}

// ---- Core checker ------------------------------------------------------------------------

/// Verify that `mesh` (in `part`'s own local frame) preserves the CAD solid's holes, bevels,
/// normals, and surface envelope. Pass `reference = Some(&meshed.mesh.view())` when checking a
/// decimated mesh to also enforce exact genus and bevel-strip retention against the meshed solid.
pub fn check_mesh_fidelity<K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    mesh: &MeshView<'_>,
    reference: Option<&MeshView<'_>>,
    o: FidelityOpts<'_>,
) -> Result<FidelityChecked, ModelError> {
    check_mesh_fidelity_planted(g, part, mesh, reference, o, NoPlant)
}

/// [`check_mesh_fidelity`] with a planted bug, for the test controls.
pub fn check_mesh_fidelity_planted<K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    mesh: &MeshView<'_>,
    reference: Option<&MeshView<'_>>,
    o: FidelityOpts<'_>,
    plant: impl Planted<FidelityPlant>,
) -> Result<FidelityChecked, ModelError> {
    if o.hole_tolerance <= Length::ZERO {
        return Err(ModelError::Argument {
            function: "check_mesh_fidelity",
            arg: "hole_tolerance",
            accepted: "a length above zero",
        });
    }
    if o.surface_tolerance <= Length::ZERO {
        return Err(ModelError::Argument {
            function: "check_mesh_fidelity",
            arg: "surface_tolerance",
            accepted: "a length above zero",
        });
    }
    if o.min_hole_feature <= Length::ZERO {
        return Err(ModelError::Argument {
            function: "check_mesh_fidelity",
            arg: "min_hole_feature",
            accepted: "a length above zero",
        });
    }
    if !(o.max_normal_error.as_radians() > 0.0 && o.max_normal_error.as_radians() < core::f64::consts::PI) {
        return Err(ModelError::Argument {
            function: "check_mesh_fidelity",
            arg: "max_normal_error",
            accepted: "an angle strictly between 0 and 180 degrees",
        });
    }

    let s = o.scratch;
    let tape = part_tape(g, part, Detail::Full, s)?;
    let mut sc = Scratch::new(s);

    let topo = topology(mesh, s).map_err(|_| ModelError::Alloc { what: "fidelity topology" })?;
    let ref_topo = match reference {
        Some(r) => Some(topology(r, s).map_err(|_| ModelError::Alloc { what: "fidelity reference topology" })?),
        None => None,
    };

    // Bevel dihedral strip retention when a pre-decimation reference is available.
    let bevel_retention = if let Some(r) = reference
        && !plant.has(FidelityPlant::IgnoreNormalDeviation)
    {
        let ref_prof = crease_profile(r, CreaseProfileOpts { scratch: s, .. })
            .map_err(|_| ModelError::Alloc { what: "reference crease profile" })?;
        if ref_prof.bevel_edge_length >= o.min_reference_bevel_length {
            let cur_prof = crease_profile(mesh, CreaseProfileOpts { scratch: s, .. })
                .map_err(|_| ModelError::Alloc { what: "exported crease profile" })?;
            let r_m = ref_prof.bevel_edge_length.as_m();
            if r_m > 0.0 {
                Some(cur_prof.bevel_edge_length.as_m() / r_m)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let hole_tol = o.hole_tolerance.as_m();
    let surf_tol = o.surface_tolerance.as_m();
    let bevel_probe = o
        .bevel_probe_tolerance
        .map_or((hole_tol.min(surf_tol) * 0.25).max(5e-5), |l| l.as_m().max(1e-6));
    let min_edge = o.min_hole_feature.as_m();
    let max_norm_rad = o.max_normal_error.as_radians();
    let cos_max_norm = blaze_math::cos(max_norm_rad);
    // Sliver area floor so microscopic corner artifacts don't trigger false bevel alarms.
    let min_tri_area = o.min_bevel_area_m2.unwrap_or_else(|| (min_edge * min_edge * 0.01).min(2.5e-7));

    let mut max_outside = 0.0f64;
    let mut max_inside = 0.0f64;
    let mut max_normal_rad = 0.0f64;
    let mut normal_area_sum = 0.0f64;
    let mut area_sum = 0.0f64;
    let mut bridged_void_samples = 0u32;
    let mut bevel_violation_samples = 0u32;
    let mut subdivisions = 0u64;
    let mut budget_hit = false;

    let mut worst_hole: Option<(Vec3, f64)> = None;
    let mut worst_clip: Option<(Vec3, f64)> = None;
    let mut worst_bevel: Option<(Vec3, f64, f64)> = None;

    let mut stack: Vec<([Vec3; 3], u8), Alloc<'_>> = Vec::new_in(s);
    let skip_subdiv = plant.has(FidelityPlant::SkipVoidSubdivision);
    let skip_normal = plant.has(FidelityPlant::IgnoreNormalDeviation);

    let pos = mesh.positions();
    let nrms = mesh.normals();
    let has_nrms = nrms.len() == pos.len();
    for tri in mesh.triangles() {
        let (i0, i1, i2) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
        let a = to_v3(pos[i0]);
        let b = to_v3(pos[i1]);
        let c = to_v3(pos[i2]);
        let cross = (b - a).cross(c - a);
        let twice_area = cross.length();
        if !(twice_area > 0.0 && twice_area.is_finite()) {
            continue;
        }
        let tri_area = 0.5 * twice_area;
        let tri_n = cross * (1.0 / twice_area);
        let (n0, n1, n2) = if has_nrms {
            (
                to_v3(nrms[i0]).normalized().unwrap_or(tri_n),
                to_v3(nrms[i1]).normalized().unwrap_or(tri_n),
                to_v3(nrms[i2]).normalized().unwrap_or(tri_n),
            )
        } else {
            (tri_n, tri_n, tri_n)
        };
        let shade_at = |p: Vec3| -> Vec3 {
            if !has_nrms {
                return tri_n;
            }
            let (w0, w1, w2) = barycentric(a, b, c, p);
            (n0 * w0 + n1 * w1 + n2 * w2).normalized().unwrap_or(tri_n)
        };
        let centroid = (a + b + c) * (1.0 / 3.0);

        // 1) Baseline centroid sample (position + CAD gradient normal vs exported shading normal).
        let (f_c, g_c) = eval_grad(&tape, centroid, &mut sc);
        if f_c.is_finite() {
            if f_c > 0.0 {
                max_outside = max_outside.max(f_c);
                if f_c > hole_tol {
                    bridged_void_samples += 1;
                    if worst_hole.is_none_or(|(_, d)| f_c > d) {
                        worst_hole = Some((centroid, f_c));
                    }
                }
            } else {
                let depth = -f_c;
                max_inside = max_inside.max(depth);
                if depth > surf_tol && worst_clip.is_none_or(|(_, d)| depth > d) {
                    worst_clip = Some((centroid, depth));
                }
            }
        }
        if !skip_normal
            && let Some(n_cad) = g_c.normalized()
        {
            let n_mesh = (n0 + n1 + n2).normalized().unwrap_or(tri_n);
            let cos_a = n_mesh.dot(n_cad).clamp(-1.0, 1.0);
            let ang = blaze_math::acos(cos_a);
            area_sum += tri_area;
            normal_area_sum += ang * tri_area;
            if tri_area >= min_tri_area {
                if cos_a < cos_max_norm {
                    // Guard against C0 sharp-crease gradient discontinuity right next to a valid
                    // 90-degree edge: verify at an inset point along the triangle's own plane.
                    let confirmed_ang = confirm_normal_mismatch(&tape, &shade_at, a, b, c, centroid, ang, &mut sc);
                    max_normal_rad = max_normal_rad.max(confirmed_ang);
                    if confirmed_ang > max_norm_rad {
                        bevel_violation_samples += 1;
                        if worst_bevel.is_none_or(|(_, a0, _)| confirmed_ang > a0) {
                            worst_bevel = Some((centroid, confirmed_ang, f_c.abs()));
                        }
                    }
                } else {
                    max_normal_rad = max_normal_rad.max(ang);
                }
            }
        }

        if skip_subdiv {
            continue;
        }

        // 2) Fast 1-Lipschitz ball certificate: because every blaze-cad-field tape satisfies
        // `|f(p) - f(q)| <= |p - q|`, if `f(c) + R_tri <= pos_bound` and `f(c) - R_tri >= -surf_tol`
        // then NO point anywhere on the triangle can violate any check!
        let pos_bound = if skip_normal { hole_tol } else { hole_tol.min(bevel_probe) };
        let r_tri = tri_radius(a, b, c, centroid);
        if f_c.is_finite() && f_c + r_tri <= pos_bound && f_c - r_tri >= -surf_tol {
            continue;
        }

        // 3) Interval-certified triangle subdivision: proves NO bridged hole or flattened bevel
        // hides anywhere inside even a giant wall triangle.
        let tri_box = tri_aabb(a, b, c);
        let (iv, _) = eval_box(&tape, &tri_box, &mut sc);
        if iv.hi() <= pos_bound && iv.lo() >= -surf_tol {
            continue;
        }
        let max_e = max_tri_edge(a, b, c);
        if max_e <= min_edge {
            continue;
        }

        stack.clear();
        stack.try_push(([a, b, c], 0u8)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;

        while let Some(([p0, p1, p2], depth)) = stack.pop() {
            if depth > 0 {
                subdivisions += 1;
                if subdivisions > o.most_subdivisions {
                    budget_hit = true;
                    break;
                }
                let sb = tri_aabb(p0, p1, p2);
                let (siv, _) = eval_box(&tape, &sb, &mut sc);
                if siv.hi() <= pos_bound && siv.lo() >= -surf_tol {
                    continue;
                }
                let mid = (p0 + p1 + p2) * (1.0 / 3.0);
                let f_m = eval_point(&tape, mid, &mut sc);
                if f_m.is_finite() {
                    let r_sub = tri_radius(p0, p1, p2, mid);
                    if f_m + r_sub <= pos_bound && f_m - r_sub >= -surf_tol {
                        continue;
                    }
                    if f_m > 0.0 {
                        max_outside = max_outside.max(f_m);
                        if f_m > hole_tol {
                            bridged_void_samples += 1;
                            if worst_hole.is_none_or(|(_, d)| f_m > d) {
                                worst_hole = Some((mid, f_m));
                            }
                        }
                    } else {
                        let d_in = -f_m;
                        max_inside = max_inside.max(d_in);
                        if d_in > surf_tol && worst_clip.is_none_or(|(_, d)| d_in > d) {
                            worst_clip = Some((mid, d_in));
                        }
                    }
                    // Catch lost convex bevels (sharp edge protruding into empty air, `f_m > bevel_probe`)
                    // and lost concave fillets (chord bridging empty air, `f_m > bevel_probe`) along
                    // edges of large triangles without falsely flagging valid inward chords of convex
                    // fillets (whose centroids already pass the exact centroid normal test above).
                    if !skip_normal && f_m > bevel_probe {
                        let (_, gm) = eval_grad(&tape, mid, &mut sc);
                        if let Some(n_cad) = gm.normalized() {
                            let n_mesh = shade_at(mid);
                            let cos_a = n_mesh.dot(n_cad).clamp(-1.0, 1.0);
                            let ang = blaze_math::acos(cos_a);
                            let sub_area = 0.5 * (p1 - p0).cross(p2 - p0).length();
                            if sub_area >= min_tri_area && cos_a < cos_max_norm {
                                let confirmed_ang = confirm_normal_mismatch(&tape, &shade_at, p0, p1, p2, mid, ang, &mut sc);
                                max_normal_rad = max_normal_rad.max(confirmed_ang);
                                if confirmed_ang > max_norm_rad {
                                    bevel_violation_samples += 1;
                                    if worst_bevel.is_none_or(|(_, a0, _)| confirmed_ang > a0) {
                                        worst_bevel = Some((mid, confirmed_ang, f_m));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Longest-edge 2-way bisection: avoids the 4^d explosion on high-aspect-ratio
            // architectural triangles (e.g. tall cylinder flue or wall strips) while still
            // shrinking every sub-triangle's diameter below `min_edge`.
            if depth < 18 {
                let e01 = (p1 - p0).length();
                let e12 = (p2 - p1).length();
                let e20 = (p0 - p2).length();
                let max_e = e01.max(e12).max(e20);
                if max_e > min_edge {
                    let nd = depth + 1;
                    if e01 >= e12 && e01 >= e20 {
                        let m = (p0 + p1) * 0.5;
                        stack.try_push(([p0, m, p2], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                        stack.try_push(([m, p1, p2], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                    } else if e12 >= e20 {
                        let m = (p1 + p2) * 0.5;
                        stack.try_push(([p0, p1, m], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                        stack.try_push(([p0, m, p2], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                    } else {
                        let m = (p2 + p0) * 0.5;
                        stack.try_push(([p0, p1, m], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                        stack.try_push(([m, p1, p2], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                    }
                }
            }
        }
        if budget_hit {
            break;
        }
    }

    let mean_normal_rad = if area_sum > 0.0 { normal_area_sum / area_sum } else { 0.0 };
    let metrics = FidelityMetrics {
        max_outside_error: Length::m(max_outside),
        max_inside_error: Length::m(max_inside),
        max_normal_error: Angle::radians(max_normal_rad),
        mean_normal_error: Angle::radians(mean_normal_rad),
        holes: topo.genus,
        reference_holes: ref_topo.and_then(|r| r.genus),
        components: topo.components,
        bevel_retention,
        bridged_void_samples,
        bevel_violation_samples,
        subdivisions,
    };

    // Priority order of findings:
    // 1. Exact topological hole/component regression vs reference mesh
    // 2. Bridged / lost hole (`f(p) > hole_tolerance`)
    // 3. Lost / flattened bevel (normal deviation or bevel-strip retention drop)
    // 4. Shaved / clipped surface (`f(p) < -surface_tolerance`)
    // 5. Budget exhausted -> Unknown (never falsely Pass!)
    let verdict = if let (Some(rt), Some(exp_g), Some(act_g)) = (ref_topo, ref_topo.and_then(|r| r.genus), topo.genus)
        && act_g < exp_g
    {
        let _ = rt;
        Verdict::Found(FidelityFinding::TopologyHoleLost { expected_holes: exp_g, actual_holes: act_g })
    } else if let Some(rt) = ref_topo
        && topo.components != rt.components
    {
        Verdict::Found(FidelityFinding::ComponentCountChanged { expected: rt.components, actual: topo.components })
    } else if let Some((p, depth)) = worst_hole {
        Verdict::Found(FidelityFinding::LostHole {
            at: [Length::m(p.x), Length::m(p.y), Length::m(p.z)],
            void_depth: Length::m(depth),
            tolerance: o.hole_tolerance,
        })
    } else if let Some((p, ang, off)) = worst_bevel {
        Verdict::Found(FidelityFinding::LostBevel {
            at: [Length::m(p.x), Length::m(p.y), Length::m(p.z)],
            normal_error: Angle::radians(ang),
            max_normal_error: o.max_normal_error,
            surface_offset: Length::m(off),
            bevel_retention,
        })
    } else if let Some(ret) = bevel_retention
        && ret < (1.0 - o.max_bevel_loss.clamp(0.0, 1.0))
    {
        Verdict::Found(FidelityFinding::LostBevel {
            at: [Length::ZERO; 3],
            normal_error: Angle::radians(max_normal_rad),
            max_normal_error: o.max_normal_error,
            surface_offset: Length::m(max_outside.max(max_inside)),
            bevel_retention: Some(ret),
        })
    } else if let Some((p, depth)) = worst_clip {
        Verdict::Found(FidelityFinding::SurfaceClipped {
            at: [Length::m(p.x), Length::m(p.y), Length::m(p.z)],
            depth: Length::m(depth),
            tolerance: o.surface_tolerance,
        })
    } else if budget_hit {
        Verdict::Unknown(Unsettled::Budget { boxes: subdivisions })
    } else {
        Verdict::Pass
    };

    Ok(FidelityChecked { verdict, metrics })
}

#[inline]
fn to_v3(p: [f32; 3]) -> Vec3 {
    Vec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2]))
}

#[inline]
fn tri_aabb(a: Vec3, b: Vec3, c: Vec3) -> Aabb3 {
    Aabb3::new(a.min(b).min(c), a.max(b).max(c))
}

#[inline]
fn max_tri_edge(a: Vec3, b: Vec3, c: Vec3) -> f64 {
    (b - a).length().max((c - b).length()).max((a - c).length())
}

#[inline]
fn tri_radius(a: Vec3, b: Vec3, c: Vec3, mid: Vec3) -> f64 {
    (a - mid).length().max((b - mid).length()).max((c - mid).length())
}

/// Barycentric weights of `p` projected onto triangle `(a, b, c)`.
#[inline]
fn barycentric(a: Vec3, b: Vec3, c: Vec3, p: Vec3) -> (f64, f64, f64) {
    let v0 = b - a;
    let v1 = c - a;
    let v2 = p - a;
    let d00 = v0.dot(v0);
    let d01 = v0.dot(v1);
    let d11 = v1.dot(v1);
    let d20 = v2.dot(v0);
    let d21 = v2.dot(v1);
    let denom = d00 * d11 - d01 * d01;
    if !(denom.abs() > 1e-24 && denom.is_finite()) {
        return (1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0);
    }
    let v = ((d11 * d20 - d01 * d21) / denom).clamp(0.0, 1.0);
    let w = ((d00 * d21 - d01 * d20) / denom).clamp(0.0, 1.0);
    let u = (1.0 - v - w).clamp(0.0, 1.0);
    (u, v, w)
}

/// Guard against a C0 sharp-crease gradient discontinuity right on a valid 90-degree corner:
/// probe the 3 inset points `0.5 * (v_i + mid)` inside the triangle; on a true flat face
/// adjacent to a sharp crease, at least one inset point sits strictly on the face (small angle),
/// whereas on a genuinely lost bevel or flattened fillet all inset points disagree with the
/// exported mesh's shading normal.
fn confirm_normal_mismatch(
    tape: &blaze_cad_field::Tape<'_>,
    shade_at: &impl Fn(Vec3) -> Vec3,
    p0: Vec3,
    p1: Vec3,
    p2: Vec3,
    mid: Vec3,
    mid_ang: f64,
    sc: &mut Scratch<'_>,
) -> f64 {
    let mut best_ang = mid_ang;
    for v in [p0, p1, p2] {
        let q = (v + mid) * 0.5;
        let (_, gq) = eval_grad(tape, q, sc);
        if let Some(n_q) = gq.normalized() {
            let n_m = shade_at(q);
            let ang = blaze_math::acos(n_m.dot(n_q).clamp(-1.0, 1.0));
            best_ang = best_ang.min(ang);
        }
    }
    best_ang
}
