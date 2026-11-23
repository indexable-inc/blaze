//! Camera-aware fidelity checks ([`check_camera_fidelity`]) and saved game-camera previews &
//! side-by-side revision comparisons rendered directly from actual exported `.glb` bytes
//! ([`GameCameraRig`], [`render_glb_views`], [`compare_glb_revisions`]).
//!
//! # Priority 12: Camera-Aware Fidelity
//!
//! World-space millimetre tolerances alone miss failures that depend on where the player looks:
//! a 1.5 mm chord error at 30 cm fills two screen pixels on a silhouette, a thin panel seam
//! disappears when its projected width drops below 1 px or loses its mesh triangles on a coarser
//! LOD, and a decal or stencil breaks up when its foreshortened stroke width drops below ~1.25 px
//! or its texel density falls below screen resolution. [`check_camera_fidelity`] projects every
//! LOD mesh through exact [`Camera`] frustums and checks all three deterministically.
//!
//! # Priority 14: Saved Game-Camera Previews & Revision Diffs
//!
//! [`GameCameraRig::standard_views`] derives a scale-invariant 6-camera inspection rig
//! (`Aiming`, `Carried`, `Inspect`, `HeroThreeQuarter`, `TopOrthographic`, `ZebraInspect`) from
//! any asset's bounding box. [`render_glb_views`] parses the actual shipped `.glb` bytes via
//! `blaze_cad_out::read_glb` (preserving node transforms, PBR materials, and embedded PNG texture
//! maps), renders every camera to RGBA8 + PNG, and composes a 2-column contact sheet.
//! [`compare_glb_revisions`] renders two `.glb` revisions from the same saved cameras and builds
//! quantitative `[Previous | Current | Diff]` revision strips and a combined comparison sheet.

#![blaze::sans_io]

use blaze_cad_field::{Name, Placement, Scratch, Tape, eval_point};
use blaze_cad_mesh::{FeatureRegion, MeshView};
use blaze_cad_out::{GlbReadError, GlbReadOpts, Limits, LoadedGlb, Material, PngOpts, Transform, Up, read_glb};
use blaze_cad_picture::{
    Camera, MaterialMaps, MeshPictureOpts, MeshScenePart, Picture, PictureError, Projection, RenderMode, Shading, StripLight,
    StudioOpts, TextureMap, View, ZebraOpts, compose_grid, compose_side_by_side, diff_pictures, render_mesh_scene,
};
use blaze_math::{Aabb3, Vec3};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, Vec};
use blaze_text::{Put, Show, Writes};
use blaze_units::{Angle, Length};

use crate::ModelError;

/// Grazing-silhouette cosine threshold (`|n · v| < 0.25`, ~75.5° from view vector).
const GRAZING_SILHOUETTE_COS: f64 = 0.25;

/// Minimum facing cosine (`n · v > 0.15`, ~81.4° from view vector) before a decal is judged facing
/// the camera.
const MIN_DECAL_FACING_COS: f64 = 0.15;

/// Contact-sheet gutter spacing in pixels between panels.
const CONTACT_SHEET_GAP_PX: u32 = 8;

/// Dark neutral studio background behind contact-sheet and comparison-strip gutters.
const CONTACT_SHEET_BG: [u8; 4] = [28, 32, 38, 255];

/// Per-channel delta threshold (`0..=255`) above which a pixel counts as changed in
/// [`compare_glb_revisions`].
const REVISION_DIFF_THRESHOLD: u8 = 2;

/// Heatmap amplification multiplier used by [`compare_glb_revisions`].
const REVISION_DIFF_AMPLIFY: f64 = 4.0;

// =============================================================================================
// Priority 12: Camera-Aware Fidelity Checks
// =============================================================================================

/// A named inspection or gameplay camera view.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct NamedCamera {
    pub name: Name,
    pub view: View,
}

impl NamedCamera {
    /// Construct a [`NamedCamera`] from a display label and [`View`].
    #[inline]
    pub fn new(name: &str, view: View) -> NamedCamera {
        NamedCamera { name: Name::new(name), view }
    }
}

/// Physical specification of a panel seam, shadow gap, or groove checked for screen-space
/// visibility across LOD meshes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SeamSpec {
    pub name: Name,
    pub region: FeatureRegion,
    pub width: Length,
    pub depth: Length,
}

impl SeamSpec {
    /// Construct a [`SeamSpec`] in the part's local coordinate frame.
    #[inline]
    pub fn new(name: &str, region: FeatureRegion, width: Length, depth: Length) -> SeamSpec {
        SeamSpec { name: Name::new(name), region, width, depth }
    }
}

/// Physical specification of a surface decal, stencil, or engraved marking checked for
/// camera-relative stroke legibility and texel density.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DecalSpec {
    pub name: Name,
    pub center: Vec3,
    pub normal: Vec3,
    pub stroke_width: Length,
    pub texels_per_metre: Option<f64>,
}

impl DecalSpec {
    /// Construct a [`DecalSpec`] in the part's local coordinate frame.
    #[inline]
    pub fn new(name: &str, center: Vec3, normal: Vec3, stroke_width: Length, texels_per_metre: Option<f64>) -> DecalSpec {
        DecalSpec { name: Name::new(name), center, normal, stroke_width, texels_per_metre }
    }
}

/// Planted bugs for [`check_camera_fidelity_planted`], caught by the test controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CameraFidelityPlant {
    None,
    /// Skips silhouette-edge shift checks (misses chord-error silhouette popping).
    IgnoreSilhouetteShift,
    /// Marks every checked seam visible even when sub-pixel or missing mesh triangles.
    IgnoreSeamLoss,
    /// Marks every checked decal legible even when foreshortened or under-sampled.
    IgnoreDecalBreakup,
}

impl Plants for CameraFidelityPlant {}

/// Options for [`check_camera_fidelity`] and [`check_camera_fidelity_planted`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct CameraFidelityOpts<'a, 's, 'c> {
    pub cameras: &'c [NamedCamera],
    pub seams: &'c [SeamSpec] = &[],
    pub decals: &'c [DecalSpec] = &[],
    pub max_silhouette_shift_px: f64 = 0.75,
    pub min_seam_px: f64 = 1.0,
    pub min_decal_stroke_px: f64 = 1.25,
    pub min_decal_texel_ratio: f64 = 0.75,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// A silhouette edge whose exact CAD surface offset exceeds the camera's pixel budget.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SilhouetteShift {
    pub camera: Name,
    pub lod: u32,
    pub world_pos: [Length; 3],
    pub pixel: [f64; 2],
    pub shift_px: f64,
    pub offset_m: Length,
}

/// Screen-space visibility result for one [`SeamSpec`] on one `(camera, lod)` pair.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SeamVisibility {
    pub camera: Name,
    pub lod: u32,
    pub seam: Name,
    pub projected_width_px: f64,
    pub resolved_triangles: u32,
    pub visible: bool,
}

/// Screen-space legibility result for one [`DecalSpec`] on one `(camera, lod)` pair.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DecalLegibility {
    pub camera: Name,
    pub lod: u32,
    pub decal: Name,
    pub projected_stroke_px: f64,
    pub texels_per_screen_px: Option<f64>,
    pub facing_cos: f64,
    pub legible: bool,
}

/// Summary row for one `(camera, lod)` pair in [`CameraFidelityReport`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CameraLodReport {
    pub camera: Name,
    pub lod: u32,
    pub triangles: u32,
    pub max_silhouette_shift_px: f64,
    pub worst_silhouette: Option<SilhouetteShift>,
    pub seams_lost: u32,
    pub decals_broken: u32,
    pub passed: bool,
}

/// Complete camera-aware fidelity report across all requested cameras and LOD meshes.
#[derive(Debug)]
pub struct CameraFidelityReport<'a> {
    pub lod_reports: Vec<CameraLodReport, Alloc<'a>>,
    pub silhouette_issues: Vec<SilhouetteShift, Alloc<'a>>,
    pub seam_checks: Vec<SeamVisibility, Alloc<'a>>,
    pub decal_checks: Vec<DecalLegibility, Alloc<'a>>,
    pub passed: bool,
}

impl Show for CameraFidelityReport<'_> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str("camera\tlod\ttris\tmax_sil_px\tseams_lost\tdecals_broken\tstatus\n");
        for row in self.lod_reports.iter() {
            out.put_str(row.camera.as_str());
            out.put_byte(b'\t');
            out.int(row.lod);
            out.put_byte(b'\t');
            out.int(row.triangles);
            out.put_byte(b'\t');
            put_fixed(out, row.max_silhouette_shift_px, 2);
            out.put_byte(b'\t');
            out.int(row.seams_lost);
            out.put_byte(b'\t');
            out.int(row.decals_broken);
            out.put_byte(b'\t');
            out.put_str(if row.passed { "pass\n" } else { "fail\n" });
        }
        for s in self.seam_checks.iter() {
            if !s.visible {
                out.put_str("seam_lost\t");
                out.put_str(s.camera.as_str());
                out.put_str("\tlod=");
                out.int(s.lod);
                out.put_byte(b'\t');
                out.put_str(s.seam.as_str());
                out.put_str("\twidth_px=");
                put_fixed(out, s.projected_width_px, 2);
                out.put_str("\ttris=");
                out.int(s.resolved_triangles);
                out.put_byte(b'\n');
            }
        }
        for d in self.decal_checks.iter() {
            if !d.legible {
                out.put_str("decal_broken\t");
                out.put_str(d.camera.as_str());
                out.put_str("\tlod=");
                out.int(d.lod);
                out.put_byte(b'\t');
                out.put_str(d.decal.as_str());
                out.put_str("\tstroke_px=");
                put_fixed(out, d.projected_stroke_px, 2);
                if let Some(r) = d.texels_per_screen_px {
                    out.put_str("\ttexels_per_px=");
                    put_fixed(out, r, 2);
                }
                out.put_str("\tfacing=");
                put_fixed(out, d.facing_cos, 2);
                out.put_byte(b'\n');
            }
        }
        out.put_str("summary\tviews=");
        out.int(self.lod_reports.len());
        out.put_str("\tsil_issues=");
        out.int(self.silhouette_issues.len());
        out.put_str("\tseams=");
        out.int(self.seam_checks.len());
        out.put_str("\tdecals=");
        out.int(self.decal_checks.len());
        out.put_byte(b'\t');
        out.put_str(if self.passed { "pass\n" } else { "fail\n" });
    }
}

/// Prepared camera frame paired with `blaze_cad_picture::Camera` (exact same basis and projection
/// equations as `Camera::new` and `Camera::ray_at`).
#[derive(Clone, Copy)]
struct CameraFrame {
    name: Name,
    cam: Camera,
    eye: Vec3,
    forward: Vec3,
    right: Vec3,
    up: Vec3,
    half: f64,
    perspective: bool,
    width: f64,
    height: f64,
}

impl CameraFrame {
    fn new(named: &NamedCamera) -> Result<CameraFrame, ModelError> {
        let v = &named.view;
        let cam = Camera::new(v)?;
        let eye = metres_to_v3(v.eye);
        let forward = (metres_to_v3(v.target) - eye).normalized().ok_or(ModelError::Picture(PictureError::EyeAtTarget))?;
        let hint = Vec3::new(v.up[0], v.up[1], v.up[2]);
        let right = forward.cross(hint).normalized().ok_or(ModelError::Picture(PictureError::UpAlongView { up: v.up }))?;
        let up = right.cross(forward);
        let (half, perspective) = match v.projection {
            Projection::Perspective { fov } => (blaze_math::tan(fov.as_radians() / 2.0), true),
            Projection::Orthographic { height } => (height.as_m() / 2.0, false),
        };
        Ok(CameraFrame {
            name: named.name,
            cam,
            eye,
            forward,
            right,
            up,
            half,
            perspective,
            width: f64::from(v.width),
            height: f64::from(v.height),
        })
    }

    /// Project a world-space point into `(px, py, depth_along_forward)` if it lies strictly in
    /// front of the camera and inside `[0, width] × [0, height]`.
    #[inline]
    fn project_screen(&self, world_p: Vec3) -> Option<(f64, f64, f64)> {
        let rel = world_p - self.eye;
        let depth = rel.dot(self.forward);
        if !(depth > 1e-9 && depth.is_finite()) {
            return None;
        }
        let (sx, sy) = if self.perspective {
            (rel.dot(self.right) / depth, rel.dot(self.up) / depth)
        } else {
            (rel.dot(self.right), rel.dot(self.up))
        };
        let aspect = self.width / self.height;
        let half_w = aspect * self.half;
        let half_h = self.half;
        if !(half_w > 0.0 && half_h > 0.0) {
            return None;
        }
        let ndc_x = sx / half_w;
        let ndc_y = sy / half_h;
        let px = (ndc_x + 1.0) * 0.5 * self.width;
        let py = (1.0 - ndc_y) * 0.5 * self.height;
        if px.is_finite() && py.is_finite() && px >= 0.0 && px <= self.width && py >= 0.0 && py <= self.height {
            Some((px, py, depth))
        } else {
            None
        }
    }

    /// Unit vector from `world_p` toward the camera (or `-forward` for orthographic cameras).
    #[inline]
    fn view_dir_from(&self, world_p: Vec3) -> Option<Vec3> {
        if self.perspective { (self.eye - world_p).normalized() } else { Some(-self.forward) }
    }

    /// World-space pixel footprint (metres per pixel) at `depth` along `forward`.
    #[inline]
    fn pixel_size_at(&self, depth: f64) -> f64 {
        self.cam.tolerance_at(depth, 1.0)
    }
}

/// Canonical bitwise position key so crease-split vertices sharing an exact 3D position are paired
/// as one geometric edge.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PosKey([u32; 3]);

#[inline]
fn pos_key(p: [f32; 3]) -> PosKey {
    let bits = |x: f32| if x == 0.0 { 0.0f32.to_bits() } else { x.to_bits() };
    PosKey([bits(p[0]), bits(p[1]), bits(p[2])])
}

#[derive(Clone, Copy)]
struct HalfEdge {
    lo: PosKey,
    hi: PosKey,
    tri: u32,
    v0: u32,
    v1: u32,
}

/// Evaluate camera-aware silhouette shift, seam visibility, and decal legibility across every
/// `(camera, lod)` pair.
pub fn check_camera_fidelity<'a>(
    tape: &Tape<'_>,
    placement: &Placement,
    lod_meshes: &[MeshView<'_>],
    o: CameraFidelityOpts<'a, '_, '_>,
) -> Result<CameraFidelityReport<'a>, ModelError> {
    check_camera_fidelity_planted(tape, placement, lod_meshes, o, NoPlant)
}

/// [`check_camera_fidelity`] with a planted bug, for the test controls.
pub fn check_camera_fidelity_planted<'a>(
    tape: &Tape<'_>,
    placement: &Placement,
    lod_meshes: &[MeshView<'_>],
    o: CameraFidelityOpts<'a, '_, '_>,
    plant: impl Planted<CameraFidelityPlant>,
) -> Result<CameraFidelityReport<'a>, ModelError> {
    if o.cameras.is_empty() {
        return Err(ModelError::Argument {
            function: "check_camera_fidelity",
            arg: "cameras",
            accepted: "at least one named camera",
        });
    }
    if lod_meshes.is_empty() {
        return Err(ModelError::Argument {
            function: "check_camera_fidelity",
            arg: "lod_meshes",
            accepted: "at least one LOD mesh",
        });
    }
    if !(o.max_silhouette_shift_px > 0.0 && o.max_silhouette_shift_px.is_finite()) {
        return Err(ModelError::Argument {
            function: "check_camera_fidelity",
            arg: "max_silhouette_shift_px",
            accepted: "a finite pixel bound above zero",
        });
    }
    if !(o.min_seam_px >= 0.0 && o.min_seam_px.is_finite()) {
        return Err(ModelError::Argument {
            function: "check_camera_fidelity",
            arg: "min_seam_px",
            accepted: "a finite non-negative pixel width",
        });
    }
    if !(o.min_decal_stroke_px >= 0.0 && o.min_decal_stroke_px.is_finite()) {
        return Err(ModelError::Argument {
            function: "check_camera_fidelity",
            arg: "min_decal_stroke_px",
            accepted: "a finite non-negative pixel stroke width",
        });
    }
    if !(o.min_decal_texel_ratio >= 0.0 && o.min_decal_texel_ratio.is_finite()) {
        return Err(ModelError::Argument {
            function: "check_camera_fidelity",
            arg: "min_decal_texel_ratio",
            accepted: "a finite non-negative texel-to-pixel ratio",
        });
    }

    let s = o.scratch;
    let lin = placement.linear();

    // Validate cameras up front.
    let mut frames: Vec<CameraFrame, Alloc<'_>> = Vec::new_in(s);
    frames
        .try_reserve(o.cameras.len())
        .map_err(|_| ModelError::Alloc { what: "camera fidelity frames" })?;
    for cam in o.cameras {
        frames
            .try_push(CameraFrame::new(cam)?)
            .map_err(|_| ModelError::Alloc { what: "camera fidelity frames" })?;
    }

    // Validate seams and precompute world-space centers.
    let mut seam_world_centers: Vec<Vec3, Alloc<'_>> = Vec::new_in(s);
    seam_world_centers
        .try_reserve(o.seams.len())
        .map_err(|_| ModelError::Alloc { what: "camera fidelity seams" })?;
    for seam in o.seams {
        if seam.width < Length::ZERO || seam.depth < Length::ZERO {
            return Err(ModelError::Argument {
                function: "check_camera_fidelity",
                arg: "seams",
                accepted: "seams with non-negative width and depth",
            });
        }
        let c_local = seam.region.aabb().center();
        if !(c_local.x.is_finite() && c_local.y.is_finite() && c_local.z.is_finite()) {
            return Err(ModelError::Argument {
                function: "check_camera_fidelity",
                arg: "seams.region",
                accepted: "a finite feature region",
            });
        }
        seam_world_centers
            .try_push(placement.apply(c_local))
            .map_err(|_| ModelError::Alloc { what: "camera fidelity seams" })?;
    }

    // Validate decals and precompute world-space centers & unit normals.
    let mut decal_world: Vec<(Vec3, Vec3), Alloc<'_>> = Vec::new_in(s);
    decal_world
        .try_reserve(o.decals.len())
        .map_err(|_| ModelError::Alloc { what: "camera fidelity decals" })?;
    for decal in o.decals {
        if !(decal.center.x.is_finite() && decal.center.y.is_finite() && decal.center.z.is_finite()) {
            return Err(ModelError::Argument {
                function: "check_camera_fidelity",
                arg: "decals.center",
                accepted: "a finite 3D position",
            });
        }
        let Some(local_n) = decal.normal.normalized() else {
            return Err(ModelError::Argument {
                function: "check_camera_fidelity",
                arg: "decals.normal",
                accepted: "a non-zero finite normal vector",
            });
        };
        if decal.stroke_width <= Length::ZERO {
            return Err(ModelError::Argument {
                function: "check_camera_fidelity",
                arg: "decals.stroke_width",
                accepted: "a length above zero",
            });
        }
        if let Some(tpm) = decal.texels_per_metre
            && !(tpm > 0.0 && tpm.is_finite())
        {
            return Err(ModelError::Argument {
                function: "check_camera_fidelity",
                arg: "decals.texels_per_metre",
                accepted: "a positive finite texel density",
            });
        }
        let world_n = lin.apply(local_n).normalized().unwrap_or(local_n);
        decal_world
            .try_push((placement.apply(decal.center), world_n))
            .map_err(|_| ModelError::Alloc { what: "camera fidelity decals" })?;
    }

    let report_cap = frames.len().saturating_mul(lod_meshes.len());
    let mut lod_reports: Vec<CameraLodReport, Alloc<'a>> = Vec::new_in(o.alloc);
    lod_reports
        .try_reserve(report_cap)
        .map_err(|_| ModelError::Alloc { what: "camera fidelity lod reports" })?;
    let mut silhouette_issues: Vec<SilhouetteShift, Alloc<'a>> = Vec::new_in(o.alloc);
    let mut seam_checks: Vec<SeamVisibility, Alloc<'a>> = Vec::new_in(o.alloc);
    let mut decal_checks: Vec<DecalLegibility, Alloc<'a>> = Vec::new_in(o.alloc);

    let ignore_silhouette = plant.has(CameraFidelityPlant::IgnoreSilhouetteShift);
    let ignore_seam = plant.has(CameraFidelityPlant::IgnoreSeamLoss);
    let ignore_decal = plant.has(CameraFidelityPlant::IgnoreDecalBreakup);

    let mut sc = Scratch::new(s);
    let mut tri_normals: Vec<Option<Vec3>, Alloc<'_>> = Vec::new_in(s);
    let mut half_edges: Vec<HalfEdge, Alloc<'_>> = Vec::new_in(s);
    let mut seam_tri_counts: Vec<u32, Alloc<'_>> = Vec::new_in(s);

    // Precompute per-LOD edge adjacency and seam triangle counts, then iterate cameras so output
    // rows follow deterministic `(camera, lod)` order.
    let mut all_passed = true;

    // Scratch storage indexed by `lod_idx * o.seams.len() + seam_idx`.
    let total_seam_slots = lod_meshes.len().saturating_mul(o.seams.len());
    seam_tri_counts
        .try_reserve(total_seam_slots)
        .map_err(|_| ModelError::Alloc { what: "camera fidelity seam triangle counts" })?;
    for mesh in lod_meshes {
        let pos = mesh.positions();
        let tris = mesh.triangles();
        for seam in o.seams {
            let mut count = 0u32;
            for tri in tris {
                let a = p3_to_v3(pos[tri[0] as usize]);
                let b = p3_to_v3(pos[tri[1] as usize]);
                let c = p3_to_v3(pos[tri[2] as usize]);
                if seam.region.intersects_triangle(a, b, c) {
                    count = count.saturating_add(1);
                }
            }
            seam_tri_counts
                .try_push(count)
                .map_err(|_| ModelError::Alloc { what: "camera fidelity seam triangle counts" })?;
        }
    }

    for frame in frames.iter() {
        for (lod_idx, mesh) in lod_meshes.iter().enumerate() {
            let lod_u32 = u32::try_from(lod_idx).unwrap_or(u32::MAX);
            let pos = mesh.positions();
            let tris = mesh.triangles();
            let tri_count = mesh.triangle_count();

            // 1. Silhouette edges: deterministic sorted geometric edge adjacency.
            let mut max_shift_px = 0.0f64;
            let mut worst_sil: Option<SilhouetteShift> = None;

            if !ignore_silhouette && !tris.is_empty() {
                tri_normals.clear();
                tri_normals
                    .try_reserve(tris.len())
                    .map_err(|_| ModelError::Alloc { what: "camera fidelity triangle normals" })?;
                half_edges.clear();
                half_edges
                    .try_reserve(tris.len().saturating_mul(3))
                    .map_err(|_| ModelError::Alloc { what: "camera fidelity edge adjacency" })?;

                for (ti, tri) in tris.iter().enumerate() {
                    let a = p3_to_v3(pos[tri[0] as usize]);
                    let b = p3_to_v3(pos[tri[1] as usize]);
                    let c = p3_to_v3(pos[tri[2] as usize]);
                    let local_n = (b - a).cross(c - a).normalized();
                    // Orthogonal placement (rotation or mirror) transforms outward normals via `lin`.
                    let world_n = local_n.and_then(|n| lin.apply(n).normalized());
                    tri_normals
                        .try_push(world_n)
                        .map_err(|_| ModelError::Alloc { what: "camera fidelity triangle normals" })?;
                    if world_n.is_none() {
                        continue;
                    }
                    let ti_u32 = ti as u32;
                    let pairs = [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])];
                    for (u, v) in pairs {
                        let ku = pos_key(pos[u as usize]);
                        let kv = pos_key(pos[v as usize]);
                        if ku == kv {
                            continue;
                        }
                        let (lo, hi, v0, v1) = if ku < kv { (ku, kv, u, v) } else { (kv, ku, v, u) };
                        half_edges
                            .try_push(HalfEdge { lo, hi, tri: ti_u32, v0, v1 })
                            .map_err(|_| ModelError::Alloc { what: "camera fidelity edge adjacency" })?;
                    }
                }

                half_edges.sort_unstable_by(|a, b| (a.lo, a.hi, a.tri).cmp(&(b.lo, b.hi, b.tri)));

                let mut i = 0usize;
                while i < half_edges.len() {
                    let first = half_edges[i];
                    let mut j = i + 1;
                    while j < half_edges.len() && half_edges[j].lo == first.lo && half_edges[j].hi == first.hi {
                        j += 1;
                    }
                    let run = &half_edges[i..j];
                    i = j;

                    let local_v0 = p3_to_v3(pos[first.v0 as usize]);
                    let local_v1 = p3_to_v3(pos[first.v1 as usize]);
                    let local_mid = (local_v0 + local_v1) * 0.5;
                    let world_mid = placement.apply(local_mid);
                    let Some(vdir) = frame.view_dir_from(world_mid) else {
                        continue;
                    };

                    let is_silhouette = if run.len() == 1 {
                        let Some(n0) = tri_normals[run[0].tri as usize] else {
                            continue;
                        };
                        // Visible or grazing open boundary edge.
                        n0.dot(vdir) >= -GRAZING_SILHOUETTE_COS
                    } else {
                        let mut min_dot = f64::INFINITY;
                        let mut max_dot = f64::NEG_INFINITY;
                        let mut min_abs = f64::INFINITY;
                        let mut valid = 0u32;
                        for he in run {
                            if let Some(n) = tri_normals[he.tri as usize] {
                                let d = n.dot(vdir);
                                min_dot = min_dot.min(d);
                                max_dot = max_dot.max(d);
                                min_abs = min_abs.min(d.abs());
                                valid += 1;
                            }
                        }
                        valid > 0 && (min_dot * max_dot <= 0.0 || min_abs < GRAZING_SILHOUETTE_COS)
                    };
                    if !is_silhouette {
                        continue;
                    }

                    let Some((mid_px, mid_py, mid_depth)) = frame.project_screen(world_mid) else {
                        continue;
                    };

                    // Evaluate exact CAD offset at edge midpoint and both endpoints, retaining the
                    // worst screen-space silhouette shift on this edge.
                    let candidates = [
                        (local_mid, world_mid, Some((mid_px, mid_py, mid_depth))),
                        (local_v0, placement.apply(local_v0), None),
                        (local_v1, placement.apply(local_v1), None),
                    ];
                    let mut edge_best: Option<SilhouetteShift> = None;
                    for (local_p, world_p, pre_proj) in candidates {
                        let Some((px, py, depth)) = pre_proj.or_else(|| frame.project_screen(world_p)) else {
                            continue;
                        };
                        let f = eval_point(tape, local_p, &mut sc);
                        if !f.is_finite() {
                            continue;
                        }
                        let offset_m = f.abs();
                        let px_size = frame.pixel_size_at(depth);
                        let shift_px = offset_m / px_size;
                        if !shift_px.is_finite() {
                            continue;
                        }
                        if edge_best.is_none_or(|b| shift_px > b.shift_px) {
                            edge_best = Some(SilhouetteShift {
                                camera: frame.name,
                                lod: lod_u32,
                                world_pos: v3_to_lengths(world_p),
                                pixel: [px, py],
                                shift_px,
                                offset_m: Length::m(offset_m),
                            });
                        }
                    }

                    if let Some(shift) = edge_best {
                        if shift.shift_px > max_shift_px {
                            max_shift_px = shift.shift_px;
                            worst_sil = Some(shift);
                        }
                        if shift.shift_px > o.max_silhouette_shift_px {
                            silhouette_issues
                                .try_push(shift)
                                .map_err(|_| ModelError::Alloc { what: "camera fidelity silhouette issues" })?;
                        }
                    }
                }
            }

            // 2. Seam visibility across this camera & LOD.
            let mut seams_lost = 0u32;
            let seam_base = lod_idx.saturating_mul(o.seams.len());
            for (si, seam) in o.seams.iter().enumerate() {
                let world_c = seam_world_centers[si];
                let Some((_, _, depth)) = frame.project_screen(world_c) else {
                    continue;
                };
                let px_size = frame.pixel_size_at(depth);
                let projected_width_px = seam.width.as_m() / px_size;
                let resolved_triangles = seam_tri_counts[seam_base + si];
                let visible = if ignore_seam {
                    true
                } else {
                    projected_width_px >= o.min_seam_px && resolved_triangles > 0
                };
                if !visible {
                    seams_lost = seams_lost.saturating_add(1);
                }
                seam_checks
                    .try_push(SeamVisibility {
                        camera: frame.name,
                        lod: lod_u32,
                        seam: seam.name,
                        projected_width_px,
                        resolved_triangles,
                        visible,
                    })
                    .map_err(|_| ModelError::Alloc { what: "camera fidelity seam checks" })?;
            }

            // 3. Decal & marking legibility across this camera & LOD.
            let mut decals_broken = 0u32;
            for (di, decal) in o.decals.iter().enumerate() {
                let (world_c, world_n) = decal_world[di];
                let Some(vdir) = frame.view_dir_from(world_c) else {
                    continue;
                };
                let facing_cos = world_n.dot(vdir);
                if !(facing_cos > MIN_DECAL_FACING_COS) {
                    continue;
                }
                let Some((_, _, depth)) = frame.project_screen(world_c) else {
                    continue;
                };
                let px_size = frame.pixel_size_at(depth);
                let projected_stroke_px = (decal.stroke_width.as_m() * facing_cos) / px_size;
                let texels_per_screen_px = decal.texels_per_metre.map(|tpm| tpm * px_size);
                let legible = if ignore_decal {
                    true
                } else {
                    projected_stroke_px >= o.min_decal_stroke_px
                        && texels_per_screen_px.is_none_or(|r| r >= o.min_decal_texel_ratio)
                };
                if !legible {
                    decals_broken = decals_broken.saturating_add(1);
                }
                decal_checks
                    .try_push(DecalLegibility {
                        camera: frame.name,
                        lod: lod_u32,
                        decal: decal.name,
                        projected_stroke_px,
                        texels_per_screen_px,
                        facing_cos,
                        legible,
                    })
                    .map_err(|_| ModelError::Alloc { what: "camera fidelity decal checks" })?;
            }

            let sil_ok = ignore_silhouette || max_shift_px <= o.max_silhouette_shift_px;
            let passed = sil_ok && seams_lost == 0 && decals_broken == 0;
            if !passed {
                all_passed = false;
            }
            lod_reports
                .try_push(CameraLodReport {
                    camera: frame.name,
                    lod: lod_u32,
                    triangles: tri_count,
                    max_silhouette_shift_px: max_shift_px,
                    worst_silhouette: worst_sil,
                    seams_lost,
                    decals_broken,
                    passed,
                })
                .map_err(|_| ModelError::Alloc { what: "camera fidelity lod reports" })?;
        }
    }

    Ok(CameraFidelityReport { lod_reports, silhouette_issues, seam_checks, decal_checks, passed: all_passed })
}

// =============================================================================================
// Priority 14: Saved Game-Camera Previews & Side-by-Side Revision Comparisons from GLB Bytes
// =============================================================================================

/// Standard gameplay & studio inspection camera presets generated by [`GameCameraRig`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GameCameraPreset {
    /// Close low-angle foreshortened FPS aiming view (`fov = 38°`, front-left shoulder close-up).
    Aiming,
    /// Mid-distance 3/4 carried-prop view (`fov = 45°`, slightly elevated right-front).
    Carried,
    /// Tight hero close-up inspection view (`fov = 34°`, primary front/shoulder face).
    Inspect,
    /// Classic studio 3/4 turnaround view (`fov = 40°`).
    HeroThreeQuarter,
    /// Orthographic top-down plan view.
    TopOrthographic,
    /// Close oblique inspection angle rendered with Class-A zebra reflection stripes.
    ZebraInspect,
}

impl GameCameraPreset {
    /// Short deterministic slug used as the preset's [`NamedCamera`] label.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            GameCameraPreset::Aiming => "aiming",
            GameCameraPreset::Carried => "carried",
            GameCameraPreset::Inspect => "inspect",
            GameCameraPreset::HeroThreeQuarter => "hero_three_quarter",
            GameCameraPreset::TopOrthographic => "top_orthographic",
            GameCameraPreset::ZebraInspect => "zebra_inspect",
        }
    }
}

/// Deterministic scale-invariant camera rig builder for gameplay and CAD review renders.
#[derive(Clone, Copy, Debug)]
pub struct GameCameraRig;

impl GameCameraRig {
    /// Compute the 6 standard game/inspection camera views cleanly framing `bounds` at
    /// `width × height` pixels, invariant to asset scale (millimetre hardware up to multi-metre
    /// hulls).
    pub fn standard_views<'a>(
        bounds: &Aabb3,
        width: u32,
        height: u32,
        alloc: Alloc<'a>,
    ) -> Result<Vec<(GameCameraPreset, NamedCamera), Alloc<'a>>, ModelError> {
        if width == 0 || height == 0 {
            return Err(ModelError::Picture(PictureError::Empty { width, height }));
        }
        let coords = [bounds.lo.x, bounds.lo.y, bounds.lo.z, bounds.hi.x, bounds.hi.y, bounds.hi.z];
        if bounds.is_empty() || !coords.iter().all(|v| v.is_finite()) {
            return Err(ModelError::Argument {
                function: "GameCameraRig::standard_views",
                arg: "bounds",
                accepted: "a finite non-empty bounding box",
            });
        }

        let center = bounds.center();
        let raw_half = (bounds.hi - bounds.lo) * 0.5;
        let radius = raw_half.length().max(1e-4);
        let aspect = f64::from(width) / f64::from(height);
        let z_up = [0.0, 0.0, 1.0];

        let mut out: Vec<(GameCameraPreset, NamedCamera), Alloc<'a>> = Vec::new_in(alloc);
        out.try_reserve(6)
            .map_err(|_| ModelError::Alloc { what: "standard game camera views" })?;

        // 1. Aiming: close low-angle foreshortened FPS view (fov = 38°, front-left shoulder).
        let aiming_target = center + Vec3::new(-raw_half.x * 0.15, -raw_half.y * 0.10, raw_half.z * 0.10);
        let aiming_view = perspective_view(
            aiming_target,
            Vec3::new(-0.42, -0.88, 0.14),
            z_up,
            radius,
            38.0,
            0.92,
            aspect,
            width,
            height,
        )?;
        out.try_push((GameCameraPreset::Aiming, NamedCamera::new(GameCameraPreset::Aiming.name(), aiming_view)))
            .map_err(|_| ModelError::Alloc { what: "standard game camera views" })?;

        // 2. Carried: mid-distance 3/4 carried-prop view (fov = 45°, elevated right-front).
        let carried_view = perspective_view(
            center,
            Vec3::new(0.62, -0.72, 0.30),
            z_up,
            radius,
            45.0,
            0.72,
            aspect,
            width,
            height,
        )?;
        out.try_push((GameCameraPreset::Carried, NamedCamera::new(GameCameraPreset::Carried.name(), carried_view)))
            .map_err(|_| ModelError::Alloc { what: "standard game camera views" })?;

        // 3. Inspect: tight hero close-up inspection view (fov = 34°, primary front/shoulder face).
        let inspect_target = center + Vec3::new(0.0, -raw_half.y * 0.15, raw_half.z * 0.05);
        let inspect_view = perspective_view(
            inspect_target,
            Vec3::new(0.18, -0.96, 0.20),
            z_up,
            radius,
            34.0,
            0.88,
            aspect,
            width,
            height,
        )?;
        out.try_push((GameCameraPreset::Inspect, NamedCamera::new(GameCameraPreset::Inspect.name(), inspect_view)))
            .map_err(|_| ModelError::Alloc { what: "standard game camera views" })?;

        // 4. HeroThreeQuarter: classic studio 3/4 turnaround view (fov = 40°).
        let hero_view = perspective_view(
            center,
            Vec3::new(0.65, -0.65, 0.40),
            z_up,
            radius,
            40.0,
            0.76,
            aspect,
            width,
            height,
        )?;
        out.try_push((
            GameCameraPreset::HeroThreeQuarter,
            NamedCamera::new(GameCameraPreset::HeroThreeQuarter.name(), hero_view),
        ))
        .map_err(|_| ModelError::Alloc { what: "standard game camera views" })?;

        // 5. TopOrthographic: orthographic top-down plan view (looking down -Z with +Y up).
        let span_x = (2.0 * raw_half.x).max(1e-4);
        let span_y = (2.0 * raw_half.y).max(1e-4);
        let ortho_h = span_y.max(span_x / aspect) * 1.18;
        let top_eye = center + Vec3::new(0.0, 0.0, radius * 3.0 + raw_half.z + 1e-3);
        let top_view = View {
            eye: v3_to_lengths(top_eye),
            target: v3_to_lengths(center),
            up: [0.0, 1.0, 0.0],
            projection: Projection::Orthographic { height: Length::m(ortho_h) },
            width,
            height,
        };
        let _cam = Camera::new(&top_view)?;
        out.try_push((
            GameCameraPreset::TopOrthographic,
            NamedCamera::new(GameCameraPreset::TopOrthographic.name(), top_view),
        ))
        .map_err(|_| ModelError::Alloc { what: "standard game camera views" })?;

        // 6. ZebraInspect: close oblique inspection angle for Class-A zebra reflection evaluation.
        let zebra_view = perspective_view(
            center,
            Vec3::new(-0.52, -0.74, 0.42),
            z_up,
            radius,
            35.0,
            0.85,
            aspect,
            width,
            height,
        )?;
        out.try_push((GameCameraPreset::ZebraInspect, NamedCamera::new(GameCameraPreset::ZebraInspect.name(), zebra_view)))
            .map_err(|_| ModelError::Alloc { what: "standard game camera views" })?;

        Ok(out)
    }
}

/// Options for [`render_glb_views`] and [`compare_glb_revisions`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct GlbPreviewOpts<'a, 's> {
    pub up: Up = Up::Z,
    pub shading: Shading = Shading::Studio(StudioOpts::DEFAULT),
    pub supersample: u8 = 2,
    pub strip_lights: [Option<StripLight>; 4] = [None; 4],
    pub limits: Limits = Limits::SHIPPED,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// One camera render from [`render_glb_views`], holding both the raw [`Picture`] and encoded PNG.
#[derive(Debug)]
pub struct GlbViewRender<'a> {
    pub name: Name,
    pub preset: Option<GameCameraPreset>,
    pub picture: Picture<'a>,
    pub png: Vec<u8, Alloc<'a>>,
}

/// Multi-camera render set produced by [`render_glb_views`], including a 2-column contact sheet.
#[derive(Debug)]
pub struct GlbRenderSet<'a> {
    pub views: Vec<GlbViewRender<'a>, Alloc<'a>>,
    pub contact_sheet: Picture<'a>,
    pub contact_sheet_png: Vec<u8, Alloc<'a>>,
}

/// One camera's `[Previous | Current | Diff]` revision comparison from [`compare_glb_revisions`].
#[derive(Debug)]
pub struct GlbRevisionView<'a> {
    pub name: Name,
    pub rms_error: f64,
    pub max_channel_delta: u8,
    pub changed_fraction: f64,
    pub side_by_side: Picture<'a>,
    pub side_by_side_png: Vec<u8, Alloc<'a>>,
}

/// Full multi-camera revision comparison returned by [`compare_glb_revisions`].
#[derive(Debug)]
pub struct GlbRevisionComparison<'a> {
    pub views: Vec<GlbRevisionView<'a>, Alloc<'a>>,
    pub sheet: Picture<'a>,
    pub sheet_png: Vec<u8, Alloc<'a>>,
}

/// Borrowed mesh parts, scalar materials, and decoded texture maps bound to a [`LoadedGlb`].
struct GlbSceneBindings<'m, 's> {
    parts: Vec<MeshScenePart<'m>, Alloc<'s>>,
    materials: Vec<Material<'m>, Alloc<'s>>,
    material_maps: Vec<MaterialMaps<'m>, Alloc<'s>>,
}

fn build_scene_bindings<'m, 's>(loaded: &'m LoadedGlb<'_>, scratch: Alloc<'s>) -> Result<GlbSceneBindings<'m, 's>, ModelError> {
    let mut parts: Vec<MeshScenePart<'m>, Alloc<'s>> = Vec::new_in(scratch);
    parts
        .try_reserve(loaded.parts.len())
        .map_err(|_| ModelError::Alloc { what: "glb preview scene parts" })?;
    for (part_idx, part) in loaded.parts.iter().enumerate() {
        let Some(ref mesh) = part.mesh else {
            continue;
        };
        // Compose ancestor transforms (including empty group nodes) via `LoadedGlb::world_matrix`.
        let world = Transform::Matrix(loaded.world_matrix(part_idx));
        parts
            .try_push(MeshScenePart { mesh: mesh.view(), parent: None, transform: world, tint_override: None })
            .map_err(|_| ModelError::Alloc { what: "glb preview scene parts" })?;
    }

    let mut materials: Vec<Material<'m>, Alloc<'s>> = Vec::new_in(scratch);
    materials
        .try_reserve(loaded.materials.len())
        .map_err(|_| ModelError::Alloc { what: "glb preview materials" })?;
    let mut material_maps: Vec<MaterialMaps<'m>, Alloc<'s>> = Vec::new_in(scratch);
    material_maps
        .try_reserve(loaded.materials.len())
        .map_err(|_| ModelError::Alloc { what: "glb preview material maps" })?;

    let tex_of = |img_idx: Option<u32>| -> Option<TextureMap<'m>> {
        let img = loaded.image(img_idx?)?;
        Some(TextureMap { width: img.width, height: img.height, rgba: &img.rgba })
    };
    for mat in loaded.materials.iter() {
        materials
            .try_push(mat.as_material())
            .map_err(|_| ModelError::Alloc { what: "glb preview materials" })?;
        material_maps
            .try_push(MaterialMaps {
                base_colour: tex_of(mat.textures.base_colour),
                metallic_roughness: tex_of(mat.textures.metallic_roughness),
                normal: tex_of(mat.textures.normal),
                normal_scale: mat.textures.normal_scale,
            })
            .map_err(|_| ModelError::Alloc { what: "glb preview material maps" })?;
    }

    Ok(GlbSceneBindings { parts, materials, material_maps })
}

/// Parse actual `.glb` bytes with `blaze_cad_out::read_glb`, render every requested camera with
/// `render_mesh_scene` (using `RenderMode::Zebra` for [`GameCameraPreset::ZebraInspect`] and
/// applying [`GlbPreviewOpts::strip_lights`] to [`Shading::Studio`]), encode every view to PNG,
/// and compose a 2-column contact sheet + PNG.
pub fn render_glb_views<'a>(
    glb_bytes: &[u8],
    views: &[(Option<GameCameraPreset>, NamedCamera)],
    o: GlbPreviewOpts<'a, '_>,
) -> Result<GlbRenderSet<'a>, ModelError> {
    if views.is_empty() {
        return Err(ModelError::Argument {
            function: "render_glb_views",
            arg: "views",
            accepted: "at least one camera view",
        });
    }

    let loaded = read_glb(glb_bytes, GlbReadOpts { up: o.up, limits: o.limits, alloc: o.scratch, scratch: o.scratch })
        .map_err(|e| map_glb_read_err("render_glb_views", e))?;
    let bindings = build_scene_bindings(&loaded, o.scratch)?;
    let shading = effective_shading(&o.shading, &o.strip_lights);

    let mut rendered: Vec<GlbViewRender<'a>, Alloc<'a>> = Vec::new_in(o.alloc);
    rendered
        .try_reserve(views.len())
        .map_err(|_| ModelError::Alloc { what: "glb rendered views" })?;

    for &(preset, named_cam) in views {
        let mode = mode_for_preset(preset);
        let picture = render_mesh_scene(
            &bindings.parts,
            &bindings.materials,
            named_cam.view,
            MeshPictureOpts {
                alloc: o.alloc,
                scratch: o.scratch,
                shading,
                mode,
                material_maps: &bindings.material_maps,
                supersample: o.supersample,
                ..
            },
        )?;
        let png = picture.png(PngOpts {
            level: blaze_deflate::Level::Fast,
            limits: o.limits,
            alloc: o.alloc,
            scratch: o.scratch,
        })?;
        rendered
            .try_push(GlbViewRender { name: named_cam.name, preset, picture, png })
            .map_err(|_| ModelError::Alloc { what: "glb rendered views" })?;
    }

    let mut panel_refs: Vec<&Picture<'a>, Alloc<'_>> = Vec::new_in(o.scratch);
    panel_refs
        .try_reserve(rendered.len())
        .map_err(|_| ModelError::Alloc { what: "glb contact sheet panels" })?;
    for v in rendered.iter() {
        panel_refs
            .try_push(&v.picture)
            .map_err(|_| ModelError::Alloc { what: "glb contact sheet panels" })?;
    }
    let contact_sheet = compose_grid(&panel_refs, 2, CONTACT_SHEET_GAP_PX, CONTACT_SHEET_BG, o.alloc)?;
    let contact_sheet_png = contact_sheet.png(PngOpts {
        level: blaze_deflate::Level::Fast,
        limits: o.limits,
        alloc: o.alloc,
        scratch: o.scratch,
    })?;

    Ok(GlbRenderSet { views: rendered, contact_sheet, contact_sheet_png })
}

/// Render BOTH actual `.glb` revisions from the exact same saved cameras, run `diff_pictures` per
/// camera, compose `[Previous | Current | Diff]` strips via `compose_side_by_side`, and stack them
/// into a combined comparison contact sheet + PNG.
pub fn compare_glb_revisions<'a>(
    previous_glb: &[u8],
    current_glb: &[u8],
    views: &[(Option<GameCameraPreset>, NamedCamera)],
    o: GlbPreviewOpts<'a, '_>,
) -> Result<GlbRevisionComparison<'a>, ModelError> {
    if views.is_empty() {
        return Err(ModelError::Argument {
            function: "compare_glb_revisions",
            arg: "views",
            accepted: "at least one camera view",
        });
    }

    let prev_loaded = read_glb(previous_glb, GlbReadOpts { up: o.up, limits: o.limits, alloc: o.scratch, scratch: o.scratch })
        .map_err(|e| map_glb_read_err("compare_glb_revisions", e))?;
    let curr_loaded = read_glb(current_glb, GlbReadOpts { up: o.up, limits: o.limits, alloc: o.scratch, scratch: o.scratch })
        .map_err(|e| map_glb_read_err("compare_glb_revisions", e))?;
    let prev_scene = build_scene_bindings(&prev_loaded, o.scratch)?;
    let curr_scene = build_scene_bindings(&curr_loaded, o.scratch)?;
    let shading = effective_shading(&o.shading, &o.strip_lights);

    let mut compared: Vec<GlbRevisionView<'a>, Alloc<'a>> = Vec::new_in(o.alloc);
    compared
        .try_reserve(views.len())
        .map_err(|_| ModelError::Alloc { what: "glb revision comparison views" })?;

    for &(preset, named_cam) in views {
        let mode = mode_for_preset(preset);
        let prev_pic = render_mesh_scene(
            &prev_scene.parts,
            &prev_scene.materials,
            named_cam.view,
            MeshPictureOpts {
                alloc: o.scratch,
                scratch: o.scratch,
                shading,
                mode,
                material_maps: &prev_scene.material_maps,
                supersample: o.supersample,
                ..
            },
        )?;
        let curr_pic = render_mesh_scene(
            &curr_scene.parts,
            &curr_scene.materials,
            named_cam.view,
            MeshPictureOpts {
                alloc: o.scratch,
                scratch: o.scratch,
                shading,
                mode,
                material_maps: &curr_scene.material_maps,
                supersample: o.supersample,
                ..
            },
        )?;
        let diff = diff_pictures(&prev_pic, &curr_pic, REVISION_DIFF_THRESHOLD, REVISION_DIFF_AMPLIFY, o.scratch)?;
        let strip = compose_side_by_side(
            &[&prev_pic, &curr_pic, &diff.heatmap],
            CONTACT_SHEET_GAP_PX,
            CONTACT_SHEET_BG,
            o.alloc,
        )?;
        let strip_png = strip.png(PngOpts {
            level: blaze_deflate::Level::Fast,
            limits: o.limits,
            alloc: o.alloc,
            scratch: o.scratch,
        })?;
        compared
            .try_push(GlbRevisionView {
                name: named_cam.name,
                rms_error: diff.rms_error,
                max_channel_delta: diff.max_channel_delta,
                changed_fraction: diff.changed_fraction,
                side_by_side: strip,
                side_by_side_png: strip_png,
            })
            .map_err(|_| ModelError::Alloc { what: "glb revision comparison views" })?;
    }

    let mut row_refs: Vec<&Picture<'a>, Alloc<'_>> = Vec::new_in(o.scratch);
    row_refs
        .try_reserve(compared.len())
        .map_err(|_| ModelError::Alloc { what: "glb revision comparison sheet panels" })?;
    for v in compared.iter() {
        row_refs
            .try_push(&v.side_by_side)
            .map_err(|_| ModelError::Alloc { what: "glb revision comparison sheet panels" })?;
    }
    let sheet = compose_grid(&row_refs, 1, CONTACT_SHEET_GAP_PX, CONTACT_SHEET_BG, o.alloc)?;
    let sheet_png = sheet.png(PngOpts {
        level: blaze_deflate::Level::Fast,
        limits: o.limits,
        alloc: o.alloc,
        scratch: o.scratch,
    })?;

    Ok(GlbRevisionComparison { views: compared, sheet, sheet_png })
}

// =============================================================================================
// Helpers
// =============================================================================================

#[inline]
fn metres_to_v3(p: [Length; 3]) -> Vec3 {
    Vec3::new(p[0].as_m(), p[1].as_m(), p[2].as_m())
}

#[inline]
fn v3_to_lengths(p: Vec3) -> [Length; 3] {
    [Length::m(p.x), Length::m(p.y), Length::m(p.z)]
}

#[inline]
fn p3_to_v3(p: [f32; 3]) -> Vec3 {
    Vec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2]))
}

#[inline]
fn mode_for_preset(preset: Option<GameCameraPreset>) -> RenderMode {
    if preset == Some(GameCameraPreset::ZebraInspect) {
        RenderMode::Zebra(ZebraOpts::DEFAULT)
    } else {
        RenderMode::Shaded
    }
}

#[inline]
fn effective_shading(shading: &Shading, strip_lights: &[Option<StripLight>; 4]) -> Shading {
    match *shading {
        Shading::Classic => Shading::Classic,
        Shading::Studio(mut studio) => {
            if strip_lights.iter().any(Option::is_some) {
                studio.strip_lights = *strip_lights;
            }
            Shading::Studio(studio)
        }
    }
}

fn perspective_view(
    target: Vec3,
    eye_dir: Vec3,
    up: [f64; 3],
    radius: f64,
    fov_deg: f64,
    fill: f64,
    aspect: f64,
    width: u32,
    height: u32,
) -> Result<View, ModelError> {
    let dir = eye_dir.normalized().unwrap_or(Vec3::new(0.0, -1.0, 0.0));
    let fov = Angle::degrees(fov_deg);
    let half_tan = blaze_math::tan(fov.as_radians() / 2.0);
    let limiting_tan = (half_tan * aspect.min(1.0)).max(1e-4);
    // Exact bounding-sphere tangency distance scaled by `fill`.
    let limiting_sin = limiting_tan / blaze_math::sqrt(1.0 + limiting_tan * limiting_tan);
    let dist = (radius / (fill * limiting_sin)).max(radius * 1.15);
    let eye = target + dir * dist;
    let view = View { eye: v3_to_lengths(eye), target: v3_to_lengths(target), up, projection: Projection::Perspective { fov }, width, height };
    let _cam = Camera::new(&view)?;
    Ok(view)
}

fn map_glb_read_err(function: &'static str, err: GlbReadError) -> ModelError {
    match err {
        GlbReadError::Alloc(_) => ModelError::Alloc { what: "glb reader" },
        GlbReadError::Shape(e) => ModelError::Shape(e),
        GlbReadError::Png { error, .. } => ModelError::Png(error),
        GlbReadError::TooShort { .. }
        | GlbReadError::BadMagic
        | GlbReadError::UnsupportedVersion { .. }
        | GlbReadError::LengthMismatch { .. }
        | GlbReadError::BadChunk { .. }
        | GlbReadError::Json { .. }
        | GlbReadError::InvalidStructure { .. }
        | GlbReadError::OutOfRange { .. }
        | GlbReadError::TooManyParts { .. }
        | GlbReadError::TooLarge { .. }
        | GlbReadError::BadAccessor { .. }
        | GlbReadError::BadMesh { .. } => ModelError::Argument {
            function,
            arg: "glb",
            accepted: "a valid glTF 2.0 binary (.glb)",
        },
    }
}

fn put_fixed<P: Put + ?Sized>(out: &mut P, value: f64, places: u32) {
    if !value.is_finite() {
        out.put_str("nan");
        return;
    }
    let places = places.min(6);
    let scale = 10u64.pow(places);
    let neg = value < 0.0;
    let scaled = core::f64::math::round(value.abs() * (scale as f64));
    let q = if scaled >= (u64::MAX as f64) { u64::MAX } else { scaled as u64 };
    if neg && q != 0 {
        out.put_byte(b'-');
    }
    let whole = q / scale;
    let frac = q % scale;
    out.int(whole);
    if places > 0 {
        out.put_byte(b'.');
        out.int_zeros(frac, places as usize);
    }
}
