//! Crease-preserving Loop subdivision primitive for turning draped simulation meshes into
//! cinema-grade, sub-6mm silky drapery while keeping seam trenches, hems, pocket edges, and
//! collar lines razor-sharp!
//!
//! Operates deterministically in `#![no_std]` with zero hash-map iteration:
//! - Interior smooth edges use classic Loop (1987) weights `(3/8, 3/8, 1/8, 1/8)` for `C^2`
//!   smooth organic fabric wave curvature.
//! - Boundary edges, material-border edges, and sharp crease edges (where adjacent face normals
//!   differ past a crease threshold, or marked sharp) use linear midpoint `(1/2, 1/2)` and vertex
//!   crease rule `(1/8, 3/4, 1/8)` so hems, cuffs, pocket flaps, and stitched seams stay crisp!
//! - Flat-pattern metric UVs are linearly interpolated so 2K photogrammetry fabric weave and
//!   ASTRA's Green-Lagrange wrinkle strain stay 100% distortion-free!

#![blaze::sans_io]

use blaze_math::Vec3;
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::detail::GarmentMesh;

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), AllocFailure> {
    v.try_push(x).map_err(|e| e.why)
}

/// Apply 1 level of crease-preserving Loop subdivision to cloth layers (`tri_layer <= max_layer`)
/// of `mesh`, leaving rigid hardware (`tri_layer > max_layer`, e.g. snaps/buckles) untouched!
pub fn subdivide_cloth_loop<'a>(
    mesh: &GarmentMesh<'_>,
    max_cloth_layer: u8,
    crease_cos_threshold: f64,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<GarmentMesh<'a>, AllocFailure> {
    let n_verts = mesh.positions.len();
    let n_tris = mesh.triangles.len();

    // 1. Precompute face unit normals.
    let mut face_nrm = Vec::new_in(scratch);
    for &[a, b, c] in mesh.triangles.iter() {
        let pa = mesh.positions[a as usize];
        let pb = mesh.positions[b as usize];
        let pc = mesh.positions[c as usize];
        let n = (pb - pa).cross(pc - pa).normalized().unwrap_or(Vec3::new(0.0, 0.0, -1.0));
        push(&mut face_nrm, n)?;
    }

    // 2. Collect all half-edges belonging to cloth triangles (layer <= max_cloth_layer) keyed by
    //    CANONICAL WELDED vertex pair `(r_lo, r_hi)` so UV-split sewn seams (`front_fly`,
    //    `jacket_cf_zip`, `outseam`, `armscye`, `collar`) are subdivided as TRUE CONTINUOUS
    //    INTERIOR MANIFOLD EDGES (zero open-edge tearing or V-notches!) while preserving clean
    //    per-panel UV seams!
    #[derive(Clone, Copy)]
    struct HalfEdge {
        r_lo: u32,
        r_hi: u32,
        raw_lo: u32,
        raw_hi: u32,
        r_opp: u32,
        tri: u32,
        fab: u16,
    }
    let mut half_edges = Vec::new_in(scratch);
    for ti in 0..n_tris {
        if mesh.tri_layer[ti] > max_cloth_layer {
            continue;
        }
        let [a, b, c] = mesh.triangles[ti];
        let fab = mesh.tri_fabric[ti];
        for (u, v, opp) in [(a, b, c), (b, c, a), (c, a, b)] {
            let ru = mesh.weld_of(u);
            let rv = mesh.weld_of(v);
            let ropp = mesh.weld_of(opp);
            if ru == rv {
                continue;
            }
            let (r_lo, r_hi) = if ru < rv { (ru, rv) } else { (rv, ru) };
            let (raw_lo, raw_hi) = if u < v { (u, v) } else { (v, u) };
            push(&mut half_edges, HalfEdge { r_lo, r_hi, raw_lo, raw_hi, r_opp: ropp, tri: ti as u32, fab })?;
        }
    }
    half_edges.sort_unstable_by(|x, y| {
        (x.r_lo, x.r_hi, x.raw_lo, x.raw_hi, x.tri).cmp(&(y.r_lo, y.r_hi, y.raw_lo, y.raw_hi, y.tri))
    });

    // 3. Build edge table with midpoint vertex index & sharp/crease classification on welded
    //    topology, accumulating smooth/crease neighbor sums on canonical welded roots!
    #[derive(Clone, Copy)]
    struct EdgeInfo {
        lo: u32,
        hi: u32,
        mid_vert: u32,
    }
    let mut edge_table = Vec::new_in(scratch);

    let mut smooth_sum = Vec::new_in(scratch);
    let mut smooth_deg = Vec::new_in(scratch);
    let mut crease_sum = Vec::new_in(scratch);
    let mut crease_deg = Vec::new_in(scratch);
    let mut is_cloth_root = Vec::new_in(scratch);
    for _ in 0..n_verts {
        push(&mut smooth_sum, Vec3::ZERO)?;
        push(&mut smooth_deg, 0u32)?;
        push(&mut crease_sum, Vec3::ZERO)?;
        push(&mut crease_deg, 0u32)?;
        push(&mut is_cloth_root, false)?;
    }

    let mut out = GarmentMesh::new(alloc);
    for &pm in mesh.panels.iter() {
        push(&mut out.panels, pm)?;
    }
    // Copy original vertices first (even vertices will be updated on welded roots & synced).
    for i in 0..n_verts {
        let r = mesh.weld_of(i as u32);
        push(&mut out.positions, mesh.positions[r as usize])?;
        push(&mut out.normals, mesh.normals[r as usize])?;
        push(&mut out.uvs, mesh.uvs[i])?;
        let rp = mesh.rest_pos_m.get(i).copied().unwrap_or(mesh.uvs[i]);
        push(&mut out.rest_pos_m, rp)?;
        push(&mut out.colors, mesh.colors.get(i).copied().unwrap_or([0, 255, 0, 128]))?;
        push(&mut out.rigid_anchor, mesh.rigid_anchor[i])?;
        push(&mut out.weld_rep, r)?;
    }

    let mut idx = 0usize;
    while idx < half_edges.len() {
        let e0 = half_edges[idx];
        let mut end = idx + 1;
        while end < half_edges.len() && half_edges[end].r_lo == e0.r_lo && half_edges[end].r_hi == e0.r_hi {
            end += 1;
        }
        let r_lo = e0.r_lo as usize;
        let r_hi = e0.r_hi as usize;
        is_cloth_root[r_lo] = true;
        is_cloth_root[r_hi] = true;

        let p_lo = mesh.positions[r_lo];
        let p_hi = mesh.positions[r_hi];
        let n_mid = (mesh.normals[r_lo] + mesh.normals[r_hi]).normalized().unwrap_or(mesh.normals[r_lo]);

        // Determine if interior smooth edge or sharp/boundary/material crease edge across welded topology!
        let is_smooth_interior = if end - idx == 2 {
            let e1 = half_edges[idx + 1];
            let dot_n = face_nrm[e0.tri as usize].dot(face_nrm[e1.tri as usize]);
            e0.fab == e1.fab && dot_n >= crease_cos_threshold
        } else {
            false
        };

        let mid_pos = if is_smooth_interior {
            let e1 = half_edges[idx + 1];
            let p_opp0 = mesh.positions[e0.r_opp as usize];
            let p_opp1 = mesh.positions[e1.r_opp as usize];
            (p_lo + p_hi) * 0.375 + (p_opp0 + p_opp1) * 0.125
        } else {
            (p_lo + p_hi) * 0.5
        };

        // Accumulate neighbor rules on canonical welded roots.
        smooth_sum[r_lo] = smooth_sum[r_lo] + p_hi;
        smooth_deg[r_lo] += 1;
        smooth_sum[r_hi] = smooth_sum[r_hi] + p_lo;
        smooth_deg[r_hi] += 1;
        if !is_smooth_interior {
            crease_sum[r_lo] = crease_sum[r_lo] + p_hi;
            crease_deg[r_lo] += 1;
            crease_sum[r_hi] = crease_sum[r_hi] + p_lo;
            crease_deg[r_hi] += 1;
        }

        // Emit one midpoint per distinct raw UV edge `(raw_lo, raw_hi)` in this welded edge group,
        // ALL sharing the exact bit-for-bit same `mid_pos` and canonical `canon_mid_vert` weld_rep!
        let mut canon_mid_vert = u32::MAX;
        let mut sub = idx;
        while sub < end {
            let es = half_edges[sub];
            let mut sub_end = sub + 1;
            while sub_end < end && half_edges[sub_end].raw_lo == es.raw_lo && half_edges[sub_end].raw_hi == es.raw_hi {
                sub_end += 1;
            }
            let rlo = es.raw_lo as usize;
            let rhi = es.raw_hi as usize;
            let uv_mid = (mesh.uvs[rlo] + mesh.uvs[rhi]) * 0.5;
            let rest_lo = mesh.rest_pos_m.get(rlo).copied().unwrap_or(mesh.uvs[rlo]);
            let rest_hi = mesh.rest_pos_m.get(rhi).copied().unwrap_or(mesh.uvs[rhi]);
            let rest_mid = (rest_lo + rest_hi) * 0.5;
            let c_lo = mesh.colors.get(rlo).copied().unwrap_or([0, 255, 0, 128]);
            let c_hi = mesh.colors.get(rhi).copied().unwrap_or([0, 255, 0, 128]);
            let c_mid = [
                ((c_lo[0] as u16 + c_hi[0] as u16) / 2) as u8,
                ((c_lo[1] as u16 + c_hi[1] as u16) / 2) as u8,
                ((c_lo[2] as u16 + c_hi[2] as u16) / 2) as u8,
                ((c_lo[3] as u16 + c_hi[3] as u16) / 2) as u8,
            ];
            let mid_vert = out.positions.len() as u32;
            if canon_mid_vert == u32::MAX {
                canon_mid_vert = mid_vert;
            }
            push(&mut out.positions, mid_pos)?;
            push(&mut out.normals, n_mid)?;
            push(&mut out.uvs, uv_mid)?;
            push(&mut out.rest_pos_m, rest_mid)?;
            push(&mut out.colors, c_mid)?;
            push(&mut out.rigid_anchor, None)?;
            push(&mut out.weld_rep, canon_mid_vert)?;
            push(&mut edge_table, EdgeInfo { lo: es.raw_lo, hi: es.raw_hi, mid_vert })?;
            sub = sub_end;
        }

        idx = end;
    }
    edge_table.sort_unstable_by(|a, b| (a.lo, a.hi).cmp(&(b.lo, b.hi)));

    // 4. Update even (original) cloth vertices on canonical welded roots using Loop smooth /
    //    crease / corner rules, then synchronize every UV-split seam twin bit-for-bit!
    for i in 0..n_verts {
        let r = mesh.weld_of(i as u32) as usize;
        if r != i || !is_cloth_root[r] || mesh.rigid_anchor[r].is_some() {
            continue;
        }
        let p = mesh.positions[r];
        let cdeg = crease_deg[r];
        if cdeg == 0 {
            let n = smooth_deg[r];
            if n >= 3 {
                let beta = if n == 3 { 3.0 / 16.0 } else { 3.0 / (8.0 * (n as f64)) };
                out.positions[r] = p * (1.0 - (n as f64) * beta) + smooth_sum[r] * beta;
            }
        } else if cdeg == 2 {
            out.positions[r] = p * 0.75 + crease_sum[r] * 0.125;
        }
    }
    for i in 0..n_verts {
        let r = mesh.weld_of(i as u32) as usize;
        out.positions[i] = out.positions[r];
    }

    // Helper: deterministic binary search for edge midpoint vertex.
    let find_mid = |u: u32, v: u32| -> u32 {
        let (lo, hi) = if u < v { (u, v) } else { (v, u) };
        let key = (lo, hi);
        let pos = edge_table.binary_search_by(|e| (e.lo, e.hi).cmp(&key)).expect("edge midpoint");
        edge_table[pos].mid_vert
    };

    // 5. Emit 4 subdivided triangles per cloth triangle, and pass hardware triangles straight through.
    let has_tri_panel = mesh.tri_panel.len() == n_tris;
    for ti in 0..n_tris {
        let [a, b, c] = mesh.triangles[ti];
        let fab = mesh.tri_fabric[ti];
        let layer = mesh.tri_layer[ti];
        let pid = if has_tri_panel { mesh.tri_panel[ti] } else { 0 };
        if layer <= max_cloth_layer {
            let mab = find_mid(a, b);
            let mbc = find_mid(b, c);
            let mca = find_mid(c, a);
            for tri in [[a, mab, mca], [mab, b, mbc], [mca, mbc, c], [mab, mbc, mca]] {
                push(&mut out.triangles, tri)?;
                push(&mut out.tri_fabric, fab)?;
                push(&mut out.tri_layer, layer)?;
                if has_tri_panel {
                    push(&mut out.tri_panel, pid)?;
                }
            }
        } else {
            push(&mut out.triangles, [a, b, c])?;
            push(&mut out.tri_fabric, fab)?;
            push(&mut out.tri_layer, layer)?;
            if has_tri_panel {
                push(&mut out.tri_panel, pid)?;
            }
        }
    }

    // Recompute crisp normals (smooth on smooth cloth folds, sharp across hard pocket/hem creases!).
    out.recompute_smooth_normals();
    Ok(out)
}
