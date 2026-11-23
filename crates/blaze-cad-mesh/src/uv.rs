//! Deterministic chart UV unwrapping and MikkTSpace-compatible tangent generation.
//!
//! [`unwrap_uvs`] partitions a mesh into heightfield charts by deterministic BFS flood-fill (never
//! crossing marked creases, material-role boundaries, part boundaries, or angles past
//! [`UvOpts::max_chart_angle`]), projects each chart at physical scale onto its deterministic
//! orthonormal tangent plane, packs all charts into `[0, 1]²` with a uniform `texels_per_metre`
//! scale and full `gutter_px` inter-chart margin via deterministic shelf packing, splits only
//! chart-seam vertices while remapping creases and preserving all vertex/triangle attributes, and
//! generates MikkTSpace-compatible orthonormal vertex tangents via [`compute_tangents`].
//!
//! Every operation is deterministic (IEEE basic operations, `sqrt`, `ceil`, and `blaze_math`'s
//! exact turn trigonometry, with total sort orders broken by ids) and iterative (every loop
//! bounded by the input or a fixed step count; no recursion, no `unsafe`, no `Global` allocator).

#![blaze::sans_io]

use blaze_math::{TurnUnit, sin_cos_turns};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::Angle;

use crate::MeshLimits;
use crate::geom::{D3, add, cross, dot, length, narrow, plus_zero, scale, sqrt, sub, tri_normal, unit, wide};
use crate::ids::{TriId, VertexId, VertexPair};
use crate::mesh::{Mesh, MeshVecs, MeshView, ShapeError, built, empty_vecs};
use crate::plant::UvPlant;
use crate::vecs::{V, at, counting, edge_runs, extend, filled, halves, len32, push, spots};

const NONE: u32 = u32::MAX;

/// Exact turn unit matching [`Angle::PER_TURN`] for deterministic `sin_cos_turns`.
const ANGLE_UNIT: TurnUnit = match TurnUnit::new(Angle::<blaze_units::Plain>::PER_TURN) {
    Ok(u) => u,
    Err(_) => unreachable!(),
};

/// Minimum dot product between a triangle normal and its chart's projection axis so no triangle in
/// a chart can fold backward in UV space (~75.5°).
const MIN_CHART_DOT: f64 = 0.25;

/// Binary search steps for maximizing uniform `texels_per_metre` in shelf packing (`2^-24`
/// relative precision, strictly bounded per rule 18).
const PACK_SEARCH_STEPS: u32 = 24;

/// Options for [`unwrap_uvs`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct UvOpts<'a, 's> {
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
    pub max_chart_angle: Angle = Angle::microarcsec(45 * 3_600_000_000),
    pub gutter_px: u32 = 4,
    pub atlas_size: u32 = 1024,
    pub limits: MeshLimits = MeshLimits::SHIPPED,
}

/// Errors returned by [`unwrap_uvs`] and [`compute_tangents`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UvError {
    EmptyMesh,
    TooManyVertices { count: usize, most: u32 },
    TooManyTriangles { count: usize, most: u32 },
    BadAtlasSize { size: u32 },
    AtlasOverflow { charts: u32, triangles: u32, size: u32, gutter_px: u32 },
    Shape(ShapeError),
    Alloc(AllocFailure),
}

impl From<AllocFailure> for UvError {
    fn from(e: AllocFailure) -> UvError {
        UvError::Alloc(e)
    }
}

/// A UV-unwrapped mesh with its uniform atlas density, chart count, and atlas utilization.
#[derive(Debug)]
pub struct UvMapped<'m> {
    pub mesh: Mesh<'m>,
    pub texels_per_metre: f64,
    pub chart_count: u32,
    pub atlas_utilization: f64,
}

#[derive(Clone, Copy, Debug)]
struct Chart {
    id: u32,
    u_axis: D3,
    v_axis: D3,
    min_u: f64,
    min_v: f64,
    w: f64,
    h: f64,
}

/// Deterministic unit vector perpendicular to unit normal `n`, choosing the canonical coordinate
/// axis with smallest `|n[k]|` so the tangent basis never flips.
#[inline]
fn canonical_tangent(n: D3) -> D3 {
    let ax = n[0].abs();
    let ay = n[1].abs();
    let az = n[2].abs();
    let ref_axis = if ax <= ay && ax <= az {
        [1.0, 0.0, 0.0]
    } else if ay <= az {
        [0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    };
    unit(cross(ref_axis, n)).unwrap_or([1.0, 0.0, 0.0])
}

/// Insert `nbr` into a triangle's up-to-3 neighbor list without duplicates.
#[inline]
fn add_nbr(row: &mut [u32; 3], len: &mut u8, nbr: u32) {
    let count = usize::from(*len);
    for &existing in &row[..count] {
        if existing == nbr {
            return;
        }
    }
    if count < 3 {
        row[count] = nbr;
        *len += 1;
    }
}

/// Deterministic shelf packing of `charts` in `order` at uniform `scale` (`texels_per_metre`)
/// into `[0, atlas]²`, padding each chart by `gutter` texels (`gutter / 2` on all 4 sides).
fn pack_shelves(
    charts: &[Chart],
    order: &[u32],
    scale: f64,
    atlas: f64,
    gutter: f64,
    mut out_pos: Option<&mut [(f64, f64)]>,
) -> bool {
    let mut cur_x = 0.0f64;
    let mut cur_y = 0.0f64;
    let mut shelf_h = 0.0f64;
    for &idx in order {
        let c = &charts[at(idx)];
        let box_w = core::f64::math::ceil(c.w * scale).max(1.0) + gutter;
        let box_h = core::f64::math::ceil(c.h * scale).max(1.0) + gutter;
        if !box_w.is_finite() || !box_h.is_finite() || box_w > atlas || box_h > atlas {
            return false;
        }
        if cur_x > 0.0 && cur_x + box_w > atlas {
            cur_x = 0.0;
            cur_y += shelf_h;
            shelf_h = 0.0;
        }
        if cur_y + box_h > atlas {
            return false;
        }
        if let Some(pos) = out_pos.as_deref_mut() {
            pos[at(idx)] = (cur_x, cur_y);
        }
        cur_x += box_w;
        if box_h > shelf_h {
            shelf_h = box_h;
        }
    }
    true
}

/// Unwrap `view` into non-overlapping charts in `[0, 1]²` and compute MikkTSpace-compatible vertex
/// tangents.
pub fn unwrap_uvs<'a>(view: &MeshView<'_>, opts: UvOpts<'a, '_>) -> Result<UvMapped<'a>, UvError> {
    unwrap_uvs_planted(view, opts, NoPlant)
}

/// [`unwrap_uvs`] with a planted bug seam, for the test controls.
pub fn unwrap_uvs_planted<'a>(
    view: &MeshView<'_>,
    opts: UvOpts<'a, '_>,
    plant: impl Planted<UvPlant>,
) -> Result<UvMapped<'a>, UvError> {
    let tris = view.triangles();
    let positions = view.positions();
    if positions.is_empty() || tris.is_empty() {
        return Err(UvError::EmptyMesh);
    }
    if positions.len() > at(opts.limits.most_vertices) {
        return Err(UvError::TooManyVertices { count: positions.len(), most: opts.limits.most_vertices });
    }
    if tris.len() > at(opts.limits.most_triangles) {
        return Err(UvError::TooManyTriangles { count: tris.len(), most: opts.limits.most_triangles });
    }
    if opts.atlas_size < 2 || opts.gutter_px >= opts.atlas_size {
        return Err(UvError::BadAtlasSize { size: opts.atlas_size });
    }

    let s = opts.scratch;
    let tri_count = view.triangle_count();
    let (_, raw_cos) =
        sin_cos_turns(opts.max_chart_angle.as_microarcsec().clamp(0, Angle::<blaze_units::Plain>::PER_TURN / 2), ANGLE_UNIT);
    let cos_max = raw_cos.max(MIN_CHART_DOT);

    // 1. Per-triangle geometric unit normals, areas, and deterministic edge->triangle adjacency.
    let mut tri_unit_n: V<'_, D3> = Vec::try_with_capacity_in(tris.len(), s)?;
    let mut tri_area: V<'_, f64> = Vec::try_with_capacity_in(tris.len(), s)?;
    for t in 0..tri_count {
        let c = view.corners(TriId(t));
        let n_raw = tri_normal(wide(c[0]), wide(c[1]), wide(c[2]));
        let len = length(n_raw);
        if len > 0.0 && len.is_finite() {
            push(&mut tri_unit_n, scale(n_raw, 1.0 / len))?;
            push(&mut tri_area, 0.5 * len)?;
        } else {
            push(&mut tri_unit_n, [0.0, 0.0, 1.0])?;
            push(&mut tri_area, 0.0)?;
        }
    }

    // Match adjacency by geometric vertex spot (not raw VertexId) so vertices previously split by
    // `shade()` along creases do not shatter smooth UV charts into disconnected islands.
    let vspots = spots(positions, s)?;
    let spot = &vspots.of_vertex;
    let mut marked_creases: V<'_, (u32, u32)> = Vec::try_with_capacity_in(view.creases().len(), s)?;
    for edge in view.creases() {
        let (a, b) = (spot[edge.lo().index()], spot[edge.hi().index()]);
        push(&mut marked_creases, if a <= b { (a, b) } else { (b, a) })?;
    }
    marked_creases.sort_unstable();
    let half_edges = halves(tris, spot, s)?;

    let mut nbrs = filled(tris.len(), [NONE; 3], s)?;
    let mut nbr_len = filled(tris.len(), 0u8, s)?;
    let creases = view.creases();
    let roles = view.roles();
    let parts = view.parts();
    for (start, end) in edge_runs(&half_edges) {
        if end - start != 2 {
            continue;
        }
        let (h1, h2) = (half_edges[start], half_edges[start + 1]);
        let (t1, t2) = (h1.tri, h2.tri);
        if t1 != t2
            && roles[at(t1)] == roles[at(t2)]
            && parts[at(t1)] == parts[at(t2)]
            && marked_creases.binary_search(&(h1.lo, h1.hi)).is_err()
        {
            add_nbr(&mut nbrs[at(t1)], &mut nbr_len[at(t1)], t2);
            add_nbr(&mut nbrs[at(t2)], &mut nbr_len[at(t2)], t1);
        }
    }
    for t in 0..tris.len() {
        let k = usize::from(nbr_len[t]);
        nbrs[t][..k].sort_unstable();
    }

    // 2. Chart segmentation by deterministic largest-area-first BFS flood-fill, plus Step 3 planar
    // projection per chart. Seeding large faces first prevents small bevel/corner slivers from
    // fragmenting smooth curved shells into thousands of tiny charts.
    let mut seed_order = counting(tri_count, s)?;
    seed_order.sort_unstable_by(|&a, &b| tri_area[at(b)].total_cmp(&tri_area[at(a)]).then_with(|| a.cmp(&b)));

    let mut chart_of = filled(tris.len(), NONE, s)?;
    let mut queue: V<'_, u32> = Vec::try_with_capacity_in(tris.len(), s)?;
    let mut raw_proj_n: V<'_, D3> = Vec::new_in(s);
    let mut raw_ranges: V<'_, (usize, usize)> = Vec::new_in(s);
    let mut raw_size: V<'_, u32> = Vec::new_in(s);

    for &seed in seed_order.iter() {
        if chart_of[at(seed)] != NONE {
            continue;
        }
        let chart_id = len32(raw_proj_n.len());
        let tri_start = queue.len();
        chart_of[at(seed)] = chart_id;
        push(&mut queue, seed)?;

        let seed_n = tri_unit_n[at(seed)];
        let mut chart_n_sum = scale(seed_n, tri_area[at(seed)].max(1e-18));
        let mut chart_n_unit = seed_n;

        let mut head = tri_start;
        while head < queue.len() {
            let t1 = queue[head];
            head += 1;
            let n1 = tri_unit_n[at(t1)];
            let count = usize::from(nbr_len[at(t1)]);
            for ni in 0..count {
                let t2 = nbrs[at(t1)][ni];
                if chart_of[at(t2)] != NONE {
                    continue;
                }
                let n2 = tri_unit_n[at(t2)];
                if dot(n2, seed_n) >= cos_max && dot(n2, n1) >= cos_max && dot(n2, chart_n_unit) > MIN_CHART_DOT {
                    let next_sum = add(chart_n_sum, scale(n2, tri_area[at(t2)]));
                    let next_unit = unit(next_sum).unwrap_or(chart_n_unit);
                    if dot(seed_n, next_unit) > MIN_CHART_DOT && dot(n2, next_unit) > MIN_CHART_DOT {
                        chart_of[at(t2)] = chart_id;
                        chart_n_sum = next_sum;
                        chart_n_unit = next_unit;
                        push(&mut queue, t2)?;
                    }
                }
            }
        }

        // Verify the final average normal faces the same hemisphere as every triangle in the chart;
        // otherwise project along `seed_n` (which every chart triangle already satisfies `>= cos_max`).
        let tri_end = queue.len();
        let mut proj_n = chart_n_unit;
        for &t in &queue[tri_start..tri_end] {
            if dot(tri_unit_n[at(t)], proj_n) <= MIN_CHART_DOT {
                proj_n = seed_n;
                break;
            }
        }
        push(&mut raw_proj_n, proj_n)?;
        push(&mut raw_ranges, (tri_start, tri_end))?;
        push(&mut raw_size, len32(tri_end - tri_start))?;
    }

    // 2b. Deterministic small-island absorption (2 bounded passes): merge tiny 1..=4 triangle
    // residual slivers into their largest front-facing adjacent chart (never crossing creases,
    // role/part boundaries, or >63 deg projection angles) to prevent gutter fragmentation.
    const MAX_ABSORB_TRIS: u32 = 4;
    const ABSORB_PROJ_DOT: f64 = 0.45;
    const ABSORB_EDGE_DOT: f64 = 0.35;
    let raw_count = raw_proj_n.len();
    let mut chart_parent = counting(len32(raw_count), s)?;
    let resolve_root = |parent: &mut [u32], mut x: u32| -> u32 {
        while parent[at(x)] != x {
            let p = parent[at(x)];
            parent[at(x)] = parent[at(p)];
            x = p;
        }
        x
    };
    for _pass in 0..2 {
        let mut merged_any = false;
        for cid in 0..raw_count {
            if chart_parent[cid] != len32(cid) {
                continue;
            }
            let sz = raw_size[cid];
            if sz == 0 || sz > MAX_ABSORB_TRIS {
                continue;
            }
            let (s_idx, e_idx) = raw_ranges[cid];
            let mut best_dst = NONE;
            let mut best_sz = sz;
            let mut best_dot = -2.0f64;
            for &t in &queue[s_idx..e_idx] {
                let nt = tri_unit_n[at(t)];
                let cnt = usize::from(nbr_len[at(t)]);
                for ni in 0..cnt {
                    let t2 = nbrs[at(t)][ni];
                    let raw_dst = chart_of[at(t2)];
                    if raw_dst == NONE {
                        continue;
                    }
                    let dst = resolve_root(&mut chart_parent, raw_dst);
                    if at(dst) == cid || raw_size[at(dst)] <= sz {
                        continue;
                    }
                    if dot(nt, tri_unit_n[at(t2)]) < ABSORB_EDGE_DOT {
                        continue;
                    }
                    let dst_n = raw_proj_n[at(dst)];
                    let mut min_d = 1.0f64;
                    let mut ok = true;
                    for &tk in &queue[s_idx..e_idx] {
                        let d = dot(tri_unit_n[at(tk)], dst_n);
                        if d < ABSORB_PROJ_DOT {
                            ok = false;
                            break;
                        }
                        if d < min_d {
                            min_d = d;
                        }
                    }
                    if !ok {
                        continue;
                    }
                    let dsz = raw_size[at(dst)];
                    if best_dst == NONE
                        || dsz > best_sz
                        || (dsz == best_sz && (min_d > best_dot || (min_d == best_dot && dst < best_dst)))
                    {
                        best_dst = dst;
                        best_sz = dsz;
                        best_dot = min_d;
                    }
                }
            }
            if best_dst != NONE {
                chart_parent[cid] = best_dst;
                raw_size[at(best_dst)] += sz;
                raw_size[cid] = 0;
                merged_any = true;
            }
        }
        if !merged_any {
            break;
        }
    }

    // 3. Compact surviving charts and project each onto its deterministic orthonormal tangent plane.
    let mut remap = filled(raw_count, NONE, s)?;
    let mut charts: V<'_, Chart> = Vec::new_in(s);
    for cid in 0..raw_count {
        if raw_size[cid] == 0 {
            continue;
        }
        let new_id = len32(charts.len());
        remap[cid] = new_id;
        let proj_n = raw_proj_n[cid];
        let u_axis = canonical_tangent(proj_n);
        let raw_v = cross(proj_n, u_axis);
        let v_axis = unit(raw_v).unwrap_or(raw_v);
        push(
            &mut charts,
            Chart {
                id: new_id,
                u_axis,
                v_axis,
                min_u: f64::INFINITY,
                min_v: f64::INFINITY,
                w: f64::NEG_INFINITY,
                h: f64::NEG_INFINITY,
            },
        )?;
    }
    for t in 0..tris.len() {
        let root_cid = resolve_root(&mut chart_parent, chart_of[t]);
        let cid = remap[at(root_cid)];
        chart_of[t] = cid;
        let c = &mut charts[at(cid)];
        for &v in &tris[t] {
            let p = wide(positions[at(v)]);
            let u = dot(p, c.u_axis);
            let vc = dot(p, c.v_axis);
            if u < c.min_u {
                c.min_u = u;
            }
            if u > c.w {
                c.w = u;
            }
            if vc < c.min_v {
                c.min_v = vc;
            }
            if vc > c.h {
                c.h = vc;
            }
        }
    }
    for c in charts.iter_mut() {
        if c.min_u.is_finite() && c.w.is_finite() {
            c.w = (c.w - c.min_u).max(0.0);
        } else {
            c.min_u = 0.0;
            c.w = 0.0;
        }
        if c.min_v.is_finite() && c.h.is_finite() {
            c.h = (c.h - c.min_v).max(0.0);
        } else {
            c.min_v = 0.0;
            c.h = 0.0;
        }
    }

    // 4. Deterministic shelf atlas packing.
    let chart_count = len32(charts.len());
    let mut order = counting(chart_count, s)?;
    order.sort_unstable_by(|&ia, &ib| {
        let a = &charts[at(ia)];
        let b = &charts[at(ib)];
        b.h.total_cmp(&a.h).then_with(|| b.w.total_cmp(&a.w)).then_with(|| a.id.cmp(&b.id))
    });

    let atlas = f64::from(opts.atlas_size);
    let gutter = f64::from(opts.gutter_px);
    let margin = gutter * 0.5;

    let mut total_area = 0.0f64;
    let mut max_dim = 0.0f64;
    for c in charts.iter() {
        total_area += c.w * c.h;
        max_dim = max_dim.max(c.w).max(c.h);
    }

    if !pack_shelves(&charts, &order, 0.0, atlas, gutter, None) {
        return Err(UvError::AtlasOverflow {
            charts: chart_count,
            triangles: tri_count,
            size: opts.atlas_size,
            gutter_px: opts.gutter_px,
        });
    }

    let best_scale = if max_dim > 0.0 && max_dim.is_finite() {
        let s_dim = (atlas - gutter) / max_dim;
        let s_hi = if total_area > 0.0 && total_area.is_finite() {
            s_dim.min(sqrt((atlas * atlas) / total_area))
        } else {
            s_dim
        };
        if pack_shelves(&charts, &order, s_hi, atlas, gutter, None) {
            s_hi
        } else {
            let mut lo = 0.0f64;
            let mut hi = s_hi;
            for _ in 0..PACK_SEARCH_STEPS {
                let mid = 0.5 * (lo + hi);
                if pack_shelves(&charts, &order, mid, atlas, gutter, None) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            lo
        }
    } else {
        1.0
    };

    let mut pack_pos = filled(charts.len(), (0.0f64, 0.0f64), s)?;
    if !pack_shelves(&charts, &order, best_scale, atlas, gutter, Some(&mut pack_pos)) {
        return Err(UvError::AtlasOverflow {
            charts: chart_count,
            triangles: tri_count,
            size: opts.atlas_size,
            gutter_px: opts.gutter_px,
        });
    }

    // 5. Seam vertex splitting & crease remapping.
    let corners = tris.len() * 3;
    let mut keys: V<'_, (u32, u32, u32)> = Vec::try_with_capacity_in(corners, s)?;
    for (t, tri) in tris.iter().enumerate() {
        let cid = chart_of[t];
        for (k, &v) in tri.iter().enumerate() {
            push(&mut keys, (v, cid, len32(t * 3 + k)))?;
        }
    }
    keys.sort_unstable();

    let mut new_index = filled(corners, 0u32, s)?;
    let mut out: MeshVecs<'a> = empty_vecs(opts.alloc);
    let mut last: Option<(u32, u32)> = None;
    let overlap_plant = plant.has(UvPlant::OverlapCharts);
    for &(orig_v, cid, corner) in keys.iter() {
        if last != Some((orig_v, cid)) {
            last = Some((orig_v, cid));
            if out.positions.len() >= at(opts.limits.most_vertices) {
                return Err(UvError::TooManyVertices {
                    count: out.positions.len() + 1,
                    most: opts.limits.most_vertices,
                });
            }
            let p_raw = positions[at(orig_v)];
            let p = wide(p_raw);
            let c = &charts[at(cid)];
            let lu = (dot(p, c.u_axis) - c.min_u).max(0.0);
            let lv = (dot(p, c.v_axis) - c.min_v).max(0.0);
            let (px, py) = if overlap_plant { (0.0, 0.0) } else { pack_pos[at(cid)] };
            let u_f32 = ((px + margin + lu * best_scale) / atlas).clamp(0.0, 1.0) as f32;
            let v_f32 = ((py + margin + lv * best_scale) / atlas).clamp(0.0, 1.0) as f32;
            push(&mut out.positions, p_raw)?;
            push(&mut out.normals, view.normals()[at(orig_v)])?;
            push(&mut out.uvs, [plus_zero(u_f32), plus_zero(v_f32)])?;
            if let Some(colours) = view.colours() {
                push(&mut out.colours, colours[at(orig_v)])?;
            }
        }
        new_index[at(corner)] = len32(out.positions.len()) - 1;
    }

    for t in 0..tris.len() {
        let base = t * 3;
        push(&mut out.triangles, [new_index[base], new_index[base + 1], new_index[base + 2]])?;
    }
    extend(&mut out.parts, parts)?;
    extend(&mut out.roles, roles)?;

    // Remap marked creases onto every split copy of each marked edge.
    if !creases.is_empty() {
        for (t, tri) in tris.iter().enumerate() {
            let base = t * 3;
            for k in 0..3 {
                let a = tri[k];
                let b = tri[(k + 1) % 3];
                if a == b {
                    continue;
                }
                let orig_edge = VertexPair::new(VertexId(a), VertexId(b));
                if creases.binary_search(&orig_edge).is_ok() {
                    let na = new_index[base + k];
                    let nb = new_index[base + (k + 1) % 3];
                    if na != nb {
                        push(&mut out.creases, VertexPair::new(VertexId(na), VertexId(nb)))?;
                    }
                }
            }
        }
        out.creases.sort_unstable();
        out.creases.dedup();
    }

    let mut uv_area = 0.0f64;
    for tri in out.triangles.iter() {
        let uv0 = out.uvs[at(tri[0])];
        let uv1 = out.uvs[at(tri[1])];
        let uv2 = out.uvs[at(tri[2])];
        let du1 = f64::from(uv1[0]) - f64::from(uv0[0]);
        let dv1 = f64::from(uv1[1]) - f64::from(uv0[1]);
        let du2 = f64::from(uv2[0]) - f64::from(uv0[0]);
        let dv2 = f64::from(uv2[1]) - f64::from(uv0[1]);
        uv_area += 0.5 * (du1 * dv2 - du2 * dv1).abs();
    }

    // 6. Compute MikkTSpace-compatible vertex tangents on the UV-mapped mesh.
    let mesh_no_tan = built(out).map_err(UvError::Shape)?;
    let tangents = compute_tangents_planted(&mesh_no_tan.view(), opts.alloc, s, plant)?;
    let mut vecs = mesh_no_tan.into_vecs();
    vecs.tangents = tangents;
    let mesh = built(vecs).map_err(UvError::Shape)?;

    Ok(UvMapped {
        mesh,
        texels_per_metre: best_scale,
        chart_count,
        atlas_utilization: uv_area,
    })
}

/// Compute MikkTSpace-compatible orthonormal vertex tangents `[Tx, Ty, Tz, w]` for `view`,
/// accumulating area-weighted tangent/bitangent frames per vertex and Gram-Schmidt
/// orthogonalizing each against its vertex normal.
pub fn compute_tangents<'a>(
    view: &MeshView<'_>,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Vec<[f32; 4], Alloc<'a>>, UvError> {
    compute_tangents_planted(view, alloc, scratch, NoPlant)
}

/// [`compute_tangents`] with a planted bug seam, for the test controls.
pub fn compute_tangents_planted<'a>(
    view: &MeshView<'_>,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
    plant: impl Planted<UvPlant>,
) -> Result<Vec<[f32; 4], Alloc<'a>>, UvError> {
    let positions = view.positions();
    let tris = view.triangles();
    if positions.is_empty() || tris.is_empty() {
        return Err(UvError::EmptyMesh);
    }

    let n_verts = positions.len();
    let mut tan1: V<'_, D3> = filled(n_verts, [0.0f64; 3], scratch)?;
    let mut tan2: V<'_, D3> = filled(n_verts, [0.0f64; 3], scratch)?;

    if let Some(uvs) = view.uvs() {
        for tri in tris {
            let i0 = at(tri[0]);
            let i1 = at(tri[1]);
            let i2 = at(tri[2]);
            let p0 = wide(positions[i0]);
            let p1 = wide(positions[i1]);
            let p2 = wide(positions[i2]);
            let dp1 = sub(p1, p0);
            let dp2 = sub(p2, p0);
            let area = 0.5 * length(cross(dp1, dp2));
            if !(area > 0.0) || !area.is_finite() {
                continue;
            }
            let uv0 = uvs[i0];
            let uv1 = uvs[i1];
            let uv2 = uvs[i2];
            let du1 = f64::from(uv1[0]) - f64::from(uv0[0]);
            let dv1 = f64::from(uv1[1]) - f64::from(uv0[1]);
            let du2 = f64::from(uv2[0]) - f64::from(uv0[0]);
            let dv2 = f64::from(uv2[1]) - f64::from(uv0[1]);
            let det = du1 * dv2 - du2 * dv1;
            if det.abs() > 1e-16 && det.is_finite() {
                let inv_det = 1.0 / det;
                let t_dir = scale(sub(scale(dp1, dv2), scale(dp2, dv1)), inv_det);
                let b_dir = scale(sub(scale(dp2, du1), scale(dp1, du2)), inv_det);
                if t_dir.iter().all(|x| x.is_finite()) && b_dir.iter().all(|x| x.is_finite()) {
                    let wt = scale(t_dir, area);
                    let wb = scale(b_dir, area);
                    tan1[i0] = add(tan1[i0], wt);
                    tan1[i1] = add(tan1[i1], wt);
                    tan1[i2] = add(tan1[i2], wt);
                    tan2[i0] = add(tan2[i0], wb);
                    tan2[i1] = add(tan2[i1], wb);
                    tan2[i2] = add(tan2[i2], wb);
                }
            }
        }
    }

    let skip_ortho = plant.has(UvPlant::SkipTangentOrtho);
    let normals = view.normals();
    let mut out: Vec<[f32; 4], Alloc<'a>> = Vec::try_with_capacity_in(n_verts, alloc)?;
    for v in 0..n_verts {
        let n = unit(wide(normals[v])).unwrap_or([0.0, 0.0, 1.0]);
        let t1 = tan1[v];
        let t2 = tan2[v];
        if skip_ortho {
            // Planted bug: skip Gram-Schmidt orthogonalization/normalization and invert handedness.
            let raw = if length(t1) > 0.0 { t1 } else { canonical_tangent(n) };
            let w = if dot(cross(n, raw), t2) < 0.0 { 1.0f32 } else { -1.0f32 };
            let rn = narrow(raw);
            push(&mut out, [plus_zero(rn[0]), plus_zero(rn[1]), plus_zero(rn[2]), w])?;
            continue;
        }
        let ortho = sub(t1, scale(n, dot(n, t1)));
        let t_unit = unit(ortho).unwrap_or_else(|| canonical_tangent(n));
        let w = if dot(cross(n, t_unit), t2) < 0.0 { -1.0f32 } else { 1.0f32 };
        let tn = narrow(t_unit);
        push(&mut out, [plus_zero(tn[0]), plus_zero(tn[1]), plus_zero(tn[2]), w])?;
    }

    Ok(out)
}
