//! ARCSim-style curvature- and compression-driven adaptive remeshing + fold-ridge edge flips
//! (Narain, Samii & O'Brien 2012) in deterministic `#![no_std]` sans-IO Rust.
//!
//! 1. **Fold-Ridge Edge Flips ([`flip_fold_edges`])**: flips interior manifold same-panel edges
//!    `(v0, v1)` to `(v2, v3)` when the opposite diagonal aligns better with the local 3D drapery
//!    fold ridge (lower dihedral bend angle or markedly improved 2D triangle aspect quality) while
//!    strictly preserving 2D `rest_uv_m` quad convexity, positive oriented area, and manifoldness.
//! 2. **Adaptive Conformal Refinement ([`adaptive_refine_cloth`])**: scores interior edges by
//!    dihedral curvature and Green-Lagrange / engineering compression strain
//!    `1.0 - |e_3d| / |e_2d|`, then executes deterministic conformal 1-to-2, 1-to-3, and 1-to-4
//!    red-green triangle splits with zero T-junctions, cleanly interpolating all vertex attributes
//!    and rebuilding `tri_dm_inv`, `tri_rest_area`, pin-preserving lumped `inv_mass`, bending
//!    hinges, and smooth normals.

#![blaze::sans_io]

use blaze_math::{Vec2, Vec3, atan2};
use blaze_own::{Alloc, Vec};
use blaze_units::Length;

use crate::mesh2d::{ClothMesh, Mesh2DError, rebuild_bending_hinges, recompute_normals};
use crate::pattern::PatternSet;

/// Parameters controlling ARCSim-style adaptive cloth refinement and fold-ridge edge flips.
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct RemeshOpts {
    /// Minimum allowed 2D rest edge length eligible for splitting (default `5.0 mm`).
    pub min_edge_len: Length = Length::nm(5_000_000),
    /// Dihedral bend angle threshold in radians above which an edge is refined (default `0.28 rad`).
    pub curvature_rad_threshold: f64 = 0.28,
    /// Compression strain threshold `1.0 - |e_3d| / |e_2d|` above which an edge is refined
    /// (default `0.06` = 6% buckling compression).
    pub compression_threshold: f64 = 0.06,
    /// Maximum number of interior edges split in one refinement pass (default `256`).
    pub max_splits: u32 = 256,
    /// Whether to run deterministic fold-ridge edge flipping alongside refinement (default `true`).
    pub enable_fold_flips: bool = true,
}

/// Summary report returned by [`adaptive_refine_cloth`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RemeshReport {
    /// Number of interior manifold edges flipped to align with fold ridges.
    pub edges_flipped: u32,
    /// Number of interior edges split.
    pub edges_split: u32,
    /// Number of newly created midpoint vertices appended to `mesh`.
    pub new_verts: u32,
    /// Net number of newly added triangles in `mesh.triangles`.
    pub new_tris: u32,
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), Mesh2DError> {
    v.try_push(x).map_err(|e| Mesh2DError::Alloc(e.why))
}

#[derive(Clone, Copy)]
struct RawHalfEdge {
    lo: u32,
    hi: u32,
    v0: u32,
    v1: u32,
    opp: u32,
    tri: u32,
}

/// Unsigned dihedral bend angle in `[0, pi]` between triangles `(p0, p1, p2)` and `(p1, p0, p3)`.
#[inline]
fn dihedral_angle_abs(p0: Vec3, p1: Vec3, p2: Vec3, p3: Vec3) -> f64 {
    let e = p1 - p0;
    let elen = e.length();
    if elen <= 1e-12 {
        return 0.0;
    }
    let e_hat = e * (1.0 / elen);
    let n1 = e.cross(p2 - p0);
    let n2 = (p3 - p0).cross(e);
    let l1 = n1.length();
    let l2 = n2.length();
    if l1 <= 1e-14 || l2 <= 1e-14 {
        return 0.0;
    }
    let n1_hat = n1 * (1.0 / l1);
    let n2_hat = n2 * (1.0 / l2);
    let sin_t = n1_hat.cross(n2_hat).dot(e_hat);
    let cos_t = n1_hat.dot(n2_hat).clamp(-1.0, 1.0);
    atan2(sin_t, cos_t).abs()
}

/// Scale-invariant 2D triangle aspect distortion (`1.0` for equilateral, larger for skinny).
#[inline]
fn tri_aspect_distortion_2d(a: Vec2, b: Vec2, c: Vec2) -> f64 {
    let e0 = b - a;
    let e1 = c - b;
    let e2 = a - c;
    let area2 = e0.cross(c - a).abs();
    if area2 <= 1e-14 {
        return 1e9;
    }
    let perim_sq = e0.dot(e0) + e1.dot(e1) + e2.dot(e2);
    // For an equilateral triangle of side s: perim_sq = 3 s^2, area2 = sqrt(3)/2 s^2 -> ratio = 2*sqrt(3).
    perim_sq / (area2 * 3.464_101_615_137_754_4)
}

/// Recompute `tri_dm_inv`, `tri_rest_area`, and welded lumped `inv_mass` while strictly preserving
/// all pinned (`inv_mass == 0.0`) vertices.
fn recompute_rest_mechanics_preserving_pins(
    mesh: &mut ClothMesh<'_>,
    patterns: &PatternSet<'_>,
    scratch: Alloc<'_>,
) -> Result<(), Mesh2DError> {
    let n_verts = mesh.pos.len();
    let mut root_pinned: Vec<bool, Alloc<'_>> = Vec::new_in(scratch);
    let mut root_mass: Vec<f64, Alloc<'_>> = Vec::new_in(scratch);
    for _ in 0..n_verts {
        push(&mut root_pinned, false)?;
        push(&mut root_mass, 0.0f64)?;
    }
    for i in 0..n_verts {
        if mesh.inv_mass[i] <= 0.0 {
            let r = mesh.weld_rep[i] as usize;
            root_pinned[r] = true;
            root_pinned[i] = true;
        }
    }

    mesh.tri_dm_inv.clear();
    mesh.tri_rest_area.clear();

    for (ti, &[a, b, c]) in mesh.triangles.iter().enumerate() {
        let ua = mesh.rest_uv_m[a as usize];
        let ub = mesh.rest_uv_m[b as usize];
        let uc = mesh.rest_uv_m[c as usize];
        let d1 = ub - ua;
        let d2 = uc - ua;
        let det = d1.x * d2.y - d2.x * d1.y;
        let area = 0.5 * det.abs();
        let dm_inv = if det.abs() > 1e-12 {
            let inv_det = 1.0 / det;
            [d2.y * inv_det, -d2.x * inv_det, -d1.y * inv_det, d1.x * inv_det]
        } else {
            [0.0; 4]
        };
        push(&mut mesh.tri_dm_inv, dm_inv)?;
        push(&mut mesh.tri_rest_area, area)?;

        let fab_idx = mesh.tri_fabric[ti] as usize;
        let density = patterns.fabrics.get(fab_idx).map(|f| f.density_kg_m2()).unwrap_or(0.32);
        let tri_mass = (area * density).max(1e-6);
        let v_mass = tri_mass / 3.0;
        let ra = mesh.weld_rep[a as usize] as usize;
        let rb = mesh.weld_rep[b as usize] as usize;
        let rc = mesh.weld_rep[c as usize] as usize;
        root_mass[ra] += v_mass;
        root_mass[rb] += v_mass;
        root_mass[rc] += v_mass;
    }

    for i in 0..n_verts {
        let r = mesh.weld_rep[i] as usize;
        if root_pinned[r] || root_pinned[i] || root_mass[r] <= 1e-12 {
            mesh.inv_mass[i] = 0.0;
        } else {
            mesh.inv_mass[i] = 1.0 / root_mass[r];
        }
    }
    Ok(())
}

/// Collect raw per-triangle half-edges sorted deterministically by `(lo, hi, tri)`.
fn collect_raw_half_edges<'s>(
    mesh: &ClothMesh<'_>,
    scratch: Alloc<'s>,
) -> Result<Vec<RawHalfEdge, Alloc<'s>>, Mesh2DError> {
    let mut edges = Vec::new_in(scratch);
    for (ti, &[a, b, c]) in mesh.triangles.iter().enumerate() {
        for (v0, v1, opp) in [(a, b, c), (b, c, a), (c, a, b)] {
            let (lo, hi) = if v0 < v1 { (v0, v1) } else { (v1, v0) };
            push(&mut edges, RawHalfEdge { lo, hi, v0, v1, opp, tri: ti as u32 })?;
        }
    }
    edges.sort_unstable_by(|x, y| (x.lo, x.hi, x.tri).cmp(&(y.lo, y.hi, y.tri)));
    Ok(edges)
}

#[inline]
fn raw_edge_exists(edges: &[RawHalfEdge], u: u32, v: u32) -> bool {
    let key = if u < v { (u, v) } else { (v, u) };
    let idx = edges.partition_point(|e| (e.lo, e.hi) < key);
    idx < edges.len() && (edges[idx].lo, edges[idx].hi) == key
}

/// Deterministically flip interior manifold edges `(v0, v1)` shared by two same-panel, same-fabric
/// triangles `(v0, v1, v2)` and `(v1, v0, v3)` when the opposite diagonal `(v2, v3)` aligns better
/// with the local fold ridge or significantly improves 2D aspect distortion while keeping a
/// strictly convex 2D `rest_uv_m` quad with positive oriented area.
pub fn flip_fold_edges(
    mesh: &mut ClothMesh<'_>,
    patterns: &PatternSet<'_>,
    scratch: Alloc<'_>,
) -> Result<u32, Mesh2DError> {
    let n_tris = mesh.triangles.len();
    if n_tris < 2 {
        return Ok(0);
    }

    let edges = collect_raw_half_edges(mesh, scratch)?;
    let mut tri_touched: Vec<bool, Alloc<'_>> = Vec::new_in(scratch);
    for _ in 0..n_tris {
        push(&mut tri_touched, false)?;
    }

    let mut flipped = 0u32;
    let mut i = 0usize;
    while i < edges.len() {
        let e0 = edges[i];
        let mut end = i + 1;
        while end < edges.len() && edges[end].lo == e0.lo && edges[end].hi == e0.hi {
            end += 1;
        }
        // Only interior manifold raw edges shared by exactly two triangles with opposite winding.
        if end - i == 2 {
            let e1 = edges[i + 1];
            let t0 = e0.tri as usize;
            let t1 = e1.tri as usize;
            if !tri_touched[t0]
                && !tri_touched[t1]
                && e0.v0 == e1.v1
                && e0.v1 == e1.v0
                && e0.opp != e1.opp
                && mesh.tri_panel[t0] == mesh.tri_panel[t1]
                && mesh.tri_fabric[t0] == mesh.tri_fabric[t1]
            {
                let v0 = e0.v0;
                let v1 = e0.v1;
                let v2 = e0.opp;
                let v3 = e1.opp;

                // Never introduce a duplicate edge that already exists in the mesh.
                if !raw_edge_exists(&edges, v2, v3) {
                    let u0 = mesh.rest_uv_m[v0 as usize];
                    let u1 = mesh.rest_uv_m[v1 as usize];
                    let u2 = mesh.rest_uv_m[v2 as usize];
                    let u3 = mesh.rest_uv_m[v3 as usize];

                    // Preserve exact panel winding orientation (`flip_normal` panels have negative 2D det).
                    let area_old0 = (u1 - u0).cross(u2 - u0);
                    let area_old1 = (u0 - u1).cross(u3 - u1);
                    let sign = if area_old0 >= 0.0 { 1.0 } else { -1.0 };
                    let total_area = area_old0.abs() + area_old1.abs();

                    if area_old0 * sign > 1e-12 && area_old1 * sign > 1e-12 {
                        // Proposed flipped triangles: [v0, v3, v2] and [v1, v2, v3].
                        let area_new0 = (u3 - u0).cross(u2 - u0);
                        let area_new1 = (u2 - u1).cross(u3 - u1);
                        let min_area = (0.15 * total_area).max(1e-12);
                        if area_new0 * sign > min_area && area_new1 * sign > min_area {
                            let p0 = mesh.pos[mesh.weld_rep[v0 as usize] as usize];
                            let p1 = mesh.pos[mesh.weld_rep[v1 as usize] as usize];
                            let p2 = mesh.pos[mesh.weld_rep[v2 as usize] as usize];
                            let p3 = mesh.pos[mesh.weld_rep[v3 as usize] as usize];

                            let theta_cur = dihedral_angle_abs(p0, p1, p2, p3);
                            let theta_new = dihedral_angle_abs(p2, p3, p1, p0);

                            let aspect_old =
                                tri_aspect_distortion_2d(u0, u1, u2).max(tri_aspect_distortion_2d(u1, u0, u3));
                            let aspect_new =
                                tri_aspect_distortion_2d(u0, u3, u2).max(tri_aspect_distortion_2d(u1, u2, u3));

                            let fold_better =
                                theta_cur > 0.16 && (theta_new + 0.06 < theta_cur) && aspect_new < 3.5;
                            let shape_better =
                                (aspect_new < aspect_old * 0.80) && (theta_new <= theta_cur + 0.12);

                            if fold_better || shape_better {
                                mesh.triangles[t0] = [v0, v3, v2];
                                mesh.triangles[t1] = [v1, v2, v3];
                                tri_touched[t0] = true;
                                tri_touched[t1] = true;
                                flipped += 1;
                            }
                        }
                    }
                }
            }
        }
        i = end;
    }

    if flipped > 0 {
        recompute_rest_mechanics_preserving_pins(mesh, patterns, scratch)?;
        rebuild_bending_hinges(mesh, patterns)?;
        recompute_normals(mesh);
    }

    Ok(flipped)
}

/// ARCSim-style curvature- and compression-driven adaptive cloth refinement with conformal
/// red-green 1-to-2, 1-to-3, and 1-to-4 triangle splits and optional fold-ridge edge flips.
///
/// Appends new midpoint vertices after existing panel grid vertices so `PanelSlice::grid_vertex`
/// and boundary seam topology remain intact, then rebuilds rest mechanics (`tri_dm_inv`,
/// `tri_rest_area`, pin-preserving lumped `inv_mass`), bending hinges, and smooth normals.
pub fn adaptive_refine_cloth(
    mesh: &mut ClothMesh<'_>,
    patterns: &PatternSet<'_>,
    opts: RemeshOpts,
    scratch: Alloc<'_>,
) -> Result<RemeshReport, Mesh2DError> {
    let orig_verts = mesh.pos.len() as u32;
    let orig_tris = mesh.triangles.len() as u32;

    if orig_tris == 0 {
        return Ok(RemeshReport::default());
    }

    let min_len = opts.min_edge_len.as_m().max(1e-5);
    let curv_thr = opts.curvature_rad_threshold.max(1e-4);
    let comp_thr = opts.compression_threshold.max(1e-4);

    let mut edges_split = 0u32;
    let mut new_verts = 0u32;
    let mut new_tris = 0u32;

    if opts.max_splits > 0 {
        let half_edges = collect_raw_half_edges(mesh, scratch)?;

        #[derive(Clone, Copy)]
        struct SplitCandidate {
            lo: u32,
            hi: u32,
            score: f64,
        }

        let mut candidates: Vec<SplitCandidate, Alloc<'_>> = Vec::new_in(scratch);

        let mut i = 0usize;
        while i < half_edges.len() {
            let e0 = half_edges[i];
            let mut end = i + 1;
            while end < half_edges.len() && half_edges[end].lo == e0.lo && half_edges[end].hi == e0.hi {
                end += 1;
            }
            // Only split interior manifold intra-panel edges (preserves 1-to-1 welded seam pairs
            // and open border grid loops without T-junctions).
            if end - i == 2 {
                let e1 = half_edges[i + 1];
                let t0 = e0.tri as usize;
                let t1 = e1.tri as usize;
                let lo = e0.lo as usize;
                let hi = e0.hi as usize;
                let same_panel = mesh.tri_panel[t0] == mesh.tri_panel[t1]
                    && mesh.vert_panel[lo] == mesh.tri_panel[t0]
                    && mesh.vert_panel[hi] == mesh.tri_panel[t0];
                let pslice = mesh.panels[mesh.tri_panel[t0] as usize];
                let is_narrow_band = pslice.nu.min(pslice.nv) <= 6;
                let is_border_edge = mesh.is_open_border[lo] && mesh.is_open_border[hi];
                if same_panel && !is_narrow_band && !is_border_edge && e0.v0 == e1.v1 && e0.v1 == e1.v0 && e0.opp != e1.opp {
                    let len_2d = (mesh.rest_uv_m[hi] - mesh.rest_uv_m[lo]).length();
                    if len_2d > min_len {
                        let r0 = mesh.weld_rep[e0.v0 as usize] as usize;
                        let r1 = mesh.weld_rep[e0.v1 as usize] as usize;
                        let r2 = mesh.weld_rep[e0.opp as usize] as usize;
                        let r3 = mesh.weld_rep[e1.opp as usize] as usize;
                        let p0 = mesh.pos[r0];
                        let p1 = mesh.pos[r1];
                        let p2 = mesh.pos[r2];
                        let p3 = mesh.pos[r3];

                        let len_3d = (p1 - p0).length();
                        let compression = 1.0 - (len_3d / len_2d);
                        let theta = dihedral_angle_abs(p0, p1, p2, p3);

                        if theta > curv_thr || compression > comp_thr {
                            // Longest-edge-weighted curvature/compression priority.
                            let metric = (theta / curv_thr).max(compression / comp_thr);
                            let score = metric * len_2d;
                            push(&mut candidates, SplitCandidate { lo: e0.lo, hi: e0.hi, score })?;
                        }
                    }
                }
            }
            i = end;
        }

        // Deterministic sort: highest score first, ties broken canonically by (lo, hi).
        candidates.sort_unstable_by(|a, b| b.score.total_cmp(&a.score).then_with(|| (a.lo, a.hi).cmp(&(b.lo, b.hi))));
        let split_count = candidates.len().min(opts.max_splits as usize);
        candidates.truncate(split_count);

        if !candidates.is_empty() {
            // Sort selected edges by (lo, hi) for deterministic O(log E) binary lookup.
            candidates.sort_unstable_by(|a, b| (a.lo, a.hi).cmp(&(b.lo, b.hi)));

            #[derive(Clone, Copy)]
            struct SplitEdge {
                lo: u32,
                hi: u32,
                mid: u32,
            }
            let mut split_table: Vec<SplitEdge, Alloc<'_>> = Vec::new_in(scratch);

            // Create midpoint vertices cleanly interpolating all ClothMesh vertex attributes.
            for cand in candidates.iter() {
                let lo = cand.lo as usize;
                let hi = cand.hi as usize;
                let r_lo = mesh.weld_rep[lo] as usize;
                let r_hi = mesh.weld_rep[hi] as usize;

                let mid_id = mesh.pos.len() as u32;
                let p_mid = (mesh.pos[r_lo] + mesh.pos[r_hi]) * 0.5;
                let init_mid = (mesh.init_pos[lo] + mesh.init_pos[hi]) * 0.5;
                let n_mid = (mesh.normals[r_lo] + mesh.normals[r_hi])
                    .normalized()
                    .unwrap_or(mesh.normals[r_lo]);
                let uv_mid = (mesh.rest_uv_m[lo] + mesh.rest_uv_m[hi]) * 0.5;
                let st_mid = (mesh.panel_st[lo] + mesh.panel_st[hi]) * 0.5;
                let panel = mesh.vert_panel[lo];
                let fabric = mesh.vert_fabric[lo];
                let seam_depth = (mesh.seam_depth_m[lo] + mesh.seam_depth_m[hi]) * 0.5;
                let open_border = mesh.is_open_border[lo] && mesh.is_open_border[hi];
                let inv_m = if mesh.inv_mass[r_lo] <= 0.0 && mesh.inv_mass[r_hi] <= 0.0 { 0.0 } else { 1.0 };

                push(&mut mesh.pos, p_mid)?;
                push(&mut mesh.init_pos, init_mid)?;
                push(&mut mesh.normals, n_mid)?;
                push(&mut mesh.rest_uv_m, uv_mid)?;
                push(&mut mesh.panel_st, st_mid)?;
                push(&mut mesh.vert_panel, panel)?;
                push(&mut mesh.inv_mass, inv_m)?;
                push(&mut mesh.vert_fabric, fabric)?;
                push(&mut mesh.weld_rep, mid_id)?;
                push(&mut mesh.seam_depth_m, seam_depth)?;
                push(&mut mesh.is_open_border, open_border)?;

                push(&mut split_table, SplitEdge { lo: cand.lo, hi: cand.hi, mid: mid_id })?;
            }

            let find_mid = |u: u32, v: u32| -> Option<u32> {
                let key = if u < v { (u, v) } else { (v, u) };
                split_table
                    .binary_search_by(|e| (e.lo, e.hi).cmp(&key))
                    .ok()
                    .map(|idx| split_table[idx].mid)
            };

            // Rebuild triangle arrays conformally (1-to-1, 1-to-2 green, 1-to-3 conformal, or 1-to-4 red).
            let mut out_tris: Vec<[u32; 3], Alloc<'_>> = Vec::new_in(scratch);
            let mut out_panel: Vec<u16, Alloc<'_>> = Vec::new_in(scratch);
            let mut out_fabric: Vec<u16, Alloc<'_>> = Vec::new_in(scratch);

            let mut emit = |tri: [u32; 3], p: u16, f: u16| -> Result<(), Mesh2DError> {
                push(&mut out_tris, tri)?;
                push(&mut out_panel, p)?;
                push(&mut out_fabric, f)?;
                Ok(())
            };

            for ti in 0..mesh.triangles.len() {
                let [a, b, c] = mesh.triangles[ti];
                let p = mesh.tri_panel[ti];
                let f = mesh.tri_fabric[ti];
                let mab = find_mid(a, b);
                let mbc = find_mid(b, c);
                let mca = find_mid(c, a);

                match (mab, mbc, mca) {
                    (None, None, None) => {
                        emit([a, b, c], p, f)?;
                    }
                    // 1-to-2 green split
                    (Some(m), None, None) => {
                        emit([a, m, c], p, f)?;
                        emit([m, b, c], p, f)?;
                    }
                    (None, Some(m), None) => {
                        emit([a, b, m], p, f)?;
                        emit([a, m, c], p, f)?;
                    }
                    (None, None, Some(m)) => {
                        emit([a, b, m], p, f)?;
                        emit([b, c, m], p, f)?;
                    }
                    // 1-to-3 conformal split (corner triangle + shorter-diagonal split of remaining quad)
                    (Some(m0), Some(m1), None) => {
                        emit([m0, b, m1], p, f)?;
                        emit_conformal_quad(&mesh.rest_uv_m, a, m0, m1, c, p, f, &mut emit)?;
                    }
                    (None, Some(m1), Some(m2)) => {
                        emit([m1, c, m2], p, f)?;
                        emit_conformal_quad(&mesh.rest_uv_m, a, b, m1, m2, p, f, &mut emit)?;
                    }
                    (Some(m0), None, Some(m2)) => {
                        emit([a, m0, m2], p, f)?;
                        emit_conformal_quad(&mesh.rest_uv_m, m0, b, c, m2, p, f, &mut emit)?;
                    }
                    // 1-to-4 red split
                    (Some(m0), Some(m1), Some(m2)) => {
                        emit([a, m0, m2], p, f)?;
                        emit([m0, b, m1], p, f)?;
                        emit([m1, c, m2], p, f)?;
                        emit([m0, m1, m2], p, f)?;
                    }
                }
            }

            mesh.triangles.clear();
            mesh.tri_panel.clear();
            mesh.tri_fabric.clear();
            for idx in 0..out_tris.len() {
                push(&mut mesh.triangles, out_tris[idx])?;
                push(&mut mesh.tri_panel, out_panel[idx])?;
                push(&mut mesh.tri_fabric, out_fabric[idx])?;
            }

            edges_split = split_table.len() as u32;
            new_verts = (mesh.pos.len() as u32).saturating_sub(orig_verts);
            new_tris = (mesh.triangles.len() as u32).saturating_sub(orig_tris);

            recompute_rest_mechanics_preserving_pins(mesh, patterns, scratch)?;
            rebuild_bending_hinges(mesh, patterns)?;
            recompute_normals(mesh);
        }
    }

    let edges_flipped = if opts.enable_fold_flips {
        flip_fold_edges(mesh, patterns, scratch)?
    } else {
        0
    };

    Ok(RemeshReport { edges_flipped, edges_split, new_verts, new_tris })
}

/// Split convex quad `(q0, q1, q2, q3)` (CCW order) along the shorter 2D rest-UV diagonal so
/// 1-to-3 conformal splits never create degenerate slivers.
#[allow(clippy::too_many_arguments)]
fn emit_conformal_quad<F>(
    rest_uv: &[Vec2],
    q0: u32,
    q1: u32,
    q2: u32,
    q3: u32,
    panel: u16,
    fabric: u16,
    emit: &mut F,
) -> Result<(), Mesh2DError>
where
    F: FnMut([u32; 3], u16, u16) -> Result<(), Mesh2DError>,
{
    let d02 = rest_uv[q2 as usize] - rest_uv[q0 as usize];
    let d13 = rest_uv[q3 as usize] - rest_uv[q1 as usize];
    if d02.dot(d02) <= d13.dot(d13) {
        emit([q0, q1, q2], panel, fabric)?;
        emit([q0, q2, q3], panel, fabric)?;
    } else {
        emit([q0, q1, q3], panel, fabric)?;
        emit([q1, q2, q3], panel, fabric)?;
    }
    Ok(())
}
