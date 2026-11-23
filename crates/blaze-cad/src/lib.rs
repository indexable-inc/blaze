//! BLAZE-CAD (D183): the CAD library a program imports to build ships, one `use` away from every
//! piece: solids from blaze-cad-field, meshes from blaze-cad-mesh, pictures from
//! blaze-cad-picture, files from blaze-cad-out, all in blaze-units' quantities.
//!
//! # A model, in the shape an author (or a language model) writes it
//!
//! ```ignore
//! use blaze_cad::prelude::*;
//!
//! let mut model = Model::new(ModelOpts { alloc, .. });
//! let g = model.graph();
//! let plate = g.cuboid(BoxOpts { size: [Length::mm(600.0), Length::mm(400.0), Length::mm(12.0)], name: Some("deck"), .. })?;
//! let hole = g.cuboid(BoxOpts { size: [Length::mm(300.0), Length::mm(200.0), Length::mm(60.0)], round: Length::mm(25.0), .. })?;
//! let deck = g.difference(plate, hole)?;
//! let (_, deck) = model.add(PartOpts { solid: deck, name: "deck" })?;
//! let wall = min_wall(model.graph_ref(), &deck, MinWallOpts { wall: Length::mm(6.0), scratch, .. })?;
//! println!("deck, 6 mm: {}", Shown(&wall.verdict)); // "pass", or "thin wall of about 4.2 mm at (...) mm"
//! assert!(wall.verdict.passed());
//! ```
//!
//! Every argument is named (`T::new(TOpts { .. })`, rule 83), every length is a [`Length`]
//! (never a bare number of unknown unit), every choice is a field with no default and every
//! tunable has one, and every error names the function and the argument it refuses
//! ([`ModelError::Argument`]) or carries the kernel's own, which names the node.
//!
//! # The pieces
//!
//! | piece | what | where |
//! |---|---|---|
//! | [`Part`] | a named solid, its placement, material role and parent, kept apart from geometry | `part` |
//! | [`assemble`] | the union of placed parts, a balanced tree | `part` |
//! | [`gyroid`] [`union_all`] [`intersect_all`] | TPMS gyroid lattice and balanced N-way booleans | `part` |
//! | [`round_edges`] | erode, solved redistance, grow: rounded edges that compile at once | `part` |
//! | [`mesh_part`] | a part's mesh in its own frame, pieces from a [`PieceCache`] | `part` |
//! | [`mass_properties`] | volume, mass, centre and inertia of a verified mesh at a density | `part` |
//! | [`min_wall`] [`no_interference`] [`no_interference_assembly`] [`fits_within`] | geometric & assembly checks, three-way verdicts with keys | `check` |
//! | [`overhang_check`] [`draft_angle_check`] | additive (3D printing) & molding/casting DFM checks | `check` |
//! | [`export_glb`] [`export_stl`] [`export_section_svg`] [`picture`] [`picture_studio`] | GPU-ready glTF, binary STL, 2D SVG engineering sections, and studio pictures | `export` |
//! | [`Model`] | the graph and its named parts together | here |
//!
//! # Records and checkpoints
//!
//! What is kept is said where it is made: mesh pieces under blaze-cad-mesh's `RegionKey` (in a
//! session's [`PieceCache`], written out at a checkpoint: a check, an export, a commit), check
//! verdicts under [`CheckKey`]. A cheap operation (adding a part, moving one, compiling a tape)
//! writes nothing: tapes and graphs are recomputed from the text in microseconds.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes, core_float_math, default_field_values)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod asset;
mod camera_fidelity;
mod check;
mod export;
mod fidelity;
mod part;
mod region;
mod surface;

#[cfg(test)]
mod math_tests;
#[cfg(test)]
mod tests;

pub use asset::{
    AssetBakeOpts, AssetFidelityPolicy, AssetPreset, AssetReport, BudgetInput, BudgetStatus, ExportedPart, GameAsset,
    GameAssetOpts, PartDecal, PartFeature, PartFeatureReport, PartMaterialRegion, PartReport, PublishCheckOpts, PublishFailure,
    PublishPlant, PublishProof, allocate_budgets, build_game_asset, build_game_asset_planted, publish_game_asset,
};
pub use blaze_cad_mesh::{
    CreaseProfile, CreaseProfileOpts, DecimateStats, Decimated, FeatureProtect, FeatureRegion, FeatureZone, GridSizing,
    MaterialRole, MeshLimits, PieceCache, PieceCacheOpts, Stop, Target, Topology, UvError, UvMapped, UvOpts, UvPlant, auto_grid,
    compute_tangents, crease_profile, topology, unwrap_uvs,
};
pub use blaze_cad_out::{
    Encoding, GlbReadError, GlbReadOpts, LoadedGlb, Material, MaterialTextures, StlUnit, TexturePack, Up, decode_png,
    glb_textured, read_glb,
};
pub use blaze_cad_picture::{
    HighlightStyle, MaterialMaps, MeshHighlight, MeshNormalSource, MeshPictureOpts, MeshScenePart, PictureDiff, RenderMode,
    Shading, StripLight, StudioOpts, TextureMap, Wireframe, ZebraOpts, compose_grid, compose_side_by_side, diff_pictures,
};
pub use camera_fidelity::{
    CameraFidelityOpts, CameraFidelityPlant, CameraFidelityReport, CameraLodReport, DecalLegibility, DecalSpec,
    GameCameraPreset, GameCameraRig, GlbPreviewOpts, GlbRenderSet, GlbRevisionComparison, GlbRevisionView, GlbViewRender,
    NamedCamera, SeamSpec, SeamVisibility, SilhouetteShift, check_camera_fidelity, check_camera_fidelity_planted,
    compare_glb_revisions, render_glb_views,
};
pub use check::{
    AssemblyInterference, CHECK_SCHEMA, CheckKey, CheckKind, Checked, DraftFace, DraftOpts, FitOpts, Interference, InterferenceOpts,
    MinWallOpts, Overhang, OverhangFace, OverhangOpts, ThinWall, Unsettled, Verdict, draft_angle_check, draft_angle_check_planted,
    fits_within, fits_within_planted, min_wall, min_wall_planted, no_interference, no_interference_assembly,
    no_interference_planted, overhang_check, overhang_check_planted,
};
/// A finding, a verdict or any other [`blaze_text::Show`] as a `Display`:
/// `println!("{}", Shown(&checked.verdict))` prints "thin wall of about 4.2 mm at (295.898,
/// 290.039, -0.586) mm".
pub use blaze_text::Shown;
pub use export::{
    ColliderMode, ExportLodOpts, ExportOpts, ExportStlOpts, Exported, ExportedStl, LodProjection, PartCount, STEEL, STEEL_ONLY,
    SectionSvgOpts, export_glb, export_section_svg, export_stl, picture, picture_exported_scene, picture_mesh, picture_studio,
    transform_of,
};
pub use fidelity::{
    FeatureKind, FeatureReport, FeatureSpec, FeatureTarget, FeatureTargetOpts, FidelityChecked, FidelityFinding, FidelityMetrics,
    FidelityOpts, FidelityPlant, FidelityReport, check_mesh_fidelity, check_mesh_fidelity_planted, check_mesh_fidelity_report,
    check_mesh_fidelity_report_planted,
};
pub use part::{
    DecimatePartOpts, MassProperties, MeshPartOpts, Part, PartBudget, PartOpts, RoundEdgesOpts, TriangleBudget, assemble,
    decimate_mesh, gyroid, intersect_all, mass_properties, mass_properties_planted, mesh_part, part_tape, placed, placed_tape,
    round_edges, round_edges_planted, union_all,
};
pub use region::{
    AttachmentFrame, Curve2dPoint, DerivedPanel, FilletKind, PanelOpts, PatternOpts, PatternResult, ProjectCurveOpts,
    RegionSelector, SelectedRoundOpts, SurfaceCurve, SurfaceCurvePoint, SurfacePattern, SurfaceRegion, SurfaceRegionOpts,
    SurfaceSweepOpts, SweptSurfaceFeature, TrimMode, derive_panel, fill_region_pattern, project_curve_to_surface, resolve_region,
    round_selected_edges, snap_to_surface, sweep_on_surface,
};
pub use surface::{
    BakeOpts, BakedMaps, BakedTexture, Decal, DecalMaterial, DecalOpts, DecalProjection, DecalSample, DecalShape, EdgeWearMask,
    FinishAssignment, FinishSample, MAX_SVG_SEGMENTS, MaterialRegion, MaterialRule, SeamClip, SurfaceFinish, SvgPath2D,
    SvgSegment, assign_material_regions, bake_surface_maps, baked_material, decal_feature_target, decal_stroke_width,
    evaluate_decal, evaluate_decal_with_footprint, evaluate_finish, parse_svg_path, svg_path_sd,
};

use blaze_cad_field::{Bound, CadError, CompileError, EvalError, Ex, Graph, InspectError, Name, Placement, Solid, Weakens};
use blaze_cad_mesh::{CacheError, DecimateError, DecomposeError, HullError, LodError, MeshError, ShapeError};
use blaze_cad_out::{GlbError, PngError, StlError, SvgError};
use blaze_cad_picture::PictureError;
use blaze_own::planted::{NoPlant, Plants};
use blaze_own::{Alloc, Vec};
use blaze_units::Length;

/// What the library refuses, each naming the thing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ModelError {
    /// An argument outside what `function` accepts.
    Argument { function: &'static str, arg: &'static str, accepted: &'static str },
    /// A solid with no points where one is needed.
    EmptySolid { function: &'static str, arg: &'static str },
    /// A part's name already taken in the model.
    DuplicateName { name: Name },
    /// A [`PartFeature`] targets a part name that does not exist in the model/export.
    UnknownFeaturePart { part: Name, feature: Name },
    /// Two [`PartFeature`] targets on the same part share the same feature name.
    DuplicateFeatureName { part: Name, feature: Name },
    /// A [`FeatureTarget`] has an invalid region or tolerance.
    InvalidFeatureTarget { part: Name, feature: Name, reason: &'static str },
    /// The asset failed [`PublishCheckOpts`] (budget, part fidelity, or requested feature targets).
    PublishRejected(PublishFailure),
    /// A part id the model never gave.
    NoPart { id: u32 },
    /// More parts than the model's cap.
    TooManyParts { most: u32 },
    /// An allocation for `what` refused.
    Alloc { what: &'static str },
    /// The kernel refused a node (it names the node and the argument).
    Cad(CadError),
    Compile(CompileError),
    Eval(EvalError),
    Inspect(InspectError),
    Mesh(MeshError),
    Decimate(DecimateError),
    Cache(CacheError),
    Lod(LodError),
    Hull(HullError),
    Decompose(DecomposeError),
    Shape(ShapeError),
    Uv(UvError),
    Picture(PictureError),
    Glb(GlbError),
    Stl(StlError),
    Svg(SvgError),
    Png(PngError),
}

// `?` on a layer's error gives the model's: one error type for an author to match.
impl From<CadError> for ModelError {
    fn from(e: CadError) -> ModelError {
        ModelError::Cad(e)
    }
}

impl From<CompileError> for ModelError {
    fn from(e: CompileError) -> ModelError {
        ModelError::Compile(e)
    }
}

impl From<EvalError> for ModelError {
    fn from(e: EvalError) -> ModelError {
        ModelError::Eval(e)
    }
}

impl From<InspectError> for ModelError {
    fn from(e: InspectError) -> ModelError {
        ModelError::Inspect(e)
    }
}

impl From<MeshError> for ModelError {
    fn from(e: MeshError) -> ModelError {
        ModelError::Mesh(e)
    }
}

impl From<PictureError> for ModelError {
    fn from(e: PictureError) -> ModelError {
        ModelError::Picture(e)
    }
}

impl From<DecimateError> for ModelError {
    fn from(e: DecimateError) -> ModelError {
        ModelError::Decimate(e)
    }
}

impl From<CacheError> for ModelError {
    fn from(e: CacheError) -> ModelError {
        ModelError::Cache(e)
    }
}

impl From<LodError> for ModelError {
    fn from(e: LodError) -> ModelError {
        ModelError::Lod(e)
    }
}

impl From<HullError> for ModelError {
    fn from(e: HullError) -> ModelError {
        ModelError::Hull(e)
    }
}

impl From<DecomposeError> for ModelError {
    fn from(e: DecomposeError) -> ModelError {
        ModelError::Decompose(e)
    }
}

impl From<ShapeError> for ModelError {
    fn from(e: ShapeError) -> ModelError {
        ModelError::Shape(e)
    }
}

impl From<UvError> for ModelError {
    fn from(e: UvError) -> ModelError {
        ModelError::Uv(e)
    }
}

impl From<GlbError> for ModelError {
    fn from(e: GlbError) -> ModelError {
        ModelError::Glb(e)
    }
}

impl From<StlError> for ModelError {
    fn from(e: StlError) -> ModelError {
        ModelError::Stl(e)
    }
}

impl From<SvgError> for ModelError {
    fn from(e: SvgError) -> ModelError {
        ModelError::Svg(e)
    }
}

impl From<PngError> for ModelError {
    fn from(e: PngError) -> ModelError {
        ModelError::Png(e)
    }
}

impl From<PublishFailure> for ModelError {
    fn from(e: PublishFailure) -> ModelError {
        ModelError::PublishRejected(e)
    }
}

/// [`Part::new`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PartPlant {
    None,
    /// The placement left in the solid: a moved part gets a new tape digest, and every piece
    /// and check record of it is recomputed.
    PlacementKept,
}

impl Plants for PartPlant {}

/// [`round_edges`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RoundPlant {
    None,
    /// Grown back by half the radius: every face moves in.
    GrowShort,
}

impl Plants for RoundPlant {}

/// [`mass_properties`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MassPlant {
    None,
    /// The inertia left per unit density.
    InertiaUnscaled,
}

impl Plants for MassPlant {}

/// [`min_wall`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WallPlant {
    None,
    /// Thin read as "a ball of diameter `wall` tangent at the point does not fit": every sharp
    /// convex edge of a thick part is reported (the false finding the inward chord removes).
    TangentBall,
    /// The bracketing removed: a sample is found by Newton's steps alone until its budget is
    /// spent, with no sign bracket and no split. On a field that is only a bound outside the
    /// solid the steps can go back and forth for ever (a ring cut by a taller cylinder, in the
    /// hole between the two caps' planes), and the place is left unsettled.
    NoBracket,
}

impl Plants for WallPlant {}

/// [`no_interference`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InterferencePlant {
    None,
    /// An unsettled gap (the budget spent, the clearance inside the interval) reported as a pass.
    UnknownPasses,
}

impl Plants for InterferencePlant {}

/// [`fits_within`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FitPlant {
    None,
    /// The envelope without the part searched, in place of the part without the envelope.
    Reversed,
}

impl Plants for FitPlant {}

/// [`overhang_check`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OverhangPlant {
    None,
    /// `build_dir` inverted when evaluating overhang normals: upward-facing roofs are flagged
    /// as overhangs while downward-facing overhangs are missed.
    FlippedBuildDir,
}

impl Plants for OverhangPlant {}

/// [`draft_angle_check`]'s planted bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DraftPlant {
    None,
    /// Uses `|n · pull_dir|` instead of signed `n · pull_dir`: undercuts (negative draft) are
    /// misclassified as positive draft and falsely pass.
    IgnoreUndercutSign,
}

impl Plants for DraftPlant {}

/// A part's index in its [`Model`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PartId(pub u32);

/// What [`Model::new`] takes.
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct ModelOpts<'a> {
    /// Where the graph and the part list live.
    pub alloc: Alloc<'a>,
    /// The graph's bounds.
    pub limits: blaze_cad_field::Limits = blaze_cad_field::Limits::SHIPPED,
    /// The most parts (rule 18).
    pub most_parts: u32 = 1 << 16,
}

/// The graph and its named parts. Building solids is the graph's ([`Model::graph`]); the model
/// keeps which of them are parts, under unique names, and where each sits.
pub struct Model<'a> {
    graph: Graph<'a, NoPlant>,
    parts: Vec<Part, Alloc<'a>>,
    features: Vec<PartFeature, Alloc<'a>>,
    most_parts: u32,
}

impl<'a> Model<'a> {
    /// An empty model.
    pub fn new(o: ModelOpts<'a>) -> Model<'a> {
        Model {
            graph: Graph::new(blaze_cad_field::GraphOpts { alloc: o.alloc, limits: o.limits }),
            parts: Vec::new_in(o.alloc),
            features: Vec::new_in(o.alloc),
            most_parts: o.most_parts,
        }
    }

    /// The graph, to build solids in.
    pub fn graph(&mut self) -> &mut Graph<'a, NoPlant> {
        &mut self.graph
    }

    /// The graph, to compile from.
    pub fn graph_ref(&self) -> &Graph<'a, NoPlant> {
        &self.graph
    }

    /// The placed solid as a named part: its id in the model and the part at its own level (for
    /// the checks that need it, such as [`min_wall`]).
    pub fn add<K: Ex + Weakens<Bound>>(&mut self, o: PartOpts<'_, K>) -> Result<(PartId, Part<K>), ModelError> {
        let name = Name::new(o.name);
        if self.parts.iter().any(|p| p.name == name) {
            return Err(ModelError::DuplicateName { name });
        }
        let id = u32::try_from(self.parts.len()).map_err(|_| ModelError::TooManyParts { most: self.most_parts })?;
        if id >= self.most_parts {
            return Err(ModelError::TooManyParts { most: self.most_parts });
        }
        let part = Part::new(&mut self.graph, o)?;
        self.parts.try_push(part.weaken::<Bound>()).map_err(|_| ModelError::Alloc { what: "the model's parts" })?;
        Ok((PartId(id), part))
    }

    /// Every part, in the order added.
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// One part.
    pub fn part(&self, id: PartId) -> Result<&Part, ModelError> {
        let at = usize::try_from(id.0).map_err(|_| ModelError::NoPart { id: id.0 })?;
        self.parts.get(at).ok_or(ModelError::NoPart { id: id.0 })
    }

    /// Move one part: only its placement changes.
    pub fn move_part(&mut self, id: PartId, placement: Placement) -> Result<(), ModelError> {
        let at = usize::try_from(id.0).map_err(|_| ModelError::NoPart { id: id.0 })?;
        let p = self.parts.get_mut(at).ok_or(ModelError::NoPart { id: id.0 })?;
        *p = p.moved_to(placement);
        Ok(())
    }

    /// Assign a material role (`ExportOpts::materials[role.0]`) to one part.
    pub fn set_material(&mut self, id: PartId, material: MaterialRole) -> Result<(), ModelError> {
        let at = usize::try_from(id.0).map_err(|_| ModelError::NoPart { id: id.0 })?;
        let p = self.parts.get_mut(at).ok_or(ModelError::NoPart { id: id.0 })?;
        *p = p.with_material(material);
        Ok(())
    }

    /// Assign a per-part triangle budget or tessellation override to one part.
    pub fn set_budget(&mut self, id: PartId, budget: PartBudget) -> Result<(), ModelError> {
        let at = usize::try_from(id.0).map_err(|_| ModelError::NoPart { id: id.0 })?;
        let p = self.parts.get_mut(at).ok_or(ModelError::NoPart { id: id.0 })?;
        *p = p.with_budget(budget);
        Ok(())
    }

    /// Mark a critical feature (keyhole, bevel, mating surface, or detail region) on `id` in
    /// that part's local coordinate frame. Marked features are preserved with extra geometry
    /// during meshing & simplification, checked per feature, and highlighted in the export preview
    /// if they miss tolerance.
    pub fn mark_feature(&mut self, id: PartId, target: FeatureTarget) -> Result<(), ModelError> {
        let at = usize::try_from(id.0).map_err(|_| ModelError::NoPart { id: id.0 })?;
        let part_name = self.parts.get(at).ok_or(ModelError::NoPart { id: id.0 })?.name;
        asset::validate_feature_target(part_name, &target)?;
        if self.features.iter().any(|pf| pf.part == part_name && pf.target.name == target.name) {
            return Err(ModelError::DuplicateFeatureName { part: part_name, feature: target.name });
        }
        self.features
            .try_push(PartFeature { part: part_name, target })
            .map_err(|_| ModelError::Alloc { what: "the model's critical features" })
    }

    /// All marked per-feature fidelity targets across the model (pass straight to
    /// `GameAssetOpts { features: model.features(), .. }`).
    pub fn features(&self) -> &[PartFeature] {
        &self.features
    }

    /// Set one part's parent in the exported scene hierarchy (`parent` must have been added before
    /// `id` so the hierarchy is a forest by construction).
    pub fn set_parent(&mut self, id: PartId, parent: Option<PartId>) -> Result<(), ModelError> {
        let at = usize::try_from(id.0).map_err(|_| ModelError::NoPart { id: id.0 })?;
        if at >= self.parts.len() {
            return Err(ModelError::NoPart { id: id.0 });
        }
        if let Some(par) = parent {
            if par.0 >= id.0 {
                return Err(ModelError::Argument {
                    function: "Model::set_parent",
                    arg: "parent",
                    accepted: "a part added earlier than this part",
                });
            }
        }
        let p = &mut self.parts[at];
        *p = p.with_parent(parent);
        Ok(())
    }

    /// Every part where it sits, as one solid.
    pub fn assembly(&mut self) -> Result<Solid<Bound>, ModelError> {
        assemble(&mut self.graph, &self.parts)
    }

    /// All-pairs interference check across every part in the model, with AABB broad-phase pruning.
    pub fn no_interference(&mut self, o: InterferenceOpts<'_>) -> Result<Checked<AssemblyInterference>, ModelError> {
        no_interference_assembly(&mut self.graph, &self.parts, o)
    }
}

/// Everything a model is written with, in one `use`.
pub mod prelude {
    pub use blaze_cad_field::{
        AffineOpts, Axis, BendOpts, BlendOpts, Bound, BoxOpts, CapsuleOpts, CircularOpts, ConeOpts, CylinderOpts, Detail,
        EdgeSelect, EllipsoidOpts, Exact, ExtrudeOpts, Graph, GraphOpts, GuideRail, GuidedLoftOpts, GuidedStation, GyroidOpts,
        HalfSpaceOpts, Inside, Join, LoftContinuity, LoftOpts, MirrorOpts, OffsetOpts, Outside, Placement, Profile, ProfileOpts,
        RadiusProfile, RadiusStop, RailPoint, RepeatOpts, RevolveOpts, RoundOpts, ScaleOpts, ShellOpts, Solid, SphereOpts,
        Station, SuperellipsoidOpts, SweepCrossSection, SweepFrame, SweepFramesOpts, TaperOpts, TorusOpts, TwistOpts,
        VariableBlendOpts, VariableOffsetOpts,
    };
    pub use blaze_cad_picture::{
        HighlightStyle, MaterialMaps, MeshHighlight, MeshNormalSource, MeshPictureOpts, MeshScenePart, PictureDiff, PictureOpts,
        Projection, RenderMode, Shading, StripLight, StudioOpts, TextureMap, View, Wireframe, ZebraOpts, compose_grid,
        compose_side_by_side, diff_pictures,
    };
    pub use blaze_units::{Angle, Area, Density, Length, Mass, Volume};

    pub use crate::Shown;
    pub use crate::{
        AssemblyInterference, AssetBakeOpts, AssetFidelityPolicy, AssetPreset, AssetReport, AttachmentFrame, BakeOpts,
        BakedMaps, BakedTexture, BudgetInput, BudgetStatus, CameraFidelityOpts, CameraFidelityPlant, CameraFidelityReport,
        CameraLodReport, ColliderMode, CreaseProfile, CreaseProfileOpts, Curve2dPoint, Decal, DecalLegibility, DecalMaterial,
        DecalOpts, DecalProjection, DecalSample, DecalShape, DecalSpec, DecimatePartOpts, DecimateStats, Decimated, DerivedPanel,
        DraftFace, DraftOpts, EdgeWearMask, Encoding, ExportLodOpts, ExportOpts, ExportStlOpts, Exported, ExportedPart,
        ExportedStl, FeatureKind, FeatureProtect, FeatureRegion, FeatureReport, FeatureSpec, FeatureTarget, FeatureTargetOpts,
        FeatureZone, FidelityChecked, FidelityFinding, FidelityMetrics, FidelityOpts, FidelityReport, FilletKind,
        FinishAssignment, FinishSample, FitOpts, GameAsset, GameAssetOpts, GameCameraPreset, GameCameraRig, GlbPreviewOpts,
        GlbRenderSet, GlbRevisionComparison, GlbRevisionView, GlbViewRender, GridSizing, InterferenceOpts, LodProjection,
        Material, MaterialRegion, MaterialRole, MaterialRule, MaterialTextures, MeshLimits, MeshPartOpts, MinWallOpts, Model,
        ModelError, ModelOpts, NamedCamera, OverhangFace, OverhangOpts, PanelOpts, Part, PartBudget, PartDecal, PartFeature,
        PartFeatureReport, PartId, PartMaterialRegion, PartOpts, PartReport, PatternOpts, PatternResult, PieceCache,
        PieceCacheOpts, ProjectCurveOpts, PublishCheckOpts, PublishFailure, PublishProof, RegionSelector, RoundEdgesOpts,
        SeamClip, SeamSpec, SeamVisibility, SectionSvgOpts, SelectedRoundOpts, SilhouetteShift, StlUnit, Stop, SurfaceCurve,
        SurfaceCurvePoint, SurfaceFinish, SurfacePattern, SurfaceRegion, SurfaceRegionOpts, SurfaceSweepOpts,
        SweptSurfaceFeature, Target, TexturePack, Topology, TriangleBudget, TrimMode, Up, UvError, UvMapped, UvOpts, Verdict,
        allocate_budgets, assemble, assign_material_regions, auto_grid, bake_surface_maps, baked_material, build_game_asset,
        check_camera_fidelity, check_mesh_fidelity, check_mesh_fidelity_report, compare_glb_revisions, compute_tangents,
        crease_profile, decal_feature_target, decal_stroke_width, decimate_mesh, derive_panel, draft_angle_check, evaluate_decal,
        evaluate_finish, export_glb, export_section_svg, export_stl, fill_region_pattern, fits_within, glb_textured, gyroid,
        intersect_all, mass_properties, mesh_part, min_wall, mm3, no_interference, no_interference_assembly, overhang_check,
        parse_svg_path, part_tape, picture, picture_exported_scene, picture_mesh, picture_studio, placed_tape,
        project_curve_to_surface, publish_game_asset, read_glb, render_glb_views, resolve_region, round_edges,
        round_selected_edges, snap_to_surface, sweep_on_surface, topology, union_all, unwrap_uvs,
    };
}

/// Lengths in millimetres from three numbers: the commonest spelling of a size.
pub fn mm3(x: f64, y: f64, z: f64) -> [Length; 3] {
    [Length::mm(x), Length::mm(y), Length::mm(z)]
}
