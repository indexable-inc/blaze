//! Parts, assemblies, solved rounding, meshing a part, and mass.

#![blaze::sans_io]

use blaze_cad_field::{
    AtLeastInside, Bound, CadPlant, CompileOpts, Detail, Ex, Graph, GyroidOpts, Join, Name, OffsetOpts, Outside, Placement, Solid,
    Tape, Weakens, compile,
};
use blaze_cad_mesh::{
    DecimateOpts, Decimated, GridSizing, MaterialRole, MeshLimits, MeshOpts, Meshed, PieceCache, RedistanceOpts, Source, Target,
    Verified, auto_grid, decimate, mesh, mesh_cached, solve_redistance,
};
use blaze_own::Alloc;
use blaze_own::planted::{NoPlant, Planted};
use blaze_units::{Density, Length, Mass, Volume};

use crate::{MassPlant, ModelError, PartId, PartPlant, RoundPlant};

/// Explicit triangle count budget for a part or an entire game-asset bundle (rule 76/85).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TriangleBudget(pub u32);

impl TriangleBudget {
    /// The budget as a `u32` triangle count.
    #[inline]
    pub const fn count(self) -> u32 {
        self.0
    }
}

/// Per-part tessellation and triangle-budget policy used by [`crate::build_game_asset`] and
/// [`crate::export_glb`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PartBudget {
    /// Use scale-aware preset defaults and proportional share of any asset-level total budget.
    Auto,
    /// Cap this part at most `TriangleBudget` triangles after error-bounded simplification.
    MaxTriangles(TriangleBudget),
    /// Fine-grained per-part budget, tessellation tolerance, minimum feature size, error ceiling,
    /// and relative budget-allocation weight (`> 0.0`, default `1.0`).
    Detailed {
        max_triangles: Option<TriangleBudget>,
        tolerance: Option<Length>,
        min_feature: Option<Length>,
        max_error: Option<Length>,
        weight: f32,
    },
}

impl PartBudget {
    /// Convenience constructor for an explicit per-part triangle cap.
    #[inline]
    pub const fn max_triangles(triangles: u32) -> PartBudget {
        PartBudget::MaxTriangles(TriangleBudget(triangles))
    }

    /// Explicit triangle cap if set on this part.
    #[inline]
    pub const fn explicit_cap(self) -> Option<u32> {
        match self {
            PartBudget::Auto => None,
            PartBudget::MaxTriangles(b) => Some(b.0),
            PartBudget::Detailed { max_triangles, .. } => match max_triangles {
                Some(b) => Some(b.0),
                None => None,
            },
        }
    }

    /// Relative allocation weight used when distributing an asset-level total budget.
    #[inline]
    pub fn weight(self) -> f32 {
        match self {
            PartBudget::Detailed { weight, .. } if weight.is_finite() && weight > 0.0 => weight,
            _ => 1.0,
        }
    }
}

/// A named solid and where it sits. PLACEMENT IS NOT GEOMETRY: `solid` is the unplaced shape
/// (its digest, and so every mesh piece and check record of it, does not move when the part
/// does) and `placement` puts it in the assembly. `K` is the solid's level, kept so a check that
/// needs an exact inside ([`crate::min_wall`]) can take the part; [`Part::weaken`] makes the
/// `Part<Bound>` an assembly takes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Part<K: Ex = Bound> {
    pub solid: Solid<K>,
    pub placement: Placement,
    pub name: Name,
    /// Material role index into `ExportOpts::materials` (defaults to `MaterialRole(0)`).
    pub material: MaterialRole,
    /// Optional parent part in a scene/model hierarchy (defaults to `None` for a root node).
    pub parent: Option<PartId>,
    /// Per-part triangle budget and tessellation override (defaults to [`PartBudget::Auto`]).
    pub budget: PartBudget,
}

/// What [`Part::new`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct PartOpts<'n, K: Ex> {
    /// The solid, placed where it goes: the placement is split off it.
    pub solid: Solid<K>,
    /// Printed in errors and written as the glTF node's name.
    pub name: &'n str,
}

impl<K: Ex> Part<K> {
    /// A part from a placed solid: its placement is taken off (`Graph::split_placement`) and
    /// kept beside it.
    pub fn new<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, o: PartOpts<'_, K>) -> Result<Part<K>, ModelError> {
        Part::new_planted(g, o, NoPlant)
    }

    /// [`Part::new`] with a planted bug, for the tests' controls.
    pub fn new_planted<P: Planted<CadPlant>>(
        g: &mut Graph<'_, P>,
        o: PartOpts<'_, K>,
        plant: impl Planted<PartPlant>,
    ) -> Result<Part<K>, ModelError> {
        if plant.has(PartPlant::PlacementKept) {
            return Ok(Part {
                solid: o.solid,
                placement: Placement::IDENTITY,
                name: Name::new(o.name),
                material: MaterialRole(0),
                parent: None,
                budget: PartBudget::Auto,
            });
        }
        let (placement, local) = g.split_placement(o.solid).map_err(ModelError::Cad)?;
        Ok(Part {
            solid: local,
            placement,
            name: Name::new(o.name),
            material: MaterialRole(0),
            parent: None,
            budget: PartBudget::Auto,
        })
    }

    /// The same part somewhere else: only the placement changes, so its pieces and records stay.
    pub fn moved_to(self, placement: Placement) -> Part<K> {
        Part { placement, ..self }
    }

    /// The same part assigned a material role for export (`ExportOpts::materials[role.0]`).
    pub fn with_material(self, material: MaterialRole) -> Part<K> {
        Part { material, ..self }
    }

    /// The same part attached under `parent` in the exported scene hierarchy.
    pub fn with_parent(self, parent: Option<PartId>) -> Part<K> {
        Part { parent, ..self }
    }

    /// The same part assigned a triangle budget or tessellation override.
    pub fn with_budget(self, budget: PartBudget) -> Part<K> {
        Part { budget, ..self }
    }

    /// The same part at a weaker level (an assembly takes `Part<Bound>`).
    pub fn weaken<J: Ex>(self) -> Part<J>
    where
        K: Weakens<J>,
    {
        Part {
            solid: self.solid.weaken::<J>(),
            placement: self.placement,
            name: self.name,
            material: self.material,
            parent: self.parent,
            budget: self.budget,
        }
    }
}

/// A triply periodic minimal surface (TPMS) gyroid sheet: intersect with a finite solid to
/// produce a lattice/infill part.
pub fn gyroid<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, o: GyroidOpts<'_>) -> Result<Solid<Bound>, ModelError> {
    g.gyroid(o).map_err(ModelError::Cad)
}

/// Balanced binary tree union of `solids` (depth `ceil(log2(n))`, preserving exactness level).
pub fn union_all<K: Join<K>, P: Planted<CadPlant>>(
    g: &mut Graph<'_, P>,
    solids: &[Solid<K>],
) -> Result<Solid<K::Union>, ModelError> {
    g.union_all(solids).map_err(ModelError::Cad)
}

/// Balanced binary tree intersection of `solids` (depth `ceil(log2(n))`, preserving exactness level).
pub fn intersect_all<K: Join<K>, P: Planted<CadPlant>>(
    g: &mut Graph<'_, P>,
    solids: &[Solid<K>],
) -> Result<Solid<K::Intersect>, ModelError> {
    g.intersect_all(solids).map_err(ModelError::Cad)
}

/// The union of the parts, each at its placement, as a balanced tree: moving one part re-keys its
/// placement node and the `log2(n)` unions above it, nothing else.
pub fn assemble<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, parts: &[Part]) -> Result<Solid<Bound>, ModelError> {
    if parts.is_empty() {
        return Err(ModelError::Argument { function: "assemble", arg: "parts", accepted: "at least one part" });
    }
    tree(g, parts)
}

/// Depth `log2(parts.len())`.
fn tree<P: Planted<CadPlant>>(g: &mut Graph<'_, P>, parts: &[Part]) -> Result<Solid<Bound>, ModelError> {
    if let [one] = parts {
        return g.place(one.solid, one.placement).map_err(ModelError::Cad);
    }
    let (left, right) = parts.split_at(parts.len() / 2);
    let a = tree(g, left)?;
    let b = tree(g, right)?;
    g.union(a, b).map_err(ModelError::Cad)
}

/// The part where it sits, as a solid of the assembly's frame.
pub fn placed<K: Ex, P: Planted<CadPlant>>(g: &mut Graph<'_, P>, part: &Part<K>) -> Result<Solid<K>, ModelError> {
    g.place(part.solid, part.placement).map_err(ModelError::Cad)
}

/// What [`round_edges`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct RoundEdgesOpts<'s, 'n> {
    /// Every convex edge's radius.
    pub radius: Length,
    /// The distance grid's spacing: the rounding is right to about this.
    pub step: Length,
    /// Where the solver's samples live.
    pub scratch: Alloc<'s>,
    pub name: Option<&'n str> = None,
}

/// Every convex edge of `s` rounded by `radius`, ready to compile: erode by the radius, take the
/// exact distance of that (`Graph::redistance`, solved here on a grid of `step` by
/// blaze-cad-mesh's `solve_redistance`), grow it back. The kernel's own `Graph::round_edges`
/// leaves the grid for the mesher to attach; this one attaches it, so the result compiles,
/// renders and is checked at once.
pub fn round_edges<K: AtLeastInside, P: Planted<CadPlant>>(
    g: &mut Graph<'_, P>,
    s: Solid<K>,
    o: RoundEdgesOpts<'_, '_>,
) -> Result<Solid<Outside>, ModelError> {
    round_edges_planted(g, s, o, NoPlant)
}

/// [`round_edges`] with a planted bug, for the tests' controls.
pub fn round_edges_planted<K: AtLeastInside, P: Planted<CadPlant>>(
    g: &mut Graph<'_, P>,
    s: Solid<K>,
    o: RoundEdgesOpts<'_, '_>,
    plant: impl Planted<RoundPlant>,
) -> Result<Solid<Outside>, ModelError> {
    if o.radius <= Length::ZERO {
        return Err(ModelError::Argument { function: "round_edges", arg: "radius", accepted: "a length above zero" });
    }
    if o.step <= Length::ZERO {
        return Err(ModelError::Argument { function: "round_edges", arg: "step", accepted: "a length above zero" });
    }
    let eroded = g.erode(s, OffsetOpts { by: o.radius, .. }).map_err(ModelError::Cad)?;
    let exact = g.redistance(eroded).map_err(ModelError::Cad)?;
    // The grown surface is where the distance is `radius`: the band holds that and four steps.
    let band = o.radius + o.step * 4.0;
    solve_redistance(g, exact, RedistanceOpts { scratch: o.scratch, step: o.step, band, .. }).map_err(ModelError::Mesh)?;
    let by = if plant.has(RoundPlant::GrowShort) { o.radius / 2.0 } else { o.radius };
    g.grow(exact, OffsetOpts { by, name: o.name }).map_err(ModelError::Cad)
}

/// The part's own tape (unplaced), at `detail`: what a check of the part alone and its mesh read.
pub fn part_tape<'a, K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    detail: Detail,
    alloc: Alloc<'a>,
) -> Result<Tape<'a>, ModelError> {
    compile(g, part.solid.id(), CompileOpts { alloc, detail, .. }).map_err(ModelError::Compile)
}

/// The part's tape where it sits in the assembly, at `detail`: what a check between parts reads.
pub fn placed_tape<'a, K: Ex, P: Planted<CadPlant>>(
    g: &mut Graph<'_, P>,
    part: &Part<K>,
    detail: Detail,
    alloc: Alloc<'a>,
) -> Result<Tape<'a>, ModelError> {
    let s = placed(g, part)?;
    compile(g, s.id(), CompileOpts { alloc, detail, .. }).map_err(ModelError::Compile)
}

/// What [`mesh_part`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct MeshPartOpts<'a, 's> {
    /// Where the mesh lives.
    pub alloc: Alloc<'a>,
    /// Where the tape and the mesher's lists live.
    pub scratch: Alloc<'s>,
    /// The most distance a vertex may be from the surface.
    pub tolerance: Length,
    /// Optional explicit octree piece size (overrides [`Self::auto_grid`]).
    pub piece: Option<Length> = None,
    /// Optional explicit octree feature cell ceiling (overrides [`Self::auto_grid`]).
    pub feature: Option<Length> = None,
    /// Smallest feature size (hole/trim/bevel width) that must never fall between octree
    /// lattice samples when [`Self::auto_grid`] computes the grid.
    pub min_feature: Option<Length> = None,
    /// Use scale-aware dyadic grid sizing (`blaze_cad_mesh::auto_grid`) so meter-scale flat
    /// faces do not explode into hundreds of thousands of tiny cells before decimation.
    pub auto_grid: bool = true,
}

/// The part's mesh in its own frame (the placement goes on the glTF node), each piece read from
/// `cache` when given and there: moving a part re-meshes nothing.
pub fn mesh_part<'a, K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    o: MeshPartOpts<'a, '_>,
    cache: Option<&mut PieceCache<'_>>,
) -> Result<Meshed<'a>, ModelError> {
    let t = part_tape(g, part, Detail::Full, o.scratch)?;
    let GridSizing { piece, feature } = if o.auto_grid {
        let auto = auto_grid(&t.aabb(), o.tolerance, o.min_feature);
        GridSizing { piece: o.piece.unwrap_or(auto.piece), feature: o.feature.unwrap_or(auto.feature) }
    } else {
        let def = MeshOpts { alloc: o.alloc, scratch: o.scratch, tolerance: o.tolerance, .. };
        GridSizing { piece: o.piece.unwrap_or(def.piece), feature: o.feature.unwrap_or(def.feature) }
    };
    let opts = MeshOpts { alloc: o.alloc, scratch: o.scratch, tolerance: o.tolerance, piece, feature, .. };
    match cache {
        Some(c) => mesh_cached(&t, opts, c),
        None => mesh(&t, opts),
    }
    .map_err(ModelError::Mesh)
}

/// Options for [`decimate_mesh`]: error-bounded or triangle-budget Garland-Heckbert QEM
/// simplification preserving sharp creases, boundaries, and material borders.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct DecimatePartOpts<'a, 's> {
    /// Simplification goal: `Target::Error(Length)` (two-sided Hausdorff bound) or `Target::Triangles(usize)`.
    pub target: Target,
    /// Crease angle cosine where sharp edges are preserved (default: `cos(30°)` ≈ `0.8660254`).
    pub cos_crease: f32 = 0.866_025_4,
    /// Mesh safety limits.
    pub limits: MeshLimits = MeshLimits::SHIPPED,
    /// Allocator for the output simplified mesh.
    pub alloc: Alloc<'a>,
    /// Scratch allocator for quadrics, successive-map pieces, and collapse heap.
    pub scratch: Alloc<'s>,
}

/// Simplifies a verified part mesh by Garland-Heckbert QEM edge collapse with Cohen-Olano-Manocha
/// successive-mapping two-sided Hausdorff error bounds, preserving sharp creases and manifoldness.
///
/// The returned [`Decimated`] carries the exact two-sided `error_bound`, work counts (`stats`),
/// and a re-verified [`Verified`] proof (`decimated.verified()`).
pub fn decimate_mesh<'a>(mesh: &Verified<'_>, o: DecimatePartOpts<'a, '_>) -> Result<Decimated<'a>, ModelError> {
    decimate(
        Source::Verified(*mesh),
        DecimateOpts {
            target: o.target,
            cos_crease: o.cos_crease,
            limits: o.limits,
            alloc: o.alloc,
            scratch: o.scratch,
            ..
        },
    )
    .map_err(ModelError::Decimate)
}

/// A part's mass, centre and inertia.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MassProperties {
    pub volume: Volume,
    pub mass: Mass,
    /// The centre of mass, in the mesh's frame.
    pub centre: [Length; 3],
    /// The inertia tensor about the centre, kilogram square metres, the mesh's axes.
    pub inertia: [[f64; 3]; 3],
}

/// Mass properties of a verified mesh of uniform `density`, by exact polyhedral integrals over
/// its triangles (blaze-cad-mesh's `mass_properties`): exact for the mesh, and within the
/// mesher's stated bound of the solid's.
pub fn mass_properties(mesh: &Verified<'_>, density: Density) -> MassProperties {
    mass_properties_planted(mesh, density, NoPlant)
}

/// [`mass_properties`] with a planted bug, for the tests' controls.
pub fn mass_properties_planted(mesh: &Verified<'_>, density: Density, plant: impl Planted<MassPlant>) -> MassProperties {
    let m = blaze_cad_mesh::mass_properties(mesh);
    let rho = if plant.has(MassPlant::InertiaUnscaled) { 1.0 } else { density.as_kg_per_m3() };
    let mut inertia = [[0.0f64; 3]; 3];
    for (row, unit) in inertia.iter_mut().zip(m.inertia.iter()) {
        for (x, u) in row.iter_mut().zip(unit.iter()) {
            *x = u * rho;
        }
    }
    MassProperties {
        volume: m.volume,
        mass: density * m.volume,
        centre: [Length::m(m.centroid[0]), Length::m(m.centroid[1]), Length::m(m.centroid[2])],
        inertia,
    }
}
