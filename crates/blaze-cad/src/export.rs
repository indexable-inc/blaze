//! A model out: a GPU-ready glTF binary of its parts (each meshed in its own frame, Tipsify
//! vertex-cache and vertex-fetch optimized, optional LOD chains and convex collider hulls,
//! per-part material roles and parent hierarchy, and mass facts), binary STL for 3D printing /
//! CAM, 2D vector SVG cross-sections via SIMD sampling, and classic or studio-lit pictures of any
//! solid.

#![blaze::sans_io]

use blaze_cad_field::{
    CadPlant, CompileOpts, Detail, Ex, Graph, InspectError, Lanes, Placement, Points, Scratch, Solid, compile, eval_box, eval_points,
    section, simplify,
};
use blaze_cad_mesh::{
    COS_32_DEGREES, CacheOpts, DecimateOpts, Decimated, DecomposeOpts, FetchOpts, Hull, HullOpts, LodOpts, MaterialRole, Mesh,
    MeshLimits, MeshVecs, MeshView, Meshed, PieceCache, Projection as MeshProjection, Source, Target, convex_hull, decimate,
    decompose, lod_chain, optimize_vertex_cache, optimize_vertex_fetch,
};
use blaze_cad_out::{
    Collider, DEFAULT_FILL, DEFAULT_STROKE, Encoding, GlbOpts, Limits, Lod, MassFacts, Material, Matrix, SectionGrid, StlOpts,
    StlUnit, SvgError, SvgOpts, Transform, Up, glb, stl, svg,
};
use blaze_cad_picture::{
    MeshPictureOpts, MeshScenePart, Picture, PictureOpts, Shading, StudioOpts, View, render, render_mesh, render_mesh_scene,
};
use blaze_math::{Aabb3, Vec3};
use blaze_own::planted::Planted;
use blaze_own::{Alloc, Vec};
use blaze_units::{Density, Length};

use crate::{MeshPartOpts, ModelError, Part, PartBudget, mass_properties, mesh_part};

/// Camera projection parameters used to compute LOD switch distances (re-export of
/// `blaze_cad_mesh::Projection`, named to avoid colliding with `blaze_cad_picture::Projection`).
pub type LodProjection = MeshProjection;

/// Painted steel: the one material [`ExportOpts`] writes unless given others.
pub const STEEL: Material<'static> = Material {
    name: "steel",
    base_colour: [0.62, 0.64, 0.68, 1.0],
    metallic: 0.9,
    roughness: 0.45,
    emissive: [0.0; 3],
    emissive_strength: None,
    clearcoat: None,
    double_sided: false,
};

/// [`STEEL`] alone.
pub const STEEL_ONLY: &[Material<'static>] = &[STEEL];

/// Opt-in level-of-detail chain generation settings for [`ExportOpts::lods`].
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ExportLodOpts {
    /// Target triangle fraction between successive LOD levels (e.g. `0.5` halves each level).
    pub ratio: f64 = 0.5,
    /// Do not generate LOD levels with fewer triangles than this.
    pub fewest_triangles: u32 = 16,
    /// Camera projection used to compute each level's switch distance.
    pub projection: LodProjection = LodProjection { focal_pixels: 935.3, most_error_pixels: 1.0 },
}

/// Opt-in physics collider generation mode for [`ExportOpts::colliders`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ColliderMode {
    /// Single convex hull per part (`blaze_cad_mesh::convex_hull`).
    ConvexHull,
    /// Approximate convex decomposition per part (`blaze_cad_mesh::decompose`).
    Decompose {
        /// Maximum concavity a piece may keep, as a fraction of mesh volume (typically `0.01..0.05`).
        concavity: f64,
        /// Side length of the ray grid used to estimate volume concavity (typically `32..128`).
        rays: u32,
    },
}

/// What [`export_glb`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct ExportOpts<'a, 's, 'm> {
    /// Where the file's bytes live.
    pub alloc: Alloc<'a>,
    /// Where the tapes, meshes and the writer's tables live.
    pub scratch: Alloc<'s>,
    /// The most distance a vertex may be from the surface.
    pub tolerance: Length,
    /// Each part's density, for its mass facts; none written when `None`.
    pub density: Option<Density> = None,
    /// The materials, by each part's [`MaterialRole`] (parts default to role 0).
    pub materials: &'m [Material<'m>] = STEEL_ONLY,
    /// The model is z-up; glTF is y-up, and the writer turns it.
    pub up: Up = Up::Z,
    pub encoding: Encoding = Encoding::Float,
    pub limits: Limits = Limits::SHIPPED,
    /// Simplify each part's mesh (blaze-cad-mesh's `decimate`, creases and borders kept) to a
    /// two-sided distance of `tolerance` from the mesher's before writing it: the octree's cells
    /// cannot stretch along a surface's flat direction, the simplifier's triangles can. A written
    /// vertex is then within `tolerance` plus the mesher's `vertex_bound` of the surface.
    pub decimate: bool = true,
    /// The crease angle, as its cosine, the simplified mesh's normals are split at.
    pub cos_crease: f32 = COS_32_DEGREES,
    /// Optimize triangle order (Tipsify post-transform cache) and vertex order (pre-transform
    /// fetch) on every exported mesh and LOD level.
    pub optimize_gpu: bool = true,
    /// FIFO post-transform vertex cache size used by Tipsify when [`Self::optimize_gpu`] is on.
    pub vertex_cache_size: u32 = 32,
    /// Optional LOD chain generated per part via `blaze_cad_mesh::lod_chain`.
    pub lods: Option<ExportLodOpts> = None,
    /// Optional physics collider hulls generated per part via `convex_hull` or `decompose`.
    pub colliders: Option<ColliderMode> = None,
}

/// A part's triangles: as meshed, and as written (fewer when [`ExportOpts::decimate`]).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PartCount {
    pub meshed: u32,
    pub written: u32,
    /// The written mesh's two-sided distance from the meshed one (zero when not simplified).
    pub simplified_within: Length,
    /// Every meshed vertex's `|f|` bound (blaze-cad-mesh's `Meshed::vertex_bound`).
    pub vertex_bound: Length,
}

/// What [`export_glb`] made: the file, and each part's triangle counts, in the parts' order.
#[derive(Debug)]
pub struct Exported<'a> {
    pub glb: Vec<u8, Alloc<'a>>,
    pub counts: Vec<PartCount, Alloc<'a>>,
}

impl Exported<'_> {
    /// The triangles written, all parts together.
    pub fn triangles(&self) -> u64 {
        self.counts.iter().map(|c| u64::from(c.written)).sum()
    }
}

/// The node transform of a placement: always a matrix (a placement is a signed permutation or a
/// grid rotation, a mirror included, which glTF's TRS cannot hold).
pub fn transform_of(p: &Placement) -> Transform {
    Transform::Matrix(Matrix { linear: p.linear().rows, translation: p.shift_m().to_array() })
}

/// Prepare a mesh for GPU export: optionally run Tipsify vertex-cache optimization followed by
/// vertex-fetch reordering, and stamp `stamp_role` onto all triangles when `Some(r)` with `r != 0`.
/// Pass `None` to preserve per-triangle multi-material roles already assigned on `view`.
pub(crate) fn prepare_gpu_mesh<'s>(
    view: &MeshView<'_>,
    stamp_role: Option<MaterialRole>,
    optimize_gpu: bool,
    cache_size: u32,
    alloc: Alloc<'s>,
    scratch: Alloc<'_>,
) -> Result<Mesh<'s>, ModelError> {
    let mesh = if optimize_gpu {
        let cached = optimize_vertex_cache(view, CacheOpts { cache_size, alloc: scratch, scratch })?;
        optimize_vertex_fetch(&cached.view(), FetchOpts { alloc, scratch })?
    } else {
        copy_view(view, alloc)?
    };
    let Some(role) = stamp_role else {
        return Ok(mesh);
    };
    if role == MaterialRole(0) {
        return Ok(mesh);
    }
    let mut vecs = mesh.into_vecs();
    vecs.roles.fill(role);
    Mesh::new(vecs).map_err(ModelError::Shape)
}

fn copy_view<'s>(view: &MeshView<'_>, alloc: Alloc<'s>) -> Result<Mesh<'s>, ModelError> {
    let c = view.columns();
    let mut positions = Vec::new_in(alloc);
    positions.try_extend_from_slice(c.positions).map_err(|_| ModelError::Alloc { what: "export mesh positions" })?;
    let mut normals = Vec::new_in(alloc);
    normals.try_extend_from_slice(c.normals).map_err(|_| ModelError::Alloc { what: "export mesh normals" })?;
    let mut tangents = Vec::new_in(alloc);
    tangents.try_extend_from_slice(c.tangents).map_err(|_| ModelError::Alloc { what: "export mesh tangents" })?;
    let mut uvs = Vec::new_in(alloc);
    uvs.try_extend_from_slice(c.uvs).map_err(|_| ModelError::Alloc { what: "export mesh uvs" })?;
    let mut colours = Vec::new_in(alloc);
    colours.try_extend_from_slice(c.colours).map_err(|_| ModelError::Alloc { what: "export mesh colours" })?;
    let mut triangles = Vec::new_in(alloc);
    triangles.try_extend_from_slice(c.triangles).map_err(|_| ModelError::Alloc { what: "export mesh triangles" })?;
    let mut parts = Vec::new_in(alloc);
    parts.try_extend_from_slice(c.parts).map_err(|_| ModelError::Alloc { what: "export mesh parts" })?;
    let mut roles = Vec::new_in(alloc);
    roles.try_extend_from_slice(c.roles).map_err(|_| ModelError::Alloc { what: "export mesh roles" })?;
    let mut creases = Vec::new_in(alloc);
    creases.try_extend_from_slice(c.creases).map_err(|_| ModelError::Alloc { what: "export mesh creases" })?;
    Mesh::new(MeshVecs { positions, normals, tangents, uvs, colours, triangles, parts, roles, creases }).map_err(ModelError::Shape)
}

/// Convert a quickhull [`Hull`] into an indexed [`Mesh`] with area-weighted unit vertex normals
/// suitable for glTF collider meshes.
pub(crate) fn hull_to_mesh<'s>(hull: &Hull<'_>, alloc: Alloc<'s>) -> Result<Mesh<'s>, ModelError> {
    let n_pts = hull.points.len();
    let n_tris = hull.triangles.len();
    let mut positions = Vec::new_in(alloc);
    positions.try_reserve(n_pts).map_err(|_| ModelError::Alloc { what: "collider hull positions" })?;
    positions.try_extend_from_slice(&hull.points).map_err(|_| ModelError::Alloc { what: "collider hull positions" })?;

    let mut accum: Vec<[f64; 3], Alloc<'s>> = Vec::new_in(alloc);
    accum.try_reserve(n_pts).map_err(|_| ModelError::Alloc { what: "collider hull normals" })?;
    for _ in 0..n_pts {
        accum.try_push([0.0; 3]).map_err(|_| ModelError::Alloc { what: "collider hull normals" })?;
    }
    for tri in hull.triangles.iter() {
        let a = hull.points[tri[0] as usize];
        let b = hull.points[tri[1] as usize];
        let c = hull.points[tri[2] as usize];
        let ux = f64::from(b[0]) - f64::from(a[0]);
        let uy = f64::from(b[1]) - f64::from(a[1]);
        let uz = f64::from(b[2]) - f64::from(a[2]);
        let vx = f64::from(c[0]) - f64::from(a[0]);
        let vy = f64::from(c[1]) - f64::from(a[1]);
        let vz = f64::from(c[2]) - f64::from(a[2]);
        let nx = uy * vz - uz * vy;
        let ny = uz * vx - ux * vz;
        let nz = ux * vy - uy * vx;
        for &idx in tri {
            let acc = &mut accum[idx as usize];
            acc[0] += nx;
            acc[1] += ny;
            acc[2] += nz;
        }
    }
    let mut normals = Vec::new_in(alloc);
    normals.try_reserve(n_pts).map_err(|_| ModelError::Alloc { what: "collider hull normals" })?;
    for acc in accum.iter() {
        let len = blaze_math::sqrt(acc[0] * acc[0] + acc[1] * acc[1] + acc[2] * acc[2]);
        let n = if len > 0.0 && len.is_finite() {
            [(acc[0] / len) as f32, (acc[1] / len) as f32, (acc[2] / len) as f32]
        } else {
            [0.0, 0.0, 1.0]
        };
        normals.try_push(n).map_err(|_| ModelError::Alloc { what: "collider hull normals" })?;
    }

    let mut triangles = Vec::new_in(alloc);
    triangles.try_reserve(n_tris).map_err(|_| ModelError::Alloc { what: "collider hull triangles" })?;
    triangles.try_extend_from_slice(&hull.triangles).map_err(|_| ModelError::Alloc { what: "collider hull triangles" })?;

    let mut parts = Vec::new_in(alloc);
    parts.try_reserve(n_tris).map_err(|_| ModelError::Alloc { what: "collider hull parts" })?;
    let mut roles = Vec::new_in(alloc);
    roles.try_reserve(n_tris).map_err(|_| ModelError::Alloc { what: "collider hull roles" })?;
    for _ in 0..n_tris {
        parts.try_push(blaze_cad_mesh::PartId(0)).map_err(|_| ModelError::Alloc { what: "collider hull parts" })?;
        roles.try_push(MaterialRole(0)).map_err(|_| ModelError::Alloc { what: "collider hull roles" })?;
    }

    Mesh::new(MeshVecs {
        positions,
        normals,
        tangents: Vec::new_in(alloc),
        uvs: Vec::new_in(alloc),
        colours: Vec::new_in(alloc),
        triangles,
        parts,
        roles,
        creases: Vec::new_in(alloc),
    })
    .map_err(ModelError::Shape)
}

/// Stored metadata for one generated LOD mesh before borrowing views into `blaze_cad_out::Lod`.
struct ReadyLod<'s> {
    mesh: Mesh<'s>,
    error: Length,
    switch_distance: Length,
}

/// The parts as a glTF binary: one node each, named, placed, meshed in its own frame (pieces read
/// from `cache` when given: a part only moved re-meshes nothing), simplified to the tolerance
/// unless [`ExportOpts::decimate`] is off, GPU vertex-cache & vertex-fetch optimized unless
/// [`ExportOpts::optimize_gpu`] is off, with optional LOD chains, physics collider hulls, per-part
/// material roles, and parent hierarchy. Mass facts are from the meshed (unsimplified) mesh.
pub fn export_glb<'a, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    parts: &[Part],
    o: ExportOpts<'a, '_, '_>,
    mut cache: Option<&mut PieceCache<'_>>,
) -> Result<Exported<'a>, ModelError> {
    let s = o.scratch;
    let mut meshes: Vec<Meshed<'_>, Alloc<'_>> = Vec::new_in(s);
    meshes.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "the export's meshes" })?;
    for part in parts {
        let (part_tol, min_feature) = match part.budget {
            PartBudget::Detailed { tolerance, min_feature, .. } => (tolerance.unwrap_or(o.tolerance), min_feature),
            _ => (o.tolerance, None),
        };
        let m = mesh_part(
            g,
            part,
            MeshPartOpts { alloc: s, scratch: s, tolerance: part_tol, min_feature, .. },
            cache.as_deref_mut(),
        )?;
        meshes.try_push(m).map_err(|_| ModelError::Alloc { what: "the export's meshes" })?;
    }
    let mut simple: Vec<Decimated<'_>, Alloc<'_>> = Vec::new_in(s);
    let mut counts: Vec<PartCount, Alloc<'a>> = Vec::new_in(o.alloc);
    counts.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "the export's counts" })?;
    if o.decimate {
        simple.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "the export's simplified meshes" })?;
        for (part, m) in parts.iter().zip(meshes.iter()) {
            let target = match part.budget {
                PartBudget::Auto => Target::Error(o.tolerance),
                PartBudget::MaxTriangles(cap) => Target::Budget { error: o.tolerance, max_triangles: cap.0, max_error: None },
                PartBudget::Detailed { max_triangles, tolerance, max_error, .. } => {
                    let tol = tolerance.unwrap_or(o.tolerance);
                    match max_triangles {
                        Some(cap) => Target::Budget { error: tol, max_triangles: cap.0, max_error },
                        None => Target::Error(tol),
                    }
                }
            };
            let d = decimate(
                Source::Verified(m.verified()),
                DecimateOpts { target, cos_crease: o.cos_crease, limits: MeshLimits::SHIPPED, alloc: s, scratch: s, .. },
            )?;
            simple.try_push(d).map_err(|_| ModelError::Alloc { what: "the export's simplified meshes" })?;
        }
    }

    // Build GPU-optimized render meshes (with per-part material role), optional LOD chains,
    // and optional physics collider hulls.
    let mut ready_meshes: Vec<Mesh<'_>, Alloc<'_>> = Vec::new_in(s);
    ready_meshes.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "the export's optimized meshes" })?;

    let mut ready_lods: Vec<ReadyLod<'_>, Alloc<'_>> = Vec::new_in(s);
    let mut lod_ranges: Vec<(usize, usize), Alloc<'_>> = Vec::new_in(s);
    lod_ranges.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "the export's lod ranges" })?;

    let mut ready_hulls: Vec<Mesh<'_>, Alloc<'_>> = Vec::new_in(s);
    let mut hull_ranges: Vec<(usize, usize), Alloc<'_>> = Vec::new_in(s);
    hull_ranges.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "the export's collider ranges" })?;

    for (n, (part, m)) in parts.iter().zip(meshes.iter()).enumerate() {
        let (base_view, verified_base, written, within) = match simple.get(n) {
            Some(d) => (
                d.mesh.view(),
                d.verified().unwrap_or_else(|| m.verified()),
                d.mesh.view().triangle_count(),
                d.error_bound,
            ),
            None => (m.mesh.view(), m.verified(), m.stats.triangles, Length::nm(0)),
        };
        let count = PartCount { meshed: m.stats.triangles, written, simplified_within: within, vertex_bound: m.vertex_bound };
        counts.try_push(count).map_err(|_| ModelError::Alloc { what: "the export's counts" })?;

        let opt_mesh = prepare_gpu_mesh(&base_view, Some(part.material), o.optimize_gpu, o.vertex_cache_size, s, s)?;
        ready_meshes.try_push(opt_mesh).map_err(|_| ModelError::Alloc { what: "the export's optimized meshes" })?;

        let lod_start = ready_lods.len();
        if let Some(lo) = o.lods {
            let chain = lod_chain(
                &verified_base,
                LodOpts {
                    ratio: lo.ratio,
                    fewest_triangles: lo.fewest_triangles,
                    projection: lo.projection,
                    cos_crease: o.cos_crease,
                    limits: MeshLimits::SHIPPED,
                    alloc: s,
                    scratch: s,
                },
            )?;
            for level in chain.levels.iter().skip(1) {
                let lv_mesh = prepare_gpu_mesh(&level.mesh.view(), Some(part.material), o.optimize_gpu, o.vertex_cache_size, s, s)?;
                ready_lods
                    .try_push(ReadyLod { mesh: lv_mesh, error: level.error, switch_distance: level.switch_distance })
                    .map_err(|_| ModelError::Alloc { what: "the export's lod meshes" })?;
            }
        }
        lod_ranges.try_push((lod_start, ready_lods.len())).map_err(|_| ModelError::Alloc { what: "the export's lod ranges" })?;

        let hull_start = ready_hulls.len();
        if let Some(mode) = o.colliders {
            match mode {
                ColliderMode::ConvexHull => {
                    let hull = convex_hull(base_view.positions(), HullOpts { limits: MeshLimits::SHIPPED, alloc: s, scratch: s })?;
                    let hm = hull_to_mesh(&hull, s)?;
                    ready_hulls.try_push(hm).map_err(|_| ModelError::Alloc { what: "the export's collider hulls" })?;
                }
                ColliderMode::Decompose { concavity, rays } => {
                    let decomp = decompose(
                        &verified_base,
                        DecomposeOpts { concavity, rays, limits: MeshLimits::SHIPPED, alloc: s, scratch: s },
                    )?;
                    for piece in decomp.pieces.iter() {
                        let hm = hull_to_mesh(&piece.hull, s)?;
                        ready_hulls.try_push(hm).map_err(|_| ModelError::Alloc { what: "the export's collider hulls" })?;
                    }
                }
            }
        }
        hull_ranges.try_push((hull_start, ready_hulls.len())).map_err(|_| ModelError::Alloc { what: "the export's collider ranges" })?;
    }

    let mut lod_views: Vec<Lod<'_>, Alloc<'_>> = Vec::new_in(s);
    lod_views.try_reserve(ready_lods.len()).map_err(|_| ModelError::Alloc { what: "the export's lod views" })?;
    for lv in ready_lods.iter() {
        lod_views
            .try_push(Lod { mesh: lv.mesh.view(), error: lv.error, switch_distance: lv.switch_distance })
            .map_err(|_| ModelError::Alloc { what: "the export's lod views" })?;
    }

    let mut hull_views: Vec<MeshView<'_>, Alloc<'_>> = Vec::new_in(s);
    hull_views.try_reserve(ready_hulls.len()).map_err(|_| ModelError::Alloc { what: "the export's collider views" })?;
    for hm in ready_hulls.iter() {
        hull_views.try_push(hm.view()).map_err(|_| ModelError::Alloc { what: "the export's collider views" })?;
    }

    let mut nodes: Vec<blaze_cad_out::Part<'_>, Alloc<'_>> = Vec::new_in(s);
    nodes.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "the export's nodes" })?;
    for (n, (part, m)) in parts.iter().zip(meshes.iter()).enumerate() {
        let view = ready_meshes[n].view();
        let (ls, le) = lod_ranges[n];
        let (hs, he) = hull_ranges[n];
        let mass = o.density.map(|d| {
            let mp = mass_properties(&m.verified(), d);
            MassFacts {
                mass: mp.mass.as_kg(),
                centre: [mp.centre[0].as_m(), mp.centre[1].as_m(), mp.centre[2].as_m()],
                inertia: mp.inertia,
            }
        });
        let collider = if hs < he { Some(Collider::Hulls(&hull_views[hs..he])) } else { None };
        let node = blaze_cad_out::Part {
            name: part.name.as_str(),
            parent: part.parent.map(|p| p.0),
            transform: transform_of(&part.placement),
            mesh: Some(view),
            lods: &lod_views[ls..le],
            mass,
            joint: None,
            collider,
        };
        nodes.try_push(node).map_err(|_| ModelError::Alloc { what: "the export's nodes" })?;
    }
    let scene = blaze_cad_out::Scene { parts: &nodes, materials: o.materials };
    let glb = glb(&scene, GlbOpts { encoding: o.encoding, up: o.up, limits: o.limits, alloc: o.alloc, scratch: s }).map_err(ModelError::Glb)?;
    Ok(Exported { glb, counts })
}

/// What [`export_stl`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct ExportStlOpts<'a, 's, 't> {
    /// Where the binary `.stl` bytes live.
    pub alloc: Alloc<'a>,
    /// Where the tape, mesh and simplifier tables live.
    pub scratch: Alloc<'s>,
    /// The most distance a vertex may be from the surface.
    pub tolerance: Length,
    /// Coordinate unit written into the binary STL (millimetres by default, for slicers/CAM).
    pub unit: StlUnit = StlUnit::Millimetres,
    /// Optional ASCII title embedded in the 80-byte STL header (defaults to the part's name when
    /// empty).
    pub title: &'t str = "",
    /// Simplify the verified mesh to `tolerance` before writing (preserving the verification proof).
    pub decimate: bool = true,
    /// Crease angle cosine used when simplifying.
    pub cos_crease: f32 = COS_32_DEGREES,
    pub limits: Limits = Limits::SHIPPED,
}

/// What [`export_stl`] made: the binary STL bytes and the part's triangle counts.
#[derive(Debug)]
pub struct ExportedStl<'a> {
    pub stl: Vec<u8, Alloc<'a>>,
    pub count: PartCount,
}

/// A part as a standard binary STL (verified closed manifold mesh, meshed in its own frame with
/// pieces read from `cache` when given).
pub fn export_stl<'a, K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    o: ExportStlOpts<'a, '_, '_>,
    cache: Option<&mut PieceCache<'_>>,
) -> Result<ExportedStl<'a>, ModelError> {
    let s = o.scratch;
    let m = mesh_part(g, part, MeshPartOpts { alloc: s, scratch: s, tolerance: o.tolerance, .. }, cache)?;
    let title = if o.title.is_empty() { part.name.as_str() } else { o.title };
    let (bytes, count) = if o.decimate {
        let d = decimate(
            Source::Verified(m.verified()),
            DecimateOpts { target: Target::Error(o.tolerance), cos_crease: o.cos_crease, limits: MeshLimits::SHIPPED, alloc: s, scratch: s, .. },
        )?;
        let verified = d.verified().unwrap_or_else(|| m.verified());
        let bytes = stl(&verified, StlOpts { title, unit: o.unit, limits: o.limits, alloc: o.alloc })?;
        let count = PartCount {
            meshed: m.stats.triangles,
            written: verified.view().triangle_count(),
            simplified_within: d.error_bound,
            vertex_bound: m.vertex_bound,
        };
        (bytes, count)
    } else {
        let bytes = stl(&m.verified(), StlOpts { title, unit: o.unit, limits: o.limits, alloc: o.alloc })?;
        let count = PartCount {
            meshed: m.stats.triangles,
            written: m.stats.triangles,
            simplified_within: Length::nm(0),
            vertex_bound: m.vertex_bound,
        };
        (bytes, count)
    };
    Ok(ExportedStl { stl: bytes, count })
}

/// What [`export_section_svg`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct SectionSvgOpts<'a, 's, 't> {
    /// Where the `.svg` UTF-8 bytes live.
    pub alloc: Alloc<'a>,
    /// Where the section tape, SIMD sample grid and contour tables live.
    pub scratch: Alloc<'s>,
    /// Grid sampling spacing along the section's `u` and `v` axes.
    pub step: Length,
    /// Cutting plane frame: maps 2D `(u, v, 0)` into 3D world space (defaults to the XY plane `z = 0`).
    pub plane: Placement = Placement::IDENTITY,
    /// Extra padding around the section box on every side (defaults to `2 * step` when `None`).
    pub margin: Option<Length> = None,
    /// Document `<title>` (omitted when empty).
    pub title: &'t str = "",
    /// CSS fill colour for closed cross-section regions.
    pub fill: &'t str = DEFAULT_FILL,
    /// CSS stroke colour for contour outlines.
    pub stroke: &'t str = DEFAULT_STROKE,
    /// Contour stroke width in millimetres (`0.0` picks a scale-proportional width).
    pub stroke_width: f64 = 0.0,
    /// Draw overall width and height dimension callouts in millimetres.
    pub show_dimensions: bool = true,
    /// Draw a subtle engineering grid and X/Y datum axes.
    pub show_grid: bool = true,
    /// Detail level compiled for the section.
    pub detail: Detail = Detail::Full,
    pub limits: Limits = Limits::SHIPPED,
}

/// Emit a standalone 2D vector engineering drawing (`.svg`) of `solid` sliced by `o.plane`:
/// builds the 2D section tape (`blaze_cad_field::section`), simplifies it to the slice plane,
/// samples the regular grid with 8-wide SIMD (`eval_points(..., Lanes::Eight, ...)`), and contours
/// it with `blaze_cad_out::svg` (contours, `fill-rule="evenodd"`, dimensions, and scale bar).
pub fn export_section_svg<'a, K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    solid: Solid<K>,
    o: SectionSvgOpts<'a, '_, '_>,
) -> Result<Vec<u8, Alloc<'a>>, ModelError> {
    if o.step <= Length::ZERO {
        return Err(ModelError::Argument { function: "export_section_svg", arg: "step", accepted: "a length above zero" });
    }
    if let Some(m) = o.margin
        && m < Length::ZERO
    {
        return Err(ModelError::Argument { function: "export_section_svg", arg: "margin", accepted: "a length at least zero" });
    }
    let s = o.scratch;
    let t3 = compile(g, solid.id(), CompileOpts { alloc: s, detail: o.detail, .. }).map_err(ModelError::Compile)?;
    let t2 = section(&t3, &o.plane, s).map_err(ModelError::Compile)?;
    let b = t2.aabb();
    if b.is_empty() {
        return Err(ModelError::EmptySolid { function: "export_section_svg", arg: "solid" });
    }
    if !(b.lo.x.is_finite() && b.lo.y.is_finite() && b.hi.x.is_finite() && b.hi.y.is_finite()) {
        return Err(ModelError::Inspect(InspectError::Unbounded { what: "the section's box" }));
    }

    let step = o.step.as_m();
    let margin = o.margin.map_or(2.0 * step, |m| m.as_m());
    let min_u = b.lo.x - margin;
    let max_u = b.hi.x + margin;
    let min_v = b.lo.y - margin;
    let max_v = b.hi.y + margin;

    let cells_u_f = blaze_math::ceil((max_u - min_u) / step).max(1.0);
    let cells_v_f = blaze_math::ceil((max_v - min_v) / step).max(1.0);
    if !(cells_u_f.is_finite() && cells_v_f.is_finite() && cells_u_f < f64::from(u32::MAX) && cells_v_f < f64::from(u32::MAX)) {
        return Err(ModelError::Svg(SvgError::TooManyCells { cells: u64::MAX, most: o.limits.most_svg_cells }));
    }
    let cells_u = cells_u_f as u64;
    let cells_v = cells_v_f as u64;
    let total_cells = cells_u.saturating_mul(cells_v);
    if total_cells > o.limits.most_svg_cells {
        return Err(ModelError::Svg(SvgError::TooManyCells { cells: total_cells, most: o.limits.most_svg_cells }));
    }
    let nx = (cells_u + 1) as u32;
    let ny = (cells_v + 1) as u32;
    let nx_us = nx as usize;
    let ny_us = ny as usize;
    let total_samples = nx_us.checked_mul(ny_us).ok_or(ModelError::Alloc { what: "the section's sample grid" })?;

    let origin_u = 0.5 * (min_u + max_u) - 0.5 * f64::from(nx - 1) * step;
    let origin_v = 0.5 * (min_v + max_v) - 0.5 * f64::from(ny - 1) * step;

    let mut sc = Scratch::new(s);
    // Simplify the section tape once over the exact 2D slice plane `w = 0` so features not
    // intersecting the cutting plane are pruned before SIMD grid sampling.
    let slice_box = Aabb3::new(
        Vec3::new(origin_u, origin_v, 0.0),
        Vec3::new(origin_u + f64::from(nx - 1) * step, origin_v + f64::from(ny - 1) * step, 0.0),
    );
    let simplified = if t2.len() >= 16 {
        let (_, choices) = eval_box(&t2, &slice_box, &mut sc);
        if choices.decided() > 0 { Some(simplify(&t2, choices, s)?) } else { None }
    } else {
        None
    };
    let eval_tape = simplified.as_ref().unwrap_or(&t2);

    let mut xs: Vec<f64, Alloc<'_>> = Vec::new_in(s);
    let mut ys: Vec<f64, Alloc<'_>> = Vec::new_in(s);
    let mut zs: Vec<f64, Alloc<'_>> = Vec::new_in(s);
    xs.try_reserve(nx_us).map_err(|_| ModelError::Alloc { what: "the section's x coordinates" })?;
    ys.try_reserve(nx_us).map_err(|_| ModelError::Alloc { what: "the section's y coordinates" })?;
    zs.try_reserve(nx_us).map_err(|_| ModelError::Alloc { what: "the section's z coordinates" })?;
    for ix in 0..nx {
        xs.try_push(origin_u + f64::from(ix) * step).map_err(|_| ModelError::Alloc { what: "the section's x coordinates" })?;
        ys.try_push(0.0).map_err(|_| ModelError::Alloc { what: "the section's y coordinates" })?;
        zs.try_push(0.0).map_err(|_| ModelError::Alloc { what: "the section's z coordinates" })?;
    }

    let mut values: Vec<f64, Alloc<'_>> = Vec::new_in(s);
    values.try_reserve(total_samples).map_err(|_| ModelError::Alloc { what: "the section's sample grid" })?;
    for _ in 0..total_samples {
        values.try_push(0.0).map_err(|_| ModelError::Alloc { what: "the section's sample grid" })?;
    }

    for iy in 0..ny {
        let y = origin_v + f64::from(iy) * step;
        ys.fill(y);
        let row_start = (iy as usize) * nx_us;
        let row = &mut values[row_start..row_start + nx_us];
        eval_points(eval_tape, Points { x: &xs, y: &ys, z: &zs }, row, Lanes::Eight, &mut sc)?;
    }

    let grid = SectionGrid { nx, ny, origin: [origin_u, origin_v], step, values: &values };
    svg(
        &grid,
        SvgOpts {
            title: o.title,
            fill: o.fill,
            stroke: o.stroke,
            stroke_width: o.stroke_width,
            show_dimensions: o.show_dimensions,
            show_grid: o.show_grid,
            limits: o.limits,
            alloc: o.alloc,
            scratch: s,
        },
    )
    .map_err(ModelError::Svg)
}

/// A picture of `solid` (compiled with every detail) from `view`.
pub fn picture<'a, K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    solid: Solid<K>,
    view: View,
    o: PictureOpts<'a, '_>,
) -> Result<Picture<'a>, ModelError> {
    let t = compile(g, solid.id(), CompileOpts { alloc: o.scratch, detail: Detail::Full, .. }).map_err(ModelError::Compile)?;
    render(&t, view, o).map_err(ModelError::Picture)
}

/// A studio-lit, 2×2 supersampled picture of `solid` from `view`: enables
/// [`Shading::Studio`](StudioOpts::DEFAULT) (unless `o.shading` is already `Shading::Studio`) and
/// `supersample = 2` (unless `o.supersample` is already `2`).
pub fn picture_studio<'a, K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    solid: Solid<K>,
    view: View,
    mut o: PictureOpts<'a, '_>,
) -> Result<Picture<'a>, ModelError> {
    if matches!(o.shading, Shading::Classic) {
        o.shading = Shading::Studio(StudioOpts::DEFAULT);
    }
    if o.supersample == 1 {
        o.supersample = 2;
    }
    picture(g, solid, view, o)
}

/// Render a preview picture directly from an exported triangle mesh (`MeshView`) with its
/// materials, shading normals, studio lighting / ray-traced shadows, and optional wireframe.
pub fn picture_mesh<'a>(
    mesh: MeshView<'_>,
    materials: &[Material<'_>],
    view: View,
    o: MeshPictureOpts<'a, '_, '_>,
) -> Result<Picture<'a>, ModelError> {
    render_mesh(mesh, materials, view, o).map_err(ModelError::Picture)
}

/// Render a preview picture directly from a multi-part exported scene (`blaze_cad_out::Scene`),
/// placing every part with its transform and parent hierarchy and shading with its materials.
pub fn picture_exported_scene<'a>(
    scene: &blaze_cad_out::Scene<'_>,
    view: View,
    o: MeshPictureOpts<'a, '_, '_>,
) -> Result<Picture<'a>, ModelError> {
    let mut parts: Vec<MeshScenePart<'_>, Alloc<'_>> = Vec::new_in(o.scratch);
    parts.try_reserve(scene.parts.len()).map_err(|_| ModelError::Alloc { what: "scene preview parts" })?;
    let mut remapped: Vec<Option<u32>, Alloc<'_>> = Vec::new_in(o.scratch);
    remapped.try_reserve(scene.parts.len()).map_err(|_| ModelError::Alloc { what: "scene preview parents" })?;
    for p in scene.parts {
        if let Some(mesh) = p.mesh {
            let parent = p.parent.and_then(|pi| remapped.get(pi as usize).copied().flatten());
            let idx = parts.len() as u32;
            parts
                .try_push(MeshScenePart { mesh, parent, transform: p.transform, tint_override: None })
                .map_err(|_| ModelError::Alloc { what: "scene preview parts" })?;
            remapped.try_push(Some(idx)).map_err(|_| ModelError::Alloc { what: "scene preview parents" })?;
        } else {
            remapped.try_push(None).map_err(|_| ModelError::Alloc { what: "scene preview parents" })?;
        }
    }
    render_mesh_scene(&parts, scene.materials, view, o).map_err(ModelError::Picture)
}
