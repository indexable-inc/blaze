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
use blaze_pbr::{
    Cc0PackView, DenoiseOpts, EnvironmentPreset, GBufferPixel, PostProcessOpts, SmartMaterial, SurfaceMasks, SurfacePbr,
    TextureView, apply_smart_material, apply_tonemap, decode_tangent_normal, denoise_atrous, dominant_light, eval_direct_brdf,
    eval_ibl_with_irradiance, fbm3, fresnel_schlick_roughness, hash2, perturb_normal, post_process_hdr, rnm_blend,
    sample_bilinear, sample_environment, sample_ggx_vndf, sample_hex_tile, sample_triplanar,
    srgb_to_linear as pbr_srgb_to_linear,
};

use crate::shade::{RGSS_2X2, ShadeContext, eval_strip_lights, linear_to_srgb, quantize, resolve_samples, shade_zebra, srgb_to_linear};
use crate::{Camera, LEAST_STEP, PbrOpts, Picture, PictureError, PictureOpts, PictureStats, RenderMode, Shading, StudioOpts, View, slab};

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
    pub base_colour: Option<TextureMap<'a>>,
    pub metallic_roughness: Option<TextureMap<'a>>,
    pub normal: Option<TextureMap<'a>>,
    pub normal_scale: f64,
}

impl MaterialMaps<'_> {
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

/// Optional CC0 PBR texture pack + smart-material wear profile attached to a `MaterialRole`
/// index during [`Shading::Pbr`] mesh rendering.
#[derive(Clone, Copy, Debug)]
pub struct MaterialLook<'m> {
    pub textures: Cc0PackView<'m>,
    pub smart: SmartMaterial,
}

impl MaterialLook<'static> {
    pub const NONE: MaterialLook<'static> = MaterialLook { textures: Cc0PackView::NONE, smart: SmartMaterial::NONE };
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
    /// Visualization mode (`Shaded`, `Normals`, `Depth`, `Matcap`).
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
    /// Optional per-`MaterialRole` CC0 texture packs and smart-material wear/grime profiles
    /// (indexed by `MaterialRole.0`, used by [`Shading::Pbr`]).
    pub material_looks: &'h [MaterialLook<'h>] = &[],
    /// Optional per-part baked tangent-space detail normal maps (indexed by part index, blended
    /// over CC0 micro-normals via RNM in [`Shading::Pbr`]).
    pub part_normal_maps: &'h [Option<TextureView<'h>>] = &[],
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

/// Immutable, `Copy + Send + Sync` read-only PBR rendering parameters for parallel tile rendering
/// with [`PreparedMeshSceneRef::render_pbr_rows`]. Holds no allocators so it can be shared
/// across worker threads with `#![forbid(unsafe_code)]`.
#[derive(Clone, Copy)]
pub struct PbrRenderParams<'m> {
    pub background: [u8; 4] = [245, 246, 248, 255],
    pub fallback_colour: [u8; 3] = [180, 185, 192],
    pub normal_source: MeshNormalSource = MeshNormalSource::Shading,
    pub supersample: u8 = 2,
    pub material_looks: &'m [MaterialLook<'m>] = &[],
    pub part_normal_maps: &'m [Option<TextureView<'m>>] = &[],
}

impl<'m> PbrRenderParams<'m> {
    /// Extract the read-only, thread-safe PBR parameters from a [`MeshPictureOpts`].
    #[must_use]
    pub const fn from_mesh_opts(o: &MeshPictureOpts<'_, '_, 'm>) -> PbrRenderParams<'m> {
        PbrRenderParams {
            background: o.background,
            fallback_colour: o.fallback_colour,
            normal_source: o.normal_source,
            supersample: o.supersample,
            material_looks: o.material_looks,
            part_normal_maps: o.part_normal_maps,
        }
    }
}

/// Prebuilt immutable BVH + emissive light scene for multi-view or multi-threaded tile rendering.
///
/// Built once via [`PreparedMeshScene::new`], then borrowed as a `Copy + Send + Sync`
/// [`PreparedMeshSceneRef`] via [`PreparedMeshScene::as_ref`] so any host scheduler or thread
/// pool can render horizontal bands in parallel with zero BVH rebuilds and zero `unsafe` code,
/// while keeping `blaze-cad-picture` 100% `#![no_std]` sans-IO.
pub struct PreparedMeshScene<'s> {
    bvh: MeshBvh<'s>,
}

/// Read-only, `Copy + Send + Sync` view of a [`PreparedMeshScene`] for parallel band rendering.
#[derive(Clone, Copy)]
pub struct PreparedMeshSceneRef<'a> {
    bvh: MeshBvhRef<'a>,
}

impl<'s> PreparedMeshScene<'s> {
    /// Build the SAH BVH and clustered emissive NEE light table once for `parts`.
    pub fn new(
        parts: &[MeshScenePart<'_>],
        materials: &[Material<'_>],
        fallback_colour: [u8; 3],
        scratch: Alloc<'s>,
    ) -> Result<PreparedMeshScene<'s>, PictureError> {
        let bvh = build_bvh(parts, materials, fallback_colour, scratch, NoPlant)?;
        Ok(PreparedMeshScene { bvh })
    }

    /// Borrow a `Copy + Send + Sync` view of the built BVH for multi-threaded band rendering.
    #[must_use]
    pub fn as_ref(&self) -> PreparedMeshSceneRef<'_> {
        PreparedMeshSceneRef { bvh: self.bvh.as_ref() }
    }

    /// Render rows `[y_start, y_end)` of a full linear HDR + G-buffer PBR frame (`Shading::Pbr`).
    #[allow(clippy::too_many_arguments)]
    pub fn render_pbr_rows(
        &self,
        view: View,
        o: &MeshPictureOpts<'_, '_, '_>,
        pbr: &PbrOpts,
        y_start: u32,
        y_end: u32,
        out_hdr: &mut [[f64; 3]],
        out_gbuf: &mut [GBufferPixel],
        out_depth: &mut [f64],
    ) -> Result<PictureStats, PictureError> {
        let params = PbrRenderParams::from_mesh_opts(o);
        self.as_ref().render_pbr_rows(view, &params, pbr, y_start, y_end, out_hdr, out_gbuf, out_depth)
    }
}

impl PreparedMeshSceneRef<'_> {
    /// Render rows `[y_start, y_end)` of a full linear HDR + G-buffer PBR frame (`Shading::Pbr`),
    /// writing `(y_end - y_start) * view.width` entries into `out_hdr`, `out_gbuf`, and `out_depth`.
    ///
    /// Supports `supersample` `1..=4` (1, 4, 9, or 16 SPP), physical thin-lens Depth of Field
    /// (`pbr.aperture_radius_mm`), VNDF rough GGX reflections, 1-bounce BVH GI, and emissive NEE.
    #[allow(clippy::too_many_arguments)]
    pub fn render_pbr_rows(
        self,
        view: View,
        params: &PbrRenderParams<'_>,
        pbr: &PbrOpts,
        y_start: u32,
        y_end: u32,
        out_hdr: &mut [[f64; 3]],
        out_gbuf: &mut [GBufferPixel],
        out_depth: &mut [f64],
    ) -> Result<PictureStats, PictureError> {
        if !(1..=4).contains(&params.supersample) {
            return Err(PictureError::BadSupersample { supersample: params.supersample });
        }
        let cam = Camera::new(&view)?;
        let y0 = y_start.min(view.height);
        let y1 = y_end.min(view.height).max(y0);
        let expected = ((y1 - y0) as usize) * (view.width as usize);
        if out_hdr.len() < expected || out_gbuf.len() < expected || out_depth.len() < expected {
            return Err(PictureError::TooManyPixels { pixels: expected as u64, most: out_hdr.len() as u64 });
        }

        let ctx = ShadeContext::from_colors(&cam, self.bvh.scene_bounds, params.fallback_colour, params.background, true);
        let use_shading_normals = matches!(params.normal_source, MeshNormalSource::Shading);
        let mut stats = PictureStats::default();

        let spp_side = usize::from(params.supersample);
        let spp = spp_side * spp_side;
        let inv_spp = 1.0 / (spp as f64);

        // Thin-lens Depth of Field parameters (metres).
        let lens_r = (pbr.aperture_radius_mm * 0.001).max(0.0);
        let eye_v = Vec3::new(view.eye[0].as_m(), view.eye[1].as_m(), view.eye[2].as_m());
        let tgt_v = Vec3::new(view.target[0].as_m(), view.target[1].as_m(), view.target[2].as_m());
        let focus_dist = if pbr.focus_distance_m > 1e-3 {
            pbr.focus_distance_m
        } else {
            (tgt_v - eye_v).length().max(0.05)
        };
        let dof_active = lens_r > 1e-6 && spp > 1 && matches!(view.projection, crate::Projection::Perspective { .. });

        let mut out_idx = 0usize;
        for y in y0..y1 {
            for x in 0..view.width {
                let mut sum = [0.0f64; 3];
                let mut g_alb = [0.0f64; 3];
                let mut g_nrm = [0.0f64; 3];
                let mut g_pres = 0.0f64;
                let mut hit_g_count = 0usize;
                let mut best_t = f64::INFINITY;
                let mut depth_sum = 0.0f64;

                let pix_hash = hash2(x as i64, y as i64, 0x3f2d);
                let pix_hash2 = hash2(x as i64, y as i64, 0x91a7);

                for si in 0..spp {
                    let seed = (y.wrapping_mul(1973) ^ x.wrapping_mul(9277) ^ ((si as u32).wrapping_mul(26699))) as i64;
                    let (ox, oy) = if spp == 1 {
                        (0.5, 0.5)
                    } else if spp == 4 {
                        RGSS_2X2[si]
                    } else {
                        // Stratified jittered grid for 3×3 (9 SPP) or 4×4 (16 SPP).
                        let gx = (si % spp_side) as f64;
                        let gy = (si / spp_side) as f64;
                        let jx = (pix_hash + (si as f64) * 0.7548776662).fract();
                        let jy = (pix_hash2 + (si as f64) * 0.5698402909).fract();
                        ((gx + 0.15 + 0.70 * jx) / (spp_side as f64), (gy + 0.15 + 0.70 * jy) / (spp_side as f64))
                    };

                    let (mut origin, mut dir) = cam.ray_at(f64::from(x) + ox, f64::from(y) + oy);
                    if dof_active {
                        // Stratified concentric disk thin-lens bokeh sampling.
                        let u1 = ((si as f64 + 0.5) / (spp as f64) + pix_hash * 0.25).fract();
                        let u2 = (pix_hash2 + (si as f64) * 0.6180339887).fract();
                        let r_d = blaze_math::sqrt(u1.clamp(0.0, 1.0)) * lens_r;
                        let ang_d = u2 * core::f64::consts::TAU;
                        let offset = cam.right * (r_d * blaze_math::cos(ang_d)) + cam.up * (r_d * blaze_math::sin(ang_d));
                        let denom = dir.dot(cam.forward).max(1e-4);
                        let p_focus = origin + dir * (focus_dist / denom);
                        origin = origin + offset;
                        dir = (p_focus - origin).normalized().unwrap_or(dir);
                    }

                    stats.rays += 1;
                    if let Some(hit) = self.bvh.trace(origin, dir, f64::INFINITY, &mut stats) {
                        stats.hits += 1;
                        best_t = best_t.min(hit.t);
                        let tri = &self.bvh.tris[hit.tri_idx as usize];
                        let p = origin + dir * hit.t;
                        let w = (1.0 - hit.u - hit.v).max(0.0);
                        let mut n = if use_shading_normals {
                            (tri.n0 * w + tri.n1 * hit.u + tri.n2 * hit.v).normalized().unwrap_or(tri.ng)
                        } else {
                            tri.ng
                        };
                        if n.dot(dir) > 0.0 {
                            n = n * -1.0;
                        }
                        let ng_face = if tri.ng.dot(dir) > 0.0 { tri.ng * -1.0 } else { tri.ng };
                        let (mut col, gp) = shade_mesh_pbr_hdr_gbuf(
                            self.bvh,
                            &ctx,
                            params,
                            pbr,
                            p,
                            dir,
                            n,
                            ng_face,
                            tri,
                            w,
                            hit.u,
                            hit.v,
                            pbr.reflection_bounces,
                            seed,
                            &mut stats,
                        );
                        // Optional outdoor aerial perspective atmospheric haze.
                        if pbr.atmosphere_haze > 1e-6 && hit.t > 2.0 {
                            let haze = 1.0 - blaze_math::exp(-(hit.t - 2.0) * pbr.atmosphere_haze);
                            let hdir = Vec3::new(dir.x, dir.y, 0.025).normalized().unwrap_or(dir);
                            let hcol = sample_environment(pbr.environment, hdir, Vec3::new(0.0, 0.0, 1.0));
                            for c in 0..3 {
                                col[c] = col[c] * (1.0 - haze) + hcol[c] * haze;
                            }
                        }
                        sum[0] += col[0];
                        sum[1] += col[1];
                        sum[2] += col[2];
                        g_alb[0] += f64::from(gp.albedo[0]);
                        g_alb[1] += f64::from(gp.albedo[1]);
                        g_alb[2] += f64::from(gp.albedo[2]);
                        g_nrm[0] += f64::from(gp.normal[0]);
                        g_nrm[1] += f64::from(gp.normal[1]);
                        g_nrm[2] += f64::from(gp.normal[2]);
                        g_pres += f64::from(gp.preserve);
                        depth_sum += hit.t;
                        hit_g_count += 1;
                    } else {
                        let (bg, miss_t, gp) = shade_pbr_miss_hdr(self.bvh, &ctx, params, pbr, origin, dir, seed, &mut stats);
                        best_t = best_t.min(miss_t);
                        sum[0] += bg[0];
                        sum[1] += bg[1];
                        sum[2] += bg[2];
                        if miss_t.is_finite() {
                            g_alb[0] += f64::from(gp.albedo[0]);
                            g_alb[1] += f64::from(gp.albedo[1]);
                            g_alb[2] += f64::from(gp.albedo[2]);
                            g_nrm[0] += f64::from(gp.normal[0]);
                            g_nrm[1] += f64::from(gp.normal[1]);
                            g_nrm[2] += f64::from(gp.normal[2]);
                            g_pres += f64::from(gp.preserve);
                            depth_sum += miss_t;
                            hit_g_count += 1;
                        }
                    }
                }

                out_hdr[out_idx] = [sum[0] * inv_spp, sum[1] * inv_spp, sum[2] * inv_spp];
                out_depth[out_idx] = best_t;
                out_gbuf[out_idx] = if hit_g_count > 0 {
                    let inv_g = 1.0 / (hit_g_count as f64);
                    let nv = Vec3::new(g_nrm[0], g_nrm[1], g_nrm[2]).normalized().unwrap_or(Vec3::new(0.0, 0.0, 1.0));
                    GBufferPixel {
                        albedo: [(g_alb[0] * inv_g) as f32, (g_alb[1] * inv_g) as f32, (g_alb[2] * inv_g) as f32],
                        normal: [nv.x as f32, nv.y as f32, nv.z as f32],
                        depth: depth_sum * inv_g,
                        preserve: (g_pres * inv_g) as f32,
                    }
                } else {
                    GBufferPixel::SKY
                };
                out_idx += 1;
            }
        }

        Ok(stats)
    }
}

/// Finish a full linear HDR + G-buffer PBR frame (produced by one or more parallel calls to
/// [`PreparedMeshScene::render_pbr_rows`]): runs edge-preserving À-Trous denoising (when
/// `pbr.denoise` is true), dual-radius HDR optical bloom, subtle vignette, and AgX/ACES tone
/// mapping.
#[allow(clippy::too_many_arguments)]
pub fn finish_pbr_frame<'a>(
    hdr: &mut [[f64; 3]],
    gbuf: &[GBufferPixel],
    depth: Vec<f64, Alloc<'a>>,
    width: u32,
    height: u32,
    pbr: &PbrOpts,
    stats: PictureStats,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Picture<'a>, PictureError> {
    if pbr.denoise {
        denoise_atrous(hdr, gbuf, width, height, DenoiseOpts::DEFAULT, scratch)?;
    }
    let pixels = post_process_hdr(
        hdr,
        width,
        height,
        PostProcessOpts {
            tonemap: pbr.tonemap,
            exposure: pbr.exposure,
            bloom_threshold: pbr.bloom_threshold,
            bloom_strength: pbr.bloom_strength,
            vignette: pbr.vignette,
        },
        alloc,
        scratch,
    )?;
    Ok(Picture { width, height, pixels, depth, stats })
}

/// [`render_mesh_scene`] with a planted bug, for the test controls.
pub fn render_mesh_scene_planted<'a>(
    parts: &[MeshScenePart<'_>],
    materials: &[Material<'_>],
    view: View,
    o: MeshPictureOpts<'a, '_, '_>,
    plant: impl Planted<MeshPicturePlant>,
) -> Result<Picture<'a>, PictureError> {
    let is_pbr_fast = matches!((o.mode, o.shading, o.wireframe), (RenderMode::Shaded, Shading::Pbr(_), Wireframe::Off))
        && (plant.has(MeshPicturePlant::IgnoreHighlights) || o.highlights.is_empty())
        && !plant.has(MeshPicturePlant::FlatOnly);

    if is_pbr_fast {
        if !(1..=4).contains(&o.supersample) {
            return Err(PictureError::BadSupersample { supersample: o.supersample });
        }
    } else if o.supersample != 1 && o.supersample != 2 {
        return Err(PictureError::BadSupersample { supersample: o.supersample });
    }
    let cam = Camera::new(&view)?;
    let n_pixels = u64::from(view.width) * u64::from(view.height);
    if n_pixels > o.most_pixels {
        return Err(PictureError::TooManyPixels { pixels: n_pixels, most: o.most_pixels });
    }
    let n_usize = usize::try_from(n_pixels).map_err(|_| PictureError::TooManyPixels { pixels: n_pixels, most: o.most_pixels })?;

    let bvh = build_bvh(parts, materials, o.fallback_colour, o.scratch, plant)?;

    // Dedicated full-HDR PBR path: routes through `PreparedMeshScene::render_pbr_rows` +
    // `finish_pbr_frame` (G-buffer À-Trous denoising + dual-radius HDR bloom + AgX/ACES)!
    if is_pbr_fast && let Shading::Pbr(pbr) = o.shading {
        let scene = PreparedMeshScene { bvh };
        let mut hdr_buf: Vec<[f64; 3], Alloc<'_>> = Vec::try_with_capacity_in(n_usize, o.scratch)?;
        let mut gbuf: Vec<GBufferPixel, Alloc<'_>> = Vec::try_with_capacity_in(n_usize, o.scratch)?;
        let mut depth: Vec<f64, Alloc<'a>> = Vec::try_with_capacity_in(n_usize, o.alloc)?;
        for _ in 0..n_usize {
            push(&mut hdr_buf, [0.0; 3])?;
            push(&mut gbuf, GBufferPixel::SKY)?;
            push(&mut depth, f64::INFINITY)?;
        }
        let stats = scene.render_pbr_rows(view, &o, &pbr, 0, view.height, &mut hdr_buf, &mut gbuf, &mut depth)?;
        return finish_pbr_frame(&mut hdr_buf, &gbuf, depth, view.width, view.height, &pbr, stats, o.alloc, o.scratch);
    }

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
                } else if let (RenderMode::Shaded, Shading::Pbr(pbr)) = (o.mode, o.shading)
                    && (pbr.ground_plane || pbr.env_background)
                {
                    let seed = (y.wrapping_mul(1973) ^ x.wrapping_mul(9277)) as i64;
                    let params = PbrRenderParams::from_mesh_opts(&o);
                    let (sample, miss_t) = shade_pbr_miss(bvh.as_ref(), &ctx, &params, &pbr, origin, dir, seed, &mut stats);
                    let rgba = resolve_samples(sample, 1.0, ctx.linear_blend);
                    pixels.try_extend_from_slice(&rgba)?;
                    push(&mut depth, miss_t)?;
                } else {
                    pixels.try_extend_from_slice(&o.background)?;
                    push(&mut depth, f64::INFINITY)?;
                }
            } else {
                let mut sum = [0.0f64; 4];
                let mut best_t = f64::INFINITY;
                let mut any_hit = false;
                for (si, &(ox, oy)) in RGSS_2X2.iter().enumerate() {
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
                    } else if let (RenderMode::Shaded, Shading::Pbr(pbr)) = (o.mode, o.shading)
                        && (pbr.ground_plane || pbr.env_background)
                    {
                        let seed = (y.wrapping_mul(1973) ^ x.wrapping_mul(9277) ^ ((si as u32).wrapping_mul(26699))) as i64;
                        let params = PbrRenderParams::from_mesh_opts(&o);
                        let (bg, _) = shade_pbr_miss(bvh.as_ref(), &ctx, &params, &pbr, origin, dir, seed, &mut stats);
                        for c in 0..4 {
                            sum[c] += bg[c];
                        }
                    } else {
                        let bg = ctx.miss_sample();
                        for c in 0..4 {
                            sum[c] += bg[c];
                        }
                    }
                }
                let pbr_miss_active = matches!((o.mode, o.shading), (RenderMode::Shaded, Shading::Pbr(p)) if p.ground_plane || p.env_background);
                let rgba = if any_hit || pbr_miss_active { resolve_samples(sum, 0.25, ctx.linear_blend) } else { o.background };
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
    uv: [[f32; 2]; 3],
    tan: [Vec3; 3],
    tan_w: f64,
    col: [[f64; 4]; 3],
    has_uv: bool,
    has_tan: bool,
    has_col: bool,
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
    clearcoat: f64,
    clearcoat_roughness: f64,
    anisotropy: f64,
    sheen_colour: [f64; 3],
    sheen_roughness: f64,
    transmission: f64,
    ior: f64,
    role_idx: u32,
    part_idx: u32,
    has_tint_override: bool,
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

/// Compact area-light proxy extracted deterministically from emissive mesh clusters for
/// noise-free Next-Event Estimation (NEE) direct lighting.
#[derive(Clone, Copy)]
struct EmissiveLight {
    pos: Vec3,
    /// Total radiant flux (`emissive * area`, scaled for inverse-square PBR evaluation).
    power: [f64; 3],
    /// Approximate bounding radius of the emissive cluster (prevents inverse-square singularity
    /// and skips self-shadowing against the emitter's own surface).
    radius: f64,
}

struct MeshBvh<'s> {
    nodes: Vec<BvhNode, Alloc<'s>>,
    tris: Vec<WorldTri, Alloc<'s>>,
    inv_xforms: Vec<Affine3, Alloc<'s>>,
    scene_bounds: Aabb3,
    emissive_lights: [EmissiveLight; 12],
    emissive_count: usize,
}

/// Borrowed, `Copy + Send + Sync` read-only view of [`MeshBvh`].
#[derive(Clone, Copy)]
struct MeshBvhRef<'a> {
    nodes: &'a [BvhNode],
    tris: &'a [WorldTri],
    scene_bounds: Aabb3,
    emissive_lights: [EmissiveLight; 12],
    emissive_count: usize,
}

impl MeshBvh<'_> {
    #[inline]
    fn as_ref(&self) -> MeshBvhRef<'_> {
        MeshBvhRef {
            nodes: &self.nodes,
            tris: &self.tris,
            scene_bounds: self.scene_bounds,
            emissive_lights: self.emissive_lights,
            emissive_count: self.emissive_count,
        }
    }
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
        let uvs_opt = view.uvs();
        let tans_opt = view.tangents();
        let cols_opt = view.colours();
        let roles = view.roles();
        let has_nrms = nrms.len() == pos.len();

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

            let (uv, has_uv) = match uvs_opt {
                Some(u) => ([u[i0], u[i1], u[i2]], true),
                None => ([[0.0; 2]; 3], false),
            };
            let (tan, tan_w, has_tan) = match tans_opt {
                Some(ts) => {
                    let t0 = mul_mat3(&xf.m, [ts[i0][0], ts[i0][1], ts[i0][2]]).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
                    let t1 = mul_mat3(&xf.m, [ts[i1][0], ts[i1][1], ts[i1][2]]).normalized().unwrap_or(t0);
                    let t2 = mul_mat3(&xf.m, [ts[i2][0], ts[i2][1], ts[i2][2]]).normalized().unwrap_or(t0);
                    let w = if (ts[i0][3] < 0.0) ^ mirrored { -1.0 } else { 1.0 };
                    ([t0, t1, t2], w, true)
                }
                None => ([Vec3::new(1.0, 0.0, 0.0); 3], 1.0, false),
            };
            let (col, has_col) = match cols_opt {
                Some(cs) => {
                    let dec = |c: [u8; 4]| [f64::from(c[0]) / 255.0, f64::from(c[1]) / 255.0, f64::from(c[2]) / 255.0, f64::from(c[3]) / 255.0];
                    ([dec(cs[i0]), dec(cs[i1]), dec(cs[i2])], true)
                }
                None => ([[1.0, 0.5, 1.0, 0.5]; 3], false),
            };

            let len_bc = (c - b).length().max(LEAST_STEP);
            let len_ca = e2.length().max(LEAST_STEP);
            let len_ab = e1.length().max(LEAST_STEP);
            let alt_scale = [twice_area / len_bc, twice_area / len_ca, twice_area / len_ab];

            let role_idx = roles.get(ti).map(|r| r.0).unwrap_or(0);
            let (
                albedo_linear,
                albedo_srgb,
                metallic,
                roughness,
                emissive,
                clearcoat,
                clearcoat_roughness,
                anisotropy,
                sheen_colour,
                sheen_roughness,
                transmission,
                ior,
            ) = if let Some(rgb) = part.tint_override {
                let srgb = [f64::from(rgb[0]) / 255.0, f64::from(rgb[1]) / 255.0, f64::from(rgb[2]) / 255.0];
                let lin = [srgb_to_linear(rgb[0]), srgb_to_linear(rgb[1]), srgb_to_linear(rgb[2])];
                (lin, srgb, 0.15, 0.55, [0.0; 3], 0.0, 0.1, 0.0, [0.0; 3], 0.5, 0.0, 1.5)
            } else if let Some(mat) = materials.get(role_idx as usize) {
                let lin = [
                    mat.base_colour[0].clamp(0.0, 1.0),
                    mat.base_colour[1].clamp(0.0, 1.0),
                    mat.base_colour[2].clamp(0.0, 1.0),
                ];
                let srgb = [linear_to_srgb(lin[0]), linear_to_srgb(lin[1]), linear_to_srgb(lin[2])];
                let em_scale = mat.emissive_strength.unwrap_or(1.0).max(0.0);
                let em = [mat.emissive[0] * em_scale, mat.emissive[1] * em_scale, mat.emissive[2] * em_scale];
                let (cc, cc_r) = mat.clearcoat.map(|c| (c.factor.clamp(0.0, 1.0), c.roughness.clamp(0.04, 1.0))).unwrap_or((0.0, 0.1));
                let aniso = mat.anisotropy.map(|a| a.strength.clamp(-0.95, 0.95)).unwrap_or(0.0);
                let (sc, sr) = mat.sheen.map(|s| (s.colour, s.roughness.clamp(0.04, 1.0))).unwrap_or(([0.0; 3], 0.5));
                let (tr, ior) = mat.transmission.map(|t| (t.factor.clamp(0.0, 1.0), t.ior.max(1.0))).unwrap_or((0.0, 1.5));
                (lin, srgb, mat.metallic.clamp(0.0, 1.0), mat.roughness.clamp(0.04, 1.0), em, cc, cc_r, aniso, sc, sr, tr, ior)
            } else {
                (fb_linear, fb_srgb, 0.2, 0.5, [0.0; 3], 0.0, 0.1, 0.0, [0.0; 3], 0.5, 0.0, 1.5)
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
                uv,
                tan,
                tan_w,
                col,
                has_uv,
                has_tan,
                has_col,
                alt_scale,
                albedo_linear,
                albedo_srgb,
                metallic,
                roughness,
                emissive,
                clearcoat,
                clearcoat_roughness,
                anisotropy,
                sheen_colour,
                sheen_roughness,
                transmission,
                ior,
                role_idx,
                part_idx: part_idx as u32,
                has_tint_override: part.tint_override.is_some(),
                bounds: Aabb3::new(lo, hi),
                centroid,
            })?;
        }
    }

    if tris.is_empty() {
        let zero_light = EmissiveLight { pos: Vec3::ZERO, power: [0.0; 3], radius: 0.05 };
        return Ok(MeshBvh {
            nodes: Vec::new_in(scratch),
            tris,
            inv_xforms,
            scene_bounds: Aabb3::EMPTY,
            emissive_lights: [zero_light; 12],
            emissive_count: 0,
        });
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

    // Deterministically cluster emissive triangles into up to 12 local area lights for noise-free NEE.
    let mut clusters_pos = [Vec3::ZERO; 12];
    let mut clusters_pow = [[0.0f64; 3]; 12];
    let mut clusters_area = [0.0f64; 12];
    let mut clusters_bounds = [Aabb3::EMPTY; 12];
    let mut cluster_n = 0usize;
    let merge_dist = (root_bounds.hi - root_bounds.lo).length() * 0.08;
    let merge_r2 = (merge_dist.clamp(0.18, 0.55)).powi(2);

    for t in tris.iter() {
        let emax = t.emissive[0].max(t.emissive[1]).max(t.emissive[2]);
        if emax <= 0.25 {
            continue;
        }
        let area = 0.5 * t.e1.cross(t.e2).length();
        if area <= 1e-9 {
            continue;
        }
        // Find nearest existing cluster within merge radius.
        let mut target = None;
        let mut best_d2 = merge_r2;
        for ci in 0..cluster_n {
            let cur_c = clusters_pos[ci] * (1.0 / clusters_area[ci].max(1e-9));
            let d = t.centroid - cur_c;
            let d2 = d.dot(d);
            if d2 < best_d2 {
                best_d2 = d2;
                target = Some(ci);
            }
        }
        let ci = if let Some(idx) = target {
            idx
        } else if cluster_n < 12 {
            let idx = cluster_n;
            cluster_n += 1;
            idx
        } else {
            // Merge into nearest cluster when >12 clusters exist.
            let mut nearest = 0usize;
            let mut nd2 = f64::INFINITY;
            for ci in 0..12 {
                let cur_c = clusters_pos[ci] * (1.0 / clusters_area[ci].max(1e-9));
                let d = t.centroid - cur_c;
                let d2 = d.dot(d);
                if d2 < nd2 {
                    nd2 = d2;
                    nearest = ci;
                }
            }
            nearest
        };
        clusters_pos[ci] = clusters_pos[ci] + t.centroid * area;
        clusters_area[ci] += area;
        clusters_bounds[ci] = union_aabb(&clusters_bounds[ci], &t.bounds);
        for c in 0..3 {
            clusters_pow[ci][c] += t.emissive[c] * area;
        }
    }

    let zero_light = EmissiveLight { pos: Vec3::ZERO, power: [0.0; 3], radius: 0.05 };
    let mut emissive_lights = [zero_light; 12];
    for ci in 0..cluster_n {
        let inv_a = 1.0 / clusters_area[ci].max(1e-9);
        let pos = clusters_pos[ci] * inv_a;
        let rad = ((clusters_bounds[ci].hi - clusters_bounds[ci].lo).length() * 0.5).max(0.06);
        emissive_lights[ci] = EmissiveLight { pos, power: clusters_pow[ci], radius: rad };
    }

    Ok(MeshBvh { nodes, tris, inv_xforms, scene_bounds: root_bounds, emissive_lights, emissive_count: cluster_n })
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
    #[inline]
    fn trace(&self, origin: Vec3, dir: Vec3, best_t: f64, stats: &mut PictureStats) -> Option<MeshHit> {
        self.as_ref().trace(origin, dir, best_t, stats)
    }

    #[inline]
    fn occluded(&self, origin: Vec3, dir: Vec3, max_t: f64, stats: &mut PictureStats) -> bool {
        self.as_ref().occluded(origin, dir, max_t, stats)
    }
}

impl MeshBvhRef<'_> {
    fn trace(self, origin: Vec3, dir: Vec3, mut best_t: f64, stats: &mut PictureStats) -> Option<MeshHit> {
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

    fn occluded(self, origin: Vec3, dir: Vec3, max_t: f64, stats: &mut PictureStats) -> bool {
        self.occluded_ex(origin, dir, max_t, false, stats)
    }

    /// Shadow-ray occlusion test that lets clear refractive glass (`transmission > 0.35`) transmit
    /// direct sunlight and local emissive light into interiors, cockpits, and optical barrels.
    fn occluded_opaque(self, origin: Vec3, dir: Vec3, max_t: f64, stats: &mut PictureStats) -> bool {
        self.occluded_ex(origin, dir, max_t, true, stats)
    }

    fn occluded_ex(self, origin: Vec3, dir: Vec3, max_t: f64, skip_glass: bool, stats: &mut PictureStats) -> bool {
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
                    let tri = &self.tris[idx];
                    if skip_glass && tri.transmission > 0.35 {
                        continue;
                    }
                    stats.points += 1;
                    if intersect_tri(origin, dir, tri, max_t).is_some() {
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
    if tri.has_uv
        && let Some(maps) = material_maps.get(tri.role_idx as usize)
    {
        let uv_u = f64::from(tri.uv[0][0]) * w + f64::from(tri.uv[1][0]) * hit.u + f64::from(tri.uv[2][0]) * hit.v;
        let uv_v = f64::from(tri.uv[0][1]) * w + f64::from(tri.uv[1][1]) * hit.u + f64::from(tri.uv[2][1]) * hit.v;
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
            && tri.has_tan
        {
            let t_raw = tri.tan[0] * w + tri.tan[1] * hit.u + tri.tan[2] * hit.v;
            if let Some(t_dir) = (t_raw - n * t_raw.dot(n)).normalized() {
                let b_dir = n.cross(t_dir) * tri.tan_w;
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
                Shading::Pbr(pbr) => {
                    let seed = ((sy as i64).wrapping_mul(1973) ^ (sx as i64).wrapping_mul(9277)) as i64;
                    let params = PbrRenderParams::from_mesh_opts(o);
                    let hdr = shade_mesh_pbr_hdr(
                        bvh.as_ref(),
                        ctx,
                        &params,
                        &pbr,
                        p,
                        dir,
                        n,
                        ng_face,
                        tri,
                        w,
                        hit.u,
                        hit.v,
                        pbr.reflection_bounces,
                        seed,
                        stats,
                    );
                    let mapped = apply_tonemap(hdr, pbr.exposure, pbr.tonemap);
                    [mapped[0], mapped[1], mapped[2], 1.0]
                }
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

// ---- Photoreal PBR (GGX + HDRI IBL + CC0 Textures + SDF Smart Materials + Bounces) -------

/// Deterministic Vogel-disk 8-point offsets for soft area shadows (zero RNG, zero banding).
const VOGEL_DISK_8: [[f64; 2]; 8] = [
    [0.25000000, 0.00000000],
    [-0.31932325, 0.29245940],
    [0.04885020, -0.55671855],
    [0.44949576, 0.48477474],
    [-0.72858899, -0.07291663],
    [0.54166402, -0.60619752],
    [-0.00797848, 0.87584050],
    [-0.64096839, -0.71315826],
];

/// Evaluate deterministic soft area shadow visibility in `[0, 1]` toward `light_dir`,
/// rotated per sample via a deterministic pixel seed so supersampled penumbras have zero banding.
#[allow(clippy::too_many_arguments)]
fn mesh_soft_shadow(
    bvh: MeshBvhRef<'_>,
    p: Vec3,
    ng: Vec3,
    light_dir: Vec3,
    angular_radius: f64,
    bias: f64,
    max_t: f64,
    samples: u8,
    seed: i64,
    stats: &mut PictureStats,
) -> f64 {
    if samples == 0 {
        return 1.0;
    }
    let origin = p + ng * bias;
    if samples == 1 || angular_radius <= 1e-5 {
        return if bvh.occluded_opaque(origin, light_dir, max_t, stats) { 0.0 } else { 1.0 };
    }
    let helper = if light_dir.z.abs() < 0.9 { Vec3::new(0.0, 0.0, 1.0) } else { Vec3::new(1.0, 0.0, 0.0) };
    let u = helper.cross(light_dir).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
    let v = light_dir.cross(u);
    let ang = hash2(seed, 17, 0x51a3) * core::f64::consts::TAU;
    let ca = blaze_math::cos(ang);
    let sa = blaze_math::sin(ang);
    let n = usize::from(samples).min(VOGEL_DISK_8.len());
    let mut lit = 0usize;
    for d in &VOGEL_DISK_8[..n] {
        let rx = (d[0] * ca - d[1] * sa) * angular_radius;
        let ry = (d[0] * sa + d[1] * ca) * angular_radius;
        let dir = (light_dir + u * rx + v * ry).normalized().unwrap_or(light_dir);
        if !bvh.occluded_opaque(origin, dir, max_t, stats) {
            lit += 1;
        }
    }
    (lit as f64) / (n as f64)
}

/// Deterministic 1-bounce BVH hemisphere global illumination + directional sky visibility +
/// indirect emissive lighting (using Cranley-Patterson rotated cosine-weighted hemisphere rays).
///
/// Returns `(diffuse_irradiance, sky_visibility)`:
/// - Open rays sample the true directional HDRI sky (`sample_environment`).
/// - Occluded rays bounce hit-surface emissive light (e.g. warm hearth windows & cyan ion
///   thrusters lighting nearby wood/stone/nozzles!) plus 1-bounce sunlit diffuse color bleeding.
#[allow(clippy::too_many_arguments)]
fn eval_bvh_gi(
    bvh: MeshBvhRef<'_>,
    pbr: &PbrOpts,
    p: Vec3,
    n: Vec3,
    ng: Vec3,
    bias: f64,
    max_t: f64,
    n_rays: u8,
    seed: i64,
    world_up: Vec3,
    dom: blaze_pbr::DominantLight,
    stats: &mut PictureStats,
) -> ([f64; 3], f64) {
    let count = usize::from(n_rays).min(12);
    if count == 0 {
        return ([0.0; 3], 1.0);
    }
    let helper = if n.z.abs() < 0.9 { Vec3::new(0.0, 0.0, 1.0) } else { Vec3::new(1.0, 0.0, 0.0) };
    let t_ax = helper.cross(n).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
    let b_ax = n.cross(t_ax);

    let rot = hash2(seed, 31, 0x9e37) * core::f64::consts::TAU;
    let r_jitter = hash2(seed, 53, 0x7f4a);
    let golden = 2.399963229728653; // pi * (3 - sqrt(5))
    let origin = p + ng * bias;
    let ao_reach = max_t * 0.28;

    let mut irr = [0.0f64; 3];
    let mut vis_sum = 0.0f64;
    let mut weight_sum = 0.0f64;

    for i in 0..count {
        // Stratified cosine-weighted hemisphere sample via Malley's disk projection.
        let r2 = ((i as f64) + 0.15 + 0.70 * ((r_jitter + (i as f64) * 0.6180339887).fract())) / (count as f64);
        let r = blaze_math::sqrt(r2.clamp(0.002, 0.985));
        let phi = rot + (i as f64) * golden;
        let lx = r * blaze_math::cos(phi);
        let ly = r * blaze_math::sin(phi);
        let lz = blaze_math::sqrt((1.0 - r * r).max(0.001));
        let wi = (t_ax * lx + b_ax * ly + n * lz).normalized().unwrap_or(n);
        if wi.dot(ng) <= 0.015 {
            continue;
        }
        weight_sum += 1.0;
        stats.rays += 1;
        if let Some(hit) = bvh.trace(origin, wi, max_t, stats) {
            let tri = &bvh.tris[hit.tri_idx as usize];
            if tri.transmission > 0.35 {
                // Clear glass transmits soft sky irradiance into interiors and cockpits!
                let sky = sample_environment(pbr.environment, wi, world_up);
                let tr = (tri.transmission * 0.85).clamp(0.0, 0.9);
                for c in 0..3 {
                    irr[c] += sky[c] * tr;
                }
                vis_sum += tr;
                continue;
            }
            let mut hn = tri.ng;
            if hn.dot(wi) > 0.0 {
                hn = hn * -1.0;
            }
            // Fast 1-bounce diffuse color bleeding from hit triangle (emissive lights are handled
            // noise-free via deterministic Next-Event Estimation `eval_emissive_nee`!).
            let ndotl = hn.dot(dom.direction).max(0.0);
            let sky_fill = 0.22 + 0.28 * (0.5 + 0.5 * hn.dot(world_up));
            let bounce_scale = (1.0 - tri.metallic * 0.7) * 0.85;
            for c in 0..3 {
                let direct_b = dom.colour[c] * pbr.direct_intensity * ndotl * 0.24;
                let amb_b = 0.35 * sky_fill * pbr.ibl_intensity;
                let bounced = (tri.albedo_linear[c] * bounce_scale * (direct_b + amb_b)).min(3.0);
                irr[c] += bounced;
            }
            // Distance-weighted sky visibility for smooth contact AO under eaves & crevices.
            let dist_open = smoothstep_f64(0.0, ao_reach, hit.t) * 0.35;
            vis_sum += dist_open;
        } else {
            let sky = sample_environment(pbr.environment, wi, world_up);
            for c in 0..3 {
                irr[c] += sky[c];
            }
            vis_sum += 1.0;
        }
    }

    if weight_sum <= 0.0 {
        return ([0.0; 3], 1.0);
    }
    let inv = 1.0 / weight_sum;
    let raw_vis = (vis_sum * inv).clamp(0.0, 1.0);
    let ao_vis = (1.0 - pbr.ao_strength) + pbr.ao_strength * raw_vis;
    ([irr[0] * inv, irr[1] * inv, irr[2] * inv], ao_vis)
}

/// Noise-free Next-Event Estimation (NEE) from clustered emissive meshes (e.g. warm brass porch
/// lantern lighting cedar walls & oak doors, or cyan ion thrusters illuminating thruster nozzles
/// and glossy hangar decks).
#[allow(clippy::too_many_arguments)]
fn eval_emissive_nee(
    bvh: MeshBvhRef<'_>,
    surf: &SurfacePbr,
    p: Vec3,
    n: Vec3,
    ng: Vec3,
    t_vec: Vec3,
    b_vec: Vec3,
    view_v: Vec3,
    bias: f64,
    stats: &mut PictureStats,
) -> [f64; 3] {
    let mut out = [0.0f64; 3];
    if bvh.emissive_count == 0 {
        return out;
    }
    let origin = p + ng * bias;
    for light in &bvh.emissive_lights[..bvh.emissive_count] {
        let to_l = light.pos - p;
        let d2 = to_l.dot(to_l);
        let d = blaze_math::sqrt(d2);
        if d <= light.radius * 0.65 {
            continue;
        }
        let l_dir = to_l * (1.0 / d);
        if n.dot(l_dir) <= 1e-4 || ng.dot(l_dir) <= 0.0 {
            continue;
        }
        let atten = 1.0 / (4.0 * core::f64::consts::PI * (d2 + light.radius * light.radius * 0.65));
        let max_rad = (light.power[0].max(light.power[1]).max(light.power[2])) * atten;
        if max_rad < 4e-3 {
            continue;
        }
        let max_t = (d - light.radius * 0.92 - bias).max(bias);
        if max_t > bias && bvh.occluded_opaque(origin, l_dir, max_t, stats) {
            continue;
        }
        let brdf = eval_direct_brdf(surf, n, t_vec, b_vec, view_v, l_dir);
        // Scale radiant flux slightly so real-world-sized bulbs/thrusters cast natural visible pools.
        let gain = 3.2 * atten;
        out[0] += brdf[0] * light.power[0] * gain;
        out[1] += brdf[1] * light.power[1] * gain;
        out[2] += brdf[2] * light.power[2] * gain;
    }
    out
}

/// Shade a primary miss ray in linear HDR: environment-matched PBR ground (alpine stone/meadow
/// terrain, glossy paneled hangar floor with real aircraft reflections, or photographic studio
/// sweep) fading smoothly into the procedural HDRI environment & atmospheric horizon.
#[allow(clippy::too_many_arguments)]
fn shade_pbr_miss_hdr(
    bvh: MeshBvhRef<'_>,
    ctx: &ShadeContext,
    o: &PbrRenderParams<'_>,
    pbr: &PbrOpts,
    origin: Vec3,
    dir: Vec3,
    seed: i64,
    stats: &mut PictureStats,
) -> ([f64; 3], f64, GBufferPixel) {
    let world_up = Vec3::new(0.0, 0.0, 1.0);
    let env_hdr = if pbr.env_background {
        sample_environment(pbr.environment, dir, world_up)
    } else {
        [ctx.bg_linear[0], ctx.bg_linear[1], ctx.bg_linear[2]]
    };

    if pbr.ground_plane && !bvh.scene_bounds.is_empty() && dir.z < -1e-6 {
        let z_floor = bvh.scene_bounds.lo.z - (ctx.scene_diag * 1e-4).max(1e-5);
        let t = (z_floor - origin.z) / dir.z;
        if t > LEAST_STEP {
            let p = origin + dir * t;
            let cx = 0.5 * (bvh.scene_bounds.lo.x + bvh.scene_bounds.hi.x);
            let cy = 0.5 * (bvh.scene_bounds.lo.y + bvh.scene_bounds.hi.y);
            let dx = p.x - cx;
            let dy = p.y - cy;
            let dist = blaze_math::sqrt(dx * dx + dy * dy);
            let max_r = ctx.scene_diag * 14.0;
            // Smooth horizon atmosphere target (slightly above z=0 so floor dissolves seamlessly into sky haze, zero dark band!).
            let horizon_dir = Vec3::new(dir.x, dir.y, 0.018).normalized().unwrap_or(dir);
            let horizon_hdr = if pbr.env_background {
                sample_environment(pbr.environment, horizon_dir, world_up)
            } else {
                [ctx.bg_linear[0], ctx.bg_linear[1], ctx.bg_linear[2]]
            };
            if dist < max_r {
                let fade = 1.0 - smoothstep_f64(max_r * 0.16, max_r, dist);
                let mut n = world_up;
                let t_ax = Vec3::new(1.0, 0.0, 0.0);
                let b_ax = Vec3::new(0.0, 1.0, 0.0);
                let v = dir * -1.0;
                let bias = (ctx.scene_diag * 2e-4).max(1e-4);
                let dom = dominant_light(pbr.environment, world_up);

                // Procedural environment-matched PBR ground material!
                let (ground_albedo, ground_rough, ground_reflect) = match pbr.environment {
                    EnvironmentPreset::GoldenHour | EnvironmentPreset::OvercastSky => {
                        // Photographic alpine mountain clearing: mossy dark forest loam, dry pine
                        // needles, weathered meadow grass tufts, and crushed slate drainage gravel.
                        let macro_n = fbm3(Vec3::new(p.x * 0.14, p.y * 0.14, 0.4), 4);
                        let mid_n = fbm3(Vec3::new(p.x * 0.72 + 2.3, p.y * 0.72 - 1.7, 1.2), 4);
                        let fine_n = fbm3(Vec3::new(p.x * 4.8, p.y * 4.8, 2.7), 3);
                        let pebbles = fbm3(Vec3::new(p.x * 16.0, p.y * 16.0, 4.1), 2);
                        let apron = 1.0 - smoothstep_f64(ctx.scene_diag * 0.30, ctx.scene_diag * 0.62, dist);
                        let meadow_col = [
                            0.048 + 0.045 * macro_n + 0.028 * mid_n + 0.014 * fine_n,
                            0.072 + 0.056 * macro_n + 0.025 * fine_n,
                            0.028 + 0.018 * macro_n,
                        ];
                        let loam_col = [
                            0.082 + 0.042 * mid_n + 0.025 * fine_n,
                            0.058 + 0.032 * mid_n + 0.018 * fine_n,
                            0.036 + 0.018 * mid_n,
                        ];
                        let gravel_col = [
                            0.115 + 0.055 * pebbles + 0.032 * fine_n,
                            0.110 + 0.050 * pebbles + 0.030 * fine_n,
                            0.102 + 0.045 * pebbles + 0.025 * fine_n,
                        ];
                        let w_loam = smoothstep_f64(0.36, 0.72, mid_n * 0.7 + macro_n * 0.3);
                        let forest_col = [
                            meadow_col[0] + (loam_col[0] - meadow_col[0]) * w_loam,
                            meadow_col[1] + (loam_col[1] - meadow_col[1]) * w_loam,
                            meadow_col[2] + (loam_col[2] - meadow_col[2]) * w_loam,
                        ];
                        let w_g = (apron * (0.50 + 0.50 * mid_n)).clamp(0.0, 1.0);
                        let col = [
                            forest_col[0] + (gravel_col[0] - forest_col[0]) * w_g,
                            forest_col[1] + (gravel_col[1] - forest_col[1]) * w_g,
                            forest_col[2] + (gravel_col[2] - forest_col[2]) * w_g,
                        ];
                        // Crisp multi-scale raking micro-bump so low golden-hour sun catches soil & gravel!
                        let gx = (fbm3(Vec3::new((p.x + 0.045) * 3.6, p.y * 3.6, 0.9), 3)
                            - fbm3(Vec3::new((p.x - 0.045) * 3.6, p.y * 3.6, 0.9), 3))
                            + 0.45 * (fbm3(Vec3::new((p.x + 0.015) * 14.0, p.y * 14.0, 2.2), 2)
                                - fbm3(Vec3::new((p.x - 0.015) * 14.0, p.y * 14.0, 2.2), 2));
                        let gy = (fbm3(Vec3::new(p.x * 3.6, (p.y + 0.045) * 3.6, 0.9), 3)
                            - fbm3(Vec3::new(p.x * 3.6, (p.y - 0.045) * 3.6, 0.9), 3))
                            + 0.45 * (fbm3(Vec3::new(p.x * 14.0, (p.y + 0.015) * 14.0, 2.2), 2)
                                - fbm3(Vec3::new(p.x * 14.0, (p.y - 0.015) * 14.0, 2.2), 2));
                        n = Vec3::new(-gx * 0.48, -gy * 0.48, 1.0).normalized().unwrap_or(world_up);
                        let rough = (0.88 - 0.16 * w_g + 0.08 * fine_n).clamp(0.62, 0.97);
                        (col, rough, 0.0)
                    }
                    EnvironmentPreset::HangarBay => {
                        // Polished dark concrete/epoxy aerospace hangar deck with crisp 2.0m expansion joints!
                        let tile = 1.8;
                        let fx = (p.x / tile).rem_euclid(1.0);
                        let fy = (p.y / tile).rem_euclid(1.0);
                        let seam = ((fx.min(1.0 - fx) / 0.012).min(fy.min(1.0 - fy) / 0.012)).clamp(0.0, 1.0);
                        let wear = fbm3(Vec3::new(p.x * 0.65, p.y * 0.65, 0.3), 4);
                        let micro = fbm3(Vec3::new(p.x * 3.8, p.y * 3.8, 1.7), 2);
                        let base_v = (0.72 + 0.38 * wear) * (0.45 + 0.55 * seam);
                        let col = [
                            pbr.ground_colour[0] * base_v,
                            pbr.ground_colour[1] * base_v * 1.02,
                            pbr.ground_colour[2] * base_v * 1.06,
                        ];
                        let rough = (0.24 + 0.20 * (1.0 - wear) + 0.06 * micro + 0.35 * (1.0 - seam)).clamp(0.16, 0.85);
                        (col, rough, 0.42 * seam)
                    }
                    EnvironmentPreset::StudioSoftbox => {
                        // Sleek satin commercial studio sweep floor with subtle contact-hardening reflection.
                        let subtle = 0.95 + 0.08 * fbm3(Vec3::new(p.x * 0.6, p.y * 0.6, 0.2), 2);
                        let col = [
                            pbr.ground_colour[0] * subtle,
                            pbr.ground_colour[1] * subtle,
                            pbr.ground_colour[2] * subtle,
                        ];
                        (col, 0.24, 0.34)
                    }
                };

                let (gi_irr, sky_vis) = eval_bvh_gi(
                    bvh,
                    pbr,
                    p,
                    n,
                    world_up,
                    bias,
                    ctx.scene_diag * 2.2,
                    pbr.ao_steps.min(8),
                    seed,
                    world_up,
                    dom,
                    stats,
                );
                let shadow = mesh_soft_shadow(
                    bvh,
                    p,
                    world_up,
                    dom.direction,
                    dom.angular_radius,
                    bias,
                    ctx.scene_diag * 3.5,
                    pbr.shadow_samples,
                    seed,
                    stats,
                );
                let ground_surf = SurfacePbr {
                    base_colour: ground_albedo,
                    metallic: 0.0,
                    roughness: ground_rough,
                    ao: sky_vis,
                    ..SurfacePbr::DEFAULT
                };
                let direct = eval_direct_brdf(&ground_surf, n, t_ax, b_ax, v, dom.direction);
                let mut ibl = eval_ibl_with_irradiance(
                    pbr.environment,
                    &ground_surf,
                    n,
                    t_ax,
                    b_ax,
                    v,
                    world_up,
                    pbr.ibl_intensity,
                    if pbr.ao_steps > 0 { Some(gi_irr) } else { None },
                );

                // VNDF rough GGX glossy floor reflection on Hangar/Studio floors: sharp at contact,
                // naturally softening with distance just like real concrete and studio floors!
                if ground_reflect > 0.05 && pbr.reflection_bounces > 0 {
                    let ndotv = n.dot(v).clamp(1e-4, 1.0);
                    let u1 = hash2(seed, 71, 0x4b1d);
                    let u2 = hash2(seed, 89, 0x8c3a);
                    let r_dir = sample_ggx_vndf(v, n, t_ax, b_ax, (ground_rough * 0.62).clamp(0.04, 0.45), 0.0, u1, u2);
                    if r_dir.z > 0.01 {
                        stats.rays += 1;
                        if let Some(rhit) = bvh.trace(p + world_up * bias, r_dir, ctx.scene_diag * 3.0, stats) {
                            let rtri = &bvh.tris[rhit.tri_idx as usize];
                            let rp = p + world_up * bias + r_dir * rhit.t;
                            let rw = (1.0 - rhit.u - rhit.v).max(0.0);
                            let mut rn = (rtri.n0 * rw + rtri.n1 * rhit.u + rtri.n2 * rhit.v).normalized().unwrap_or(rtri.ng);
                            if rn.dot(r_dir) > 0.0 {
                                rn = rn * -1.0;
                            }
                            let rng = if rtri.ng.dot(r_dir) > 0.0 { rtri.ng * -1.0 } else { rtri.ng };
                            let rcol = shade_mesh_pbr_hdr(
                                bvh,
                                ctx,
                                o,
                                pbr,
                                rp,
                                r_dir,
                                rn,
                                rng,
                                rtri,
                                rw,
                                rhit.u,
                                rhit.v,
                                0,
                                seed ^ 0x2a7,
                                stats,
                            );
                            let fres = fresnel_schlick_roughness(ndotv, [0.05; 3], ground_rough)[0];
                            let dist_fade = (1.0 - smoothstep_f64(0.0, ctx.scene_diag * 1.4, rhit.t)).max(0.15);
                            let rw_blend = (ground_reflect * (0.35 + 1.8 * fres) * dist_fade).clamp(0.0, 0.65);
                            for c in 0..3 {
                                ibl[c] = ibl[c] * (1.0 - rw_blend * 0.5) + rcol[c] * rw_blend;
                            }
                        }
                    }
                }

                let nee = eval_emissive_nee(bvh, &ground_surf, p, n, world_up, t_ax, b_ax, v, bias, stats);
                let floor_hdr = [
                    direct[0] * dom.colour[0] * pbr.direct_intensity * shadow + ibl[0] + nee[0],
                    direct[1] * dom.colour[1] * pbr.direct_intensity * shadow + ibl[1] + nee[1],
                    direct[2] * dom.colour[2] * pbr.direct_intensity * shadow + ibl[2] + nee[2],
                ];
                let blended = [
                    horizon_hdr[0] + (floor_hdr[0] - horizon_hdr[0]) * fade,
                    horizon_hdr[1] + (floor_hdr[1] - horizon_hdr[1]) * fade,
                    horizon_hdr[2] + (floor_hdr[2] - horizon_hdr[2]) * fade,
                ];
                let gp = GBufferPixel {
                    albedo: [ground_albedo[0] as f32, ground_albedo[1] as f32, ground_albedo[2] as f32],
                    normal: [n.x as f32, n.y as f32, n.z as f32],
                    depth: t,
                    preserve: if fade < 0.25 { 0.85 } else { 0.0 },
                };
                return (blended, t, gp);
            }
            return (horizon_hdr, f64::INFINITY, GBufferPixel::SKY);
        }
    }

    (env_hdr, f64::INFINITY, GBufferPixel::SKY)
}

#[allow(clippy::too_many_arguments)]
fn shade_pbr_miss(
    bvh: MeshBvhRef<'_>,
    ctx: &ShadeContext,
    o: &PbrRenderParams<'_>,
    pbr: &PbrOpts,
    origin: Vec3,
    dir: Vec3,
    seed: i64,
    stats: &mut PictureStats,
) -> ([f64; 4], f64) {
    let (hdr, t, _) = shade_pbr_miss_hdr(bvh, ctx, o, pbr, origin, dir, seed, stats);
    if pbr.env_background || t.is_finite() {
        let mapped = apply_tonemap(hdr, pbr.exposure, pbr.tonemap);
        ([mapped[0], mapped[1], mapped[2], 1.0], t)
    } else {
        (ctx.bg_linear, f64::INFINITY)
    }
}

/// Evaluate linear HDR radiance at a mesh hit using `blaze-pbr`.
#[allow(clippy::too_many_arguments)]
fn shade_mesh_pbr_hdr(
    bvh: MeshBvhRef<'_>,
    ctx: &ShadeContext,
    o: &PbrRenderParams<'_>,
    pbr: &PbrOpts,
    p: Vec3,
    dir: Vec3,
    n_base: Vec3,
    ng: Vec3,
    tri: &WorldTri,
    w: f64,
    u: f64,
    v_bary: f64,
    bounces_left: u8,
    seed: i64,
    stats: &mut PictureStats,
) -> [f64; 3] {
    shade_mesh_pbr_hdr_gbuf(bvh, ctx, o, pbr, p, dir, n_base, ng, tri, w, u, v_bary, bounces_left, seed, stats).0
}

/// Evaluate linear HDR radiance + G-buffer pixel guidance (`albedo`, `normal`, `preserve`) at a
/// mesh hit using `blaze-pbr` (Cook-Torrance GGX + VNDF rough reflections + 1-bounce BVH GI +
/// emissive NEE + CC0 textures + SDF smart-material wear/grime + closed-solid glass pass-through).
#[allow(clippy::too_many_arguments)]
fn shade_mesh_pbr_hdr_gbuf(
    bvh: MeshBvhRef<'_>,
    ctx: &ShadeContext,
    o: &PbrRenderParams<'_>,
    pbr: &PbrOpts,
    p: Vec3,
    dir: Vec3,
    n_base: Vec3,
    ng: Vec3,
    tri: &WorldTri,
    w: f64,
    u: f64,
    v_bary: f64,
    bounces_left: u8,
    seed: i64,
    stats: &mut PictureStats,
) -> ([f64; 3], GBufferPixel) {
    let world_up = Vec3::new(0.0, 0.0, 1.0);
    let view_v = dir * -1.0;

    // Build orthonormal tangent frame `(t_vec, b_vec, n_base)`.
    let (t_vec, b_vec) = if tri.has_tan {
        let t_interp = tri.tan[0] * w + tri.tan[1] * u + tri.tan[2] * v_bary;
        let t_orth = (t_interp - n_base * n_base.dot(t_interp)).normalized().unwrap_or_else(|| {
            let helper = if n_base.z.abs() < 0.9 { Vec3::new(0.0, 0.0, 1.0) } else { Vec3::new(1.0, 0.0, 0.0) };
            helper.cross(n_base).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0))
        });
        (t_orth, n_base.cross(t_orth) * tri.tan_w)
    } else {
        let helper = if n_base.z.abs() < 0.9 { Vec3::new(0.0, 0.0, 1.0) } else { Vec3::new(1.0, 0.0, 0.0) };
        let t_orth = helper.cross(n_base).normalized().unwrap_or(Vec3::new(1.0, 0.0, 0.0));
        (t_orth, n_base.cross(t_orth))
    };

    // Start from the material's scalar PBR factors.
    let mut surf = SurfacePbr {
        base_colour: tri.albedo_linear,
        metallic: tri.metallic,
        roughness: tri.roughness,
        ao: 1.0,
        emissive: tri.emissive,
        clearcoat: tri.clearcoat,
        clearcoat_roughness: tri.clearcoat_roughness,
        anisotropy: tri.anisotropy,
        sheen_colour: tri.sheen_colour,
        sheen_roughness: tri.sheen_roughness,
        transmission: tri.transmission,
        ior: tri.ior,
    };

    let mut n = n_base;
    let uv = [
        f64::from(tri.uv[0][0]) * w + f64::from(tri.uv[1][0]) * u + f64::from(tri.uv[2][0]) * v_bary,
        f64::from(tri.uv[0][1]) * w + f64::from(tri.uv[1][1]) * u + f64::from(tri.uv[2][1]) * v_bary,
    ];

    // 1. Sample optional CC0 PBR texture pack + optional per-part baked SDF detail normal map.
    let mut combined_tn: Option<[f64; 3]> = None;
    if let Some(Some(part_nmap)) = o.part_normal_maps.get(tri.part_idx as usize)
        && tri.has_uv
    {
        let s = sample_bilinear(part_nmap, uv);
        combined_tn = Some(decode_tangent_normal(s, 1.0));
    }

    if let Some(look) = o.material_looks.get(tri.role_idx as usize) {
        let pack = &look.textures;
        let scaled_uv = [uv[0] * pack.uv_scale[0], uv[1] * pack.uv_scale[1]];
        let sample_map = |tex: &TextureView<'_>| -> [f64; 4] {
            if tri.has_uv {
                if pack.hex_tile { sample_hex_tile(tex, scaled_uv, 0.45) } else { sample_bilinear(tex, scaled_uv) }
            } else {
                sample_triplanar(tex, p, n_base, 1.0 / pack.uv_scale[0].max(1e-3), pack.hex_tile)
            }
        };
        if let Some(albedo_tex) = &pack.albedo {
            let s = sample_map(albedo_tex);
            // glTF 2.0: linear baseColorFactor * sRGB-decoded baseColorTexture.
            surf.base_colour = [
                pbr_srgb_to_linear(s[0]) * tri.albedo_linear[0],
                pbr_srgb_to_linear(s[1]) * tri.albedo_linear[1],
                pbr_srgb_to_linear(s[2]) * tri.albedo_linear[2],
            ];
        }
        if let Some(orm_tex) = &pack.orm {
            let s = sample_map(orm_tex);
            surf.ao *= s[0].clamp(0.0, 1.0);
            surf.roughness = (s[1] * tri.roughness).clamp(0.04, 1.0);
            surf.metallic = (s[2] * tri.metallic).clamp(0.0, 1.0);
        }
        if let Some(norm_tex) = &pack.normal {
            let s = sample_map(norm_tex);
            let detail_tn = decode_tangent_normal(s, pack.normal_scale);
            combined_tn = Some(match combined_tn {
                Some(base_tn) => rnm_blend(base_tn, detail_tn),
                None => detail_tn,
            });
        }
        if let Some(tn) = combined_tn {
            n = perturb_normal(n_base, t_vec, b_vec, tn);
            // Keep perturbed normal in the front-facing hemisphere so grazing pixels never black out.
            if n.dot(view_v) < 0.02 {
                n = (n + view_v * (0.04 - n.dot(view_v))).normalized().unwrap_or(n_base);
            }
        }

        // 2. Apply SDF-mask smart-material layering (edge wear, crevice grime, upward dust, macro breakup).
        let masks = if tri.has_col {
            let c0 = tri.col[0];
            let c1 = tri.col[1];
            let c2 = tri.col[2];
            SurfaceMasks {
                ao: c0[0] * w + c1[0] * u + c2[0] * v_bary,
                curvature: c0[1] * w + c1[1] * u + c2[1] * v_bary,
                cavity: c0[2] * w + c1[2] * u + c2[2] * v_bary,
                height: c0[3] * w + c1[3] * u + c2[3] * v_bary,
            }
        } else {
            SurfaceMasks::NEUTRAL
        };
        surf = apply_smart_material(surf, masks, p, n, world_up, &look.smart, pbr.instance_wear);
    } else if let Some(tn) = combined_tn {
        n = perturb_normal(n_base, t_vec, b_vec, tn);
    } else if tri.has_col {
        // Even without an explicit SmartMaterial, honor baked SDF AO in `COLOR_0.r`.
        let baked_ao = tri.col[0][0] * w + tri.col[1][0] * u + tri.col[2][0] * v_bary;
        surf.ao *= baked_ao.clamp(0.0, 1.0);
    }

    // 3. Direct dominant light + 1-bounce BVH hemisphere GI (directional sky visibility,
    //    color bleeding, and indirect emissive light!) combined with baked SDF & texture AO.
    let bias = (ctx.scene_diag * 2e-4).max(LEAST_STEP * 100.0);
    let dom = dominant_light(pbr.environment, world_up);
    let micro_ao = surf.ao.clamp(0.0, 1.0);
    let gi_steps = if bounces_left == pbr.reflection_bounces {
        pbr.ao_steps
    } else if bounces_left + 1 == pbr.reflection_bounces {
        // Enclosed interiors & optical barrels seen through glass still need true occlusion/GI
        // so interior rooms stay warmly enclosed instead of leaking unoccluded outdoor sky!
        pbr.ao_steps.min(3)
    } else {
        0
    };
    let gi_override = if gi_steps > 0 {
        let (gi_irr, rt_ao) = eval_bvh_gi(
            bvh,
            pbr,
            p,
            n,
            ng,
            bias,
            ctx.scene_diag * 2.2,
            gi_steps,
            seed,
            world_up,
            dom,
            stats,
        );
        surf.ao = (micro_ao * rt_ao).clamp(0.0, 1.0);
        Some([gi_irr[0] * micro_ao, gi_irr[1] * micro_ao, gi_irr[2] * micro_ao])
    } else {
        None
    };

    // 4. Direct dominant light with Cranley-Patterson rotated Vogel-disk soft area shadows.
    let shadow = if n.dot(dom.direction) > 0.0 {
        let s_samples = if bounces_left == pbr.reflection_bounces { pbr.shadow_samples } else { pbr.shadow_samples.min(1) };
        mesh_soft_shadow(bvh, p, ng, dom.direction, dom.angular_radius, bias, ctx.scene_diag * 2.5, s_samples, seed, stats)
    } else {
        0.0
    };
    let direct = eval_direct_brdf(&surf, n, t_vec, b_vec, view_v, dom.direction);

    // 5. Environment IBL (1-bounce GI diffuse + prefiltered GGX specular + clearcoat + sheen).
    let mut ibl = eval_ibl_with_irradiance(
        pbr.environment,
        &surf,
        n,
        t_vec,
        b_vec,
        view_v,
        world_up,
        pbr.ibl_intensity,
        gi_override,
    );

    // 6. Real BVH secondary ray bounces for glossy/metallic reflections & refractive glass canopies!
    if bounces_left > 0 {
        let ndotv = n.dot(view_v).clamp(1e-4, 1.0);
        let f = fresnel_schlick_roughness(ndotv, surf.f0(), surf.roughness);

        // Glossy / metallic / clearcoat VNDF GGX rough reflection bounce (contact hardening!).
        let spec_strength = (1.0 - surf.roughness).max(surf.clearcoat * (1.0 - surf.clearcoat_roughness));
        if spec_strength > 0.30 {
            let eff_r = if surf.clearcoat > 0.35 && surf.clearcoat_roughness < surf.roughness {
                surf.clearcoat_roughness
            } else {
                surf.roughness
            };
            let u1 = hash2(seed, 101, 0x6c4e);
            let u2 = hash2(seed, 137, 0x3b91);
            let r_dir = if eff_r < 0.06 {
                (n * (2.0 * ndotv) - view_v).normalized().unwrap_or(n)
            } else {
                sample_ggx_vndf(view_v, n, t_vec, b_vec, eff_r * 0.72, surf.anisotropy, u1, u2)
            };
            if r_dir.dot(ng) > 0.02 {
                stats.rays += 1;
                if let Some(rhit) = bvh.trace(p + ng * bias, r_dir, ctx.scene_diag * 3.0, stats) {
                    let rtri = &bvh.tris[rhit.tri_idx as usize];
                    let rp = p + ng * bias + r_dir * rhit.t;
                    let rw = (1.0 - rhit.u - rhit.v).max(0.0);
                    let mut rn = (rtri.n0 * rw + rtri.n1 * rhit.u + rtri.n2 * rhit.v).normalized().unwrap_or(rtri.ng);
                    if rn.dot(r_dir) > 0.0 {
                        rn = rn * -1.0;
                    }
                    let rng = if rtri.ng.dot(r_dir) > 0.0 { rtri.ng * -1.0 } else { rtri.ng };
                    let bounce_col = shade_mesh_pbr_hdr(
                        bvh,
                        ctx,
                        o,
                        pbr,
                        rp,
                        r_dir,
                        rn,
                        rng,
                        rtri,
                        rw,
                        rhit.u,
                        rhit.v,
                        bounces_left - 1,
                        seed ^ 0x5c3,
                        stats,
                    );
                    let blend = ((spec_strength - 0.32) / 0.68).clamp(0.0, 0.85);
                    for c in 0..3 {
                        ibl[c] = ibl[c] * (1.0 - blend * f[c]) + bounce_col[c] * blend * f[c];
                    }
                }
            }
        }

        // Physical dielectric transmission bounce (sees interior cockpit / cabin rooms through
        // closed-solid CAD glass slabs by stepping cleanly past the glass exit face!).
        if surf.transmission > 0.05 {
            let eta = 1.0 / surf.ior.max(1.0);
            let k = (1.0 - eta * eta * (1.0 - ndotv * ndotv)).max(0.0);
            let refract_dir = ((view_v * -eta) + n * (eta * ndotv - blaze_math::sqrt(k))).normalized().unwrap_or(dir);
            let mut t_origin = p - ng * (bias * 2.0);
            let mut ray_dir = refract_dir;
            let mut behind_opt: Option<[f64; 3]> = None;
            // Allow stepping across up to 2 transmissive glass boundaries (entry -> exit face of solid pane!)
            for _step in 0..3 {
                stats.rays += 1;
                if let Some(thit) = bvh.trace(t_origin, ray_dir, ctx.scene_diag * 3.0, stats) {
                    let ttri = &bvh.tris[thit.tri_idx as usize];
                    let tp = t_origin + ray_dir * thit.t;
                    if ttri.transmission > 0.05 && ttri.part_idx == tri.part_idx {
                        // Exiting the back face of the same solid CAD glass pane: continue straight into the interior!
                        t_origin = tp + ray_dir * (bias * 3.0);
                        ray_dir = dir;
                        continue;
                    }
                    let tw = (1.0 - thit.u - thit.v).max(0.0);
                    let mut tn = (ttri.n0 * tw + ttri.n1 * thit.u + ttri.n2 * thit.v).normalized().unwrap_or(ttri.ng);
                    if tn.dot(ray_dir) > 0.0 {
                        tn = tn * -1.0;
                    }
                    let tng = if ttri.ng.dot(ray_dir) > 0.0 { ttri.ng * -1.0 } else { ttri.ng };
                    behind_opt = Some(shade_mesh_pbr_hdr(
                        bvh,
                        ctx,
                        o,
                        pbr,
                        tp,
                        ray_dir,
                        tn,
                        tng,
                        ttri,
                        tw,
                        thit.u,
                        thit.v,
                        bounces_left - 1,
                        seed ^ 0x9b1,
                        stats,
                    ));
                    break;
                } else {
                    break;
                }
            }
            if let Some(behind) = behind_opt {
                let tr = surf.transmission.clamp(0.0, 1.0);
                for c in 0..3 {
                    let wt = (1.0 - f[c]) * tr;
                    ibl[c] = ibl[c] * (1.0 - wt * 0.90) + behind[c] * surf.base_colour[c] * wt;
                }
            }
        }
    }

    // 7. Noise-free Next-Event Estimation from local emissive meshes (porch lanterns, interior
    //    Edison lamps visible through glass windows, cockpit HUDs, and cyan ion cores).
    let nee = if bounces_left + 1 >= pbr.reflection_bounces {
        eval_emissive_nee(bvh, &surf, p, n, ng, t_vec, b_vec, view_v, bias, stats)
    } else {
        [0.0; 3]
    };

    let hdr = [
        (direct[0] * dom.colour[0] * pbr.direct_intensity * shadow + ibl[0] + nee[0] + surf.emissive[0]).max(0.0),
        (direct[1] * dom.colour[1] * pbr.direct_intensity * shadow + ibl[1] + nee[1] + surf.emissive[1]).max(0.0),
        (direct[2] * dom.colour[2] * pbr.direct_intensity * shadow + ibl[2] + nee[2] + surf.emissive[2]).max(0.0),
    ];
    let emax = surf.emissive[0].max(surf.emissive[1]).max(surf.emissive[2]);
    let preserve = if emax > 0.4 || surf.transmission > 0.25 {
        0.95
    } else if surf.roughness < 0.14 && (surf.metallic > 0.6 || surf.clearcoat > 0.7) {
        0.65
    } else {
        0.0
    };
    let gp = GBufferPixel {
        albedo: [surf.base_colour[0] as f32, surf.base_colour[1] as f32, surf.base_colour[2] as f32],
        normal: [n.x as f32, n.y as f32, n.z as f32],
        depth: 0.0, // filled with primary hit.t by caller
        preserve,
    };
    (hdr, gp)
}

#[inline]
fn smoothstep_f64(edge0: f64, edge1: f64, x: f64) -> f64 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
