//! Deterministic BVH-accelerated preview renderer for exported triangle meshes and multi-part
//! scenes (`render_mesh`, `render_mesh_scene`).
//!
//! Renders the ACTUAL exported triangles (with their per-part transforms, parent hierarchy,
//! `MaterialRole` PBR colours/roughness/metallic, interpolated vertex normals or flat geometric
//! normals, studio 3-point lighting with BVH ray-traced shadows and Fibonacci-hemisphere AO,
//! and screen-space-consistent anti-aliased wireframe overlays).
//!
//! Nothing allocates through `Global`, nothing recurses (explicit stacks, rule 18), and every
//! BVH split, tie-break and hemisphere AO direction is fixed and deterministic (rule 25).

#![blaze::sans_io]

use blaze_cad_mesh::{FeatureRegion, MeshView};
use blaze_cad_out::{Material, Transform};
use blaze_math::{Aabb3, Vec3};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, AllocFailure, Vec};

use crate::shade::{
    RGSS_2X2, ShadeContext, eval_strip_lights, linear_to_srgb, quantize, resolve_samples, shade_zebra, srgb_to_linear,
};
use crate::{Camera, LEAST_STEP, Picture, PictureError, PictureOpts, PictureStats, RenderMode, Shading, StudioOpts, View, slab};

/// Maximum triangles stored in a single BVH leaf.
const LEAF_TRIS: usize = 4;

/// Number of SAH buckets per axis during BVH construction.
const SAH_BINS: usize = 8;

#[inline]
fn push<T>(v: &mut Vec<T, Alloc<'_>>, x: T) -> Result<(), AllocFailure> {
    v.try_push(x).map_err(|r| r.why)
}

/// Planted bugs for [`render_mesh_scene_planted`], caught by the test controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MeshPicturePlant {
    None,
    /// Ignores each part's transform (renders every part at the identity transform).
    IgnoreTransform,
    /// Ignores the mesh's shading normals even when [`MeshNormalSource::Shading`] is selected.
    FlatOnly,
    /// Ignores [`MeshPictureOpts::highlights`] (never draws missed-feature callouts on the preview).
    IgnoreHighlights,
    /// Ignores [`MeshPictureOpts::material_maps`] (never samples baked base-colour,
    /// metallic-roughness, or tangent-space normal maps).
    IgnoreTextures,
}

impl Plants for MeshPicturePlant {}

/// Which surface normal the mesh preview shades with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MeshNormalSource {
    /// Interpolated vertex normals from the exported mesh's `normals` column (shows exact
    /// game-engine crease splits and smooth-shading artifacts).
    Shading,
    /// Flat per-triangle geometric normal (reveals raw tessellation and bevel faceting).
    Geometric,
}

/// Optional wireframe overlay drawn on the exported triangles.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Wireframe {
    /// Solid surface only.
    Off,
    /// Anti-aliased triangle edges composited over the shaded surface.
    Overlay {
        /// Edge stroke width in screen pixels (typically `0.75..=1.5`).
        width_px: f64,
        /// Edge RGB colour.
        colour: [u8; 3],
    },
    /// Wireframe edges over a flat light/tinted surface for pure topology inspection.
    Only {
        /// Edge stroke width in screen pixels.
        width_px: f64,
        /// Edge RGB colour.
        colour: [u8; 3],
    },
}

/// One renderable part in a multi-part exported scene.
#[derive(Clone, Copy, Debug)]
pub struct MeshScenePart<'a> {
    pub mesh: MeshView<'a>,
    /// Index of the parent part (`< current index`), or `None` for a root node.
    pub parent: Option<u32>,
    /// Local-to-parent (or local-to-world if root) transform.
    pub transform: Transform,
    /// Optional RGB override (e.g. for per-part false-colour or budget heatmap previews).
    pub tint_override: Option<[u8; 3]>,
}

/// Visual highlight style for a spatial feature region on an exported mesh preview.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HighlightStyle {
    /// Failed fidelity target: vivid surface tint with screen-consistent diagonal hazard stripes
    /// and a crisp solid boundary contour so missed keyholes, bevels, or mating surfaces jump out
    /// immediately on any material colour.
    Missed { colour: [u8; 3] },
    /// Passing critical feature: subtle anti-aliased boundary contour showing the protected zone
    /// without obscuring surface shading.
    Outline { colour: [u8; 3] },
}

/// Spatial region on one part of a [`MeshScenePart`] scene to highlight in the export preview.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MeshHighlight {
    /// Index into the `parts` slice passed to [`render_mesh_scene`].
    pub part_index: u32,
    /// Feature region in that part's local coordinate frame (exact under transforms and parent
    /// hierarchies).
    pub region: FeatureRegion,
    pub style: HighlightStyle,
}

/// Borrowed RGBA8 texture image sampled by [`render_mesh_scene`] (`width × height × 4` bytes,
/// row-major from the top).
#[derive(Clone, Copy, Debug)]
pub struct TextureMap<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba: &'a [u8],
}

impl TextureMap<'_> {
    /// Deterministic clamp-to-edge bilinear sample returning normalized `[r, g, b, a]` in `0.0..=1.0`.
    pub fn sample_bilinear(&self, u: f64, v: f64) -> [f64; 4] {
        let Some((x0, y0, x1, y1, tx, ty)) = self.bilinear_coords(u, v) else {
            return [1.0, 1.0, 1.0, 1.0];
        };
        let p00 = self.texel(x0, y0);
        let p10 = self.texel(x1, y0);
        let p01 = self.texel(x0, y1);
        let p11 = self.texel(x1, y1);
        let wx0 = 1.0 - tx;
        let wy0 = 1.0 - ty;
        let mut out = [0.0f64; 4];
        for c in 0..4 {
            let top = f64::from(p00[c]) * wx0 + f64::from(p10[c]) * tx;
            let bot = f64::from(p01[c]) * wx0 + f64::from(p11[c]) * tx;
            out[c] = ((top * wy0 + bot * ty) / 255.0).clamp(0.0, 1.0);
        }
        out
    }

    /// Deterministic clamp-to-edge bilinear sample decoding sRGB RGB texels to linear light before
    /// filtering (alpha stays linear).
    pub(crate) fn sample_srgb_linear_bilinear(&self, u: f64, v: f64) -> [f64; 4] {
        let Some((x0, y0, x1, y1, tx, ty)) = self.bilinear_coords(u, v) else {
            return [1.0, 1.0, 1.0, 1.0];
        };
        let p00 = self.texel(x0, y0);
        let p10 = self.texel(x1, y0);
        let p01 = self.texel(x0, y1);
        let p11 = self.texel(x1, y1);
        let wx0 = 1.0 - tx;
        let wy0 = 1.0 - ty;
        let mut out = [0.0f64; 4];
        for c in 0..3 {
            let top = srgb_to_linear(p00[c]) * wx0 + srgb_to_linear(p10[c]) * tx;
            let bot = srgb_to_linear(p01[c]) * wx0 + srgb_to_linear(p11[c]) * tx;
            out[c] = (top * wy0 + bot * ty).clamp(0.0, 1.0);
        }
        let a_top = f64::from(p00[3]) * wx0 + f64::from(p10[3]) * tx;
        let a_bot = f64::from(p01[3]) * wx0 + f64::from(p11[3]) * tx;
        out[3] = ((a_top * wy0 + a_bot * ty) / 255.0).clamp(0.0, 1.0);
        out
    }

    #[inline]
    fn bilinear_coords(&self, u: f64, v: f64) -> Option<(u32, u32, u32, u32, f64, f64)> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let needed = usize::try_from(self.width)
            .ok()?
            .checked_mul(usize::try_from(self.height).ok()?)?
            .checked_mul(4)?;
        if self.rgba.len() < needed {
            return None;
        }
        let uc = if u.is_finite() { u.clamp(0.0, 1.0) } else { 0.0 };
        let vc = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        let wf = f64::from(self.width);
        let hf = f64::from(self.height);
        let fx = (uc * wf - 0.5).clamp(0.0, (wf - 1.0).max(0.0));
        let fy = (vc * hf - 0.5).clamp(0.0, (hf - 1.0).max(0.0));
        let x0 = (blaze_math::floor(fx) as u32).min(self.width - 1);
        let y0 = (blaze_math::floor(fy) as u32).min(self.height - 1);
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let tx = (fx - f64::from(x0)).clamp(0.0, 1.0);
        let ty = (fy - f64::from(y0)).clamp(0.0, 1.0);
        Some((x0, y0, x1, y1, tx, ty))
    }

    #[inline]
    fn texel(&self, x: u32, y: u32) -> [u8; 4] {
        let off = ((y as usize) * (self.width as usize) + (x as usize)) * 4;
        [self.rgba[off], self.rgba[off + 1], self.rgba[off + 2], self.rgba[off + 3]]
    }
}

/// Baked PBR texture maps for one material role, indexed in parallel with `materials: &[Material]`
/// in [`MeshPictureOpts::material_maps`].
#[derive(Clone, Copy, Debug)]
pub struct MaterialMaps<'a> {
    /// Base-colour / albedo RGBA8 map (sRGB-encoded RGB, multiplied with `Material::base_colour`).
    pub base_colour: Option<TextureMap<'a>>,
    /// glTF-convention metallic-roughness RGBA8 map (G = roughness factor, B = metallic factor).
    pub metallic_roughness: Option<TextureMap<'a>>,
    /// Tangent-space normal map RGBA8 texture (RGB unpacked from `[0, 1]` to `[-1, 1]`).
    pub normal: Option<TextureMap<'a>>,
    /// Tangent-space XY perturbation scale (typically `1.0`).
    pub normal_scale: f64,
}

impl MaterialMaps<'_> {
    /// Empty material map set with no textures bound.
    pub const NONE: MaterialMaps<'static> = MaterialMaps {
        base_colour: None,
        metallic_roughness: None,
        normal: None,
        normal_scale: 1.0,
    };
}

impl Default for MaterialMaps<'_> {
    fn default() -> MaterialMaps<'static> {
        MaterialMaps::NONE
    }
}

/// Options for [`render_mesh`] and [`render_mesh_scene`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct MeshPictureOpts<'a, 's, 'h> {
    /// Where the output picture's pixels and depths live.
    pub alloc: Alloc<'a>,
    /// Where the BVH and transformed triangle table live.
    pub scratch: Alloc<'s>,
    pub most_pixels: u64 = 1 << 26,
    /// Background RGBA for rays that hit nothing.
    pub background: [u8; 4] = [246, 246, 244, 255],
    /// Fallback RGB colour when a triangle's role has no matching [`Material`].
    pub fallback_colour: [u8; 3] = [168, 178, 192],
    /// Lighting model in [`RenderMode::Shaded`].
    pub shading: Shading = Shading::Studio(StudioOpts::DEFAULT),
    /// Visualization mode (`Shaded`, `Normals`, `Depth`, `Matcap`, `Zebra`).
    pub mode: RenderMode = RenderMode::Shaded,
    /// Whether to shade with exported interpolated normals or flat geometric normals.
    pub normal_source: MeshNormalSource = MeshNormalSource::Shading,
    /// Optional anti-aliased triangle wireframe overlay.
    pub wireframe: Wireframe = Wireframe::Off,
    /// Optional spatial feature highlights (e.g. missed fidelity targets) rendered directly on
    /// the exported triangles in part-local coordinates.
    pub highlights: &'h [MeshHighlight] = &[],
    /// Optional baked PBR texture & tangent-space normal maps indexed in parallel with `materials`.
    pub material_maps: &'h [MaterialMaps<'h>] = &[],
    /// Deterministic supersampling per axis: `1` or `2` (2×2 RGSS).
    pub supersample: u8 = 1,
}

/// Render a single exported mesh (in its own frame, identity transform) from `view`.
pub fn render_mesh<'a>(
    mesh: MeshView<'_>,
    materials: &[Material<'_>],
    view: View,
    o: MeshPictureOpts<'a, '_, '_>,
) -> Result<Picture<'a>, PictureError> {
    let part = [MeshScenePart { mesh, parent: None, transform: Transform::IDENTITY, tint_override: None }];
    render_mesh_scene(&part, materials, view, o)
}

/// Render a multi-part exported scene (with transforms, parent hierarchy, and per-role materials)
/// from `view`.
pub fn render_mesh_scene<'a>(
    parts: &[MeshScenePart<'_>],
    materials: &[Material<'_>],
    view: View,
    o: MeshPictureOpts<'a, '_, '_>,
) -> Result<Picture<'a>, PictureError> {
    render_mesh_scene_planted(parts, materials, view, o, NoPlant)
}

/// [`render_mesh_scene`] with a planted bug, for the test controls.
pub fn render_mesh_scene_planted<'a>(
    parts: &[MeshScenePart<'_>],
    materials: &[Material<'_>],
    view: View,
    o: MeshPictureOpts<'a, '_, '_>,
    plant: impl Planted<MeshPicturePlant>,
) -> Result<Picture<'a>, PictureError> {
    if o.supersample != 1 && o.supersample != 2 {
        return Err(PictureError::BadSupersample { supersample: o.supersample });
    }
    let cam = Camera::new(&view)?;
    let n_pixels = u64::from(view.width) * u64::from(view.height);
    if n_pixels > o.most_pixels {
        return Err(PictureError::TooManyPixels { pixels: n_pixels, most: o.most_pixels });
    }
    let n_usize = usize::try_from(n_pixels).map_err(|_| PictureError::TooManyPixels { pixels: n_pixels, most: o.most_pixels })?;

    let bvh = build_bvh(parts, materials, o.fallback_colour, o.scratch, plant)?;

    let mut pixels: Vec<u8, Alloc<'a>> = Vec::try_with_capacity_in(n_usize * 4, o.alloc)?;
    let mut depth: Vec<f64, Alloc<'a>> = Vec::try_with_capacity_in(n_usize, o.alloc)?;

    let dummy_pic_opts = PictureOpts {
        alloc: o.scratch,
        scratch: o.scratch,
        background: o.background,
        colour: o.fallback_colour,
        shading: o.shading,
        mode: o.mode,
        supersample: o.supersample,
        ..
    };
    let ctx = ShadeContext::new(&cam, bvh.scene_bounds, &dummy_pic_opts);
    let mut stats = PictureStats::default();
    let use_shading_normals = matches!(o.normal_source, MeshNormalSource::Shading) && !plant.has(MeshPicturePlant::FlatOnly);
    let active_highlights: &[MeshHighlight] = if plant.has(MeshPicturePlant::IgnoreHighlights) { &[] } else { o.highlights };
    let active_maps: &[MaterialMaps<'_>] = if plant.has(MeshPicturePlant::IgnoreTextures) { &[] } else { o.material_maps };

    for y in 0..view.height {
        for x in 0..view.width {
            if o.supersample == 1 {
                let (origin, dir) = cam.ray(x, y);
                stats.rays += 1;
                if let Some(hit) = bvh.trace(origin, dir, f64::INFINITY, &mut stats) {
                    stats.hits += 1;
                    let sample = shade_mesh_hit(
                        &bvh,
                        &cam,
                        &ctx,
                        &o,
                        active_highlights,
                        active_maps,
                        f64::from(x) + 0.5,
                        f64::from(y) + 0.5,
                        origin,
                        dir,
                        &hit,
                        use_shading_normals,
                        &mut stats,
                    );
                    let rgba = resolve_samples(sample, 1.0, ctx.linear_blend);
                    pixels.try_extend_from_slice(&rgba)?;
                    push(&mut depth, hit.t)?;
                } else {
                    pixels.try_extend_from_slice(&o.background)?;
                    push(&mut depth, f64::INFINITY)?;
                }
            } else {
                let mut sum = [0.0f64; 4];
                let mut best_t = f64::INFINITY;
                let mut any_hit = false;
                for &(ox, oy) in &RGSS_2X2 {
                    let (sx, sy) = (f64::from(x) + ox, f64::from(y) + oy);
                    let (origin, dir) = cam.ray_at(sx, sy);
                    stats.rays += 1;
                    if let Some(hit) = bvh.trace(origin, dir, f64::INFINITY, &mut stats) {
                        stats.hits += 1;
                        any_hit = true;
                        best_t = best_t.min(hit.t);
                        let s = shade_mesh_hit(
                            &bvh,
                            &cam,
                            &ctx,
                            &o,
                            active_highlights,
                            active_maps,
                            sx,
                            sy,
                            origin,
                            dir,
                            &hit,
                            use_shading_normals,
                            &mut stats,
                        );
                        for c in 0..4 {
                            sum[c] += s[c];
                        }
                    } else {
                        let bg = ctx.miss_sample();
                        for c in 0..4 {
                            sum[c] += bg[c];
                        }
                    }
                }
                let rgba = if any_hit { resolve_samples(sum, 0.25, ctx.linear_blend) } else { o.background };
                pixels.try_extend_from_slice(&rgba)?;
                push(&mut depth, best_t)?;
            }
        }
    }

    Ok(Picture { width: view.width, height: view.height, pixels, depth, stats })
}

// ---- affine transforms -------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Affine3 {
    m: [[f64; 3]; 3],
    t: [f64; 3],
}

impl Affine3 {
    const IDENTITY: Affine3 = Affine3 { m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], t: [0.0; 3] };

    fn from_transform(tr: Transform) -> Affine3 {
        match tr {
            Transform::Matrix(mat) => Affine3 { m: mat.linear, t: mat.translation },
            Transform::Trs(trs) => {
                let [qx, qy, qz, qw] = trs.rotation;
                let [sx, sy, sz] = trs.scale;
                let xx = qx * qx;
                let yy = qy * qy;
                let zz = qz * qz;
                let xy = qx * qy;
                let xz = qx * qz;
                let yz = qy * qz;
                let wx = qw * qx;
                let wy = qw * qy;
                let wz = qw * qz;
                let r = [
                    [1.0 - 2.0 * (yy + zz), 2.0 * (xy - wz), 2.0 * (xz + wy)],
                    [2.0 * (xy + wz), 1.0 - 2.0 * (xx + zz), 2.0 * (yz - wx)],
                    [2.0 * (xz - wy), 2.0 * (yz + wx), 1.0 - 2.0 * (xx + yy)],
                ];
                let m = [
                    [r[0][0] * sx, r[0][1] * sy, r[0][2] * sz],
                    [r[1][0] * sx, r[1][1] * sy, r[1][2] * sz],
                    [r[2][0] * sx, r[2][1] * sy, r[2][2] * sz],
                ];
                Affine3 { m, t: trs.translation }
            }
        }
    }

    fn compose(&self, child: &Affine3) -> Affine3 {
        let mut m = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] = self.m[i][0] * child.m[0][j] + self.m[i][1] * child.m[1][j] + self.m[i][2] * child.m[2][j];
            }
        }
        let t = [
            self.m[0][0] * child.t[0] + self.m[0][1] * child.t[1] + self.m[0][2] * child.t[2] + self.t[0],
            self.m[1][0] * child.t[0] + self.m[1][1] * child.t[1] + self.m[1][2] * child.t[2] + self.t[1],
            self.m[2][0] * child.t[0] + self.m[2][1] * child.t[1] + self.m[2][2] * child.t[2] + self.t[2],
        ];
        Affine3 { m, t }
    }

    #[inline]
    fn point(&self, p: [f32; 3]) -> Vec3 {
        let (x, y, z) = (f64::from(p[0]), f64::from(p[1]), f64::from(p[2]));
        Vec3::new(
            self.m[0][0] * x + self.m[0][1] * y + self.m[0][2] * z + self.t[0],
            self.m[1][0] * x + self.m[1][1] * y + self.m[1][2] * z + self.t[1],
            self.m[2][0] * x + self.m[2][1] * y + self.m[2][2] * z + self.t[2],
        )
    }

    #[inline]
    fn point_v3(&self, p: Vec3) -> Vec3 {
        Vec3::new(
            self.m[0][0] * p.x + self.m[0][1] * p.y + self.m[0][2] * p.z + self.t[0],
            self.m[1][0] * p.x + self.m[1][1] * p.y + self.m[1][2] * p.z + self.t[1],
            self.m[2][0] * p.x + self.m[2][1] * p.y + self.m[2][2] * p.z + self.t[2],
        )
    }

    fn inverse(&self) -> Affine3 {
        let m = self.m;
        let det = self.det();
        if !(det.abs() > 1e-18 && det.is_finite()) {
            return Affine3::IDENTITY;
        }
        let inv_det = 1.0 / det;
        let inv_m = [
            [
                (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det,
                (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det,
                (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det,
            ],
            [
                (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det,
                (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det,
                (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det,
            ],
            [
                (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det,
                (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det,
                (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det,
            ],
        ];
        let t = self.t;
        let inv_t = [
            -(inv_m[0][0] * t[0] + inv_m[0][1] * t[1] + inv_m[0][2] * t[2]),
            -(inv_m[1][0] * t[0] + inv_m[1][1] * t[1] + inv_m[1][2] * t[2]),
            -(inv_m[2][0] * t[0] + inv_m[2][1] * t[1] + inv_m[2][2] * t[2]),
        ];
        Affine3 { m: inv_m, t: inv_t }
    }

    fn det(&self) -> f64 {
        let m = self.m;
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    }

    /// Cofactor matrix (proportional to `inv(m)^T`, signed by `det` so orientation follows
    /// positive determinant; for mirrors we negate so outward normals stay outward).
    fn normal_matrix(&self) -> [[f64; 3]; 3] {
        let m = self.m;
        let c = [
            [
                m[1][1] * m[2][2] - m[1][2] * m[2][1],
                m[1][2] * m[2][0] - m[1][0] * m[2][2],
                m[1][0] * m[2][1] - m[1][1] * m[2][0],
            ],
            [
                m[0][2] * m[2][1] - m[0][1] * m[2][2],
                m[0][0] * m[2][2] - m[0][2] * m[2][0],
                m[0][1] * m[2][0] - m[0][0] * m[2][1],
            ],
            [
                m[0][1] * m[1][2] - m[0][2] * m[1][1],
                m[0][2] * m[1][0] - m[0][0] * m[1][2],
                m[0][0] * m[1][1] - m[0][1] * m[1][0],
            ],
        ];
        if self.det() < 0.0 {
            [[-c[0][0], -c[0][1], -c[0][2]], [-c[1][0], -c[1][1], -c[1][2]], [-c[2][0], -c[2][1], -c[2][2]]]
        } else {
            c
        }
    }
}

#[inline]
fn mul_mat3(m: &[[f64; 3]; 3], v: [f32; 3]) -> Vec3 {
    let (x, y, z) = (f64::from(v[0]), f64::from(v[1]), f64::from(v[2]));
    Vec3::new(
        m[0][0] * x + m[0][1] * y + m[0][2] * z,
        m[1][0] * x + m[1][1] * y + m[1][2] * z,
        m[2][0] * x + m[2][1] * y + m[2][2] * z,
    )
}

// ---- BVH ---------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct WorldTri {
    a: Vec3,
    e1: Vec3,
    e2: Vec3,
    n0: Vec3,
    n1: Vec3,
    n2: Vec3,
    ng: Vec3,
    /// Optional per-vertex UV coordinates `(u, v)` copied from the exported mesh's `uvs` column.
    uvs: Option<[[f64; 2]; 3]>,
    /// Optional world-space unit vertex tangents `[tx, ty, tz, handedness]` transformed by the
    /// part's linear matrix and orthonormalized against the vertex normals.
    tangents: Option<[[f64; 4]; 3]>,
    /// `2 * area / edge_len` for each of the 3 opposite edges (used for exact screen-space
    /// pixel distance to the nearest triangle edge in wireframe mode).
    alt_scale: [f64; 3],
    /// Material colour in linear RGB `[0, 1]`.
    albedo_linear: [f64; 3],
    /// Material colour in sRGB `[0, 1]` (for classic mode).
    albedo_srgb: [f64; 3],
    metallic: f64,
    roughness: f64,
    emissive: [f64; 3],
    role: u32,
    has_tint_override: bool,
    part_idx: u32,
    bounds: Aabb3,
    centroid: Vec3,
}

#[derive(Clone, Copy)]
struct BvhNode {
    bounds: Aabb3,
    /// If `count > 0`, leaf spanning `tris[first .. first + count]`.
    /// Otherwise internal node with children `first` and `first + 1`.
    first: u32,
    count: u32,
}

struct MeshBvh<'s> {
    nodes: Vec<BvhNode, Alloc<'s>>,
    tris: Vec<WorldTri, Alloc<'s>>,
    inv_xforms: Vec<Affine3, Alloc<'s>>,
    scene_bounds: Aabb3,
}

struct MeshHit {
    t: f64,
    u: f64,
    v: f64,
    tri_idx: u32,
}

fn build_bvh<'s>(
    parts: &[MeshScenePart<'_>],
    materials: &[Material<'_>],
    fallback_colour: [u8; 3],
    scratch: Alloc<'s>,
    plant: impl Planted<MeshPicturePlant>,
) -> Result<MeshBvh<'s>, AllocFailure> {
    let mut world_xforms: Vec<Affine3, Alloc<'s>> = Vec::try_with_capacity_in(parts.len(), scratch)?;
    let mut inv_xforms: Vec<Affine3, Alloc<'s>> = Vec::try_with_capacity_in(parts.len(), scratch)?;
    for part in parts {
        let local = if plant.has(MeshPicturePlant::IgnoreTransform) {
            Affine3::IDENTITY
        } else {
            Affine3::from_transform(part.transform)
        };
        let world = match part.parent {
            Some(p) if (p as usize) < world_xforms.len() => world_xforms[p as usize].compose(&local),
            _ => local,
        };
        push(&mut world_xforms, world)?;
        push(&mut inv_xforms, world.inverse())?;
    }

    let total_tris: usize = parts.iter().map(|p| p.mesh.triangle_count() as usize).sum();
    let mut tris: Vec<WorldTri, Alloc<'s>> = Vec::try_with_capacity_in(total_tris, scratch)?;
    let fb_srgb = [
        f64::from(fallback_colour[0]) / 255.0,
        f64::from(fallback_colour[1]) / 255.0,
        f64::from(fallback_colour[2]) / 255.0,
    ];
    let fb_linear = [
        srgb_to_linear(fallback_colour[0]),
        srgb_to_linear(fallback_colour[1]),
        srgb_to_linear(fallback_colour[2]),
    ];

    for (part_idx, part) in parts.iter().enumerate() {
        let xf = world_xforms[part_idx];
        let nmat = xf.normal_matrix();
        let mirrored = xf.det() < 0.0;
        let view = &part.mesh;
        let pos = view.positions();
        let nrms = view.normals();
        let uvs_col = view.uvs();
        let tans_col = view.tangents();
        let roles = view.roles();
        let has_nrms = nrms.len() == pos.len();
        let has_tint_override = part.tint_override.is_some();

        for (ti, tri) in view.triangles().iter().enumerate() {
            let (i0, i1, i2) = if mirrored {
                (tri[0] as usize, tri[2] as usize, tri[1] as usize)
            } else {
                (tri[0] as usize, tri[1] as usize, tri[2] as usize)
            };
            let a = xf.point(pos[i0]);
            let b = xf.point(pos[i1]);
            let c = xf.point(pos[i2]);
            let e1 = b - a;
            let e2 = c - a;
            let cross = e1.cross(e2);
            let twice_area = cross.length();
            if !(twice_area > 0.0 && twice_area.is_finite()) {
                continue;
            }
            let ng = cross * (1.0 / twice_area);

            let (n0, n1, n2) = if has_nrms {
                (
                    mul_mat3(&nmat, nrms[i0]).normalized().unwrap_or(ng),
                    mul_mat3(&nmat, nrms[i1]).normalized().unwrap_or(ng),
                    mul_mat3(&nmat, nrms[i2]).normalized().unwrap_or(ng),
                )
            } else {
                (ng, ng, ng)
            };

            let uvs = uvs_col.map(|col| {
                [
                    [f64::from(col[i0][0]), f64::from(col[i0][1])],
                    [f64::from(col[i1][0]), f64::from(col[i1][1])],
                    [f64::from(col[i2][0]), f64::from(col[i2][1])],
                ]
            });

            let tangents = tans_col.map(|col| {
                let xf_tan = |raw: [f32; 4], nv: Vec3| -> [f64; 4] {
                    let tw = mul_mat3(&xf.m, [raw[0], raw[1], raw[2]]);
                    let t_ortho = (tw - nv * tw.dot(nv)).normalized().unwrap_or_else(|| {
                        let helper = if nv.z.abs() < 0.9 { Vec3::new(0.0, 0.0, 1.0) } else { Vec3::new(1.0, 0.0, 0.0) };
                        helper.cross(nv).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0))
                    });
                    let w_raw = if raw[3] < 0.0 { -1.0 } else { 1.0 };
                    let w_sign = if mirrored { -w_raw } else { w_raw };
                    [t_ortho.x, t_ortho.y, t_ortho.z, w_sign]
                };
                [xf_tan(col[i0], n0), xf_tan(col[i1], n1), xf_tan(col[i2], n2)]
            });

            let len_bc = (c - b).length().max(LEAST_STEP);
            let len_ca = e2.length().max(LEAST_STEP);
            let len_ab = e1.length().max(LEAST_STEP);
            let alt_scale = [twice_area / len_bc, twice_area / len_ca, twice_area / len_ab];

            let role = roles.get(ti).map_or(0u32, |r| r.0);
            let (albedo_linear, albedo_srgb, metallic, roughness, emissive) = if let Some(rgb) = part.tint_override {
                let srgb = [f64::from(rgb[0]) / 255.0, f64::from(rgb[1]) / 255.0, f64::from(rgb[2]) / 255.0];
                let lin = [srgb_to_linear(rgb[0]), srgb_to_linear(rgb[1]), srgb_to_linear(rgb[2])];
                (lin, srgb, 0.15, 0.55, [0.0; 3])
            } else if let Some(mat) = roles.get(ti).and_then(|r| materials.get(r.0 as usize)) {
                let lin = [
                    mat.base_colour[0].clamp(0.0, 1.0),
                    mat.base_colour[1].clamp(0.0, 1.0),
                    mat.base_colour[2].clamp(0.0, 1.0),
                ];
                let srgb = [linear_to_srgb(lin[0]), linear_to_srgb(lin[1]), linear_to_srgb(lin[2])];
                let em_scale = mat.emissive_strength.unwrap_or(1.0).max(0.0);
                let em = [mat.emissive[0] * em_scale, mat.emissive[1] * em_scale, mat.emissive[2] * em_scale];
                (lin, srgb, mat.metallic.clamp(0.0, 1.0), mat.roughness.clamp(0.04, 1.0), em)
            } else {
                (fb_linear, fb_srgb, 0.2, 0.5, [0.0; 3])
            };

            let lo = a.min(b).min(c);
            let hi = a.max(b).max(c);
            let centroid = (a + b + c) * (1.0 / 3.0);
            push(&mut tris, WorldTri {
                a,
                e1,
                e2,
                n0,
                n1,
                n2,
                ng,
                uvs,
                tangents,
                alt_scale,
                albedo_linear,
                albedo_srgb,
                metallic,
                roughness,
                emissive,
                role,
                has_tint_override,
                part_idx: part_idx as u32,
                bounds: Aabb3::new(lo, hi),
                centroid,
            })?;
        }
    }

    if tris.is_empty() {
        return Ok(MeshBvh { nodes: Vec::new_in(scratch), tris, inv_xforms, scene_bounds: Aabb3::EMPTY });
    }

    let mut nodes: Vec<BvhNode, Alloc<'s>> = Vec::try_with_capacity_in(tris.len().saturating_mul(2), scratch)?;
    let root_bounds = range_bounds(&tris);
    push(&mut nodes, BvhNode { bounds: root_bounds, first: 0, count: tris.len() as u32 })?;

    // Iterative top-down binned-SAH split stack (explicit bound, zero recursion).
    let mut work: Vec<(u32, usize, usize), Alloc<'s>> = Vec::try_with_capacity_in(64, scratch)?;
    push(&mut work, (0, 0, tris.len()))?;

    while let Some((node_idx, start, end)) = work.pop() {
        let count = end - start;
        if count <= LEAF_TRIS {
            continue;
        }
        let mut c_lo = Vec3::new(f64::INFINITY, f64::INFINITY, f64::INFINITY);
        let mut c_hi = Vec3::new(f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for t in &tris[start..end] {
            c_lo = c_lo.min(t.centroid);
            c_hi = c_hi.max(t.centroid);
        }
        let extent = c_hi - c_lo;
        let axis = if extent.x >= extent.y && extent.x >= extent.z {
            0
        } else if extent.y >= extent.z {
            1
        } else {
            2
        };
        let span = axis_coord(extent, axis);
        let mid = if span <= 1e-12 {
            start + count / 2
        } else {
            let min_c = axis_coord(c_lo, axis);
            let mut bin_bounds = [Aabb3::EMPTY; SAH_BINS];
            let mut bin_counts = [0usize; SAH_BINS];
            let inv_span = (SAH_BINS as f64) / span;
            for t in &tris[start..end] {
                let raw = ((axis_coord(t.centroid, axis) - min_c) * inv_span) as isize;
                let b = raw.clamp(0, (SAH_BINS - 1) as isize) as usize;
                bin_counts[b] += 1;
                bin_bounds[b] = union_aabb(&bin_bounds[b], &t.bounds);
            }
            let mut left_area = [0.0f64; SAH_BINS - 1];
            let mut left_count = [0usize; SAH_BINS - 1];
            let mut acc_b = Aabb3::EMPTY;
            let mut acc_n = 0usize;
            for i in 0..(SAH_BINS - 1) {
                acc_b = union_aabb(&acc_b, &bin_bounds[i]);
                acc_n += bin_counts[i];
                left_area[i] = surface_area(&acc_b);
                left_count[i] = acc_n;
            }
            let mut best_cost = f64::INFINITY;
            let mut best_split = 0usize;
            let mut acc_rb = Aabb3::EMPTY;
            let mut acc_rn = 0usize;
            for i in (0..(SAH_BINS - 1)).rev() {
                acc_rb = union_aabb(&acc_rb, &bin_bounds[i + 1]);
                acc_rn += bin_counts[i + 1];
                if left_count[i] > 0 && acc_rn > 0 {
                    let cost = left_area[i] * (left_count[i] as f64) + surface_area(&acc_rb) * (acc_rn as f64);
                    if cost < best_cost {
                        best_cost = cost;
                        best_split = i + 1;
                    }
                }
            }
            if best_split == 0 {
                start + count / 2
            } else {
                let split_val = min_c + span * (best_split as f64) / (SAH_BINS as f64);
                let mut i = start;
                let mut j = end;
                while i < j {
                    if axis_coord(tris[i].centroid, axis) < split_val {
                        i += 1;
                    } else {
                        j -= 1;
                        tris.swap(i, j);
                    }
                }
                if i == start || i == end { start + count / 2 } else { i }
            }
        };

        let left_idx = nodes.len() as u32;
        let right_idx = left_idx + 1;
        let lb = range_bounds(&tris[start..mid]);
        let rb = range_bounds(&tris[mid..end]);
        push(&mut nodes, BvhNode { bounds: lb, first: start as u32, count: (mid - start) as u32 })?;
        push(&mut nodes, BvhNode { bounds: rb, first: mid as u32, count: (end - mid) as u32 })?;
        nodes[node_idx as usize].first = left_idx;
        nodes[node_idx as usize].count = 0;

        push(&mut work, (right_idx, mid, end))?;
        push(&mut work, (left_idx, start, mid))?;
    }

    Ok(MeshBvh { nodes, tris, inv_xforms, scene_bounds: root_bounds })
}

#[inline]
fn axis_coord(v: Vec3, axis: usize) -> f64 {
    match axis {
        0 => v.x,
        1 => v.y,
        _ => v.z,
    }
}

#[inline]
fn union_aabb(a: &Aabb3, b: &Aabb3) -> Aabb3 {
    if a.is_empty() {
        *b
    } else if b.is_empty() {
        *a
    } else {
        Aabb3::new(a.lo.min(b.lo), a.hi.max(b.hi))
    }
}

#[inline]
fn surface_area(b: &Aabb3) -> f64 {
    if b.is_empty() {
        0.0
    } else {
        let d = b.hi - b.lo;
        2.0 * (d.x * d.y + d.y * d.z + d.z * d.x)
    }
}

fn range_bounds(tris: &[WorldTri]) -> Aabb3 {
    let mut b = Aabb3::EMPTY;
    for t in tris {
        b = union_aabb(&b, &t.bounds);
    }
    b
}

impl MeshBvh<'_> {
    fn trace(&self, origin: Vec3, dir: Vec3, mut best_t: f64, stats: &mut PictureStats) -> Option<MeshHit> {
        if self.nodes.is_empty() {
            return None;
        }
        let mut stack = [0u32; 64];
        let mut sp = 0usize;
        stats.boxes += 1;
        if slab(&self.nodes[0].bounds, origin, dir).is_none_or(|(t0, _)| t0 > best_t) {
            return None;
        }
        stack[sp] = 0;
        sp += 1;
        let mut best: Option<MeshHit> = None;

        while sp > 0 {
            sp -= 1;
            let node = self.nodes[stack[sp] as usize];
            if node.count > 0 {
                let start = node.first as usize;
                let end = start + node.count as usize;
                for idx in start..end {
                    let tri = &self.tris[idx];
                    stats.points += 1;
                    if let Some((t, u, v)) = intersect_tri(origin, dir, tri, best_t) {
                        best_t = t;
                        best = Some(MeshHit { t, u, v, tri_idx: idx as u32 });
                    }
                }
            } else {
                let l = node.first as usize;
                let r = l + 1;
                stats.boxes += 2;
                let hl = slab(&self.nodes[l].bounds, origin, dir).filter(|&(t0, _)| t0 <= best_t);
                let hr = slab(&self.nodes[r].bounds, origin, dir).filter(|&(t0, _)| t0 <= best_t);
                match (hl, hr) {
                    (Some((tl, _)), Some((tr, _))) => {
                        if tl <= tr {
                            if sp + 2 <= stack.len() {
                                stack[sp] = r as u32;
                                stack[sp + 1] = l as u32;
                                sp += 2;
                            }
                        } else if sp + 2 <= stack.len() {
                            stack[sp] = l as u32;
                            stack[sp + 1] = r as u32;
                            sp += 2;
                        }
                    }
                    (Some(_), None) => {
                        if sp < stack.len() {
                            stack[sp] = l as u32;
                            sp += 1;
                        }
                    }
                    (None, Some(_)) => {
                        if sp < stack.len() {
                            stack[sp] = r as u32;
                            sp += 1;
                        }
                    }
                    (None, None) => {}
                }
            }
        }
        best
    }

    fn occluded(&self, origin: Vec3, dir: Vec3, max_t: f64, stats: &mut PictureStats) -> bool {
        if self.nodes.is_empty() {
            return false;
        }
        let mut stack = [0u32; 64];
        let mut sp = 0usize;
        stats.boxes += 1;
        if slab(&self.nodes[0].bounds, origin, dir).is_none_or(|(t0, _)| t0 > max_t) {
            return false;
        }
        stack[sp] = 0;
        sp += 1;

        while sp > 0 {
            sp -= 1;
            let node = self.nodes[stack[sp] as usize];
            if node.count > 0 {
                let start = node.first as usize;
                let end = start + node.count as usize;
                for idx in start..end {
                    stats.points += 1;
                    if intersect_tri(origin, dir, &self.tris[idx], max_t).is_some() {
                        return true;
                    }
                }
            } else {
                let l = node.first as usize;
                let r = l + 1;
                stats.boxes += 2;
                if slab(&self.nodes[l].bounds, origin, dir).is_some_and(|(t0, _)| t0 <= max_t) && sp < stack.len() {
                    stack[sp] = l as u32;
                    sp += 1;
                }
                if slab(&self.nodes[r].bounds, origin, dir).is_some_and(|(t0, _)| t0 <= max_t) && sp < stack.len() {
                    stack[sp] = r as u32;
                    sp += 1;
                }
            }
        }
        false
    }
}

/// Möller–Trumbore ray-triangle intersection in f64 (two-sided, watertight eps guard).
#[inline]
fn intersect_tri(origin: Vec3, dir: Vec3, tri: &WorldTri, max_t: f64) -> Option<(f64, f64, f64)> {
    let pvec = dir.cross(tri.e2);
    let det = tri.e1.dot(pvec);
    if det.abs() < 1e-15 {
        return None;
    }
    let inv_det = 1.0 / det;
    let tvec = origin - tri.a;
    let u = tvec.dot(pvec) * inv_det;
    if !(-1e-9..=1.0 + 1e-9).contains(&u) {
        return None;
    }
    let qvec = tvec.cross(tri.e1);
    let v = dir.dot(qvec) * inv_det;
    if v < -1e-9 || u + v > 1.0 + 1e-9 {
        return None;
    }
    let t = tri.e2.dot(qvec) * inv_det;
    if t > LEAST_STEP && t < max_t {
        Some((t, u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)))
    } else {
        None
    }
}

// ---- Shading + Wireframe -----------------------------------------------------------------

/// Deterministic Fibonacci hemisphere directions (`8` cosine-distributed directions in local +Z
/// hemisphere) for zero-RNG mesh ambient occlusion.
const HEMI_DIRS: [[f64; 3]; 8] = [
    [0.35355339, 0.00000000, 0.93541435],
    [-0.45198512, 0.41391344, 0.78979504],
    [0.06804998, -0.74298924, 0.66583496],
    [0.57845937, 0.59202330, 0.56115342],
    [-0.86996393, -0.07250085, 0.48775647],
    [0.62511417, -0.66322274, 0.41154329],
    [-0.01106651, 0.93956283, 0.34219763],
    [-0.65913068, -0.71300496, 0.23910394],
];

fn mesh_ao(
    bvh: &MeshBvh<'_>,
    p: Vec3,
    n: Vec3,
    bias: f64,
    max_dist: f64,
    steps: u8,
    strength: f64,
    stats: &mut PictureStats,
) -> f64 {
    let strength = strength.clamp(0.0, 1.0);
    if steps == 0 || !(strength > 0.0) {
        return 1.0;
    }
    // Build orthonormal tangent basis `(t, b, n)` deterministically from `n`.
    let helper = if n.z.abs() < 0.9 { Vec3::new(0.0, 0.0, 1.0) } else { Vec3::new(1.0, 0.0, 0.0) };
    let tangent = helper.cross(n).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
    let bitangent = n.cross(tangent);
    let n_rays = usize::from(steps).min(HEMI_DIRS.len());
    let origin = p + n * bias;
    let mut hits = 0usize;
    for d in &HEMI_DIRS[..n_rays] {
        let dir = (tangent * d[0] + bitangent * d[1] + n * d[2]).normalized().unwrap_or(n);
        if bvh.occluded(origin, dir, max_dist, stats) {
            hits += 1;
        }
    }
    let open = 1.0 - (hits as f64) / (n_rays as f64);
    (1.0 - strength) + strength * open
}

/// Effective per-hit PBR material parameters after optional baked texture sampling.
#[derive(Clone, Copy)]
struct SampledSurface {
    albedo_linear: [f64; 3],
    albedo_srgb: [f64; 3],
    metallic: f64,
    roughness: f64,
}

#[allow(clippy::too_many_arguments)]
fn shade_mesh_hit(
    bvh: &MeshBvh<'_>,
    cam: &Camera,
    ctx: &ShadeContext,
    o: &MeshPictureOpts<'_, '_, '_>,
    highlights: &[MeshHighlight],
    material_maps: &[MaterialMaps<'_>],
    sx: f64,
    sy: f64,
    origin: Vec3,
    dir: Vec3,
    hit: &MeshHit,
    use_shading_normals: bool,
    stats: &mut PictureStats,
) -> [f64; 4] {
    let tri = &bvh.tris[hit.tri_idx as usize];
    let p = origin + dir * hit.t;
    let w = (1.0 - hit.u - hit.v).max(0.0);
    let mut n = if use_shading_normals {
        (tri.n0 * w + tri.n1 * hit.u + tri.n2 * hit.v).normalized().unwrap_or(tri.ng)
    } else {
        tri.ng
    };
    // Keep front-facing orientation toward the camera for double-sided / grazing visibility.
    if n.dot(dir) > 0.0 {
        n = n * -1.0;
    }
    let ng_face = if tri.ng.dot(dir) > 0.0 { tri.ng * -1.0 } else { tri.ng };

    let mut surface = SampledSurface {
        albedo_linear: tri.albedo_linear,
        albedo_srgb: tri.albedo_srgb,
        metallic: tri.metallic,
        roughness: tri.roughness,
    };

    if let Some(uvs) = tri.uvs
        && let Some(maps) = material_maps.get(tri.role as usize)
    {
        let uv_u = uvs[0][0] * w + uvs[1][0] * hit.u + uvs[2][0] * hit.v;
        let uv_v = uvs[0][1] * w + uvs[1][1] * hit.u + uvs[2][1] * hit.v;

        if !tri.has_tint_override
            && let Some(tex) = maps.base_colour
        {
            let s_lin = tex.sample_srgb_linear_bilinear(uv_u, uv_v);
            let s_raw = tex.sample_bilinear(uv_u, uv_v);
            for c in 0..3 {
                surface.albedo_linear[c] = (surface.albedo_linear[c] * s_lin[c]).clamp(0.0, 1.0);
                surface.albedo_srgb[c] = (surface.albedo_srgb[c] * s_raw[c]).clamp(0.0, 1.0);
            }
        }

        if let Some(tex) = maps.metallic_roughness {
            let mr = tex.sample_bilinear(uv_u, uv_v);
            surface.roughness = (surface.roughness * mr[1]).clamp(0.04, 1.0);
            surface.metallic = (surface.metallic * mr[2]).clamp(0.0, 1.0);
        }

        if let Some(tex) = maps.normal
            && let Some(tans) = tri.tangents
        {
            let tx = tans[0][0] * w + tans[1][0] * hit.u + tans[2][0] * hit.v;
            let ty = tans[0][1] * w + tans[1][1] * hit.u + tans[2][1] * hit.v;
            let tz = tans[0][2] * w + tans[1][2] * hit.u + tans[2][2] * hit.v;
            let tw = if tans[0][3] * w + tans[1][3] * hit.u + tans[2][3] * hit.v < 0.0 { -1.0 } else { 1.0 };
            let t_raw = Vec3::new(tx, ty, tz);
            if let Some(t_dir) = (t_raw - n * t_raw.dot(n)).normalized() {
                let b_dir = n.cross(t_dir) * tw;
                let nm = tex.sample_bilinear(uv_u, uv_v);
                let scale = if maps.normal_scale.is_finite() { maps.normal_scale } else { 1.0 };
                let nx = (nm[0] * 2.0 - 1.0) * scale;
                let ny = (nm[1] * 2.0 - 1.0) * scale;
                let nz = nm[2] * 2.0 - 1.0;
                if let Some(perturbed) = (t_dir * nx + b_dir * ny + n * nz).normalized() {
                    n = perturbed;
                }
            }
        }
    }

    let mut colour = match o.mode {
        RenderMode::Normals => [(n.x * 0.5 + 0.5).clamp(0.0, 1.0), (n.y * 0.5 + 0.5).clamp(0.0, 1.0), (n.z * 0.5 + 0.5).clamp(0.0, 1.0), 1.0],
        RenderMode::Depth => {
            let proj = (p - cam.eye).dot(cam.forward);
            let span = (ctx.z_far - ctx.z_near).max(LEAST_STEP);
            let v = 1.0 - ((proj - ctx.z_near) / span).clamp(0.0, 1.0);
            [v, v, v, 1.0]
        }
        RenderMode::Matcap => {
            let nx = n.dot(cam.right);
            let ny = n.dot(cam.up);
            let nz = n.dot(cam.forward * -1.0).clamp(0.0, 1.0);
            let up_t = (ny * 0.5 + 0.5).clamp(0.0, 1.0);
            let dome = [0.24 + 0.52 * up_t, 0.25 + 0.54 * up_t, 0.28 + 0.56 * up_t];
            let l1 = Vec3::new(-0.35, 0.58, 0.735).normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));
            let nv = Vec3::new(nx, ny, nz);
            let d1 = nv.dot(l1).max(0.0);
            let lobe1 = blaze_math::pow(d1, 24.0) * 0.55 + d1 * 0.25;
            let rim = blaze_math::pow((1.0 - nz).clamp(0.0, 1.0), 3.0) * 0.28;
            let base = surface.albedo_srgb;
            [
                (dome[0] * (0.35 + 0.65 * base[0]) + lobe1 + rim * 0.95).clamp(0.0, 1.0),
                (dome[1] * (0.35 + 0.65 * base[1]) + lobe1 + rim * 0.95).clamp(0.0, 1.0),
                (dome[2] * (0.35 + 0.65 * base[2]) + lobe1 + rim * 0.95).clamp(0.0, 1.0),
                1.0,
            ]
        }
        RenderMode::Zebra(zebra) => shade_zebra(cam, dir, n, &zebra),
        RenderMode::Shaded => match o.wireframe {
            Wireframe::Only { .. } => {
                // Clean matte surface under wireframe-only inspection.
                let s = 0.72 + 0.28 * n.dot(ctx.classic_light).max(0.0);
                if ctx.linear_blend {
                    let b = srgb_to_linear(236) * s;
                    [b, b, b, 1.0]
                } else {
                    let b = 0.92 * s;
                    [b, b, b, 1.0]
                }
            }
            _ => match o.shading {
                Shading::Classic => {
                    let s = 0.22 + 0.78 * n.dot(ctx.classic_light).max(0.0);
                    [
                        (surface.albedo_srgb[0] * s).clamp(0.0, 1.0),
                        (surface.albedo_srgb[1] * s).clamp(0.0, 1.0),
                        (surface.albedo_srgb[2] * s).clamp(0.0, 1.0),
                        1.0,
                    ]
                }
                Shading::Studio(studio) => shade_mesh_studio(bvh, cam, ctx, &studio, p, dir, n, ng_face, tri, &surface, stats),
            },
        },
    };

    // Optional anti-aliased screen-space wireframe overlay.
    match o.wireframe {
        Wireframe::Off => {}
        Wireframe::Overlay { width_px, colour: wc } | Wireframe::Only { width_px, colour: wc } => {
            let px_size = cam.tolerance_at(hit.t, 1.0);
            let d_world = (w * tri.alt_scale[0]).min(hit.u * tri.alt_scale[1]).min(hit.v * tri.alt_scale[2]);
            let d_px = d_world / px_size;
            let half_w = (width_px * 0.5).max(0.25);
            if d_px < half_w + 0.5 {
                let alpha = (1.0 - ((d_px - (half_w - 0.5)).clamp(0.0, 1.0))).clamp(0.0, 1.0);
                let edge_col = if ctx.linear_blend {
                    [srgb_to_linear(wc[0]), srgb_to_linear(wc[1]), srgb_to_linear(wc[2])]
                } else {
                    [f64::from(wc[0]) / 255.0, f64::from(wc[1]) / 255.0, f64::from(wc[2]) / 255.0]
                };
                for c in 0..3 {
                    colour[c] = (colour[c] * (1.0 - alpha) + edge_col[c] * alpha).clamp(0.0, 1.0);
                }
            }
        }
    }

    // Optional spatial feature highlights (missed tolerance callouts & critical feature outlines)
    // evaluated in the hit part's exact local frame.
    if !highlights.is_empty() {
        let px_size = cam.tolerance_at(hit.t, 1.0);
        let p_local = bvh.inv_xforms[tri.part_idx as usize].point_v3(p);
        for h in highlights {
            if h.part_index != tri.part_idx {
                continue;
            }
            let sd = h.region.signed_distance(p_local);
            match h.style {
                HighlightStyle::Missed { colour: hc } => {
                    let ring_half = 1.6 * px_size;
                    let alpha = if sd.abs() <= ring_half + 0.5 * px_size {
                        let t = ((sd.abs() - ring_half) / px_size).clamp(0.0, 1.0);
                        0.94 * (1.0 - t) + if sd <= 0.0 { 0.45 * t } else { 0.0 }
                    } else if sd < 0.0 {
                        // Subtle 45-degree screen-space diagonal hatch so missed regions remain
                        // unmistakably readable on any underlying material colour.
                        let phase = ((sx + sy) * 0.14).rem_euclid(1.0);
                        if phase < 0.32 { 0.62 } else { 0.36 }
                    } else {
                        0.0
                    };
                    if alpha > 0.0 {
                        let hcol = if ctx.linear_blend {
                            [srgb_to_linear(hc[0]), srgb_to_linear(hc[1]), srgb_to_linear(hc[2])]
                        } else {
                            [f64::from(hc[0]) / 255.0, f64::from(hc[1]) / 255.0, f64::from(hc[2]) / 255.0]
                        };
                        for c in 0..3 {
                            colour[c] = (colour[c] * (1.0 - alpha) + hcol[c] * alpha).clamp(0.0, 1.0);
                        }
                    }
                }
                HighlightStyle::Outline { colour: hc } => {
                    let ring_half = 1.1 * px_size;
                    if sd.abs() <= ring_half + 0.5 * px_size {
                        let t = ((sd.abs() - ring_half) / px_size).clamp(0.0, 1.0);
                        let alpha = 0.85 * (1.0 - t);
                        if alpha > 0.0 {
                            let hcol = if ctx.linear_blend {
                                [srgb_to_linear(hc[0]), srgb_to_linear(hc[1]), srgb_to_linear(hc[2])]
                            } else {
                                [f64::from(hc[0]) / 255.0, f64::from(hc[1]) / 255.0, f64::from(hc[2]) / 255.0]
                            };
                            for c in 0..3 {
                                colour[c] = (colour[c] * (1.0 - alpha) + hcol[c] * alpha).clamp(0.0, 1.0);
                            }
                        }
                    }
                }
            }
        }
    }

    colour
}

#[allow(clippy::too_many_arguments)]
fn shade_mesh_studio(
    bvh: &MeshBvh<'_>,
    cam: &Camera,
    ctx: &ShadeContext,
    studio: &StudioOpts,
    p: Vec3,
    dir: Vec3,
    n: Vec3,
    ng: Vec3,
    tri: &WorldTri,
    surface: &SampledSurface,
    stats: &mut PictureStats,
) -> [f64; 4] {
    let v = dir * -1.0;
    let bias = (ctx.scene_diag * 2e-4).max(LEAST_STEP * 100.0);
    let ao = mesh_ao(bvh, p, ng, bias, ctx.scene_diag * 0.12, studio.ao_steps, studio.ao_strength, stats);
    let shadow = if studio.shadow_steps > 0 && studio.shadow_strength > 0.0 && n.dot(ctx.key_dir) > 0.0 {
        let hit_shadow = bvh.occluded(p + ng * bias, ctx.key_dir, ctx.scene_diag * 2.0, stats);
        let vis = if hit_shadow { 0.0 } else { 1.0 };
        (1.0 - studio.shadow_strength.clamp(0.0, 1.0)) + studio.shadow_strength.clamp(0.0, 1.0) * vis
    } else if n.dot(ctx.key_dir) <= 0.0 {
        1.0 - studio.shadow_strength.clamp(0.0, 1.0)
    } else {
        1.0
    };

    let albedo = if studio.srgb { surface.albedo_linear } else { surface.albedo_srgb };
    // Metallic surfaces reflect tinted specular and attenuate diffuse; rough surfaces broaden highlights.
    let diff_weight = 1.0 - 0.65 * surface.metallic;
    let spec_weight = studio.specular * (0.4 + 1.2 * (1.0 - surface.roughness) + 0.6 * surface.metallic);
    let shininess = (studio.shininess * (1.15 - surface.roughness).max(0.15)).clamp(4.0, 256.0);

    let key_ao = 0.65 + 0.35 * ao;
    let ndotl_key = n.dot(ctx.key_dir).max(0.0) * studio.key * shadow * key_ao;
    let key_tint = [1.03, 1.00, 0.96];

    let ndotl_fill = n.dot(ctx.fill_dir).max(0.0) * studio.fill * ao;
    let fill_tint = [0.86, 0.94, 1.06];

    let hemi = (0.5 + 0.5 * n.dot(cam.up)).clamp(0.0, 1.0);
    let amb_scale = studio.ambient * ao;
    let sky_rgb = [0.92, 0.96, 1.04];
    let gnd_rgb = [0.55, 0.52, 0.48];
    let hemi_rgb = [
        (gnd_rgb[0] + (sky_rgb[0] - gnd_rgb[0]) * hemi) * amb_scale,
        (gnd_rgb[1] + (sky_rgb[1] - gnd_rgb[1]) * hemi) * amb_scale,
        (gnd_rgb[2] + (sky_rgb[2] - gnd_rgb[2]) * hemi) * amb_scale,
    ];

    let spec = if spec_weight > 0.0 && ndotl_key > 0.0 {
        let half_v = (ctx.key_dir + v).normalized().unwrap_or(ctx.key_dir);
        let ndoth = n.dot(half_v).max(0.0);
        if ndoth > 0.0 { blaze_math::pow(ndoth, shininess) * spec_weight * shadow } else { 0.0 }
    } else {
        0.0
    };

    let rim = if studio.rim > 0.0 {
        let fresnel = blaze_math::pow((1.0 - n.dot(v).max(0.0)).clamp(0.0, 1.0), 3.0);
        let rim_dir_mask = 0.35 + 0.65 * n.dot(ctx.rim_dir).max(0.0);
        fresnel * rim_dir_mask * studio.rim * ao
    } else {
        0.0
    };
    let rim_tint = [0.95, 0.98, 1.04];

    // Optional movable studio strip lights (exact no-op when all slots are `None`).
    let strip_contrib = eval_strip_lights(cam, &studio.strip_lights, n, v, surface.roughness, ao);

    let mut out = [0.0f64; 4];
    for c in 0..3 {
        let diff = albedo[c] * diff_weight * (ndotl_key * key_tint[c] + ndotl_fill * fill_tint[c] + hemi_rgb[c]);
        let spec_tint = (1.0 - surface.metallic) * key_tint[c] + surface.metallic * albedo[c];
        let mut col = diff + spec * spec_tint + rim * rim_tint[c] + tri.emissive[c];
        if let Some((s_diff, s_spec)) = strip_contrib {
            let strip_spec_tint = (1.0 - surface.metallic) + surface.metallic * albedo[c];
            col += albedo[c] * diff_weight * s_diff[c] + s_spec[c] * strip_spec_tint;
        }
        out[c] = col.clamp(0.0, 1.0);
    }
    out[3] = 1.0;
    let _ = quantize;
    out
}
