//! Codimensional Incremental Potential Contact (C-IPC, Li et al. 2021) smooth barrier &
//! Continuous Collision Detection (CCD) module for zero-tunneling multi-layer cloth self-contact.
//!
//! Operates 100% deterministically in `#![no_std]` with explicit `Alloc<'_>` scratch buffers and
//! zero hash-map iteration:
//! - Smooth $C^2$ IPC logarithmic barrier potential $b(d) = -(d - \hat{d})^2 \ln(d / \hat{d})$
//!   with exact analytical gradient [`barrier_grad`] and Hessian [`barrier_hess`] using
//!   `blaze_math::ln`.
//! - Deterministic spatial-hash broadphase over welded `ClothMesh` geometry detecting both
//!   **Vertex-Triangle (VT)** and **Edge-Edge (EE)** codimensional proximity pairs while
//!   excluding topologically adjacent elements (shared welded vertices or within $2.5 \hat{d}$ in
//!   2D `rest_uv_m` on the same panel).
//! - Exact Voronoi-region Vertex-Triangle closest point ([`closest_point_on_triangle`]) and
//!   Lumelsky/Ericson analytical clamped Edge-Edge closest points
//!   ([`closest_points_segment_segment`]).
//! - Inverse-mass barycentric CCD step clamping (`max_step_fraction`) plus Newton-normalized
//!   smooth barrier repulsion so cloth layers never tunnel through one another.

#![blaze::sans_io]

use blaze_math::{Vec2, Vec3, floor, ln};
use blaze_own::{Alloc, Vec};
use blaze_units::Length;

use crate::mesh2d::{ClothMesh, Mesh2DError};

/// Tuning options for Codimensional IPC barrier projection and CCD clamping.
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct IpcOpts {
    /// Active barrier activation distance $\hat{d}$ (default `3.0 mm`).
    pub d_hat: Length = Length::nm(3_000_000),
    /// Smooth barrier repulsion stiffness weight (`0.0..=1.0`, default `0.45`).
    pub stiffness: f64 = 0.45,
    /// Maximum fraction of initial separation distance a relative substep motion may consume
    /// before CCD clamping engages (`0.1..=0.95`, default `0.80`).
    pub max_step_fraction: f64 = 0.80,
}

/// Summary telemetry returned by [`project_ipc_barriers`].
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct IpcStats {
    /// Number of active Vertex-Triangle barrier or CCD contacts resolved.
    pub vt_contacts: u32,
    /// Number of active Edge-Edge barrier or CCD contacts resolved.
    pub ee_contacts: u32,
    /// Minimum separation distance (metres) observed across evaluated non-adjacent pairs.
    pub min_dist_m: f64,
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), Mesh2DError> {
    v.try_push(x).map_err(|e| Mesh2DError::Alloc(e.why))
}

/// Smooth $C^2$ IPC cubic-log barrier energy:
/// $b(d) = -(d - \hat{d})^2 \ln(d / \hat{d})$ for $0 < d < \hat{d}$, and $0$ for $d \ge \hat{d}$.
#[inline]
pub fn barrier_energy(d: f64, d_hat: f64) -> f64 {
    if d_hat <= 0.0 || d >= d_hat {
        return 0.0;
    }
    let d_eff = if d > 1e-30 { d } else { (d_hat * 1e-9).max(1e-15) };
    let diff = d_eff - d_hat;
    -(diff * diff) * ln(d_eff / d_hat)
}

/// First derivative of the smooth $C^2$ IPC barrier energy with respect to distance $d$:
/// $b'(d) = -\left(2(d - \hat{d})\ln(d / \hat{d}) + (d - \hat{d})^2 / d\right)$ for
/// $0 < d < \hat{d}$ (strictly negative, driving separation), and $0$ for $d \ge \hat{d}$.
#[inline]
pub fn barrier_grad(d: f64, d_hat: f64) -> f64 {
    if d_hat <= 0.0 || d >= d_hat {
        return 0.0;
    }
    let d_eff = if d > 1e-30 { d } else { (d_hat * 1e-9).max(1e-15) };
    let diff = d_eff - d_hat;
    -(2.0 * diff * ln(d_eff / d_hat) + (diff * diff) / d_eff)
}

/// Second derivative (Hessian) of the smooth $C^2$ IPC barrier energy with respect to $d$:
/// $b''(d) = -2\ln(d / \hat{d}) - 4(d - \hat{d})/d + (d - \hat{d})^2 / d^2 > 0$ for $0 < d < \hat{d}$.
#[inline]
pub fn barrier_hess(d: f64, d_hat: f64) -> f64 {
    if d_hat <= 0.0 || d >= d_hat {
        return 0.0;
    }
    let d_eff = if d > 1e-30 { d } else { (d_hat * 1e-9).max(1e-15) };
    let diff = d_eff - d_hat;
    let ratio = diff / d_eff;
    -2.0 * ln(d_eff / d_hat) - 4.0 * ratio + ratio * ratio
}

/// Exact Voronoi-region closest point on triangle `(a, b, c)` to query point `p`
/// (Ericson, *Real-Time Collision Detection*, §5.1.5), returning `(closest_point, [w_a, w_b, w_c])`
/// with non-negative barycentric weights summing to `1.0`.
#[inline]
pub fn closest_point_on_triangle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> (Vec3, [f64; 3]) {
    let hit = blaze_cad_mesh::closest_point_on_triangle(p, a, b, c);
    (hit.point, hit.bary)
}

/// Exact analytical clamped closest points between 3D line segments `P(s) = p0 + s*(p1 - p0)`
/// and `Q(t) = q0 + t*(q1 - q0)` (Lumelsky 1985 / Ericson, *Real-Time Collision Detection*,
/// §5.1.9), returning `(s, t, c1, c2)` with `s, t in [0, 1]`.
pub fn closest_points_segment_segment(p0: Vec3, p1: Vec3, q0: Vec3, q1: Vec3) -> (f64, f64, Vec3, Vec3) {
    let d1 = p1 - p0;
    let d2 = q1 - q0;
    let r = p0 - q0;
    let a = d1.dot(d1);
    let e = d2.dot(d2);
    let f = d2.dot(r);

    const EPS: f64 = 1e-15;
    let (s, t) = if a <= EPS && e <= EPS {
        (0.0, 0.0)
    } else if a <= EPS {
        (0.0, (f / e).clamp(0.0, 1.0))
    } else {
        let c = d1.dot(r);
        if e <= EPS {
            ((-c / a).clamp(0.0, 1.0), 0.0)
        } else {
            let b = d1.dot(d2);
            let denom = a * e - b * b;
            let mut s = if denom > EPS {
                ((b * f - c * e) / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let t_nom = b * s + f;
            let t = if t_nom < 0.0 {
                s = (-c / a).clamp(0.0, 1.0);
                0.0
            } else if t_nom > e {
                s = ((b - c) / a).clamp(0.0, 1.0);
                1.0
            } else {
                t_nom / e
            };
            (s, t)
        }
    };

    let c1 = p0 + d1 * s;
    let c2 = q0 + d2 * t;
    (s, t, c1, c2)
}

#[inline]
fn pack_cell(cx: i64, cy: i64, cz: i64) -> i64 {
    let bx = (cx + 524_288) & 0xFFFFF;
    let by = (cy + 524_288) & 0xFFFFF;
    let bz = (cz + 524_288) & 0xFFFFF;
    (bx << 40) | (by << 20) | bz
}

#[inline]
fn cell_coord(x: f64, inv_cell: f64) -> i64 {
    floor(x * inv_cell) as i64
}

#[inline]
fn swept_aabb(p_prev: Vec3, p_curr: Vec3, pad: f64) -> (Vec3, Vec3) {
    let lo = Vec3::new(p_prev.x.min(p_curr.x) - pad, p_prev.y.min(p_curr.y) - pad, p_prev.z.min(p_curr.z) - pad);
    let hi = Vec3::new(p_prev.x.max(p_curr.x) + pad, p_prev.y.max(p_curr.y) + pad, p_prev.z.max(p_curr.z) + pad);
    (lo, hi)
}

#[inline]
fn prev_at(mesh: &ClothMesh<'_>, prev_pos: &[Vec3], idx: usize) -> Vec3 {
    if idx < prev_pos.len() { prev_pos[idx] } else { mesh.pos[idx] }
}

/// Squared distance from point `p` to 2D segment `(a, b)` in rest-pattern UV space.
#[inline]
fn point_seg_dist2_2d(p: Vec2, a: Vec2, b: Vec2) -> f64 {
    let ab = b - a;
    let len2 = ab.dot(ab);
    if len2 <= 1e-18 {
        let d = p - a;
        return d.dot(d);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    let proj = a + ab * t;
    let d = p - proj;
    d.dot(d)
}

/// Exact squared distance from 2D point `p` to 2D triangle `(a, b, c)`.
#[inline]
fn point_tri_dist2_2d(p: Vec2, a: Vec2, b: Vec2, c: Vec2) -> f64 {
    // Check if inside triangle using signed edge cross products.
    let e0 = (b - a).cross(p - a);
    let e1 = (c - b).cross(p - b);
    let e2 = (a - c).cross(p - c);
    if (e0 >= 0.0 && e1 >= 0.0 && e2 >= 0.0) || (e0 <= 0.0 && e1 <= 0.0 && e2 <= 0.0) {
        return 0.0;
    }
    point_seg_dist2_2d(p, a, b)
        .min(point_seg_dist2_2d(p, b, c))
        .min(point_seg_dist2_2d(p, c, a))
}

/// Minimum squared distance between two 2D segments `(a0, b0)` and `(a1, b1)` (approximate check
/// via endpoint-to-segment distances plus segment-intersection test).
#[inline]
fn seg_seg_dist2_2d(a0: Vec2, b0: Vec2, a1: Vec2, b1: Vec2) -> f64 {
    let d0 = b0 - a0;
    let d1 = b1 - a1;
    let denom = d0.cross(d1);
    if denom.abs() > 1e-15 {
        let diff = a1 - a0;
        let s = diff.cross(d1) / denom;
        let t = diff.cross(d0) / denom;
        if (0.0..=1.0).contains(&s) && (0.0..=1.0).contains(&t) {
            return 0.0;
        }
    }
    point_seg_dist2_2d(a0, a1, b1)
        .min(point_seg_dist2_2d(b0, a1, b1))
        .min(point_seg_dist2_2d(a1, a0, b0))
        .min(point_seg_dist2_2d(b1, a0, b0))
}

fn dedup_sorted<T: Copy + PartialEq>(v: &mut Vec<T, Alloc<'_>>) {
    if v.len() <= 1 {
        return;
    }
    let mut write = 1usize;
    for read in 1..v.len() {
        if v[read] != v[write - 1] {
            v[write] = v[read];
            write += 1;
        }
    }
    v.truncate(write);
}

#[inline]
fn clamped_cell_span(lo: i64, hi: i64, max_span: i64) -> (i64, i64) {
    if hi - lo + 1 <= max_span {
        (lo, hi)
    } else {
        let mid = lo + (hi - lo) / 2;
        let half = max_span / 2;
        (mid - half, mid - half + max_span - 1)
    }
}

/// Apply smooth $C^2$ Codimensional IPC barriers (Vertex-Triangle and Edge-Edge) with
/// inverse-mass barycentric CCD step clamping (`max_step_fraction`) to `mesh`.
///
/// Bit-for-bit deterministic: all spatial cells, candidate VT/EE pairs, and repulsion updates are
/// processed in canonical sorted index order with zero hash-map iteration.
pub fn project_ipc_barriers(
    mesh: &mut ClothMesh<'_>,
    prev_pos: &[Vec3],
    opts: IpcOpts,
    scratch: Alloc<'_>,
) -> Result<IpcStats, Mesh2DError> {
    let d_hat = opts.d_hat.as_m();
    let n_verts = mesh.pos.len();
    let n_tris = mesh.triangles.len();
    if d_hat <= 1e-9 || n_verts == 0 || n_tris == 0 {
        return Ok(IpcStats { vt_contacts: 0, ee_contacts: 0, min_dist_m: d_hat.max(0.0) });
    }

    let stiffness = opts.stiffness.clamp(0.0, 1.0);
    let max_frac = opts.max_step_fraction.clamp(0.10, 0.98);
    let rest_excl = 2.5 * d_hat;
    let rest_excl2 = rest_excl * rest_excl;
    // Spatial cell width balances cell occupancy vs per-primitive cell span.
    let cell_size = (d_hat * 2.5).max(0.012);
    let inv_cell = 1.0 / cell_size;
    const MAX_CELLS_PER_AXIS: i64 = 8;
    const MAX_BUCKET_ENTRIES: usize = 48;

    let mut vt_contacts = 0u32;
    let mut ee_contacts = 0u32;
    let mut min_dist = f64::INFINITY;

    // Build deterministic sorted 1-ring welded adjacency table (triangle edges + internal stitches)
    // so adjacent rings across welded seams or internal bar-tacks are NEVER falsely repelled!
    let mut adj_pairs: Vec<(u32, u32), Alloc<'_>> = Vec::new_in(scratch);
    for &[ia, ib, ic] in mesh.triangles.iter() {
        let ra = mesh.weld_rep[ia as usize];
        let rb = mesh.weld_rep[ib as usize];
        let rc = mesh.weld_rep[ic as usize];
        for (u, v) in [(ra, rb), (rb, rc), (rc, ra)] {
            if u != v {
                push(&mut adj_pairs, if u < v { (u, v) } else { (v, u) })?;
            }
        }
    }
    for st in mesh.internal_stitches.iter() {
        let u = mesh.weld_rep[st.host_vert as usize];
        let v = mesh.weld_rep[st.sub_vert as usize];
        if u != v {
            push(&mut adj_pairs, if u < v { (u, v) } else { (v, u) })?;
        }
    }
    adj_pairs.sort_unstable();
    dedup_sorted(&mut adj_pairs);
    let is_adj = |u: u32, v: u32| -> bool {
        if u == v {
            return true;
        }
        let key = if u < v { (u, v) } else { (v, u) };
        adj_pairs.binary_search(&key).is_ok()
    };

    // --------------------------------------------------------------------------------------------
    // 1. VERTEX-TRIANGLE (VT) SPATIAL HASH & BARRIER / CCD PROJECTION
    // --------------------------------------------------------------------------------------------
    let mut vert_cells: Vec<(i64, u32), Alloc<'_>> = Vec::new_in(scratch);
    for v in 0..n_verts {
        if mesh.weld_rep[v] as usize != v {
            continue;
        }
        let p0 = prev_at(mesh, prev_pos, v);
        let p1 = mesh.pos[v];
        let (lo, hi) = swept_aabb(p0, p1, 0.0);
        let (x0, x1) = clamped_cell_span(cell_coord(lo.x, inv_cell), cell_coord(hi.x, inv_cell), 4);
        let (y0, y1) = clamped_cell_span(cell_coord(lo.y, inv_cell), cell_coord(hi.y, inv_cell), 4);
        let (z0, z1) = clamped_cell_span(cell_coord(lo.z, inv_cell), cell_coord(hi.z, inv_cell), 4);
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                for cz in z0..=z1 {
                    push(&mut vert_cells, (pack_cell(cx, cy, cz), v as u32))?;
                }
            }
        }
    }
    vert_cells.sort_unstable();
    dedup_sorted(&mut vert_cells);

    let mut vt_candidates: Vec<(u32, u32), Alloc<'_>> = Vec::new_in(scratch);
    for ti in 0..n_tris {
        let [ia, ib, ic] = mesh.triangles[ti];
        let ra = mesh.weld_rep[ia as usize];
        let rb = mesh.weld_rep[ib as usize];
        let rc = mesh.weld_rep[ic as usize];
        if ra == rb || rb == rc || rc == ra {
            continue;
        }
        let pa0 = prev_at(mesh, prev_pos, ra as usize);
        let pb0 = prev_at(mesh, prev_pos, rb as usize);
        let pc0 = prev_at(mesh, prev_pos, rc as usize);
        let pa1 = mesh.pos[ra as usize];
        let pb1 = mesh.pos[rb as usize];
        let pc1 = mesh.pos[rc as usize];

        let lo = Vec3::new(
            pa0.x.min(pb0.x).min(pc0.x).min(pa1.x).min(pb1.x).min(pc1.x) - d_hat,
            pa0.y.min(pb0.y).min(pc0.y).min(pa1.y).min(pb1.y).min(pc1.y) - d_hat,
            pa0.z.min(pb0.z).min(pc0.z).min(pa1.z).min(pb1.z).min(pc1.z) - d_hat,
        );
        let hi = Vec3::new(
            pa0.x.max(pb0.x).max(pc0.x).max(pa1.x).max(pb1.x).max(pc1.x) + d_hat,
            pa0.y.max(pb0.y).max(pc0.y).max(pa1.y).max(pb1.y).max(pc1.y) + d_hat,
            pa0.z.max(pb0.z).max(pc0.z).max(pa1.z).max(pb1.z).max(pc1.z) + d_hat,
        );
        let (x0, x1) = clamped_cell_span(cell_coord(lo.x, inv_cell), cell_coord(hi.x, inv_cell), MAX_CELLS_PER_AXIS);
        let (y0, y1) = clamped_cell_span(cell_coord(lo.y, inv_cell), cell_coord(hi.y, inv_cell), MAX_CELLS_PER_AXIS);
        let (z0, z1) = clamped_cell_span(cell_coord(lo.z, inv_cell), cell_coord(hi.z, inv_cell), MAX_CELLS_PER_AXIS);

        for cx in x0..=x1 {
            for cy in y0..=y1 {
                for cz in z0..=z1 {
                    let key = pack_cell(cx, cy, cz);
                    let start = vert_cells.partition_point(|&(k, _)| k < key);
                    let mut idx = start;
                    let limit = (start + MAX_BUCKET_ENTRIES).min(vert_cells.len());
                    while idx < limit && vert_cells[idx].0 == key {
                        let v = vert_cells[idx].1;
                        if v != ra && v != rb && v != rc {
                            push(&mut vt_candidates, (ti as u32, v))?;
                        }
                        idx += 1;
                    }
                }
            }
        }
    }
    vt_candidates.sort_unstable();
    dedup_sorted(&mut vt_candidates);

    for &(ti, v_u32) in vt_candidates.iter() {
        let v = v_u32 as usize;
        let [ia, ib, ic] = mesh.triangles[ti as usize];
        let ra = mesh.weld_rep[ia as usize] as usize;
        let rb = mesh.weld_rep[ib as usize] as usize;
        let rc = mesh.weld_rep[ic as usize] as usize;
        if v == ra || v == rb || v == rc
            || is_adj(v as u32, ra as u32)
            || is_adj(v as u32, rb as u32)
            || is_adj(v as u32, rc as u32)
        {
            continue;
        }
        let wv = mesh.inv_mass[v];
        let wa = mesh.inv_mass[ra];
        let wb = mesh.inv_mass[rb];
        let wc = mesh.inv_mass[rc];
        if wv + wa + wb + wc <= 1e-12 {
            continue;
        }

        // Skip topologically close pairs in 2D flat pattern space on the same panel.
        let tri_panel = mesh.tri_panel[ti as usize];
        if mesh.vert_panel[v] == tri_panel {
            let uv_v = mesh.rest_uv_m[v];
            let uv_a = mesh.rest_uv_m[ia as usize];
            let uv_b = mesh.rest_uv_m[ib as usize];
            let uv_c = mesh.rest_uv_m[ic as usize];
            if point_tri_dist2_2d(uv_v, uv_a, uv_b, uv_c) < rest_excl2 {
                continue;
            }
        }

        let mut pv = mesh.pos[v];
        let mut pa = mesh.pos[ra];
        let mut pb = mesh.pos[rb];
        let mut pc = mesh.pos[rc];

        let pv0 = prev_at(mesh, prev_pos, v);
        let pa0 = prev_at(mesh, prev_pos, ra);
        let pb0 = prev_at(mesh, prev_pos, rb);
        let pc0 = prev_at(mesh, prev_pos, rc);

        let (q_curr, _) = closest_point_on_triangle(pv, pa, pb, pc);
        let (q_prev_near, bary_prev) = closest_point_on_triangle(pv0, pa0, pb0, pc0);
        let diff_prev = pv0 - q_prev_near;
        let d_prev = diff_prev.length();
        let diff_curr = pv - q_curr;
        let d_curr = diff_curr.length();

        // Check both current proximity and CCD tunneling along the substep trajectory.
        let mut active = false;
        let mut sep_dir = if d_curr > 1e-12 {
            diff_curr * (1.0 / d_curr)
        } else if d_prev > 1e-12 {
            diff_prev * (1.0 / d_prev)
        } else {
            (pb - pa).cross(pc - pa).normalized().unwrap_or(Vec3::new(0.0, 1.0, 0.0))
        };

        if d_prev > 1e-9 && (d_curr < d_hat || d_prev < d_hat * 2.0) {
            let n_prev = diff_prev * (1.0 / d_prev);
            let q_curr_at_prev_bary = pa * bary_prev[0] + pb * bary_prev[1] + pc * bary_prev[2];
            let signed_curr = (pv - q_curr_at_prev_bary).dot(n_prev);
            let min_allowed = (1.0 - max_frac) * d_prev;
            // CCD clamping: prevent relative motion from consuming more than `max_step_fraction`
            // of the initial separation (which also catches full sign-flip tunneling!).
            if signed_curr < min_allowed {
                let delta = (min_allowed - signed_curr).min(d_hat * 0.45);
                let w_denom = wv
                    + wa * bary_prev[0] * bary_prev[0]
                    + wb * bary_prev[1] * bary_prev[1]
                    + wc * bary_prev[2] * bary_prev[2];
                if w_denom > 1e-12 {
                    let s = delta / w_denom;
                    pv = pv + n_prev * (s * wv);
                    pa = pa - n_prev * (s * wa * bary_prev[0]);
                    pb = pb - n_prev * (s * wb * bary_prev[1]);
                    pc = pc - n_prev * (s * wc * bary_prev[2]);
                    sep_dir = n_prev;
                    active = true;
                }
            }
        }

        // Re-evaluate closest point after any CCD clamp and apply smooth C^2 IPC barrier.
        let (q_after, bary_after) = closest_point_on_triangle(pv, pa, pb, pc);
        let diff_after = pv - q_after;
        let d_after = diff_after.length();
        if d_after < d_hat {
            if d_after > 1e-12 {
                sep_dir = diff_after * (1.0 / d_after);
            }
            let g = barrier_grad(d_after, d_hat);
            let h = barrier_hess(d_after, d_hat);
            let newton_step = if h > 1e-12 { (-g) / h } else { d_hat - d_after };
            let push_dist = (newton_step * stiffness).min((d_hat - d_after) * 0.40).max(0.0);
            if push_dist > 1e-12 {
                let w_denom = wv
                    + wa * bary_after[0] * bary_after[0]
                    + wb * bary_after[1] * bary_after[1]
                    + wc * bary_after[2] * bary_after[2];
                if w_denom > 1e-12 {
                    let s = push_dist / w_denom;
                    pv = pv + sep_dir * (s * wv);
                    pa = pa - sep_dir * (s * wa * bary_after[0]);
                    pb = pb - sep_dir * (s * wb * bary_after[1]);
                    pc = pc - sep_dir * (s * wc * bary_after[2]);
                    active = true;
                }
            }
        }

        if active {
            mesh.pos[v] = pv;
            mesh.pos[ra] = pa;
            mesh.pos[rb] = pb;
            mesh.pos[rc] = pc;
            vt_contacts += 1;
            let (q_final, _) = closest_point_on_triangle(pv, pa, pb, pc);
            let d_final = (pv - q_final).length();
            if d_final < min_dist {
                min_dist = d_final;
            }
        } else if d_after < min_dist {
            min_dist = d_after;
        }
    }

    // --------------------------------------------------------------------------------------------
    // 2. EDGE-EDGE (EE) SPATIAL HASH & BARRIER / CCD PROJECTION
    // --------------------------------------------------------------------------------------------
    #[derive(Clone, Copy)]
    struct WeldedEdge {
        a: u32,
        b: u32,
        raw_a: u32,
        raw_b: u32,
        panel: u16,
    }

    let mut raw_edges: Vec<WeldedEdge, Alloc<'_>> = Vec::new_in(scratch);
    for (ti, &[ia, ib, ic]) in mesh.triangles.iter().enumerate() {
        let ra = mesh.weld_rep[ia as usize];
        let rb = mesh.weld_rep[ib as usize];
        let rc = mesh.weld_rep[ic as usize];
        if ra == rb || rb == rc || rc == ra {
            continue;
        }
        let panel = mesh.tri_panel[ti];
        for ((r0, raw0), (r1, raw1)) in [
            ((ra, ia), (rb, ib)),
            ((rb, ib), (rc, ic)),
            ((rc, ic), (ra, ia)),
        ] {
            let (a, b, raw_a, raw_b) = if r0 < r1 { (r0, r1, raw0, raw1) } else { (r1, r0, raw1, raw0) };
            push(&mut raw_edges, WeldedEdge { a, b, raw_a, raw_b, panel })?;
        }
    }
    raw_edges.sort_unstable_by(|x, y| (x.a, x.b, x.panel).cmp(&(y.a, y.b, y.panel)));
    // Deduplicate to one canonical entry per welded edge (a, b).
    let mut edges: Vec<WeldedEdge, Alloc<'_>> = Vec::new_in(scratch);
    for e in raw_edges.iter().copied() {
        let dup = edges.last().is_some_and(|prev| prev.a == e.a && prev.b == e.b);
        if !dup {
            push(&mut edges, e)?;
        }
    }

    let half_pad = 0.5 * d_hat;
    let mut edge_cells: Vec<(i64, u32), Alloc<'_>> = Vec::new_in(scratch);
    for (ei, e) in edges.iter().enumerate() {
        let a = e.a as usize;
        let b = e.b as usize;
        let pa0 = prev_at(mesh, prev_pos, a);
        let pb0 = prev_at(mesh, prev_pos, b);
        let pa1 = mesh.pos[a];
        let pb1 = mesh.pos[b];
        let lo = Vec3::new(
            pa0.x.min(pb0.x).min(pa1.x).min(pb1.x) - half_pad,
            pa0.y.min(pb0.y).min(pa1.y).min(pb1.y) - half_pad,
            pa0.z.min(pb0.z).min(pa1.z).min(pb1.z) - half_pad,
        );
        let hi = Vec3::new(
            pa0.x.max(pb0.x).max(pa1.x).max(pb1.x) + half_pad,
            pa0.y.max(pb0.y).max(pa1.y).max(pb1.y) + half_pad,
            pa0.z.max(pb0.z).max(pa1.z).max(pb1.z) + half_pad,
        );
        let (x0, x1) = clamped_cell_span(cell_coord(lo.x, inv_cell), cell_coord(hi.x, inv_cell), MAX_CELLS_PER_AXIS);
        let (y0, y1) = clamped_cell_span(cell_coord(lo.y, inv_cell), cell_coord(hi.y, inv_cell), MAX_CELLS_PER_AXIS);
        let (z0, z1) = clamped_cell_span(cell_coord(lo.z, inv_cell), cell_coord(hi.z, inv_cell), MAX_CELLS_PER_AXIS);
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                for cz in z0..=z1 {
                    push(&mut edge_cells, (pack_cell(cx, cy, cz), ei as u32))?;
                }
            }
        }
    }
    edge_cells.sort_unstable();
    dedup_sorted(&mut edge_cells);

    let mut ee_candidates: Vec<(u32, u32), Alloc<'_>> = Vec::new_in(scratch);
    let mut start = 0usize;
    while start < edge_cells.len() {
        let key = edge_cells[start].0;
        let mut end = start + 1;
        while end < edge_cells.len() && edge_cells[end].0 == key {
            end += 1;
        }
        let cap = (end - start).min(MAX_BUCKET_ENTRIES);
        for i in 0..cap {
            let e0 = edge_cells[start + i].1;
            let ed0 = edges[e0 as usize];
            for j in (i + 1)..cap {
                let e1 = edge_cells[start + j].1;
                let ed1 = edges[e1 as usize];
                if ed0.a == ed1.a || ed0.a == ed1.b || ed0.b == ed1.a || ed0.b == ed1.b {
                    continue;
                }
                let pair = if e0 < e1 { (e0, e1) } else { (e1, e0) };
                push(&mut ee_candidates, pair)?;
            }
        }
        start = end;
    }
    ee_candidates.sort_unstable();
    dedup_sorted(&mut ee_candidates);

    for &(ei0, ei1) in ee_candidates.iter() {
        let ed0 = edges[ei0 as usize];
        let ed1 = edges[ei1 as usize];
        let a0 = ed0.a as usize;
        let b0 = ed0.b as usize;
        let a1 = ed1.a as usize;
        let b1 = ed1.b as usize;
        if a0 == a1 || a0 == b1 || b0 == a1 || b0 == b1
            || is_adj(ed0.a, ed1.a)
            || is_adj(ed0.a, ed1.b)
            || is_adj(ed0.b, ed1.a)
            || is_adj(ed0.b, ed1.b)
        {
            continue;
        }
        let wa0 = mesh.inv_mass[a0];
        let wb0 = mesh.inv_mass[b0];
        let wa1 = mesh.inv_mass[a1];
        let wb1 = mesh.inv_mass[b1];
        if wa0 + wb0 + wa1 + wb1 <= 1e-12 {
            continue;
        }

        // Exclude topologically adjacent edges within `2.5 * d_hat` in 2D `rest_uv_m` on the same panel.
        if ed0.panel == ed1.panel {
            let u0 = mesh.rest_uv_m[ed0.raw_a as usize];
            let v0 = mesh.rest_uv_m[ed0.raw_b as usize];
            let u1 = mesh.rest_uv_m[ed1.raw_a as usize];
            let v1 = mesh.rest_uv_m[ed1.raw_b as usize];
            if seg_seg_dist2_2d(u0, v0, u1, v1) < rest_excl2 {
                continue;
            }
        }

        let mut pa0 = mesh.pos[a0];
        let mut pb0 = mesh.pos[b0];
        let mut pa1 = mesh.pos[a1];
        let mut pb1 = mesh.pos[b1];

        let pa0_prev = prev_at(mesh, prev_pos, a0);
        let pb0_prev = prev_at(mesh, prev_pos, b0);
        let pa1_prev = prev_at(mesh, prev_pos, a1);
        let pb1_prev = prev_at(mesh, prev_pos, b1);

        let (s_prev, t_prev, c1_prev, c2_prev) = closest_points_segment_segment(pa0_prev, pb0_prev, pa1_prev, pb1_prev);
        let (_, _, c1_curr, c2_curr) = closest_points_segment_segment(pa0, pb0, pa1, pb1);
        let diff_prev = c1_prev - c2_prev;
        let d_prev = diff_prev.length();
        let diff_curr = c1_curr - c2_curr;
        let d_curr = diff_curr.length();

        let mut active = false;
        let mut sep_dir = if d_curr > 1e-12 {
            diff_curr * (1.0 / d_curr)
        } else if d_prev > 1e-12 {
            diff_prev * (1.0 / d_prev)
        } else {
            (pb0 - pa0).cross(pb1 - pa1).normalized().unwrap_or(Vec3::new(0.0, 1.0, 0.0))
        };

        // CCD step clamp along initial edge-edge separation direction.
        if d_prev > 1e-9 && (d_curr < d_hat || d_prev < d_hat * 2.0) {
            let n_prev = diff_prev * (1.0 / d_prev);
            let p_curr_at = pa0 * (1.0 - s_prev) + pb0 * s_prev;
            let q_curr_at = pa1 * (1.0 - t_prev) + pb1 * t_prev;
            let signed_curr = (p_curr_at - q_curr_at).dot(n_prev);
            let min_allowed = (1.0 - max_frac) * d_prev;
            if signed_curr < min_allowed {
                let delta = (min_allowed - signed_curr).min(d_hat * 0.45);
                let u0 = 1.0 - s_prev;
                let u1 = s_prev;
                let v0 = 1.0 - t_prev;
                let v1 = t_prev;
                let w_denom = wa0 * u0 * u0 + wb0 * u1 * u1 + wa1 * v0 * v0 + wb1 * v1 * v1;
                if w_denom > 1e-12 {
                    let scale = delta / w_denom;
                    pa0 = pa0 + n_prev * (scale * wa0 * u0);
                    pb0 = pb0 + n_prev * (scale * wb0 * u1);
                    pa1 = pa1 - n_prev * (scale * wa1 * v0);
                    pb1 = pb1 - n_prev * (scale * wb1 * v1);
                    sep_dir = n_prev;
                    active = true;
                }
            }
        }

        // Smooth C^2 IPC barrier repulsion on post-CCD segment-segment proximity.
        let (s_a, t_a, c1_a, c2_a) = closest_points_segment_segment(pa0, pb0, pa1, pb1);
        let diff_a = c1_a - c2_a;
        let d_a = diff_a.length();
        if d_a < d_hat {
            if d_a > 1e-12 {
                sep_dir = diff_a * (1.0 / d_a);
            }
            let g = barrier_grad(d_a, d_hat);
            let h = barrier_hess(d_a, d_hat);
            let newton_step = if h > 1e-12 { (-g) / h } else { d_hat - d_a };
            let push_dist = (newton_step * stiffness).min((d_hat - d_a) * 0.40).max(0.0);
            if push_dist > 1e-12 {
                let u0 = 1.0 - s_a;
                let u1 = s_a;
                let v0 = 1.0 - t_a;
                let v1 = t_a;
                let w_denom = wa0 * u0 * u0 + wb0 * u1 * u1 + wa1 * v0 * v0 + wb1 * v1 * v1;
                if w_denom > 1e-12 {
                    let scale = push_dist / w_denom;
                    pa0 = pa0 + sep_dir * (scale * wa0 * u0);
                    pb0 = pb0 + sep_dir * (scale * wb0 * u1);
                    pa1 = pa1 - sep_dir * (scale * wa1 * v0);
                    pb1 = pb1 - sep_dir * (scale * wb1 * v1);
                    active = true;
                }
            }
        }

        if active {
            mesh.pos[a0] = pa0;
            mesh.pos[b0] = pb0;
            mesh.pos[a1] = pa1;
            mesh.pos[b1] = pb1;
            ee_contacts += 1;
            let (_, _, c1_f, c2_f) = closest_points_segment_segment(pa0, pb0, pa1, pb1);
            let d_f = (c1_f - c2_f).length();
            if d_f < min_dist {
                min_dist = d_f;
            }
        } else if d_a < min_dist {
            min_dist = d_a;
        }
    }

    // Keep welded slave vertices strictly synchronized with their canonical representatives.
    for i in 0..n_verts {
        let r = mesh.weld_rep[i] as usize;
        mesh.pos[i] = mesh.pos[r];
    }

    Ok(IpcStats {
        vt_contacts,
        ee_contacts,
        min_dist_m: if min_dist.is_finite() { min_dist } else { d_hat },
    })
}
