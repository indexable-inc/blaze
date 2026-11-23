//! Fabric grain & physical rest dimensions strictly independent of texture UVs (`TEXCOORD_0`).
//!
//! Why physical rest coordinates (`GarmentMesh::rest_pos_m`) must be decoupled from texture UVs:
//! - Artists freely rescale, rotate, tile, or atlas texture UVs (`TEXCOORD_0`) for weave scale,
//!   camo patterns, or trim sheets without changing the physical textile's warp/weft grainline or
//!   metres-per-edge rest length.
//! - Measuring the exact 2D -> 3D deformation gradient `F = [e1, e2] * Dm^{-1}` strictly from
//!   `rest_pos_m` (aligned to `(weft, warp)` by `-grain_angle_rad`) lets Blaze distinguish where
//!   cloth pulls taut (`warp_stretch > 1.0`, e.g. front of a bent knee) from where cloth actually
//!   compresses (`principal_min < 0.96`, e.g. behind the knee), and lets [`relax_compression_zones`]
//!   gather compressed folds cleanly along `compression_dir_world` without bulk ballooning!

#![blaze::sans_io]

use blaze_math::{Vec2, Vec3, cos, sin, sqrt};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::Length;

use crate::collider::MeshCollider;
use crate::detail::{GarmentMesh, PanelKind};

/// Physical fabric grainline orientation and completely independent texture UV transform.
///
/// - `grain_angle_rad`: orientation of the fabric's warp grain relative to the 2D pattern `+y`
///   axis (`0.0` = straight grain, `PI / 4` = true bias cut). Physical `rest_pos_m` rotates 2D
///   pattern coordinates by `-grain_angle_rad` so `+x` is always true weft and `+y` is always
///   true warp.
/// - `uv_scale`, `uv_rotation_rad`, `uv_offset`: artistic texture-space transform applied ONLY to
///   `GarmentMesh::uvs` (`TEXCOORD_0`), never affecting `rest_pos_m` or cloth strain!
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FabricGrain {
    pub grain_angle_rad: f64,
    pub uv_scale: [f64; 2],
    pub uv_rotation_rad: f64,
    pub uv_offset: [f64; 2],
}

impl FabricGrain {
    /// Standard straight-grain cut (`grain_angle_rad = 0`) with 1:1 unrotated texture UVs.
    pub const STRAIGHT: Self = Self {
        grain_angle_rad: 0.0,
        uv_scale: [1.0, 1.0],
        uv_rotation_rad: 0.0,
        uv_offset: [0.0, 0.0],
    };

    /// Map flat 2D pattern coordinates `uv_m` (in metres) into true grain-aligned physical rest
    /// coordinates `(weft_m, warp_m)` by rotating by `-grain_angle_rad`.
    #[inline]
    pub fn to_grain_rest(&self, uv_m: Vec2) -> Vec2 {
        if self.grain_angle_rad == 0.0 {
            return uv_m;
        }
        let c = cos(self.grain_angle_rad);
        let s = sin(self.grain_angle_rad);
        Vec2::new(uv_m.x * c + uv_m.y * s, -uv_m.x * s + uv_m.y * c)
    }

    /// Map flat 2D pattern coordinates `uv_m` (in metres) into artistic texture UVs (`TEXCOORD_0`)
    /// using `uv_meters` repeat size plus independent `uv_rotation_rad`, `uv_scale`, and `uv_offset`.
    #[inline]
    pub fn to_texture_uv(&self, uv_m: Vec2, uv_meters: f64) -> Vec2 {
        let inv = 1.0 / uv_meters.max(1e-4);
        let bx = uv_m.x * inv;
        let by = uv_m.y * inv;
        let (rx, ry) = if self.uv_rotation_rad == 0.0 {
            (bx, by)
        } else {
            let c = cos(self.uv_rotation_rad);
            let s = sin(self.uv_rotation_rad);
            (bx * c - by * s, bx * s + by * c)
        };
        Vec2::new(
            rx * self.uv_scale[0] + self.uv_offset[0],
            ry * self.uv_scale[1] + self.uv_offset[1],
        )
    }
}

impl Default for FabricGrain {
    #[inline]
    fn default() -> Self {
        Self::STRAIGHT
    }
}

/// Planted-bug control enum proving strain measurement strictly ignores `TEXCOORD_0` texture UVs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GrainPlant {
    None,
    /// Planted bug: erroneously reads `garment.uvs` (`TEXCOORD_0`) instead of `garment.rest_pos_m`.
    UseTextureUvsInsteadOfRest,
}

impl Plants for GrainPlant {}

/// Exact per-triangle continuum strain tensor telemetry measured from physical `rest_pos_m`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TriangleStrain {
    pub tri: u32,
    pub panel_id: u16,
    /// Stretch ratio along the fabric's true warp (`+y` grain) axis (`1.0` = rest, `> 1` = taut).
    pub warp_stretch: f64,
    /// Stretch ratio along the fabric's true weft (`+x` grain) axis (`1.0` = rest, `> 1` = taut).
    pub weft_stretch: f64,
    /// Normalized in-plane bias shear `C_uv / (weft * warp)` (`0.0` = orthogonal, non-zero = shear).
    pub bias_shear: f64,
    /// Maximum principal stretch `sqrt(lambda_max)` of `C = F^T F`.
    pub principal_max: f64,
    /// Minimum principal stretch `sqrt(lambda_min)` of `C = F^T F` (`< 1.0` = compression).
    pub principal_min: f64,
    /// Deformed-to-rest surface area ratio `area_3d / area_rest_2d`.
    pub area_ratio: f64,
    /// 3D world-space unit vector along which the triangle experiences maximum compression
    /// (`normalize(F * v_min)`), indicating the fold-gathering direction.
    pub compression_dir_world: Vec3,
}

/// Per-panel aggregated cloth tension and compression summary.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PanelStrainSummary {
    pub panel_id: u16,
    pub panel_name: &'static str,
    pub triangles: u32,
    pub taut_triangles: u32,
    pub compressed_triangles: u32,
    pub max_warp_stretch: f64,
    pub min_warp_stretch: f64,
    pub min_principal_stretch: f64,
    pub mean_area_ratio: f64,
}

/// Complete garment strain report produced by [`measure_cloth_strain`].
pub struct StrainReport<'a> {
    pub triangles: Vec<TriangleStrain, Alloc<'a>>,
    pub panels: Vec<PanelStrainSummary, Alloc<'a>>,
    pub max_warp_stretch: f64,
    pub min_principal_stretch: f64,
    pub taut_count: u32,
    pub compressed_count: u32,
}

/// Options controlling compression-zone fold gathering in [`relax_compression_zones`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct KneeGatherOpts<'a, 's> {
    pub iterations: u32 = 12,
    pub taut_stiffness: f64 = 0.85,
    pub gather_damping: f64 = 0.55,
    pub fold_amplitude: Length = Length::nm(3_500_000),
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), AllocFailure> {
    v.try_push(x).map_err(|e| e.why)
}

/// Arbitrarily rescale, rotate, and translate texture UVs (`GarmentMesh::uvs` / `TEXCOORD_0`)
/// globally (`panel_name == None`) or for a specific named panel (`Some(name)`), without touching
/// physical grain rest coordinates (`GarmentMesh::rest_pos_m`).
pub fn set_garment_texture_uv_transform(
    garment: &mut GarmentMesh<'_>,
    panel_name: Option<&str>,
    scale: [f64; 2],
    rotation_rad: f64,
    offset: [f64; 2],
) {
    let c = cos(rotation_rad);
    let s = sin(rotation_rad);
    let xform = |uv: Vec2| -> Vec2 {
        let rx = uv.x * c - uv.y * s;
        let ry = uv.x * s + uv.y * c;
        Vec2::new(rx * scale[0] + offset[0], ry * scale[1] + offset[1])
    };

    let n_verts = garment.uvs.len();
    let Some(target) = panel_name else {
        for uv in garment.uvs.iter_mut() {
            *uv = xform(*uv);
        }
        return;
    };

    // Zero-allocation chunked bitset over vertex indices so every vertex belonging to `target`
    // is transformed at most once, even when vertex indices interleave across panels.
    const WORDS: usize = 256;
    const CHUNK: usize = WORDS * 64;
    let mut base = 0usize;
    while base < n_verts {
        let mut bits = [0u64; WORDS];
        let limit = (base + CHUNK).min(n_verts);
        for (ti, &[a, b, c_idx]) in garment.triangles.iter().enumerate() {
            let pid = garment.tri_panel.get(ti).copied().unwrap_or(u16::MAX);
            let matches = garment
                .panels
                .get(pid as usize)
                .map(|p| p.name == target)
                .unwrap_or(false);
            if !matches {
                continue;
            }
            for v in [a as usize, b as usize, c_idx as usize] {
                if v >= base && v < limit {
                    let rel = v - base;
                    bits[rel >> 6] |= 1u64 << (rel & 63);
                }
            }
        }
        for (wi, &word) in bits.iter().enumerate() {
            let mut w = word;
            while w != 0 {
                let tz = w.trailing_zeros() as usize;
                let vi = base + (wi << 6) + tz;
                if vi < n_verts {
                    garment.uvs[vi] = xform(garment.uvs[vi]);
                }
                w &= w - 1;
            }
        }
        base = limit;
    }
}

/// Evaluate exact 2D -> 3D triangle deformation gradient `F = [e1, e2] * Dm^{-1}` and Cauchy-Green
/// invariants `C = F^T F` for one triangle.
#[inline]
fn eval_tri_strain(
    tri: u32,
    panel_id: u16,
    pa: Vec3,
    pb: Vec3,
    pc: Vec3,
    ra: Vec2,
    rb: Vec2,
    rc: Vec2,
) -> (TriangleStrain, bool) {
    let d1 = rb - ra;
    let d2 = rc - ra;
    let det = d1.x * d2.y - d2.x * d1.y;
    if det.abs() <= 1e-14 {
        return (
            TriangleStrain {
                tri,
                panel_id,
                warp_stretch: 1.0,
                weft_stretch: 1.0,
                bias_shear: 0.0,
                principal_max: 1.0,
                principal_min: 1.0,
                area_ratio: 1.0,
                compression_dir_world: Vec3::ZERO,
            },
            false,
        );
    }

    let inv_det = 1.0 / det;
    let i00 = d2.y * inv_det;
    let i01 = -d2.x * inv_det;
    let i10 = -d1.y * inv_det;
    let i11 = d1.x * inv_det;

    let e1 = pb - pa;
    let e2 = pc - pa;

    // Columns of F = [dX/du_weft, dX/dv_warp]
    let f_u = e1 * i00 + e2 * i10;
    let f_v = e1 * i01 + e2 * i11;

    let c_uu = f_u.dot(f_u);
    let c_vv = f_v.dot(f_v);
    let c_uv = f_u.dot(f_v);

    let weft_stretch = sqrt(c_uu.max(0.0));
    let warp_stretch = sqrt(c_vv.max(0.0));
    let denom = (weft_stretch * warp_stretch).max(1e-12);
    let bias_shear = (c_uv / denom).clamp(-1.0, 1.0);

    let tr = c_uu + c_vv;
    let diff = c_uu - c_vv;
    let disc = sqrt((diff * diff + 4.0 * c_uv * c_uv).max(0.0));
    let lam_max = (0.5 * (tr + disc)).max(0.0);
    let lam_min = (0.5 * (tr - disc)).max(0.0);

    let principal_max = sqrt(lam_max);
    let principal_min = sqrt(lam_min);
    let area_ratio = sqrt((c_uu * c_vv - c_uv * c_uv).max(0.0));

    // Analytical eigenvector of C corresponding to lam_min (maximum compression direction).
    let cand0 = Vec2::new(-c_uv, c_uu - lam_min);
    let cand1 = Vec2::new(c_vv - lam_min, -c_uv);
    let n0 = cand0.dot(cand0);
    let n1 = cand1.dot(cand1);
    let mut v_min = if n0 >= n1 && n0 > 1e-24 {
        cand0 * (1.0 / sqrt(n0))
    } else if n1 > 1e-24 {
        cand1 * (1.0 / sqrt(n1))
    } else {
        Vec2::new(0.0, 1.0)
    };
    if v_min.y < 0.0 || (v_min.y == 0.0 && v_min.x < 0.0) {
        v_min = Vec2::new(-v_min.x, -v_min.y);
    }

    let dir_world = (f_u * v_min.x + f_v * v_min.y).normalized().unwrap_or(Vec3::ZERO);

    (
        TriangleStrain {
            tri,
            panel_id,
            warp_stretch,
            weft_stretch,
            bias_shear,
            principal_max,
            principal_min,
            area_ratio,
            compression_dir_world: dir_world,
        },
        true,
    )
}

/// Measure exact continuum fabric strain strictly from physical `GarmentMesh::rest_pos_m` (never
/// texture UVs `TEXCOORD_0`), identifying taut warp zones and compressed fold-gathering zones.
pub fn measure_cloth_strain<'a>(
    garment: &GarmentMesh<'_>,
    posed_positions: Option<&[Vec3]>,
    taut_threshold: f64,
    compress_threshold: f64,
    alloc: Alloc<'a>,
) -> Result<StrainReport<'a>, AllocFailure> {
    measure_cloth_strain_planted(garment, posed_positions, taut_threshold, compress_threshold, alloc, NoPlant)
}

/// Planted-bug seam entry point for [`measure_cloth_strain`].
pub fn measure_cloth_strain_planted<'a>(
    garment: &GarmentMesh<'_>,
    posed_positions: Option<&[Vec3]>,
    taut_threshold: f64,
    compress_threshold: f64,
    alloc: Alloc<'a>,
    plant: impl Planted<GrainPlant>,
) -> Result<StrainReport<'a>, AllocFailure> {
    let positions = posed_positions.unwrap_or(&garment.positions);
    let use_uvs = plant.has(GrainPlant::UseTextureUvsInsteadOfRest);
    let rest_coords: &[Vec2] = if use_uvs { &garment.uvs } else { &garment.rest_pos_m };

    let mut triangles = Vec::new_in(alloc);
    let mut panels = Vec::new_in(alloc);
    for pm in garment.panels.iter() {
        push(
            &mut panels,
            PanelStrainSummary {
                panel_id: pm.id,
                panel_name: pm.name,
                triangles: 0,
                taut_triangles: 0,
                compressed_triangles: 0,
                max_warp_stretch: 0.0,
                min_warp_stretch: f64::INFINITY,
                min_principal_stretch: f64::INFINITY,
                mean_area_ratio: 0.0,
            },
        )?;
    }

    let mut max_warp_stretch = 0.0f64;
    let mut min_principal_stretch = f64::INFINITY;
    let mut taut_count = 0u32;
    let mut compressed_count = 0u32;

    for (ti, &[ia, ib, ic]) in garment.triangles.iter().enumerate() {
        let a = ia as usize;
        let b = ib as usize;
        let c = ic as usize;
        let panel_id = garment.tri_panel.get(ti).copied().unwrap_or(0);
        if a >= positions.len()
            || b >= positions.len()
            || c >= positions.len()
            || a >= rest_coords.len()
            || b >= rest_coords.len()
            || c >= rest_coords.len()
        {
            continue;
        }
        let (st, valid) = eval_tri_strain(
            ti as u32,
            panel_id,
            positions[a],
            positions[b],
            positions[c],
            rest_coords[a],
            rest_coords[b],
            rest_coords[c],
        );
        push(&mut triangles, st)?;

        if !valid {
            continue;
        }

        let is_hardware = garment
            .panels
            .get(panel_id as usize)
            .map(|p| p.kind == PanelKind::Hardware)
            .unwrap_or(false);
        if is_hardware {
            continue;
        }

        let is_taut = st.warp_stretch > taut_threshold || st.principal_max > taut_threshold;
        let is_compressed = st.principal_min < compress_threshold;

        if st.warp_stretch > max_warp_stretch {
            max_warp_stretch = st.warp_stretch;
        }
        if st.principal_min < min_principal_stretch {
            min_principal_stretch = st.principal_min;
        }
        if is_taut {
            taut_count += 1;
        }
        if is_compressed {
            compressed_count += 1;
        }

        if let Some(ps) = panels.get_mut(panel_id as usize) {
            ps.triangles += 1;
            if is_taut {
                ps.taut_triangles += 1;
            }
            if is_compressed {
                ps.compressed_triangles += 1;
            }
            if st.warp_stretch > ps.max_warp_stretch {
                ps.max_warp_stretch = st.warp_stretch;
            }
            if st.warp_stretch < ps.min_warp_stretch {
                ps.min_warp_stretch = st.warp_stretch;
            }
            if st.principal_min < ps.min_principal_stretch {
                ps.min_principal_stretch = st.principal_min;
            }
            ps.mean_area_ratio += st.area_ratio;
        }
    }

    for ps in panels.iter_mut() {
        if ps.triangles > 0 {
            ps.mean_area_ratio /= ps.triangles as f64;
        } else {
            ps.min_warp_stretch = 1.0;
            ps.min_principal_stretch = 1.0;
            ps.mean_area_ratio = 1.0;
        }
    }
    if !min_principal_stretch.is_finite() {
        min_principal_stretch = 1.0;
    }

    Ok(StrainReport {
        triangles,
        panels,
        max_warp_stretch,
        min_principal_stretch,
        taut_count,
        compressed_count,
    })
}

/// Relax posed garment vertices using true grain-aligned `rest_pos_m` strain so taut zones (e.g.
/// front of a bent knee where `warp_stretch > 1.0`) pull cleanly tight over the joint while
/// compressed zones (`principal_min < 0.96`, e.g. behind the knee in a deep squat) gather inward
/// along `compression_dir_world` without radial ballooning or penetrating `body_collider`.
pub fn relax_compression_zones<'a>(
    garment: &GarmentMesh<'_>,
    posed_positions: &[Vec3],
    body_collider: Option<&MeshCollider<'_>>,
    opts: KneeGatherOpts<'a, '_>,
) -> Result<Vec<Vec3, Alloc<'a>>, AllocFailure> {
    let n_verts = posed_positions.len();
    let mut pos = Vec::new_in(opts.alloc);
    for &p in posed_positions.iter() {
        push(&mut pos, p)?;
    }
    if n_verts == 0 || garment.triangles.is_empty() || garment.rest_pos_m.len() != n_verts {
        return Ok(pos);
    }

    // Build unique welded soft-cloth edges with exact 2D physical rest lengths from `rest_pos_m`.
    #[derive(Clone, Copy)]
    struct RestEdge {
        a: u32,
        b: u32,
        rest_len: f64,
    }
    let mut raw_edges = Vec::new_in(opts.scratch);
    let mut is_soft_root = Vec::new_in(opts.scratch);
    for _ in 0..n_verts {
        push(&mut is_soft_root, false)?;
    }

    for (ti, &[ia, ib, ic]) in garment.triangles.iter().enumerate() {
        if garment.tri_layer.get(ti).copied().unwrap_or(0) > 2 {
            continue;
        }
        let ra = garment.weld_of(ia);
        let rb = garment.weld_of(ib);
        let rc = garment.weld_of(ic);
        if (ra as usize) < n_verts && garment.rigid_anchor.get(ra as usize).copied().flatten().is_none() {
            is_soft_root[ra as usize] = true;
        }
        if (rb as usize) < n_verts && garment.rigid_anchor.get(rb as usize).copied().flatten().is_none() {
            is_soft_root[rb as usize] = true;
        }
        if (rc as usize) < n_verts && garment.rigid_anchor.get(rc as usize).copied().flatten().is_none() {
            is_soft_root[rc as usize] = true;
        }
        for (u, v, ru, rv) in [(ia, ib, ra, rb), (ib, ic, rb, rc), (ic, ia, rc, ra)] {
            if ru != rv && (u as usize) < n_verts && (v as usize) < n_verts {
                let (lo, hi) = if ru < rv { (ru, rv) } else { (rv, ru) };
                let rl = (garment.rest_pos_m[v as usize] - garment.rest_pos_m[u as usize]).length();
                if rl > 1e-6 {
                    push(&mut raw_edges, (lo, hi, rl))?;
                }
            }
        }
    }
    raw_edges.sort_unstable_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)));

    let mut edges = Vec::new_in(opts.scratch);
    let mut idx = 0usize;
    while idx < raw_edges.len() {
        let (lo, hi, _) = raw_edges[idx];
        let mut end = idx + 1;
        let mut rl_sum = raw_edges[idx].2;
        while end < raw_edges.len() && raw_edges[end].0 == lo && raw_edges[end].1 == hi {
            rl_sum += raw_edges[end].2;
            end += 1;
        }
        let rl_avg = rl_sum / ((end - idx) as f64);
        push(&mut edges, RestEdge { a: lo, b: hi, rest_len: rl_avg })?;
        idx = end;
    }

    // Detect boundary/pinned ring vertices (vertices belonging to only 1 boundary edge) so the
    // cylinder/panel ends stay anchored while interior knee cloth relaxes smoothly.
    let mut vert_normals = Vec::new_in(opts.scratch);
    let mut vert_comp = Vec::new_in(opts.scratch);
    let mut vert_taut = Vec::new_in(opts.scratch);
    let mut vert_tri_cnt = Vec::new_in(opts.scratch);
    let mut vert_cdir = Vec::new_in(opts.scratch);
    let mut nbr_sum = Vec::new_in(opts.scratch);
    let mut nbr_cnt = Vec::new_in(opts.scratch);
    for _ in 0..n_verts {
        push(&mut vert_normals, Vec3::ZERO)?;
        push(&mut vert_comp, 0.0f64)?;
        push(&mut vert_taut, 0.0f64)?;
        push(&mut vert_tri_cnt, 0u32)?;
        push(&mut vert_cdir, Vec3::ZERO)?;
        push(&mut nbr_sum, Vec3::ZERO)?;
        push(&mut nbr_cnt, 0u32)?;
    }

    let taut_k = opts.taut_stiffness.clamp(0.05, 1.0);
    let gather_damp = opts.gather_damping.clamp(0.0, 0.95);
    let fold_amp_m = opts.fold_amplitude.as_m().max(0.0);
    let min_body_clear_m = 0.002;

    for _iter in 0..opts.iterations {
        for i in 0..n_verts {
            vert_normals[i] = Vec3::ZERO;
            vert_comp[i] = 0.0;
            vert_taut[i] = 0.0;
            vert_tri_cnt[i] = 0;
            vert_cdir[i] = Vec3::ZERO;
            nbr_sum[i] = Vec3::ZERO;
            nbr_cnt[i] = 0;
        }

        // 1. Evaluate per-triangle physical strain from `rest_pos_m` and smooth normals.
        for (ti, &[ia, ib, ic]) in garment.triangles.iter().enumerate() {
            if garment.tri_layer.get(ti).copied().unwrap_or(0) > 2 {
                continue;
            }
            let ra = garment.weld_of(ia) as usize;
            let rb = garment.weld_of(ib) as usize;
            let rc = garment.weld_of(ic) as usize;
            if ra >= n_verts || rb >= n_verts || rc >= n_verts {
                continue;
            }
            let pa = pos[ra];
            let pb = pos[rb];
            let pc = pos[rc];
            let fnrm = (pb - pa).cross(pc - pa);
            vert_normals[ra] = vert_normals[ra] + fnrm;
            vert_normals[rb] = vert_normals[rb] + fnrm;
            vert_normals[rc] = vert_normals[rc] + fnrm;

            let (st, valid) = eval_tri_strain(
                ti as u32,
                garment.tri_panel.get(ti).copied().unwrap_or(0),
                pa,
                pb,
                pc,
                garment.rest_pos_m[ia as usize],
                garment.rest_pos_m[ib as usize],
                garment.rest_pos_m[ic as usize],
            );
            if valid {
                let comp = (0.96 - st.principal_min).max(0.0);
                let taut = (st.warp_stretch - 1.0).max(0.0);
                for r in [ra, rb, rc] {
                    vert_comp[r] += comp;
                    vert_taut[r] += taut;
                    vert_cdir[r] = vert_cdir[r] + st.compression_dir_world * comp;
                    vert_tri_cnt[r] += 1;
                }
            }
        }
        for i in 0..n_verts {
            let l = vert_normals[i].length();
            vert_normals[i] = if l > 1e-12 {
                vert_normals[i] * (1.0 / l)
            } else {
                Vec3::new(0.0, 0.0, -1.0)
            };
            if vert_tri_cnt[i] > 0 {
                let inv = 1.0 / (vert_tri_cnt[i] as f64);
                vert_comp[i] *= inv;
                vert_taut[i] *= inv;
            }
        }

        // 2. Anisotropic edge length pass using exact `rest_pos_m` rest lengths:
        //    - Taut edges (`d > rest_len`) pull tightly with `taut_stiffness`.
        //    - Compressed edges (`d < rest_len`) yield softly `(1 - gather_damping)` so excess
        //      fabric gathers into compact folds instead of ballooning outward!
        for e in edges.iter() {
            let ia = e.a as usize;
            let ib = e.b as usize;
            if !is_soft_root[ia] || !is_soft_root[ib] {
                continue;
            }
            let diff = pos[ib] - pos[ia];
            let d2 = diff.dot(diff);
            if d2 <= 1e-12 {
                continue;
            }
            let d = sqrt(d2);
            let err = d - e.rest_len;
            let k = if err > 0.0 {
                0.35 * taut_k
            } else {
                0.10 * (1.0 - gather_damp)
            };
            let corr = diff * (err * k / (2.0 * d));
            pos[ia] = pos[ia] + corr;
            pos[ib] = pos[ib] - corr;
        }

        // 3. Accumulate 1-ring welded neighborhood centers for controlled compression gathering.
        for e in edges.iter() {
            let ia = e.a as usize;
            let ib = e.b as usize;
            nbr_sum[ia] = nbr_sum[ia] + pos[ib];
            nbr_cnt[ia] += 1;
            nbr_sum[ib] = nbr_sum[ib] + pos[ia];
            nbr_cnt[ib] += 1;
        }

        for i in 0..n_verts {
            if !is_soft_root[i] || nbr_cnt[i] == 0 {
                continue;
            }
            let avg = nbr_sum[i] * (1.0 / (nbr_cnt[i] as f64));
            let lap = avg - pos[i];
            let nrm = vert_normals[i];
            let comp = vert_comp[i];
            let taut = vert_taut[i];

            if comp > 1e-4 {
                // Compressed zone (e.g. behind a bent knee): damp outward radial bulk smoothly
                // toward the local crease while forming subtle compact pleats along `rest_pos_m.y`
                // bounded by `fold_amplitude`!
                let alpha = (gather_damp * (comp / 0.12).clamp(0.25, 1.0)).clamp(0.0, 0.75);
                let cdir = vert_cdir[i].normalized().unwrap_or(Vec3::ZERO);
                let comp_slide = cdir * (lap.dot(cdir) * 0.25 * alpha);
                let gather_shift = lap * (0.35 * alpha) + comp_slide;
                // Keep pleats strictly inward/compact (non-ballooning, mean <= 0 along nrm).
                let wave = 0.5 * (sin(garment.rest_pos_m[i].y * 120.0) - 1.0);
                let pleat = nrm * (wave * fold_amp_m * (comp / 0.20).clamp(0.0, 1.0) * 0.15);
                pos[i] = pos[i] + gather_shift + pleat;
            } else if taut > 1e-4 {
                // Taut zone (e.g. front of bent knee): tangential-only smoothing preserves crisp
                // stretched profile over the patella without sagging inward.
                let lap_tang = lap - nrm * lap.dot(nrm);
                pos[i] = pos[i] + lap_tang * (0.18 * taut_k);
            }
        }

        // 4. Hard body-clearance projection so relaxed folds NEVER penetrate the mannequin.
        if let Some(col) = body_collider {
            for i in 0..n_verts {
                if !is_soft_root[i] {
                    continue;
                }
                let q = col.query(pos[i]);
                if q.signed_dist < min_body_clear_m {
                    pos[i] = q.closest + q.normal * min_body_clear_m;
                }
            }
        }

        // 5. Keep UV-split welded seam twins bit-for-bit coincident.
        for i in 0..n_verts {
            let r = garment.weld_of(i as u32) as usize;
            if r != i && r < n_verts {
                pos[i] = pos[r];
            }
        }
    }

    Ok(pos)
}
