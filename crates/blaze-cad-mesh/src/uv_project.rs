//! Semantic metric UV projection and MikkTSpace-compatible orthonormal vertex tangents.
//!
//! CAD parts know their own local coordinate frames and real-world dimensions, so UVs can be
//! projected directly in metric units (1 UV unit = one material tile in metres) with grain
//! aligned to the part's dominant axis — or packed into a non-overlapping `[0, 1]^2` 6-face
//! chart atlas for per-asset baked normal and surface mask maps.
//!
//! Every operation is deterministic (rule 25): vertices are split cleanly at UV chart seams so
//! textures never stretch across 90° corners, and tangents use Gram-Schmidt orthonormalization
//! in `f64` with explicit glTF 2.0 handedness (`w = ±1.0`).

#![blaze::sans_io]

use blaze_math::atan2;
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::Length;

use crate::geom::{D3, add, cross, dot, length, narrow, scale, sub, tri_normal, unit, wide};
use crate::ids::{TriId, VertexId, VertexPair};
use crate::mesh::{Mesh, MeshVecs, MeshView, ShapeError, built, empty_vecs};
use crate::plant::UvProjectPlant;
use crate::vecs::{V, at, filled, len32, push};

/// A principal local axis of a part.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UvAxis {
    X,
    Y,
    Z,
}

/// Which local direction the `u` (grain / brushed) texture axis prefers in [`UvMode::BoxMetric`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UvGrainAxis {
    /// Align `u` with the part's longest bounding-box axis (ideal for timber beams, planks,
    /// brushed extrusions, and hull plates).
    Longest,
    /// Prefer an explicit local axis whenever it is not perpendicular to the projected chart.
    Axis(UvAxis),
}

/// How [`project_uvs`] maps 3D local positions onto 2D texture coordinates.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum UvMode {
    /// Six-face axis-aligned box projection in real-world metric units (`1.0` UV unit =
    /// `tile_size`). Ideal for seamless CC0 PBR texture packs (`AmbientCG`, `Poly Haven`).
    BoxMetric {
        /// Physical size of one texture tile (must be strictly positive, e.g. `Length::m(1.0)`).
        tile_size: Length,
        /// Preferred `u` (grain) orientation.
        grain: UvGrainAxis,
    },
    /// Six-face box charts packed without overlap into `[0, 1]^2` with pixel-accurate chart
    /// gutters. Ideal for baking per-part normal maps and SDF surface mask maps.
    BoxAtlas {
        /// Target texture resolution in pixels (used to size chart gutters cleanly).
        resolution: u32,
        /// Gutter padding around each chart in pixels (typically `4..=16`).
        gutter_px: u32,
    },
    /// Single planar projection perpendicular to `normal_axis` in metric units.
    Planar {
        normal_axis: UvAxis,
        tile_size: Length,
    },
    /// Metric cylindrical wrap around `axis` with automatic seam splitting.
    Cylindrical {
        axis: UvAxis,
        /// Reference radius used to scale circumferential arc length (`2 * pi * radius`).
        radius: Length,
        tile_size: Length,
    },
}

/// Options for [`project_uvs`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct ProjectUvOpts<'a, 's> {
    pub mode: UvMode,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// Options for [`attach_tangents`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct TangentOpts<'a, 's> {
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// Errors returned by [`project_uvs`] and [`attach_tangents`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProjectUvError {
    /// A tile size, radius, or resolution argument was zero, negative, or non-finite.
    InvalidParameter { arg: &'static str },
    /// [`attach_tangents`] requires a mesh that already carries a `uvs` column.
    MissingUvs,
    /// A triangle had zero area and could not be assigned a projection chart.
    ZeroArea { tri: TriId },
    Alloc(AllocFailure),
    Built(ShapeError),
}

impl From<AllocFailure> for ProjectUvError {
    fn from(e: AllocFailure) -> ProjectUvError {
        ProjectUvError::Alloc(e)
    }
}

/// Project deterministic UVs onto `view`, splitting vertices at chart seams so every triangle
/// maps cleanly without distortion across corners.
pub fn project_uvs<'a>(view: &MeshView<'_>, opts: ProjectUvOpts<'a, '_>) -> Result<Mesh<'a>, ProjectUvError> {
    project_uvs_planted(view, opts, NoPlant)
}

/// [`project_uvs`] with a planted bug hook for oracle controls.
pub fn project_uvs_planted<'a>(
    view: &MeshView<'_>,
    opts: ProjectUvOpts<'a, '_>,
    plant: impl Planted<UvProjectPlant>,
) -> Result<Mesh<'a>, ProjectUvError> {
    let tris = view.triangles();
    let positions = view.positions();
    let s = opts.scratch;

    // Validate parameters up front.
    match opts.mode {
        UvMode::BoxMetric { tile_size, .. } | UvMode::Planar { tile_size, .. } => {
            if !(tile_size.as_m().is_finite() && tile_size.as_m() > 0.0) {
                return Err(ProjectUvError::InvalidParameter { arg: "tile_size" });
            }
        }
        UvMode::Cylindrical { radius, tile_size, .. } => {
            if !(tile_size.as_m().is_finite() && tile_size.as_m() > 0.0) {
                return Err(ProjectUvError::InvalidParameter { arg: "tile_size" });
            }
            if !(radius.as_m().is_finite() && radius.as_m() > 0.0) {
                return Err(ProjectUvError::InvalidParameter { arg: "radius" });
            }
        }
        UvMode::BoxAtlas { resolution, gutter_px } => {
            if resolution < 16 {
                return Err(ProjectUvError::InvalidParameter { arg: "resolution" });
            }
            if gutter_px * 8 >= resolution {
                return Err(ProjectUvError::InvalidParameter { arg: "gutter_px" });
            }
        }
    }

    if tris.is_empty() {
        let mut out = crate::mesh::vecs_of(view, opts.alloc)?;
        out.uvs.clear();
        for _ in 0..positions.len() {
            push(&mut out.uvs, [0.0f32, 0.0f32])?;
        }
        return built(out).map_err(ProjectUvError::Built);
    }

    // Compute mesh AABB for grain-axis selection and BoxAtlas normalization.
    let mut bmin = [f64::INFINITY; 3];
    let mut bmax = [f64::NEG_INFINITY; 3];
    for &p in positions {
        let pw = wide(p);
        for a in 0..3 {
            bmin[a] = bmin[a].min(pw[a]);
            bmax[a] = bmax[a].max(pw[a]);
        }
    }
    let ext = [
        (bmax[0] - bmin[0]).max(1e-9),
        (bmax[1] - bmin[1]).max(1e-9),
        (bmax[2] - bmin[2]).max(1e-9),
    ];
    let longest_axis = if ext[0] >= ext[1] && ext[0] >= ext[2] {
        UvAxis::X
    } else if ext[1] >= ext[2] {
        UvAxis::Y
    } else {
        UvAxis::Z
    };

    // For each triangle corner, compute `(chart_id, uv)` in f64.
    let corner_count = tris.len() * 3;
    let mut corner_chart: V<'_, u8> = Vec::try_with_capacity_in(corner_count, s)?;
    let mut corner_uv: V<'_, [f32; 2]> = Vec::try_with_capacity_in(corner_count, s)?;

    // Track per-chart 2D bounds for BoxAtlas packing.
    let mut chart_min = [[f64::INFINITY; 2]; 6];
    let mut chart_max = [[f64::NEG_INFINITY; 2]; 6];
    let mut raw_atlas_uv: V<'_, [f64; 2]> = Vec::new_in(s);
    if matches!(opts.mode, UvMode::BoxAtlas { .. }) {
        raw_atlas_uv.try_reserve(corner_count)?;
    }

    for (ti, tri) in tris.iter().enumerate() {
        let p0 = wide(positions[at(tri[0])]);
        let p1 = wide(positions[at(tri[1])]);
        let p2 = wide(positions[at(tri[2])]);
        let fnrm = tri_normal(p0, p1, p2);
        let Some(n) = unit(fnrm) else {
            return Err(ProjectUvError::ZeroArea { tri: TriId(len32(ti)) });
        };

        match opts.mode {
            UvMode::BoxMetric { tile_size, grain } => {
                let inv_tile = 1.0 / tile_size.as_m();
                let pref = match grain {
                    UvGrainAxis::Longest => longest_axis,
                    UvGrainAxis::Axis(ax) => ax,
                };
                let chart = if plant.has(UvProjectPlant::SkipSeamSplit) { 0u8 } else { dominant_box_chart(n) };
                for &vi in tri {
                    let pw = wide(positions[at(vi)]);
                    let [u, v] = box_chart_uv(chart, pw, pref);
                    push(&mut corner_chart, chart)?;
                    push(&mut corner_uv, [(u * inv_tile) as f32, (v * inv_tile) as f32])?;
                }
            }
            UvMode::BoxAtlas { .. } => {
                let chart = if plant.has(UvProjectPlant::SkipSeamSplit) { 0u8 } else { dominant_box_chart(n) };
                let ci = usize::from(chart);
                for &vi in tri {
                    let pw = wide(positions[at(vi)]);
                    let uv = box_chart_uv(chart, pw, longest_axis);
                    chart_min[ci][0] = chart_min[ci][0].min(uv[0]);
                    chart_min[ci][1] = chart_min[ci][1].min(uv[1]);
                    chart_max[ci][0] = chart_max[ci][0].max(uv[0]);
                    chart_max[ci][1] = chart_max[ci][1].max(uv[1]);
                    push(&mut corner_chart, chart)?;
                    push(&mut raw_atlas_uv, uv)?;
                }
            }
            UvMode::Planar { normal_axis, tile_size } => {
                let inv_tile = 1.0 / tile_size.as_m();
                let chart = match normal_axis {
                    UvAxis::X => 0u8,
                    UvAxis::Y => 2u8,
                    UvAxis::Z => 4u8,
                };
                for &vi in tri {
                    let pw = wide(positions[at(vi)]);
                    let [u, v] = box_chart_uv(chart, pw, longest_axis);
                    push(&mut corner_chart, 0)?;
                    push(&mut corner_uv, [(u * inv_tile) as f32, (v * inv_tile) as f32])?;
                }
            }
            UvMode::Cylindrical { axis, radius, tile_size } => {
                let inv_tile = 1.0 / tile_size.as_m();
                let r_m = radius.as_m();
                let center = [
                    0.5 * (bmin[0] + bmax[0]),
                    0.5 * (bmin[1] + bmax[1]),
                    0.5 * (bmin[2] + bmax[2]),
                ];
                let mut angs = [0.0f64; 3];
                let mut hs = [0.0f64; 3];
                for (k, &vi) in tri.iter().enumerate() {
                    let pw = sub(wide(positions[at(vi)]), center);
                    let (rx, ry, h) = match axis {
                        UvAxis::X => (pw[1], pw[2], pw[0]),
                        UvAxis::Y => (pw[2], pw[0], pw[1]),
                        UvAxis::Z => (pw[0], pw[1], pw[2]),
                    };
                    // Map angle from [-pi, pi] to [0, 2*pi).
                    let a = atan2(ry, rx);
                    angs[k] = if a < 0.0 { a + core::f64::consts::TAU } else { a };
                    hs[k] = h;
                }
                // Unwrap seam-crossing triangles so edges never stretch backward across 0/2pi.
                let min_a = angs[0].min(angs[1]).min(angs[2]);
                let max_a = angs[0].max(angs[1]).max(angs[2]);
                let wraps = !plant.has(UvProjectPlant::SkipSeamSplit) && (max_a - min_a) > core::f64::consts::PI;
                for k in 0..3 {
                    let mut a = angs[k];
                    let mut chart = 0u8;
                    if wraps && a < core::f64::consts::PI {
                        a += core::f64::consts::TAU;
                        chart = 1u8;
                    }
                    let u = a * r_m * inv_tile;
                    let v = hs[k] * inv_tile;
                    push(&mut corner_chart, chart)?;
                    push(&mut corner_uv, [u as f32, v as f32])?;
                }
            }
        }
    }

    // Finalize BoxAtlas UVs: shelf-pack the active box charts into [0, 1]^2 with uniform
    // texel density and pixel-accurate gutters. Charts taller than wide are rotated 90° so
    // elongated CAD parts pack tightly instead of wasting grid cells.
    if let UvMode::BoxAtlas { resolution, gutter_px } = opts.mode {
        let pad = f64::from(gutter_px) / f64::from(resolution);
        let mut active = [false; 6];
        let mut rot = [false; 6];
        let mut cw = [0.0f64; 6];
        let mut ch = [0.0f64; 6];
        let mut order = [0usize; 6];
        let mut count = 0usize;
        let mut max_dim = 1e-9f64;
        for ci in 0..6 {
            if chart_min[ci][0].is_finite() {
                let du = (chart_max[ci][0] - chart_min[ci][0]).max(1e-9);
                let dv = (chart_max[ci][1] - chart_min[ci][1]).max(1e-9);
                active[ci] = true;
                if dv > du {
                    rot[ci] = true;
                    cw[ci] = dv;
                    ch[ci] = du;
                } else {
                    cw[ci] = du;
                    ch[ci] = dv;
                }
                max_dim = max_dim.max(cw[ci]).max(ch[ci]);
                order[count] = ci;
                count += 1;
            }
        }
        // Sort active charts deterministically by descending height, then descending width, then id.
        for i in 1..count {
            let mut j = i;
            while j > 0 {
                let a = order[j - 1];
                let b = order[j];
                let swap = ch[b] > ch[a]
                    || (ch[b] == ch[a] && cw[b] > cw[a])
                    || (ch[b] == ch[a] && cw[b] == cw[a] && b < a);
                if swap {
                    order.swap(j - 1, j);
                    j -= 1;
                } else {
                    break;
                }
            }
        }

        let try_pack = |scale: f64, pos: &mut [[f64; 2]; 6]| -> bool {
            let mut cx = 0.0f64;
            let mut cy = 0.0f64;
            let mut shelf_h = 0.0f64;
            for &ci in &order[..count] {
                let rw = cw[ci] * scale + 2.0 * pad;
                let rh = ch[ci] * scale + 2.0 * pad;
                if rw > 1.0 + 1e-12 || rh > 1.0 + 1e-12 {
                    return false;
                }
                if cx > 0.0 && cx + rw > 1.0 + 1e-12 {
                    cy += shelf_h;
                    cx = 0.0;
                    shelf_h = 0.0;
                }
                if cy + rh > 1.0 + 1e-12 {
                    return false;
                }
                pos[ci] = [cx + pad, cy + pad];
                cx += rw;
                shelf_h = shelf_h.max(rh);
            }
            true
        };

        let mut lo = 0.0f64;
        let mut hi = ((1.0 - 2.0 * pad).max(1e-4)) / max_dim;
        let mut best_pos = [[pad; 2]; 6];
        let mut tmp_pos = [[pad; 2]; 6];
        // Ensure hi is an upper bound (already guaranteed since a single chart of size max_dim fills [0,1]).
        for _ in 0..44 {
            let mid = 0.5 * (lo + hi);
            if try_pack(mid, &mut tmp_pos) {
                lo = mid;
                best_pos = tmp_pos;
            } else {
                hi = mid;
            }
        }
        let scale = lo;

        for idx_c in 0..corner_count {
            let ci = usize::from(corner_chart[idx_c]);
            let du = raw_atlas_uv[idx_c][0] - chart_min[ci][0];
            let dv = raw_atlas_uv[idx_c][1] - chart_min[ci][1];
            let (lu, lv) = if rot[ci] { (dv, du) } else { (du, dv) };
            let u = best_pos[ci][0] + lu * scale;
            let v = best_pos[ci][1] + lv * scale;
            push(&mut corner_uv, [u as f32, v as f32])?;
        }
    }

    // Deduplicate vertices by `(old_vertex, chart_id)` deterministically, splitting only at
    // chart boundaries.
    let mut keys: V<'_, (u32, u8, u32)> = Vec::try_with_capacity_in(corner_count, s)?;
    for (ti, tri) in tris.iter().enumerate() {
        for (k, &v) in tri.iter().enumerate() {
            let c = len32(ti * 3 + k);
            push(&mut keys, (v, corner_chart[at(c)], c))?;
        }
    }
    keys.sort_unstable();

    let mut new_index = filled(corner_count, 0u32, s)?;
    let mut out: MeshVecs<'a> = empty_vecs(opts.alloc);
    let mut last: Option<(u32, u8)> = None;
    for &(v, chart, corner) in keys.iter() {
        if last != Some((v, chart)) {
            last = Some((v, chart));
            let vi = at(v);
            push(&mut out.positions, positions[vi])?;
            push(&mut out.normals, view.normals()[vi])?;
            if let Some(t) = view.tangents() {
                push(&mut out.tangents, t[vi])?;
            }
            push(&mut out.uvs, corner_uv[at(corner)])?;
            if let Some(col) = view.colours() {
                push(&mut out.colours, col[vi])?;
            }
        }
        new_index[at(corner)] = len32(out.positions.len()) - 1;
    }

    for ti in 0..tris.len() {
        let c = ti * 3;
        push(&mut out.triangles, [new_index[c], new_index[c + 1], new_index[c + 2]])?;
    }
    crate::vecs::extend(&mut out.parts, view.parts())?;
    crate::vecs::extend(&mut out.roles, view.roles())?;

    // Map any existing crease marks onto every split vertex pair of each triangle edge.
    if !view.creases().is_empty() {
        let orig_creases = view.creases();
        for (ti, tri) in tris.iter().enumerate() {
            let c = ti * 3;
            for &(ka, kb) in &[(0usize, 1usize), (1, 2), (2, 0)] {
                let orig_edge = VertexPair::new(VertexId(tri[ka]), VertexId(tri[kb]));
                if orig_creases.binary_search(&orig_edge).is_ok() {
                    let na = new_index[c + ka];
                    let nb = new_index[c + kb];
                    if na != nb {
                        push(&mut out.creases, VertexPair::new(VertexId(na), VertexId(nb)))?;
                    }
                }
            }
        }
        out.creases.sort_unstable();
        out.creases.dedup();
    }

    built(out).map_err(ProjectUvError::Built)
}

/// Compute glTF 2.0 vertex tangents `[tx, ty, tz, w]` (`TANGENT`) for a mesh that carries UVs.
///
/// Uses area-weighted UV derivative accumulation per vertex followed by Gram-Schmidt
/// orthonormalization against each vertex's shading normal and handedness computation
/// (`w = +1.0` or `-1.0`, matching MikkTSpace / glTF 2.0 section 3.7.2.1).
pub fn attach_tangents<'a>(view: &MeshView<'_>, opts: TangentOpts<'a, '_>) -> Result<Mesh<'a>, ProjectUvError> {
    attach_tangents_planted(view, opts, NoPlant)
}

/// [`attach_tangents`] with a planted bug hook for oracle controls.
pub fn attach_tangents_planted<'a>(
    view: &MeshView<'_>,
    opts: TangentOpts<'a, '_>,
    plant: impl Planted<UvProjectPlant>,
) -> Result<Mesh<'a>, ProjectUvError> {
    let Some(uvs) = view.uvs() else {
        return Err(ProjectUvError::MissingUvs);
    };
    let n_verts = view.positions().len();
    let s = opts.scratch;

    let mut tan_acc: V<'_, D3> = filled(n_verts, [0.0f64; 3], s)?;
    let mut bitan_acc: V<'_, D3> = filled(n_verts, [0.0f64; 3], s)?;

    for tri in view.triangles() {
        let i0 = at(tri[0]);
        let i1 = at(tri[1]);
        let i2 = at(tri[2]);
        let p0 = wide(view.positions()[i0]);
        let p1 = wide(view.positions()[i1]);
        let p2 = wide(view.positions()[i2]);
        let e1 = sub(p1, p0);
        let e2 = sub(p2, p0);

        let uv0 = uvs[i0];
        let uv1 = uvs[i1];
        let uv2 = uvs[i2];
        let du1 = f64::from(uv1[0]) - f64::from(uv0[0]);
        let dv1 = f64::from(uv1[1]) - f64::from(uv0[1]);
        let du2 = f64::from(uv2[0]) - f64::from(uv0[0]);
        let dv2 = f64::from(uv2[1]) - f64::from(uv0[1]);

        let det = du1 * dv2 - du2 * dv1;
        if det.abs() <= 1e-18 || !det.is_finite() {
            continue;
        }
        // Weight by triangle area (sign(det) keeps magnitude proportional to triangle area * UV scale).
        let inv = if det > 0.0 { 1.0 } else { -1.0 };
        let t_dir = scale(sub(scale(e1, dv2), scale(e2, dv1)), inv);
        let b_dir = scale(sub(scale(e2, du1), scale(e1, du2)), inv);

        for &idx_v in &[i0, i1, i2] {
            tan_acc[idx_v] = add(tan_acc[idx_v], t_dir);
            bitan_acc[idx_v] = add(bitan_acc[idx_v], b_dir);
        }
    }

    let mut out = crate::mesh::vecs_of(view, opts.alloc)?;
    out.tangents.clear();
    out.tangents.try_reserve(n_verts)?;

    for v in 0..n_verts {
        let n = unit(wide(view.normals()[v])).unwrap_or([0.0, 0.0, 1.0]);
        let t = tan_acc[v];
        // Gram-Schmidt orthogonalize `t` against `n`.
        let t_proj = sub(t, scale(n, dot(n, t)));
        let (t_unit, w) = if let Some(tu) = unit(t_proj) {
            let b = bitan_acc[v];
            let handed = if length(b) > 1e-18 && dot(cross(n, tu), b) < 0.0 { -1.0f32 } else { 1.0f32 };
            let w = if plant.has(UvProjectPlant::FlipHandedness) { -handed } else { handed };
            (tu, w)
        } else {
            // Deterministic orthonormal fallback when a vertex has degenerate UVs.
            let helper = if n[2].abs() < 0.9 { [0.0, 0.0, 1.0] } else { [1.0, 0.0, 0.0] };
            let tu = unit(cross(helper, n)).unwrap_or([1.0, 0.0, 0.0]);
            (tu, 1.0f32)
        };
        let tn = narrow(t_unit);
        push(&mut out.tangents, [tn[0], tn[1], tn[2], w])?;
    }

    built(out).map_err(ProjectUvError::Built)
}

/// Dominant signed axis chart index `0..6`: `0:+X, 1:-X, 2:+Y, 3:-Y, 4:+Z, 5:-Z`.
/// Ties break deterministically (`Z > Y > X`, positive > negative).
#[inline]
fn dominant_box_chart(n: D3) -> u8 {
    let ax = n[0].abs();
    let ay = n[1].abs();
    let az = n[2].abs();
    if az >= ax && az >= ay {
        if n[2] >= 0.0 { 4 } else { 5 }
    } else if ay >= ax {
        if n[1] >= 0.0 { 2 } else { 3 }
    } else if n[0] >= 0.0 {
        0
    } else {
        1
    }
}

/// Project a 3D point onto one of the 6 box faces with right-handed `(u, v)` and grain alignment.
#[inline]
fn box_chart_uv(chart: u8, p: D3, grain: UvAxis) -> [f64; 2] {
    // Base right-handed (u, v) per signed face so tangent cross bitangent aligns with outward normal.
    let (u, v, u_axis, v_axis) = match chart {
        0 => (p[1], p[2], UvAxis::Y, UvAxis::Z),   // +X
        1 => (-p[1], p[2], UvAxis::Y, UvAxis::Z),  // -X
        2 => (-p[0], p[2], UvAxis::X, UvAxis::Z),  // +Y
        3 => (p[0], p[2], UvAxis::X, UvAxis::Z),   // -Y
        4 => (p[0], p[1], UvAxis::X, UvAxis::Y),   // +Z
        _ => (p[0], -p[1], UvAxis::X, UvAxis::Y),  // -Z
    };
    // Rotate 90° in-plane when the caller's preferred grain axis is `v_axis` (keeping handedness!).
    if grain == v_axis && grain != u_axis {
        [v, -u]
    } else {
        [u, v]
    }
}
