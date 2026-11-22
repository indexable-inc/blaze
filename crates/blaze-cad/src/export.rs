//! A model out: a glTF binary of its parts (each meshed in its own frame, its placement on the
//! node, its mass when a density is given) and a picture of any solid.

#![blaze::sans_io]

use blaze_cad_field::{CadPlant, CompileOpts, Detail, Ex, Graph, Placement, Solid, compile};
use blaze_cad_mesh::{COS_32_DEGREES, DecimateOpts, Decimated, MeshLimits, Meshed, PieceCache, Source, Target, decimate};
use blaze_cad_out::{Encoding, GlbOpts, Limits, MassFacts, Material, Matrix, Transform, Up, glb};
use blaze_cad_picture::{Picture, PictureOpts, View, render};
use blaze_own::planted::Planted;
use blaze_own::{Alloc, Vec};
use blaze_units::{Density, Length};

use crate::{MeshPartOpts, ModelError, Part, mass_properties, mesh_part};

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
    /// The materials, by the meshes' roles (the mesher writes role 0).
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

/// The parts as a glTF binary: one node each, named, placed, meshed in its own frame (pieces read
/// from `cache` when given: a part only moved re-meshes nothing), simplified to the tolerance
/// unless [`ExportOpts::decimate`] is off; with each part's counts. Mass facts are from the
/// meshed (unsimplified) mesh.
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
        let m = mesh_part(g, part, MeshPartOpts { alloc: s, scratch: s, tolerance: o.tolerance }, cache.as_deref_mut())?;
        meshes.try_push(m).map_err(|_| ModelError::Alloc { what: "the export's meshes" })?;
    }
    let mut simple: Vec<Decimated<'_>, Alloc<'_>> = Vec::new_in(s);
    let mut counts: Vec<PartCount, Alloc<'a>> = Vec::new_in(o.alloc);
    counts.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "the export's counts" })?;
    if o.decimate {
        simple.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "the export's simplified meshes" })?;
        for m in meshes.iter() {
            let d = decimate(
                Source::Verified(m.verified()),
                DecimateOpts { target: Target::Error(o.tolerance), cos_crease: o.cos_crease, limits: MeshLimits::SHIPPED, alloc: s, scratch: s },
            )?;
            simple.try_push(d).map_err(|_| ModelError::Alloc { what: "the export's simplified meshes" })?;
        }
    }
    for (n, m) in meshes.iter().enumerate() {
        let (written, within) = match simple.get(n) {
            Some(d) => (d.mesh.view().triangle_count(), d.error_bound),
            None => (m.stats.triangles, Length::nm(0)),
        };
        let count = PartCount { meshed: m.stats.triangles, written, simplified_within: within, vertex_bound: m.vertex_bound };
        counts.try_push(count).map_err(|_| ModelError::Alloc { what: "the export's counts" })?;
    }
    let mut nodes: Vec<blaze_cad_out::Part<'_>, Alloc<'_>> = Vec::new_in(s);
    nodes.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "the export's nodes" })?;
    for (n, (part, m)) in parts.iter().zip(meshes.iter()).enumerate() {
        let view = match simple.get(n) {
            Some(d) => d.mesh.view(),
            None => m.mesh.view(),
        };
        let mass = o.density.map(|d| {
            let mp = mass_properties(&m.verified(), d);
            MassFacts {
                mass: mp.mass.as_kg(),
                centre: [mp.centre[0].as_m(), mp.centre[1].as_m(), mp.centre[2].as_m()],
                inertia: mp.inertia,
            }
        });
        let node = blaze_cad_out::Part {
            name: part.name.as_str(),
            parent: None,
            transform: transform_of(&part.placement),
            mesh: Some(view),
            lods: &[],
            mass,
            joint: None,
            collider: None,
        };
        nodes.try_push(node).map_err(|_| ModelError::Alloc { what: "the export's nodes" })?;
    }
    let scene = blaze_cad_out::Scene { parts: &nodes, materials: o.materials };
    let glb = glb(&scene, GlbOpts { encoding: o.encoding, up: o.up, limits: o.limits, alloc: o.alloc, scratch: s }).map_err(ModelError::Glb)?;
    Ok(Exported { glb, counts })
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
