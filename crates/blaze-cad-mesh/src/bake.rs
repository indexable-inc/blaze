//! Analytic SDF surface intelligence (`COLOR_0`) and tangent-space normal/mask texture baking.
//!
//! Traditional polygon pipelines spend hours cage-raycasting high-poly meshes onto low-poly
//! UV sheets to estimate curvature, ambient occlusion, cavity, and bevel normals. Because
//! `blaze-cad-field` holds the exact signed distance function and analytic gradients of the
//! solid (or whole assembled model), this module evaluates them directly with zero ray-cage
//! artifacts:
//!
//! - [`bake_sdf_surface`]: packs `[AO, Curvature, Cavity, HeightOrThickness]` into vertex
//!   `COLOR_0` (`[u8; 4]`), driving context-aware edge wear, contact darkening, and crevice
//!   grime on top of CC0 PBR materials.
//! - [`bake_sdf_normal_map`]: rasterizes a UV+tangent mesh against the exact SDF surface
//!   gradient (`eval_grad`) to produce an RGBA8 tangent-space normal map with deterministic
//!   gutter dilation.
//! - [`bake_sdf_mask_map`]: rasterizes the same `[AO, Curvature, Cavity, HeightOrThickness]`
//!   channels at full texture resolution into an RGBA8 smart-material mask texture.

#![blaze::sans_io]

use blaze_cad_field::{Scratch, Tape, eval_grad, eval_point};
use blaze_math::Vec3;
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_units::Length;

use crate::geom::{D3, dot, round_even, scale, sub, unit, wide};
use crate::mesh::{Mesh, MeshView, ShapeError, built};
use crate::plant::BakePlant;
use crate::vecs::{V, at, filled, push};

/// A 3×4 row-major affine map from a part's local frame into the field tape's frame
/// (`world = linear * local + translation`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FieldTransform {
    pub linear: [[f64; 3]; 3],
    pub translation: [f64; 3],
}

impl FieldTransform {
    pub const IDENTITY: FieldTransform = FieldTransform {
        linear: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        translation: [0.0, 0.0, 0.0],
    };

    #[inline]
    pub fn point(&self, p: D3) -> Vec3 {
        let m = self.linear;
        let t = self.translation;
        Vec3::new(
            m[0][0] * p[0] + m[0][1] * p[1] + m[0][2] * p[2] + t[0],
            m[1][0] * p[0] + m[1][1] * p[1] + m[1][2] * p[2] + t[1],
            m[2][0] * p[0] + m[2][1] * p[1] + m[2][2] * p[2] + t[2],
        )
    }

    #[inline]
    pub fn direction(&self, d: D3) -> Vec3 {
        let m = self.linear;
        let v = Vec3::new(
            m[0][0] * d[0] + m[0][1] * d[1] + m[0][2] * d[2],
            m[1][0] * d[0] + m[1][1] * d[1] + m[1][2] * d[2],
            m[2][0] * d[0] + m[2][1] * d[1] + m[2][2] * d[2],
        );
        v.normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0))
    }

    /// Transform a unit normal from field space back to local space using `linear^T`
    /// (exact for rigid / orthogonal placements; inverse-transpose for general uniform scales).
    #[inline]
    pub fn normal_to_local(&self, n_field: Vec3) -> D3 {
        let m = self.linear;
        let v = [
            m[0][0] * n_field.x + m[1][0] * n_field.y + m[2][0] * n_field.z,
            m[0][1] * n_field.x + m[1][1] * n_field.y + m[2][1] * n_field.z,
            m[0][2] * n_field.x + m[1][2] * n_field.y + m[2][2] * n_field.z,
        ];
        unit(v).unwrap_or([0.0, 0.0, 1.0])
    }
}

/// What the alpha channel of baked SDF surface masks encodes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SurfaceAlphaMode {
    /// Normalized vertical Z height in `[0, 1]` over the mesh's bounding box (useful for ground
    /// splash / waterline / elevation weathering).
    LocalHeight,
    /// Optical solid thickness measured inward along `-N` up to `max_thickness` (useful for
    /// thin-edge translucency and subsurface scattering).
    Thickness { max_thickness: Length },
}

/// Options for [`bake_sdf_surface`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct BakeSurfaceOpts<'a, 's> {
    /// Number of multi-scale SDF AO probes along and around the surface normal (`1..=8`).
    pub ao_steps: u8 = 5,
    /// Maximum reach of ambient occlusion probes (e.g. `Length::mm(150.0)`).
    pub ao_radius: Length,
    /// Probe step for finite-difference SDF Laplacian mean curvature (`> 0`).
    pub curvature_radius: Length,
    /// Multiplier for curvature contrast before smooth saturation (`1.0` is neutral).
    pub curvature_gain: f64 = 1.5,
    /// Short-range crevice / groove probe radius (`> 0`).
    pub cavity_radius: Length,
    /// What to pack into the alpha channel (`COLOR_0.a`).
    pub alpha_mode: SurfaceAlphaMode = SurfaceAlphaMode::LocalHeight,
    /// Transform from mesh local space into `tape`'s coordinate frame (use `IDENTITY` when
    /// baking against the part's own unplaced tape, or the part's placement when baking against
    /// the whole assembled model).
    pub local_to_field: FieldTransform = FieldTransform::IDENTITY,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// Options for [`bake_sdf_normal_map`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct BakeNormalOpts<'a, 's> {
    pub width: u32,
    pub height: u32,
    /// Number of deterministic Newton projection steps onto the exact SDF zero-set (`0..=4`).
    pub snap_steps: u8 = 2,
    /// Maximum snap distance per step (caps projection for coarsely decimated triangles).
    pub max_snap: Length,
    /// Number of 8-neighbor dilation passes to pad chart borders against mipmap seam bleeding.
    pub gutter_px: u32 = 8,
    /// Transform from mesh local space into `tape`'s coordinate frame.
    pub local_to_field: FieldTransform = FieldTransform::IDENTITY,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// Options for [`bake_sdf_mask_map`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct BakeMaskMapOpts<'a, 's> {
    pub width: u32,
    pub height: u32,
    pub ao_steps: u8 = 4,
    pub ao_radius: Length,
    pub curvature_radius: Length,
    pub curvature_gain: f64 = 1.5,
    pub cavity_radius: Length,
    pub alpha_mode: SurfaceAlphaMode = SurfaceAlphaMode::LocalHeight,
    pub gutter_px: u32 = 8,
    pub local_to_field: FieldTransform = FieldTransform::IDENTITY,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// An owned RGBA8 baked texture (row-major, top-to-bottom, 4 bytes per pixel).
#[derive(Debug)]
pub struct BakedImage<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8, Alloc<'a>>,
    /// Number of texels covered directly by UV triangles before gutter dilation.
    pub covered_texels: u64,
}

/// Errors returned by SDF baking routines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BakeError {
    InvalidParameter { arg: &'static str },
    /// Texture baking requires `uvs` (and `tangents` for tangent-space normal maps).
    MissingColumn { column: &'static str },
    TooLarge { pixels: u64, most: u64 },
    Alloc(AllocFailure),
    Built(ShapeError),
}

impl From<AllocFailure> for BakeError {
    fn from(e: AllocFailure) -> BakeError {
        BakeError::Alloc(e)
    }
}

/// Maximum pixels allowed for a baked texture (`16,384 × 16,384`, matching `blaze-cad-out`).
const MOST_PIXELS: u64 = 1 << 28;

/// Bake `[AO, Curvature, Cavity, HeightOrThickness]` into vertex `COLOR_0` (`[u8; 4]`) by
/// evaluating `tape` directly at every vertex of `view`.
pub fn bake_sdf_surface<'a>(
    view: &MeshView<'_>,
    tape: &Tape<'_>,
    opts: BakeSurfaceOpts<'a, '_>,
) -> Result<Mesh<'a>, BakeError> {
    bake_sdf_surface_planted(view, tape, opts, NoPlant)
}

/// [`bake_sdf_surface`] with a planted bug hook for oracle controls.
pub fn bake_sdf_surface_planted<'a>(
    view: &MeshView<'_>,
    tape: &Tape<'_>,
    opts: BakeSurfaceOpts<'a, '_>,
    plant: impl Planted<BakePlant>,
) -> Result<Mesh<'a>, BakeError> {
    validate_surface_params(
        opts.ao_radius,
        opts.curvature_radius,
        opts.curvature_gain,
        opts.cavity_radius,
        opts.alpha_mode,
    )?;

    let positions = view.positions();
    let normals = view.normals();
    let n_verts = positions.len();

    let (z_min, z_span) = vertical_span(positions);
    let mut field_sc = Scratch::new(opts.scratch);

    let mut out = crate::mesh::vecs_of(view, opts.alloc)?;
    out.colours.clear();
    out.colours.try_reserve(n_verts)?;

    for i in 0..n_verts {
        let p_local = wide(positions[i]);
        let n_local = unit(wide(normals[i])).unwrap_or([0.0, 0.0, 1.0]);
        let p_field = opts.local_to_field.point(p_local);
        let n_field = opts.local_to_field.direction(n_local);
        let rgba = sample_sdf_surface_rgba(
            tape,
            p_field,
            n_field,
            p_local[2],
            z_min,
            z_span,
            opts.ao_steps,
            opts.ao_radius.as_m(),
            opts.curvature_radius.as_m(),
            opts.curvature_gain,
            opts.cavity_radius.as_m(),
            opts.alpha_mode,
            &mut field_sc,
            &plant,
        );
        push(&mut out.colours, rgba)?;
    }

    built(out).map_err(BakeError::Built)
}

/// Bake a tangent-space RGBA8 normal map of `tape`'s exact analytic surface gradient onto `view`'s
/// UV layout (`[0, 1]^2`), with deterministic 8-neighbor gutter dilation.
pub fn bake_sdf_normal_map<'a>(
    view: &MeshView<'_>,
    tape: &Tape<'_>,
    opts: BakeNormalOpts<'a, '_>,
) -> Result<BakedImage<'a>, BakeError> {
    bake_sdf_normal_map_planted(view, tape, opts, NoPlant)
}

/// [`bake_sdf_normal_map`] with a planted bug hook for oracle controls.
pub fn bake_sdf_normal_map_planted<'a>(
    view: &MeshView<'_>,
    tape: &Tape<'_>,
    opts: BakeNormalOpts<'a, '_>,
    plant: impl Planted<BakePlant>,
) -> Result<BakedImage<'a>, BakeError> {
    let Some(uvs) = view.uvs() else {
        return Err(BakeError::MissingColumn { column: "uvs" });
    };
    let Some(tangents) = view.tangents() else {
        return Err(BakeError::MissingColumn { column: "tangents" });
    };
    if opts.width == 0 || opts.height == 0 {
        return Err(BakeError::InvalidParameter { arg: "dimensions" });
    }
    if !(opts.max_snap.as_m().is_finite() && opts.max_snap.as_m() >= 0.0) {
        return Err(BakeError::InvalidParameter { arg: "max_snap" });
    }
    let pixel_count = u64::from(opts.width) * u64::from(opts.height);
    if pixel_count > MOST_PIXELS {
        return Err(BakeError::TooLarge { pixels: pixel_count, most: MOST_PIXELS });
    }

    let n_px = pixel_count as usize;
    let mut pixels: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(n_px * 4, opts.alloc)?;
    // Fill background with flat tangent-space normal `[128, 128, 255, 255]`.
    for _ in 0..n_px {
        push(&mut pixels, 128)?;
        push(&mut pixels, 128)?;
        push(&mut pixels, 255)?;
        push(&mut pixels, 255)?;
    }
    let mut mask: V<'_, u8> = filled(n_px, 0u8, opts.scratch)?;
    let mut field_sc = Scratch::new(opts.scratch);
    let max_snap = opts.max_snap.as_m();

    let w_f = f64::from(opts.width);
    let h_f = f64::from(opts.height);
    let mut covered = 0u64;

    for tri in view.triangles() {
        let i0 = at(tri[0]);
        let i1 = at(tri[1]);
        let i2 = at(tri[2]);
        let uv0 = [f64::from(uvs[i0][0]) * w_f, (1.0 - f64::from(uvs[i0][1])) * h_f];
        let uv1 = [f64::from(uvs[i1][0]) * w_f, (1.0 - f64::from(uvs[i1][1])) * h_f];
        let uv2 = [f64::from(uvs[i2][0]) * w_f, (1.0 - f64::from(uvs[i2][1])) * h_f];

        let area = edge_fn(uv0, uv1, uv2);
        if area.abs() < 1e-9 {
            continue;
        }
        let inv_area = 1.0 / area;

        let x_min = (uv0[0].min(uv1[0]).min(uv2[0]) - 0.5).max(0.0) as u32;
        let x_max = ((uv0[0].max(uv1[0]).max(uv2[0]) + 0.5).ceil() as u32).min(opts.width.saturating_sub(1));
        let y_min = (uv0[1].min(uv1[1]).min(uv2[1]) - 0.5).max(0.0) as u32;
        let y_max = ((uv0[1].max(uv1[1]).max(uv2[1]) + 0.5).ceil() as u32).min(opts.height.saturating_sub(1));

        let p0 = wide(view.positions()[i0]);
        let p1 = wide(view.positions()[i1]);
        let p2 = wide(view.positions()[i2]);
        let n0 = wide(view.normals()[i0]);
        let n1 = wide(view.normals()[i1]);
        let n2 = wide(view.normals()[i2]);
        let t0 = tangents[i0];
        let t1 = tangents[i1];
        let t2 = tangents[i2];

        for py in y_min..=y_max {
            let sy = f64::from(py) + 0.5;
            for px in x_min..=x_max {
                let sx = f64::from(px) + 0.5;
                let w0 = edge_fn(uv1, uv2, [sx, sy]) * inv_area;
                let w1 = edge_fn(uv2, uv0, [sx, sy]) * inv_area;
                let w2 = edge_fn(uv0, uv1, [sx, sy]) * inv_area;
                if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                    continue;
                }
                let p_local = [
                    p0[0] * w0 + p1[0] * w1 + p2[0] * w2,
                    p0[1] * w0 + p1[1] * w1 + p2[1] * w2,
                    p0[2] * w0 + p1[2] * w1 + p2[2] * w2,
                ];
                let n_interp = unit([
                    n0[0] * w0 + n1[0] * w1 + n2[0] * w2,
                    n0[1] * w0 + n1[1] * w1 + n2[1] * w2,
                    n0[2] * w0 + n1[2] * w1 + n2[2] * w2,
                ])
                .unwrap_or([0.0, 0.0, 1.0]);
                let t_raw = [
                    f64::from(t0[0]) * w0 + f64::from(t1[0]) * w1 + f64::from(t2[0]) * w2,
                    f64::from(t0[1]) * w0 + f64::from(t1[1]) * w1 + f64::from(t2[1]) * w2,
                    f64::from(t0[2]) * w0 + f64::from(t1[2]) * w1 + f64::from(t2[2]) * w2,
                ];
                // Gram-Schmidt orthonormalize tangent against interpolated vertex normal.
                let t_vec = unit(sub(t_raw, scale(n_interp, dot(n_interp, t_raw)))).unwrap_or_else(|| {
                    let h = if n_interp[2].abs() < 0.9 { [0.0, 0.0, 1.0] } else { [1.0, 0.0, 0.0] };
                    unit(crate::geom::cross(h, n_interp)).unwrap_or([1.0, 0.0, 0.0])
                });
                let handed = if f64::from(t0[3]) * w0 + f64::from(t1[3]) * w1 + f64::from(t2[3]) * w2 < 0.0 {
                    -1.0
                } else {
                    1.0
                };
                let b_vec = scale(crate::geom::cross(n_interp, t_vec), handed);

                // Snap onto the exact SDF zero-level-set and query its analytic gradient.
                let mut p_field = opts.local_to_field.point(p_local);
                for _ in 0..opts.snap_steps {
                    let (f, g) = eval_grad(tape, p_field, &mut field_sc);
                    let g2 = g.dot(g);
                    if g2 > 1e-18 && f.is_finite() {
                        let step_len = (f / g2).clamp(-max_snap, max_snap);
                        p_field = p_field - g * step_len;
                    }
                }
                let (_, g_hi) = eval_grad(tape, p_field, &mut field_sc);
                let n_hi_local = if let Some(n_f) = g_hi.normalized() {
                    opts.local_to_field.normal_to_local(n_f)
                } else {
                    n_interp
                };

                let tn = if plant.has(BakePlant::FlatNormalMap) {
                    [0.0, 0.0, 1.0]
                } else {
                    let raw = [dot(n_hi_local, t_vec), dot(n_hi_local, b_vec), dot(n_hi_local, n_interp)];
                    unit(raw).unwrap_or([0.0, 0.0, 1.0])
                };

                let idx = (py as usize) * (opts.width as usize) + (px as usize);
                if mask[idx] == 0 {
                    mask[idx] = 1;
                    covered += 1;
                }
                let off = idx * 4;
                pixels[off] = to_u8(tn[0] * 0.5 + 0.5);
                pixels[off + 1] = to_u8(tn[1] * 0.5 + 0.5);
                pixels[off + 2] = to_u8(tn[2] * 0.5 + 0.5);
                pixels[off + 3] = 255;
            }
        }
    }

    if !plant.has(BakePlant::SkipDilation) {
        dilate_rgba8(&mut pixels, &mut mask, opts.width, opts.height, opts.gutter_px, opts.scratch)?;
    }

    Ok(BakedImage { width: opts.width, height: opts.height, pixels, covered_texels: covered })
}

/// Bake a texture-resolution RGBA8 SDF surface mask map (`[AO, Curvature, Cavity, HeightOrThickness]`)
/// onto `view`'s UV layout (`[0, 1]^2`), with deterministic gutter dilation.
pub fn bake_sdf_mask_map<'a>(
    view: &MeshView<'_>,
    tape: &Tape<'_>,
    opts: BakeMaskMapOpts<'a, '_>,
) -> Result<BakedImage<'a>, BakeError> {
    bake_sdf_mask_map_planted(view, tape, opts, NoPlant)
}

/// [`bake_sdf_mask_map`] with a planted bug hook for oracle controls.
pub fn bake_sdf_mask_map_planted<'a>(
    view: &MeshView<'_>,
    tape: &Tape<'_>,
    opts: BakeMaskMapOpts<'a, '_>,
    plant: impl Planted<BakePlant>,
) -> Result<BakedImage<'a>, BakeError> {
    let Some(uvs) = view.uvs() else {
        return Err(BakeError::MissingColumn { column: "uvs" });
    };
    if opts.width == 0 || opts.height == 0 {
        return Err(BakeError::InvalidParameter { arg: "dimensions" });
    }
    validate_surface_params(
        opts.ao_radius,
        opts.curvature_radius,
        opts.curvature_gain,
        opts.cavity_radius,
        opts.alpha_mode,
    )?;
    let pixel_count = u64::from(opts.width) * u64::from(opts.height);
    if pixel_count > MOST_PIXELS {
        return Err(BakeError::TooLarge { pixels: pixel_count, most: MOST_PIXELS });
    }

    let n_px = pixel_count as usize;
    let mut pixels: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(n_px * 4, opts.alloc)?;
    // Neutral background: full AO (255), flat curvature (128), open cavity (255), mid height (128).
    for _ in 0..n_px {
        push(&mut pixels, 255)?;
        push(&mut pixels, 128)?;
        push(&mut pixels, 255)?;
        push(&mut pixels, 128)?;
    }
    let mut mask: V<'_, u8> = filled(n_px, 0u8, opts.scratch)?;
    let mut field_sc = Scratch::new(opts.scratch);
    let (z_min, z_span) = vertical_span(view.positions());

    let w_f = f64::from(opts.width);
    let h_f = f64::from(opts.height);
    let mut covered = 0u64;

    for tri in view.triangles() {
        let i0 = at(tri[0]);
        let i1 = at(tri[1]);
        let i2 = at(tri[2]);
        let uv0 = [f64::from(uvs[i0][0]) * w_f, (1.0 - f64::from(uvs[i0][1])) * h_f];
        let uv1 = [f64::from(uvs[i1][0]) * w_f, (1.0 - f64::from(uvs[i1][1])) * h_f];
        let uv2 = [f64::from(uvs[i2][0]) * w_f, (1.0 - f64::from(uvs[i2][1])) * h_f];

        let area = edge_fn(uv0, uv1, uv2);
        if area.abs() < 1e-9 {
            continue;
        }
        let inv_area = 1.0 / area;

        let x_min = (uv0[0].min(uv1[0]).min(uv2[0]) - 0.5).max(0.0) as u32;
        let x_max = ((uv0[0].max(uv1[0]).max(uv2[0]) + 0.5).ceil() as u32).min(opts.width.saturating_sub(1));
        let y_min = (uv0[1].min(uv1[1]).min(uv2[1]) - 0.5).max(0.0) as u32;
        let y_max = ((uv0[1].max(uv1[1]).max(uv2[1]) + 0.5).ceil() as u32).min(opts.height.saturating_sub(1));

        let p0 = wide(view.positions()[i0]);
        let p1 = wide(view.positions()[i1]);
        let p2 = wide(view.positions()[i2]);
        let n0 = wide(view.normals()[i0]);
        let n1 = wide(view.normals()[i1]);
        let n2 = wide(view.normals()[i2]);

        for py in y_min..=y_max {
            let sy = f64::from(py) + 0.5;
            for px in x_min..=x_max {
                let sx = f64::from(px) + 0.5;
                let w0 = edge_fn(uv1, uv2, [sx, sy]) * inv_area;
                let w1 = edge_fn(uv2, uv0, [sx, sy]) * inv_area;
                let w2 = edge_fn(uv0, uv1, [sx, sy]) * inv_area;
                if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                    continue;
                }
                let p_local = [
                    p0[0] * w0 + p1[0] * w1 + p2[0] * w2,
                    p0[1] * w0 + p1[1] * w1 + p2[1] * w2,
                    p0[2] * w0 + p1[2] * w1 + p2[2] * w2,
                ];
                let n_local = unit([
                    n0[0] * w0 + n1[0] * w1 + n2[0] * w2,
                    n0[1] * w0 + n1[1] * w1 + n2[1] * w2,
                    n0[2] * w0 + n1[2] * w1 + n2[2] * w2,
                ])
                .unwrap_or([0.0, 0.0, 1.0]);
                let p_field = opts.local_to_field.point(p_local);
                let n_field = opts.local_to_field.direction(n_local);
                let rgba = sample_sdf_surface_rgba(
                    tape,
                    p_field,
                    n_field,
                    p_local[2],
                    z_min,
                    z_span,
                    opts.ao_steps,
                    opts.ao_radius.as_m(),
                    opts.curvature_radius.as_m(),
                    opts.curvature_gain,
                    opts.cavity_radius.as_m(),
                    opts.alpha_mode,
                    &mut field_sc,
                    &plant,
                );
                let idx = (py as usize) * (opts.width as usize) + (px as usize);
                if mask[idx] == 0 {
                    mask[idx] = 1;
                    covered += 1;
                }
                let off = idx * 4;
                pixels[off..off + 4].copy_from_slice(&rgba);
            }
        }
    }

    if !plant.has(BakePlant::SkipDilation) {
        dilate_rgba8(&mut pixels, &mut mask, opts.width, opts.height, opts.gutter_px, opts.scratch)?;
    }

    Ok(BakedImage { width: opts.width, height: opts.height, pixels, covered_texels: covered })
}

// ---- Core SDF Sampling -------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn sample_sdf_surface_rgba(
    tape: &Tape<'_>,
    p: Vec3,
    n: Vec3,
    local_z: f64,
    z_min: f64,
    z_span: f64,
    ao_steps: u8,
    ao_radius: f64,
    curv_radius: f64,
    curv_gain: f64,
    cavity_radius: f64,
    alpha_mode: SurfaceAlphaMode,
    sc: &mut Scratch<'_>,
    plant: &impl Planted<BakePlant>,
) -> [u8; 4] {
    // Build orthonormal tangent basis `(t, b, n)` in field space.
    let helper = if n.z.abs() < 0.9 { Vec3::new(0.0, 0.0, 1.0) } else { Vec3::new(1.0, 0.0, 0.0) };
    let t = helper.cross(n).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
    let b = n.cross(t);

    // 1. Multi-scale SDF Ambient Occlusion (normal axis + 4 tilted hemisphere probes for true
    //    assembly-wide contact occlusion under ledges and beside adjacent parts).
    let ao = if plant.has(BakePlant::FlatAo) || ao_steps == 0 {
        1.0
    } else {
        let mut occ = 0.0f64;
        let mut w_sum = 0.0f64;
        let mut w = 0.5f64;
        let n_steps = f64::from(ao_steps);
        for i in 1..=ao_steps {
            let frac = f64::from(i) / n_steps;
            let h = ao_radius * frac;
            let d = eval_point(tape, p + n * h, sc);
            let deficit = ((h - d) / h).clamp(0.0, 1.0);
            occ += w * deficit;
            w_sum += w;
            w *= 0.55;
        }
        // Tilted hemisphere contact probes at ~45 deg around `n` at 0.35 * ao_radius.
        let h_tilt = ao_radius * 0.35;
        let dirs = [
            (n * 0.70710678 + t * 0.70710678),
            (n * 0.70710678 - t * 0.70710678),
            (n * 0.70710678 + b * 0.70710678),
            (n * 0.70710678 - b * 0.70710678),
        ];
        let mut tilt_occ = 0.0f64;
        for dir in dirs {
            let d = eval_point(tape, p + dir * h_tilt, sc);
            tilt_occ += ((h_tilt - d) / h_tilt).clamp(0.0, 1.0);
        }
        let normal_ao = if w_sum > 0.0 { 1.0 - (1.45 * occ / w_sum).clamp(0.0, 1.0) } else { 1.0 };
        let tilt_ao = 1.0 - (0.35 * tilt_occ).clamp(0.0, 1.0);
        (0.65 * normal_ao + 0.35 * tilt_ao).clamp(0.0, 1.0)
    };

    // 2. Analytic SDF Mean Curvature via 6-point 3D stencil Laplacian:
    //    For a signed distance field (negative inside, positive outside), convex corners have
    //    average neighbor SDF > center SDF (positive curvature -> > 0.5 -> wear/chipped paint);
    //    concave crevices have average neighbor SDF < center SDF (< 0.5 -> dirt/grime).
    let curvature = if plant.has(BakePlant::ZeroCurvature) {
        0.5
    } else {
        let h = curv_radius;
        let f0 = eval_point(tape, p, sc);
        let sum = eval_point(tape, p + Vec3::new(h, 0.0, 0.0), sc)
            + eval_point(tape, p - Vec3::new(h, 0.0, 0.0), sc)
            + eval_point(tape, p + Vec3::new(0.0, h, 0.0), sc)
            + eval_point(tape, p - Vec3::new(0.0, h, 0.0), sc)
            + eval_point(tape, p + Vec3::new(0.0, 0.0, h), sc)
            + eval_point(tape, p - Vec3::new(0.0, 0.0, h), sc);
        // Dimensionless mean curvature measure `h * H = (avg - f0) / h`.
        let signed = ((sum / 6.0) - f0) / h * 3.0 * curv_gain;
        let mapped = 0.5 + 0.5 * (signed / (1.0 + signed.abs()));
        mapped.clamp(0.0, 1.0)
    };

    // 3. Short-range Cavity mask (1.0 = exposed surface, 0.0 = tight concave groove / seam).
    let cavity = {
        let h = cavity_radius;
        let d_n = eval_point(tape, p + n * h, sc);
        let deficit_n = ((h - d_n) / h).clamp(0.0, 1.0);
        let d_t0 = eval_point(tape, p + (n * 0.5 + t * 0.8660254) * h, sc);
        let d_t1 = eval_point(tape, p + (n * 0.5 - t * 0.8660254) * h, sc);
        let d_b0 = eval_point(tape, p + (n * 0.5 + b * 0.8660254) * h, sc);
        let d_b1 = eval_point(tape, p + (n * 0.5 - b * 0.8660254) * h, sc);
        let ring = (((h - d_t0) / h).clamp(0.0, 1.0)
            + ((h - d_t1) / h).clamp(0.0, 1.0)
            + ((h - d_b0) / h).clamp(0.0, 1.0)
            + ((h - d_b1) / h).clamp(0.0, 1.0))
            * 0.25;
        (1.0 - (0.55 * deficit_n + 0.45 * ring) * 1.6).clamp(0.0, 1.0)
    };

    // 4. Alpha channel: local height or optical thickness.
    let alpha = match alpha_mode {
        SurfaceAlphaMode::LocalHeight => ((local_z - z_min) / z_span).clamp(0.0, 1.0),
        SurfaceAlphaMode::Thickness { max_thickness } => {
            let max_t = max_thickness.as_m();
            let mut acc = 0.0f64;
            let mut w_s = 0.0f64;
            let mut wt = 0.5f64;
            for i in 1..=4u8 {
                let h = max_t * (f64::from(i) / 4.0);
                // Inside solid, `f < 0`; `-f` is distance to nearest boundary from inside.
                let d_in = -eval_point(tape, p - n * h, sc);
                let fill = (d_in / h).clamp(0.0, 1.0);
                acc += wt * fill;
                w_s += wt;
                wt *= 0.55;
            }
            if w_s > 0.0 { (acc / w_s).clamp(0.0, 1.0) } else { 1.0 }
        }
    };

    [to_u8(ao), to_u8(curvature), to_u8(cavity), to_u8(alpha)]
}

fn validate_surface_params(
    ao_radius: Length,
    curv_radius: Length,
    curv_gain: f64,
    cavity_radius: Length,
    alpha_mode: SurfaceAlphaMode,
) -> Result<(), BakeError> {
    if !(ao_radius.as_m().is_finite() && ao_radius.as_m() > 0.0) {
        return Err(BakeError::InvalidParameter { arg: "ao_radius" });
    }
    if !(curv_radius.as_m().is_finite() && curv_radius.as_m() > 0.0) {
        return Err(BakeError::InvalidParameter { arg: "curvature_radius" });
    }
    if !(curv_gain.is_finite() && curv_gain >= 0.0) {
        return Err(BakeError::InvalidParameter { arg: "curvature_gain" });
    }
    if !(cavity_radius.as_m().is_finite() && cavity_radius.as_m() > 0.0) {
        return Err(BakeError::InvalidParameter { arg: "cavity_radius" });
    }
    if let SurfaceAlphaMode::Thickness { max_thickness } = alpha_mode
        && !(max_thickness.as_m().is_finite() && max_thickness.as_m() > 0.0)
    {
        return Err(BakeError::InvalidParameter { arg: "max_thickness" });
    }
    Ok(())
}

fn vertical_span(positions: &[[f32; 3]]) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for p in positions {
        let z = f64::from(p[2]);
        lo = lo.min(z);
        hi = hi.max(z);
    }
    if lo.is_finite() && hi.is_finite() {
        (lo, (hi - lo).max(1e-6))
    } else {
        (0.0, 1.0)
    }
}

#[inline]
fn edge_fn(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (c[0] - a[0]) * (b[1] - a[1]) - (c[1] - a[1]) * (b[0] - a[0])
}

#[inline]
fn to_u8(v: f64) -> u8 {
    round_even(v.clamp(0.0, 1.0) * 255.0).clamp(0.0, 255.0) as u8
}

/// Deterministic 8-neighbor gutter dilation so uncovered texels adjacent to UV islands inherit
/// the average colour of their covered neighbors (preventing dark mipmap bleeding at UV seams).
fn dilate_rgba8(
    pixels: &mut [u8],
    mask: &mut [u8],
    width: u32,
    height: u32,
    passes: u32,
    scratch: Alloc<'_>,
) -> Result<(), BakeError> {
    if passes == 0 {
        return Ok(());
    }
    let w = width as usize;
    let h = height as usize;
    let n = w * h;
    let mut next_mask: V<'_, u8> = filled(n, 0u8, scratch)?;
    let mut next_px: V<'_, u8> = filled(n * 4, 0u8, scratch)?;

    for _ in 0..passes {
        next_mask.copy_from_slice(mask);
        next_px.copy_from_slice(pixels);
        let mut changed = false;
        for y in 0..h {
            for x in 0..w {
                let idx = y * w + x;
                if mask[idx] != 0 {
                    continue;
                }
                let mut sum = [0u32; 4];
                let mut count = 0u32;
                let y0 = y.saturating_sub(1);
                let y1 = (y + 1).min(h - 1);
                let x0 = x.saturating_sub(1);
                let x1 = (x + 1).min(w - 1);
                for ny in y0..=y1 {
                    for nx in x0..=x1 {
                        let nidx = ny * w + nx;
                        if mask[nidx] != 0 {
                            let off = nidx * 4;
                            sum[0] += u32::from(pixels[off]);
                            sum[1] += u32::from(pixels[off + 1]);
                            sum[2] += u32::from(pixels[off + 2]);
                            sum[3] += u32::from(pixels[off + 3]);
                            count += 1;
                        }
                    }
                }
                if count > 0 {
                    let off = idx * 4;
                    next_px[off] = ((sum[0] + count / 2) / count) as u8;
                    next_px[off + 1] = ((sum[1] + count / 2) / count) as u8;
                    next_px[off + 2] = ((sum[2] + count / 2) / count) as u8;
                    next_px[off + 3] = ((sum[3] + count / 2) / count) as u8;
                    next_mask[idx] = 1;
                    changed = true;
                }
            }
        }
        pixels.copy_from_slice(&next_px);
        mask.copy_from_slice(&next_mask);
        if !changed {
            break;
        }
    }
    Ok(())
}
