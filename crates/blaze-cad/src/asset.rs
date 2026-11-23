//! End-to-end game-asset workflow: scale-aware tessellation defaults ([`AssetPreset`]), per-part
//! and bundle-wide triangle budgets ([`allocate_budgets`], [`PartBudget`], [`TriangleBudget`]),
//! sharper fidelity checks for lost holes and bevels ([`check_mesh_fidelity`]), a structured and
//! human/agent-readable per-part report ([`AssetReport`], [`PartReport`]), GPU-optimized GLB
//! export, and direct preview rendering using the **actual exported meshes** ([`GameAsset::preview`]).

#![blaze::sans_io]

use blaze_cad_field::{CadPlant, Detail, Graph, Name, Placement};
use blaze_cad_mesh::{
    COS_32_DEGREES, DecimateOpts, Decimated, DecomposeOpts, FeatureProtect, FeatureRegion, FeatureZone, GridSizing, HullOpts,
    LodOpts, MaterialRole, Mesh, MeshLimits, MeshOpts, MeshView, Meshed, PieceCache, Source, Stop, Target, UvOpts, auto_grid,
    convex_hull, decimate, decompose, lod_chain, mesh, mesh_cached, topology, unwrap_uvs,
};
use blaze_cad_out::{
    Collider, Encoding, GlbOpts, Limits, Lod, MassFacts, Material, MaterialTextures, TexturePack, Up, glb, glb_textured,
};
use blaze_cad_picture::{
    HighlightStyle, MaterialMaps, MeshHighlight, MeshPictureOpts, MeshScenePart, Picture, View, Wireframe, render_mesh_scene,
};
use blaze_math::Aabb3;
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, Vec};
use blaze_text::{Put, Show, Writes};
use blaze_units::{Angle, Density, Length};

use crate::check::{put_deg, put_mm};
use crate::export::{hull_to_mesh, prepare_gpu_mesh};
use crate::surface::{
    BakeOpts, BakedMaps, Decal, FinishAssignment, MaterialRegion, assign_material_regions, bake_surface_maps, baked_material,
};
use crate::{
    ColliderMode, ExportLodOpts, FeatureKind, FeatureReport, FeatureTarget, FidelityChecked, FidelityFinding, FidelityOpts,
    ModelError, Part, PartBudget, PartId, STEEL_ONLY, TriangleBudget, Unsettled, Verdict, check_mesh_fidelity_report,
    mass_properties, part_tape, transform_of,
};

/// Scale-aware tessellation and fidelity preset for [`build_game_asset`].
///
/// Instead of hardcoding a fixed millimeter grid (which explodes meter-scale flat walls into
/// hundreds of thousands of triangles), each preset picks a scale-proportional tolerance and
/// dyadic octree grid from each part's own bounding box.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AssetPreset {
    /// Close-up hero prop or weapon/character piece: tight silhouette (`extent / 600`, clamped to
    /// `0.25..=2.0 mm`), crisp bevel preservation (`max_normal_error = 14°`).
    HeroAsset,
    /// General real-time game prop: balanced silhouette (`extent / 300`, clamped to
    /// `0.5..=4.0 mm`), solid bevel preservation (`max_normal_error = 18°`).
    GameProp,
    /// Large architectural or environment asset (e.g. a cabin, hull, or structure):
    /// (`extent / 200`, clamped to `1.0..=8.0 mm`), clean walls without over-tessellating flat
    /// spans (`max_normal_error = 20°`).
    Environment,
    /// Explicit caller-chosen tolerance and optional smallest feature width.
    Custom {
        tolerance: Length,
        min_feature: Option<Length>,
    },
}

impl AssetPreset {
    /// Resolve the default geometric tolerance for a part with bounding box `bounds`.
    pub fn tolerance_for(self, bounds: &Aabb3) -> Length {
        let extent_mm = if bounds.is_empty() {
            100.0
        } else {
            let d = bounds.hi - bounds.lo;
            let m = d.x.max(d.y).max(d.z);
            if m.is_finite() && m > 0.0 { m * 1000.0 } else { 100.0 }
        };
        match self {
            AssetPreset::HeroAsset => Length::mm((extent_mm / 1200.0).clamp(0.20, 1.0)),
            AssetPreset::GameProp => Length::mm((extent_mm / 800.0).clamp(0.40, 2.0)),
            AssetPreset::Environment => Length::mm((extent_mm / 1000.0).clamp(0.75, 2.5)),
            AssetPreset::Custom { tolerance, .. } => tolerance,
        }
    }

    /// Default crease angle cosine for shading & bevel splits.
    pub const fn default_cos_crease(self) -> f32 {
        match self {
            AssetPreset::HeroAsset => 0.906_307_8,    // cos(25°)
            AssetPreset::GameProp => 0.866_025_4,     // cos(30°)
            AssetPreset::Environment => COS_32_DEGREES, // cos(32°)
            AssetPreset::Custom { .. } => COS_32_DEGREES,
        }
    }

    /// Default normal-error limit for lost-bevel checking.
    pub fn default_max_normal_error(self) -> Angle {
        match self {
            AssetPreset::HeroAsset => Angle::degrees(18.0),
            AssetPreset::GameProp => Angle::degrees(22.0),
            AssetPreset::Environment => Angle::degrees(25.0),
            AssetPreset::Custom { .. } => Angle::degrees(20.0),
        }
    }

    /// Optional explicit minimum feature size carried by [`AssetPreset::Custom`].
    pub const fn min_feature(self) -> Option<Length> {
        match self {
            AssetPreset::Custom { min_feature, .. } => min_feature,
            _ => None,
        }
    }
}

/// Configuration for automatic per-part hole & bevel fidelity checks during [`build_game_asset`].
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct AssetFidelityPolicy {
    /// Override for bridged-hole tolerance (`None` uses `max(2 × part_tolerance, 2.0 mm)`).
    pub hole_tolerance: Option<Length> = None,
    /// Override for shaved-surface tolerance (`None` uses `max(2 × part_tolerance, 2.0 mm)`).
    pub surface_tolerance: Option<Length> = None,
    /// Override for normal deviation limit (`None` uses the preset's limit).
    pub max_normal_error: Option<Angle> = None,
    /// Override for minimum resolved hole feature (`None` uses `max(4 × part_tolerance, 4.0 mm)`).
    pub min_hole_feature: Option<Length> = None,
    /// Maximum fraction of reference bevel edge length allowed to disappear during decimation.
    pub max_bevel_loss: f64 = 0.45,
    /// Bounded subdivision cap per part (rule 18).
    pub most_subdivisions: u64 = 1 << 18,
}

impl AssetFidelityPolicy {
    pub const DEFAULT: AssetFidelityPolicy = AssetFidelityPolicy {
        hole_tolerance: None,
        surface_tolerance: None,
        max_normal_error: None,
        min_hole_feature: None,
        max_bevel_loss: 0.45,
        most_subdivisions: 1 << 18,
    };
}

/// A [`FeatureTarget`] attached to a named part in a [`build_game_asset`] call or [`crate::Model`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PartFeature {
    pub part: Name,
    pub target: FeatureTarget,
}

impl PartFeature {
    /// Attach `target` to the part named `part`.
    #[inline]
    pub fn new(part: &str, target: FeatureTarget) -> PartFeature {
        PartFeature { part: Name::new(part), target }
    }
}

/// One per-feature fidelity report entry bound to its owning part name.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PartFeatureReport {
    pub part: Name,
    pub report: FeatureReport,
}

/// A [`MaterialRegion`] attached to a named part in a [`build_game_asset`] call (Priority 7:
/// multi-material continuous shells that survive booleans and simplification).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PartMaterialRegion {
    pub part: Name,
    pub region: MaterialRegion,
}

impl PartMaterialRegion {
    /// Attach `region` to the part named `part`.
    #[inline]
    pub fn new(part: &str, region: MaterialRegion) -> PartMaterialRegion {
        PartMaterialRegion { part: Name::new(part), region }
    }
}

/// A surface-conforming [`Decal`] attached to a named part in a [`build_game_asset`] call
/// (Priority 3: vector lettering, stripes, and SVG marks baked directly onto curved shells).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PartDecal<'d> {
    pub part: Name,
    pub decal: Decal<'d>,
}

impl<'d> PartDecal<'d> {
    /// Attach `decal` to the part named `part`.
    #[inline]
    pub fn new(part: &str, decal: Decal<'d>) -> PartDecal<'d> {
        PartDecal { part: Name::new(part), decal }
    }
}

/// UV unwrapping and PBR texture baking settings for [`GameAssetOpts::bake`] (Priority 9).
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct AssetBakeOpts {
    /// Square texture atlas resolution in pixels per baked part (e.g. `512` or `1024`).
    pub size: u32 = 512,
    /// Conservative gutter dilation radius in texels to prevent mipmap/bilinear seam bleeding.
    pub gutter_px: u32 = 4,
    /// Bake high-resolution CAD field gradients into the tangent-space normal map.
    pub bake_cad_normals: bool = true,
    /// Normal map scale written to glTF `normalTexture.scale` and applied in preview renders.
    pub normal_strength: f64 = 1.0,
}

impl AssetBakeOpts {
    pub const DEFAULT: AssetBakeOpts = AssetBakeOpts {
        size: 512,
        gutter_px: 4,
        bake_cad_normals: true,
        normal_strength: 1.0,
    };
}

/// Options for [`build_game_asset`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct GameAssetOpts<'a, 's, 'm, 'f> {
    /// Allocator for the returned GLB bytes, report, and retained preview meshes.
    pub alloc: Alloc<'a>,
    /// Scratch allocator for tapes, intermediate meshes, simplification maps, and fidelity checks.
    pub scratch: Alloc<'s>,
    /// Scale-aware tessellation & fidelity preset (defaults to [`AssetPreset::GameProp`]).
    pub preset: AssetPreset = AssetPreset::GameProp,
    /// Optional total triangle budget across all parts. Parts with explicit
    /// [`PartBudget::MaxTriangles`] keep their own cap; remaining triangles are allocated across
    /// `Auto`/weighted parts proportionally to geometric complexity (`weight × natural^0.75`).
    pub total_budget: Option<TriangleBudget> = None,
    /// Minimum triangles reserved for any non-trivial part during automatic budget allocation
    /// (unless the part naturally simplifies below this count).
    pub min_part_triangles: u32 = 24,
    /// Material table indexed by each part's [`MaterialRole`].
    pub materials: &'m [Material<'m>] = STEEL_ONLY,
    /// Optional per-feature critical targets (keyholes, bevels, mating surfaces) bound by part
    /// name: preserved with extra geometry during meshing & decimation, checked per feature, and
    /// highlighted in the export preview when any misses tolerance.
    pub features: &'f [PartFeature] = &[],
    /// Optional per-part multi-material regions applied before decimation so material boundaries
    /// survive simplification on one continuous shell.
    pub material_regions: &'f [PartMaterialRegion] = &[],
    /// Optional surface-conforming vector decals attached by part name (automatically triggers UV
    /// unwrapping and PBR texture baking on those parts).
    pub decals: &'f [PartDecal<'f>] = &[],
    /// Optional physically-scaled directional finishes and edge wear masks by [`MaterialRole`].
    pub finishes: &'f [FinishAssignment] = &[],
    /// Optional explicit UV + PBR detail baking configuration (`None` bakes only parts that carry
    /// decals or matching finishes; `Some(opts)` bakes every part at `opts.size`).
    pub bake: Option<AssetBakeOpts> = None,
    /// Optional uniform density for mass facts in the GLB.
    pub density: Option<Density> = None,
    /// Coordinate convention of the model (`Up::Z` turned to glTF's Y-up).
    pub up: Up = Up::Z,
    /// Vertex encoding (`Encoding::Float` or `Encoding::Quantized`).
    pub encoding: Encoding = Encoding::Float,
    /// Crease cosine override (`None` uses the preset's default).
    pub cos_crease: Option<f32> = None,
    /// Run Tipsify vertex-cache and vertex-fetch optimization on exported meshes and LODs.
    pub optimize_gpu: bool = true,
    /// Post-transform FIFO vertex cache size for Tipsify.
    pub vertex_cache_size: u32 = 32,
    /// Optional per-part LOD chain settings.
    pub lods: Option<ExportLodOpts> = None,
    /// Optional per-part physics collider mode.
    pub colliders: Option<ColliderMode> = None,
    /// Automatic hole & bevel fidelity check policy (`Some(AssetFidelityPolicy::DEFAULT)` by default).
    pub fidelity: Option<AssetFidelityPolicy> = Some(AssetFidelityPolicy::DEFAULT),
    pub limits: Limits = Limits::SHIPPED,
}

/// How a part's final triangle count compares to its budget.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BudgetStatus {
    /// No triangle budget was set on this part or the asset.
    Unbounded,
    /// Written triangles are within budget with geometric error inside target tolerance.
    WithinBudget,
    /// Decimated down to meet the budget cleanly (`Stop::Reached`).
    AtBudget,
    /// Exceeded budget because further collapses would breach `max_error` (`Stop::ErrorCeiling`).
    OverErrorCeiling,
    /// Exceeded budget because no valid manifold collapse remained (`Stop::NoValidCollapse`).
    OverNoValidCollapse,
}

impl BudgetStatus {
    /// True unless the part exceeded its triangle budget.
    #[inline]
    pub const fn ok(self) -> bool {
        matches!(self, BudgetStatus::Unbounded | BudgetStatus::WithinBudget | BudgetStatus::AtBudget)
    }

    const fn tag(self) -> &'static str {
        match self {
            BudgetStatus::Unbounded => "auto",
            BudgetStatus::WithinBudget => "ok",
            BudgetStatus::AtBudget => "fit",
            BudgetStatus::OverErrorCeiling => "over(err)",
            BudgetStatus::OverNoValidCollapse => "over(topo)",
        }
    }
}

/// Per-part telemetry, budget outcome, topology, and fidelity metrics.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PartReport {
    pub name: Name,
    pub material: MaterialRole,
    pub meshed_triangles: u32,
    pub written_triangles: u32,
    pub written_vertices: u32,
    pub budget: Option<u32>,
    pub budget_status: BudgetStatus,
    pub tolerance_used: Length,
    pub feature_grid: Length,
    pub simplified_within: Length,
    pub vertex_bound: Length,
    pub lod_count: u32,
    pub collider_hulls: u32,
    /// Exact topological genus (through-holes) of the exported part mesh.
    pub holes: Option<u32>,
    pub fidelity: Option<FidelityChecked>,
}

/// Planted bugs for strict export validation and publish gating, caught by test controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PublishPlant {
    None,
    /// Silently ignores a [`PartFeature`] whose `part` name does not match any part in the export.
    IgnoreUnknownFeaturePart,
    /// Allows [`AssetReport::check_publish`] to succeed even when fewer feature targets were
    /// checked/passed than requested.
    AllowPartialFeatures,
    /// Allows [`AssetReport::check_publish`] to succeed even when a requested feature target
    /// missed tolerance.
    AllowFailedFeature,
    /// Allows [`AssetReport::check_publish`] to succeed even when a part or bundle exceeded its
    /// triangle budget.
    AllowOverBudget,
}

impl Plants for PublishPlant {}

/// Policy for [`AssetReport::check_publish`], [`GameAsset::check_publish`], and
/// [`publish_game_asset`].
#[blaze::args]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PublishCheckOpts {
    /// Require every part and the bundle total to stay within triangle budgets.
    pub require_budget: bool = true,
    /// Require every part to have been fidelity-checked and return `Verdict::Pass`.
    pub require_fidelity: bool = true,
    /// Require every requested [`PartFeature`] target to have been evaluated and return
    /// `Verdict::Pass` (`checked_features == requested_features && passed_features == requested_features`).
    pub require_all_features: bool = true,
    /// Optional CI guard: refuse publishing if zero critical feature targets were declared.
    pub require_non_empty_features: bool = false,
}

/// Typed refusal returned by [`AssetReport::check_publish`] / [`GameAsset::check_publish`] naming
/// the exact part, feature, budget, or fidelity rule that blocked publishing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PublishFailure {
    /// A part (`Some(name)`) or the bundle total (`None`) exceeded its triangle budget.
    BudgetExceeded {
        part: Option<Name>,
        written: u64,
        budget: u32,
        status: BudgetStatus,
    },
    /// Fidelity checking was disabled (`GameAssetOpts::fidelity = None`) while `require_fidelity`
    /// was enabled.
    PartFidelityNotChecked { part: Name },
    /// A part failed its geometric or topological fidelity check.
    PartFidelityFailed { part: Name, finding: FidelityFinding },
    /// A part's fidelity check did not settle (`Verdict::Unknown`).
    PartFidelityUnsettled { part: Name, reason: Unsettled },
    /// Fewer feature targets were checked or passed than were requested.
    FeatureTargetMismatch { requested: u32, checked: u32, passed: u32 },
    /// A requested critical feature target missed its tolerance.
    FeatureFailed { part: Name, feature: Name, kind: FeatureKind, finding: FidelityFinding },
    /// A requested critical feature target did not settle (`Verdict::Unknown`).
    FeatureUnsettled { part: Name, feature: Name, reason: Unsettled },
    /// `PublishCheckOpts::require_non_empty_features` was set, but no feature targets were declared.
    NoFeaturesDeclared,
}

impl Show for PublishFailure {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match *self {
            PublishFailure::BudgetExceeded { part, written, budget, status } => {
                out.put_str("publish blocked: ");
                match part {
                    Some(name) => {
                        out.put_str("part ");
                        out.put_str(name.as_str());
                    }
                    None => out.put_str("bundle total"),
                }
                out.put_str(" wrote ");
                out.int(written);
                out.put_str(" tris over budget ");
                out.int(budget);
                out.put_str(" (");
                out.put_str(status.tag());
                out.put_byte(b')');
            }
            PublishFailure::PartFidelityNotChecked { part } => {
                out.put_str("publish blocked: fidelity check was not run on part ");
                out.put_str(part.as_str());
            }
            PublishFailure::PartFidelityFailed { part, finding } => {
                out.put_str("publish blocked: part ");
                out.put_str(part.as_str());
                out.put_str(" failed fidelity: ");
                finding.show(out);
            }
            PublishFailure::PartFidelityUnsettled { part, reason } => {
                out.put_str("publish blocked: part ");
                out.put_str(part.as_str());
                out.put_str(" fidelity unsettled: ");
                reason.show(out);
            }
            PublishFailure::FeatureTargetMismatch { requested, checked, passed } => {
                out.put_str("publish blocked: requested ");
                out.int(requested);
                out.put_str(" feature targets, checked ");
                out.int(checked);
                out.put_str(", passed ");
                out.int(passed);
            }
            PublishFailure::FeatureFailed { part, feature, kind, finding } => {
                out.put_str("publish blocked: ");
                out.put_str(part.as_str());
                out.put_byte(b'.');
                out.put_str(feature.as_str());
                out.put_str(" [");
                out.put_str(kind.tag());
                out.put_str("] failed: ");
                finding.show(out);
            }
            PublishFailure::FeatureUnsettled { part, feature, reason } => {
                out.put_str("publish blocked: ");
                out.put_str(part.as_str());
                out.put_byte(b'.');
                out.put_str(feature.as_str());
                out.put_str(" unsettled: ");
                reason.show(out);
            }
            PublishFailure::NoFeaturesDeclared => {
                out.put_str("publish blocked: no critical feature targets were declared");
            }
        }
    }
}

/// Sealed proof minted only by [`AssetReport::check_publish`] / [`GameAsset::check_publish`]
/// certifying that budgets, part fidelity, and every requested feature target passed (rule 74).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PublishProof {
    parts: u32,
    requested_features: u32,
    passed_features: u32,
    written_triangles: u64,
    glb_bytes: u64,
    _sealed: (),
}

impl PublishProof {
    #[inline]
    pub const fn parts(&self) -> u32 {
        self.parts
    }

    #[inline]
    pub const fn requested_features(&self) -> u32 {
        self.requested_features
    }

    #[inline]
    pub const fn passed_features(&self) -> u32 {
        self.passed_features
    }

    #[inline]
    pub const fn written_triangles(&self) -> u64 {
        self.written_triangles
    }

    #[inline]
    pub const fn glb_bytes(&self) -> u64 {
        self.glb_bytes
    }
}

/// Validate a single [`FeatureTarget`]'s spatial region and tolerances upfront so malformed
/// feature specs fail immediately by name instead of silently skipping or exploding meshing.
pub(crate) fn validate_feature_target(part: Name, target: &FeatureTarget) -> Result<(), ModelError> {
    if target.name.as_str().is_empty() {
        return Err(ModelError::InvalidFeatureTarget { part, feature: target.name, reason: "feature name must not be empty" });
    }
    match target.region {
        FeatureRegion::Box(b) => {
            let coords = [b.lo.x, b.lo.y, b.lo.z, b.hi.x, b.hi.y, b.hi.z];
            if !coords.iter().all(|v| v.is_finite()) || b.is_empty() {
                return Err(ModelError::InvalidFeatureTarget {
                    part,
                    feature: target.name,
                    reason: "box region must be finite and non-empty (lo <= hi)",
                });
            }
        }
        FeatureRegion::Ball { center, radius } => {
            if !(center.x.is_finite() && center.y.is_finite() && center.z.is_finite() && radius > Length::ZERO) {
                return Err(ModelError::InvalidFeatureTarget {
                    part,
                    feature: target.name,
                    reason: "ball region must have finite center and radius > 0",
                });
            }
        }
        FeatureRegion::Capsule { a, b, radius } => {
            let coords = [a.x, a.y, a.z, b.x, b.y, b.z];
            if !(coords.iter().all(|v| v.is_finite()) && radius > Length::ZERO) {
                return Err(ModelError::InvalidFeatureTarget {
                    part,
                    feature: target.name,
                    reason: "capsule region must have finite endpoints and radius > 0",
                });
            }
        }
    }
    if let Some(me) = target.max_error
        && me <= Length::ZERO
    {
        return Err(ModelError::InvalidFeatureTarget {
            part,
            feature: target.name,
            reason: "max_error must be greater than zero",
        });
    }
    if let Some(mf) = target.min_feature
        && mf <= Length::ZERO
    {
        return Err(ModelError::InvalidFeatureTarget {
            part,
            feature: target.name,
            reason: "min_feature must be greater than zero",
        });
    }
    if let Some(ang) = target.max_normal_error {
        let rad = ang.as_radians();
        if !(rad > 0.0 && rad < core::f64::consts::PI) {
            return Err(ModelError::InvalidFeatureTarget {
                part,
                feature: target.name,
                reason: "max_normal_error must be strictly between 0 and 180 degrees",
            });
        }
    }
    Ok(())
}

/// Bundle-level report across every part in a [`GameAsset`]. Implements [`Show`] to render a
/// concise, deterministic per-part table + per-feature critical-target table + summary footer.
#[derive(Debug)]
pub struct AssetReport<'a> {
    pub parts: Vec<PartReport, Alloc<'a>>,
    pub features: Vec<PartFeatureReport, Alloc<'a>>,
    /// Number of [`PartFeature`] targets requested on the build call.
    pub requested_features: u32,
    /// Number of requested [`PartFeature`] targets actually evaluated.
    pub checked_features: u32,
    /// Number of evaluated [`PartFeature`] targets that returned `Verdict::Pass`.
    pub passed_features: u32,
    pub total_meshed: u64,
    pub total_written: u64,
    pub total_vertices: u64,
    pub total_budget: Option<u32>,
    pub glb_bytes: u64,
    pub budget_passed: bool,
    pub fidelity_passed: bool,
}

impl AssetReport<'_> {
    /// Strict publish gate: verifies budgets, per-part fidelity, and that EVERY requested feature
    /// target was evaluated and passed (`checked == requested && passed == requested`), minting a
    /// [`PublishProof`] on success or returning the exact typed [`PublishFailure`].
    pub fn check_publish(&self, opts: PublishCheckOpts) -> Result<PublishProof, PublishFailure> {
        self.check_publish_planted(opts, NoPlant)
    }

    /// [`AssetReport::check_publish`] with a planted bug, for the test controls.
    pub fn check_publish_planted(
        &self,
        opts: PublishCheckOpts,
        plant: impl Planted<PublishPlant>,
    ) -> Result<PublishProof, PublishFailure> {
        if opts.require_non_empty_features && self.requested_features == 0 {
            return Err(PublishFailure::NoFeaturesDeclared);
        }
        if opts.require_budget && !plant.has(PublishPlant::AllowOverBudget) {
            for p in self.parts.iter() {
                if !p.budget_status.ok() {
                    return Err(PublishFailure::BudgetExceeded {
                        part: Some(p.name),
                        written: u64::from(p.written_triangles),
                        budget: p.budget.unwrap_or(0),
                        status: p.budget_status,
                    });
                }
            }
            if let Some(tb) = self.total_budget
                && self.total_written > u64::from(tb)
            {
                return Err(PublishFailure::BudgetExceeded {
                    part: None,
                    written: self.total_written,
                    budget: tb,
                    status: BudgetStatus::OverNoValidCollapse,
                });
            }
        }
        if opts.require_all_features {
            if !plant.has(PublishPlant::AllowFailedFeature) {
                for pf in self.features.iter() {
                    match pf.report.verdict {
                        Verdict::Pass => {}
                        Verdict::Found(finding) => {
                            return Err(PublishFailure::FeatureFailed {
                                part: pf.part,
                                feature: pf.report.name,
                                kind: pf.report.kind,
                                finding,
                            });
                        }
                        Verdict::Unknown(reason) => {
                            return Err(PublishFailure::FeatureUnsettled {
                                part: pf.part,
                                feature: pf.report.name,
                                reason,
                            });
                        }
                    }
                }
            }
            if !plant.has(PublishPlant::AllowPartialFeatures)
                && (self.checked_features != self.requested_features
                    || u32::try_from(self.features.len()).ok() != Some(self.requested_features)
                    || (!plant.has(PublishPlant::AllowFailedFeature) && self.passed_features != self.requested_features))
            {
                return Err(PublishFailure::FeatureTargetMismatch {
                    requested: self.requested_features,
                    checked: self.checked_features,
                    passed: self.passed_features,
                });
            }
        }
        if opts.require_fidelity {
            for p in self.parts.iter() {
                let Some(fc) = p.fidelity else {
                    return Err(PublishFailure::PartFidelityNotChecked { part: p.name });
                };
                match fc.verdict {
                    Verdict::Pass => {}
                    Verdict::Found(finding) => {
                        if plant.has(PublishPlant::AllowFailedFeature)
                            && matches!(finding, FidelityFinding::FeatureMissed { .. })
                        {
                            continue;
                        }
                        return Err(PublishFailure::PartFidelityFailed { part: p.name, finding });
                    }
                    Verdict::Unknown(reason) => {
                        return Err(PublishFailure::PartFidelityUnsettled { part: p.name, reason });
                    }
                }
            }
        }
        Ok(PublishProof {
            parts: u32::try_from(self.parts.len()).unwrap_or(u32::MAX),
            requested_features: self.requested_features,
            passed_features: self.passed_features,
            written_triangles: self.total_written,
            glb_bytes: self.glb_bytes,
            _sealed: (),
        })
    }
}

impl Show for AssetReport<'_> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.put_str("part\trole\tmeshed\twritten\tbudget\tstatus\terr_mm\tnorm_deg\tholes\tbevel%\tlods\tfidelity\n");
        for p in self.parts.iter() {
            out.put_str(p.name.as_str());
            out.put_str("\t#");
            out.int(p.material.0);
            out.put_byte(b'\t');
            out.int(p.meshed_triangles);
            out.put_byte(b'\t');
            out.int(p.written_triangles);
            out.put_byte(b'\t');
            match p.budget {
                Some(b) => {
                    out.int(b);
                }
                None => out.put_byte(b'-'),
            }
            out.put_byte(b'\t');
            out.put_str(p.budget_status.tag());
            out.put_byte(b'\t');
            put_mm(out, p.simplified_within.as_nm(), 2);
            out.put_byte(b'\t');
            match p.fidelity {
                Some(fc) => put_deg(out, fc.metrics.max_normal_error.as_microarcsec(), 1),
                None => out.put_byte(b'-'),
            }
            out.put_byte(b'\t');
            match p.holes {
                Some(h) => {
                    out.int(h);
                }
                None => out.put_byte(b'?'),
            }
            out.put_byte(b'\t');
            match p.fidelity.and_then(|fc| fc.metrics.bevel_retention) {
                Some(r) => {
                    let pct = (r.clamp(0.0, 2.0) * 100.0 + 0.5) as u64;
                    out.int(pct);
                    out.put_byte(b'%');
                }
                None => out.put_byte(b'-'),
            }
            out.put_byte(b'\t');
            out.int(p.lod_count);
            out.put_byte(b'\t');
            match p.fidelity {
                Some(fc) => fc.verdict.show(out),
                None => out.put_str("-"),
            }
            out.put_byte(b'\n');
        }
        out.put_str("TOTAL (");
        out.int(self.parts.len());
        out.put_str(" parts)\tmeshed=");
        out.int(self.total_meshed);
        out.put_str("\twritten=");
        out.int(self.total_written);
        out.put_str("\tvertices=");
        out.int(self.total_vertices);
        out.put_str("\tbudget=");
        match self.total_budget {
            Some(b) => {
                out.int(b);
            }
            None => out.put_byte(b'-'),
        }
        out.put_str("\tglb_bytes=");
        out.int(self.glb_bytes);
        out.put_str("\tbudget_ok=");
        out.put_str(if self.budget_passed { "PASS" } else { "FAIL" });
        out.put_str("\tfidelity=");
        out.put_str(if self.fidelity_passed { "PASS" } else { "FAIL" });
        if self.requested_features > 0 {
            out.put_str("\tfeatures=");
            out.int(self.passed_features);
            out.put_byte(b'/');
            out.int(self.requested_features);
        }
        out.put_byte(b'\n');
        if !self.features.is_empty() {
            out.put_str("FEATURES\tpart\tfeature\tkind\tprotect\ttris\terr_mm\tnorm_deg\tstatus\n");
            for pf in self.features.iter() {
                let r = &pf.report;
                out.put_str("feature\t");
                out.put_str(pf.part.as_str());
                out.put_byte(b'\t');
                out.put_str(r.name.as_str());
                out.put_byte(b'\t');
                out.put_str(r.kind.tag());
                out.put_byte(b'\t');
                out.put_str(match r.protect {
                    FeatureProtect::Critical => "critical",
                    FeatureProtect::Pinned => "pinned",
                });
                out.put_byte(b'\t');
                out.int(r.triangles);
                out.put_byte(b'\t');
                put_mm(out, r.measured_error().as_nm(), 2);
                out.put_byte(b'/');
                put_mm(out, r.max_error.as_nm(), 2);
                out.put_byte(b'\t');
                put_deg(out, r.measured_normal_error.as_microarcsec(), 1);
                out.put_byte(b'/');
                put_deg(out, r.max_normal_error.as_microarcsec(), 1);
                out.put_byte(b'\t');
                r.verdict.show(out);
                out.put_byte(b'\n');
            }
        }
    }
}

/// One exported, GPU-optimized part retained inside [`GameAsset`] for direct mesh previewing.
#[derive(Debug)]
pub struct ExportedPart<'a> {
    pub name: Name,
    pub parent: Option<PartId>,
    pub placement: Placement,
    pub material: MaterialRole,
    pub mesh: Mesh<'a>,
    /// Optional baked PBR texture atlas (`base_colour`, `metallic_roughness`, `normal`) when UV
    /// unwrapping + detail baking ran for this part.
    pub baked: Option<BakedMaps<'a>>,
    /// Dedicated baked material role slot in the exported GLB (when `baked.is_some()`).
    pub baked_role: Option<MaterialRole>,
    /// UV atlas physical density (`texels / metre`) when UVs were unwrapped for this part.
    pub texels_per_metre: Option<f64>,
}

/// Vivid crimson highlight colour for features that missed their fidelity target.
const MISSED_HIGHLIGHT_RGB: [u8; 3] = [245, 42, 68];

/// Cool cyan contour colour for passing critical features in [`GameAsset::preview_fidelity`].
const PASS_OUTLINE_RGB: [u8; 3] = [46, 196, 214];

/// Complete game-asset build output: GLB bytes, structured per-part & per-feature report,
/// retained exported meshes for WYSIWYG preview rendering, and automatic highlight overlays for
/// any marked features that missed tolerance.
#[derive(Debug)]
pub struct GameAsset<'a> {
    pub glb: Vec<u8, Alloc<'a>>,
    pub report: AssetReport<'a>,
    pub parts: Vec<ExportedPart<'a>, Alloc<'a>>,
    /// Spatial highlight regions for marked features that missed tolerance (drawn automatically by
    /// [`GameAsset::preview`] when `opts.highlights` is empty).
    pub missed_highlights: Vec<MeshHighlight, Alloc<'a>>,
    /// Spatial highlight regions for all marked features (vivid crimson for missed features,
    /// subtle cyan outline for passing critical features; drawn by [`GameAsset::preview_fidelity`]).
    pub all_highlights: Vec<MeshHighlight, Alloc<'a>>,
}

/// Distinct, colorblind-friendly deterministic palette for [`GameAsset::preview_part_colors`].
const PART_PALETTE: [[u8; 3]; 10] = [
    [198, 128, 78],  // warm cedar
    [94, 134, 174],  // slate blue
    [122, 168, 108], // pine green
    [196, 166, 86],  // ochre trim
    [156, 118, 168], // muted plum
    [176, 112, 112], // terracotta
    [104, 168, 164], // sea teal
    [164, 156, 146], // warm stone
    [214, 148, 114], // peach timber
    [126, 138, 184], // steel indigo
];

impl GameAsset<'_> {
    /// Total triangles written across every part.
    pub fn triangles(&self) -> u64 {
        self.report.total_written
    }

    /// Strict publish gate: requires budgets, per-part fidelity, and every requested feature
    /// target to pass according to `opts`, minting a [`PublishProof`] on success or returning the
    /// exact typed [`PublishFailure`].
    pub fn check_publish(&self, opts: PublishCheckOpts) -> Result<PublishProof, PublishFailure> {
        self.report.check_publish(opts)
    }

    /// Return the publishable GLB bytes once [`Self::check_publish`] has minted a [`PublishProof`]
    /// (rule 74: verify once and pass the proof forward).
    #[inline]
    pub fn publish_glb(&self, _proof: PublishProof) -> &[u8] {
        &self.glb
    }

    /// Render a preview using the **actual exported, decimated, GPU-optimized meshes** at their
    /// real placements and parent hierarchy, shaded with `materials`.
    ///
    /// When `opts.highlights` is empty, any marked critical feature that missed tolerance is
    /// highlighted directly on the exported mesh automatically.
    pub fn preview<'p>(
        &self,
        materials: &[Material<'_>],
        view: View,
        opts: MeshPictureOpts<'p, '_, '_>,
    ) -> Result<Picture<'p>, ModelError> {
        let mut scene_parts: Vec<MeshScenePart<'_>, Alloc<'_>> = Vec::new_in(opts.scratch);
        scene_parts.try_reserve(self.parts.len()).map_err(|_| ModelError::Alloc { what: "asset preview parts" })?;
        for p in self.parts.iter() {
            scene_parts
                .try_push(MeshScenePart {
                    mesh: p.mesh.view(),
                    parent: p.parent.map(|id| id.0),
                    transform: transform_of(&p.placement),
                    tint_override: None,
                })
                .map_err(|_| ModelError::Alloc { what: "asset preview parts" })?;
        }
        let has_baked = self.parts.iter().any(|p| p.baked.is_some());
        if has_baked && opts.material_maps.is_empty() {
            let mut ext_mats: Vec<Material<'_>, Alloc<'_>> = Vec::new_in(opts.scratch);
            let mut ext_maps: Vec<MaterialMaps<'_>, Alloc<'_>> = Vec::new_in(opts.scratch);
            for &m in materials {
                ext_mats.try_push(m).map_err(|_| ModelError::Alloc { what: "preview materials" })?;
                ext_maps.try_push(MaterialMaps::NONE).map_err(|_| ModelError::Alloc { what: "preview material maps" })?;
            }
            for p in self.parts.iter() {
                if let (Some(baked), Some(role)) = (&p.baked, p.baked_role) {
                    let want = (role.0 as usize) + 1;
                    while ext_mats.len() < want {
                        ext_mats
                            .try_push(baked_material(p.name.as_str()))
                            .map_err(|_| ModelError::Alloc { what: "preview baked materials" })?;
                        ext_maps
                            .try_push(MaterialMaps::NONE)
                            .map_err(|_| ModelError::Alloc { what: "preview baked maps" })?;
                    }
                    ext_mats[role.0 as usize] = baked_material(p.name.as_str());
                    ext_maps[role.0 as usize] = baked.as_material_maps(1.0);
                }
            }
            let eff_opts = MeshPictureOpts {
                highlights: if opts.highlights.is_empty() && !self.missed_highlights.is_empty() {
                    &self.missed_highlights
                } else {
                    opts.highlights
                },
                material_maps: &ext_maps,
                ..opts
            };
            return render_mesh_scene(&scene_parts, &ext_mats, view, eff_opts).map_err(ModelError::Picture);
        }
        let eff_opts = if opts.highlights.is_empty() && !self.missed_highlights.is_empty() {
            MeshPictureOpts { highlights: &self.missed_highlights, ..opts }
        } else {
            opts
        };
        render_mesh_scene(&scene_parts, materials, view, eff_opts).map_err(ModelError::Picture)
    }

    /// Render a fidelity inspection preview of the **actual exported meshes**: shows both missed
    /// critical features (vivid crimson tint + hazard hatch + boundary ring) and passing critical
    /// features (crisp cyan contour outline), adding a subtle wireframe when `opts.wireframe` is
    /// [`Wireframe::Off`].
    pub fn preview_fidelity<'p>(
        &self,
        materials: &[Material<'_>],
        view: View,
        opts: MeshPictureOpts<'p, '_, '_>,
    ) -> Result<Picture<'p>, ModelError> {
        let mut scene_parts: Vec<MeshScenePart<'_>, Alloc<'_>> = Vec::new_in(opts.scratch);
        scene_parts.try_reserve(self.parts.len()).map_err(|_| ModelError::Alloc { what: "asset preview parts" })?;
        for p in self.parts.iter() {
            scene_parts
                .try_push(MeshScenePart {
                    mesh: p.mesh.view(),
                    parent: p.parent.map(|id| id.0),
                    transform: transform_of(&p.placement),
                    tint_override: None,
                })
                .map_err(|_| ModelError::Alloc { what: "asset preview parts" })?;
        }
        let wf = match opts.wireframe {
            Wireframe::Off => Wireframe::Overlay { width_px: 0.75, colour: [36, 40, 48] },
            other => other,
        };
        let eff_opts = MeshPictureOpts {
            wireframe: wf,
            highlights: if opts.highlights.is_empty() { &self.all_highlights } else { opts.highlights },
            ..opts
        };
        render_mesh_scene(&scene_parts, materials, view, eff_opts).map_err(ModelError::Picture)
    }

    /// Render a preview using the **actual exported meshes** tinted with one distinct colour per
    /// named part (ideal for verifying part boundaries, wireframe density, and budget allocation).
    pub fn preview_part_colors<'p>(&self, view: View, opts: MeshPictureOpts<'p, '_, '_>) -> Result<Picture<'p>, ModelError> {
        let mut scene_parts: Vec<MeshScenePart<'_>, Alloc<'_>> = Vec::new_in(opts.scratch);
        scene_parts.try_reserve(self.parts.len()).map_err(|_| ModelError::Alloc { what: "asset preview parts" })?;
        for (i, p) in self.parts.iter().enumerate() {
            scene_parts
                .try_push(MeshScenePart {
                    mesh: p.mesh.view(),
                    parent: p.parent.map(|id| id.0),
                    transform: transform_of(&p.placement),
                    tint_override: Some(PART_PALETTE[i % PART_PALETTE.len()]),
                })
                .map_err(|_| ModelError::Alloc { what: "asset preview parts" })?;
        }
        let eff_opts = if opts.highlights.is_empty() && !self.missed_highlights.is_empty() {
            MeshPictureOpts { highlights: &self.missed_highlights, ..opts }
        } else {
            opts
        };
        render_mesh_scene(&scene_parts, &[], view, eff_opts).map_err(ModelError::Picture)
    }
}

/// Deterministic budget-allocation input for one part.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BudgetInput {
    /// Natural triangle count after Phase-1 error-bounded simplification.
    pub natural_triangles: u32,
    /// Optional explicit per-part cap (`PartBudget::MaxTriangles` / `Detailed::max_triangles`).
    pub explicit_cap: Option<u32>,
    /// Relative importance weight (`> 0.0`, default `1.0`).
    pub weight: f32,
}

/// Deterministically allocate a bundle-level `total_budget` across `inputs`.
///
/// - Parts with `explicit_cap` receive `min(natural_triangles, cap)`.
/// - If the resulting total is already `<= total_budget`, every `Auto` part keeps its natural
///   triangle count (zero unnecessary quality loss).
/// - Otherwise, the remaining budget after fixed-cap parts is distributed across `Auto` parts
///   proportionally to `weight × natural^0.75` (sublinear weighting so detailed curved hero parts
///   keep crisp silhouettes while oversized parts compress smoothly), reserving at least
///   `min(natural, min_part_triangles)` per part whenever the remaining budget permits, and never
///   allocating more triangles to a part than its `natural_triangles`.
pub fn allocate_budgets<'a>(
    inputs: &[BudgetInput],
    total_budget: u32,
    min_part_triangles: u32,
    alloc: Alloc<'a>,
) -> Result<Vec<u32, Alloc<'a>>, ModelError> {
    let n = inputs.len();
    let mut out: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    out.try_reserve(n).map_err(|_| ModelError::Alloc { what: "budget allocation" })?;

    let mut fixed_sum = 0u64;
    let mut auto_natural_sum = 0u64;
    for inp in inputs {
        if let Some(cap) = inp.explicit_cap {
            let take = inp.natural_triangles.min(cap);
            out.try_push(take).map_err(|_| ModelError::Alloc { what: "budget allocation" })?;
            fixed_sum += u64::from(take);
        } else {
            out.try_push(inp.natural_triangles).map_err(|_| ModelError::Alloc { what: "budget allocation" })?;
            auto_natural_sum += u64::from(inp.natural_triangles);
        }
    }

    if fixed_sum + auto_natural_sum <= u64::from(total_budget) {
        return Ok(out);
    }

    let remaining = u64::from(total_budget).saturating_sub(fixed_sum);
    // Reserve floors for Auto parts (scaled down proportionally if even the floors exceed remaining).
    let mut floor_sum = 0u64;
    for inp in inputs {
        if inp.explicit_cap.is_none() {
            floor_sum += u64::from(inp.natural_triangles.min(min_part_triangles));
        }
    }
    let floor_scale = if floor_sum > remaining && floor_sum > 0 { (remaining as f64) / (floor_sum as f64) } else { 1.0 };

    let mut assigned = 0u64;
    for (i, inp) in inputs.iter().enumerate() {
        if inp.explicit_cap.is_none() {
            let f = ((f64::from(inp.natural_triangles.min(min_part_triangles)) * floor_scale) as u32).min(inp.natural_triangles);
            out[i] = f;
            assigned += u64::from(f);
        }
    }

    // Iteratively distribute surplus `remaining - assigned` proportionally to `weight * natural^0.75`,
    // capping each part at `natural_triangles` so leftover slack redistributes cleanly.
    for _ in 0..8 {
        let mut surplus = remaining.saturating_sub(assigned);
        if surplus == 0 {
            break;
        }
        let mut weight_sum = 0.0f64;
        for (i, inp) in inputs.iter().enumerate() {
            if inp.explicit_cap.is_none() && out[i] < inp.natural_triangles {
                let w = f64::from(if inp.weight.is_finite() && inp.weight > 0.0 { inp.weight } else { 1.0 });
                let score = w * blaze_math::pow(f64::from(inp.natural_triangles.max(1)), 0.75);
                weight_sum += score;
            }
        }
        if !(weight_sum > 0.0) {
            break;
        }
        let mut added_any = false;
        let pool = surplus as f64;
        for (i, inp) in inputs.iter().enumerate() {
            if inp.explicit_cap.is_none() && out[i] < inp.natural_triangles {
                let w = f64::from(if inp.weight.is_finite() && inp.weight > 0.0 { inp.weight } else { 1.0 });
                let score = w * blaze_math::pow(f64::from(inp.natural_triangles.max(1)), 0.75);
                let share = ((pool * (score / weight_sum)) as u64).min(surplus);
                let room = u64::from(inp.natural_triangles - out[i]);
                let give = share.min(room);
                if give > 0 {
                    out[i] += give as u32;
                    assigned += give;
                    surplus -= give;
                    added_any = true;
                }
            }
        }
        if !added_any {
            // Distribute remaining fractional triangles deterministically (+1 each in part order).
            for (i, inp) in inputs.iter().enumerate() {
                if surplus == 0 {
                    break;
                }
                if inp.explicit_cap.is_none() && out[i] < inp.natural_triangles {
                    out[i] += 1;
                    surplus -= 1;
                }
            }
            break;
        }
    }
    Ok(out)
}

struct ReadyLod<'s> {
    mesh: Mesh<'s>,
    error: Length,
    switch_distance: Length,
}

/// Build an entire game asset end to end:
///
/// 0. Strict upfront validation: rejects empty part lists, duplicate part names, unknown/misspelled
///    part names on [`PartFeature`] targets ([`ModelError::UnknownFeaturePart`]), duplicate
///    `(part, feature)` names ([`ModelError::DuplicateFeatureName`]), and invalid feature regions
///    or tolerances ([`ModelError::InvalidFeatureTarget`]).
/// 1. Scale-aware tessellation (`auto_grid`) with per-cell local refinement around marked features.
/// 2. Phase-1 error-bounded QEM decimation, followed by deterministic [`allocate_budgets`] and
///    incremental Phase-2 budget-constrained decimation.
/// 3. GPU Tipsify vertex-cache and vertex-fetch optimization, optional LOD chains, and optional
///    physics collider hulls.
/// 4. Automatic per-part and per-feature fidelity checks ([`check_mesh_fidelity_report`]) and
///    exact Euler genus calculation.
/// 5. glTF 2.0 `.glb` export, full [`AssetReport`], retained previewable meshes, and automatic
///    missed-feature preview highlights.
pub fn build_game_asset<'a, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    parts: &[Part],
    o: GameAssetOpts<'a, '_, '_, '_>,
    cache: Option<&mut PieceCache<'_>>,
) -> Result<GameAsset<'a>, ModelError> {
    build_game_asset_planted(g, parts, o, cache, NoPlant)
}

/// Build an asset and enforce [`PublishCheckOpts`] in one call, returning the asset together with
/// its [`PublishProof`] or failing with [`ModelError::PublishRejected`] if any budget, part
/// fidelity check, or requested feature target did not pass.
pub fn publish_game_asset<'a, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    parts: &[Part],
    o: GameAssetOpts<'a, '_, '_, '_>,
    publish: PublishCheckOpts,
    cache: Option<&mut PieceCache<'_>>,
) -> Result<(GameAsset<'a>, PublishProof), ModelError> {
    let asset = build_game_asset(g, parts, o, cache)?;
    let proof = asset.check_publish(publish)?;
    Ok((asset, proof))
}

/// [`build_game_asset`] with a planted validation bug, for the test controls.
pub fn build_game_asset_planted<'a, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    parts: &[Part],
    o: GameAssetOpts<'a, '_, '_, '_>,
    mut cache: Option<&mut PieceCache<'_>>,
    plant: impl Planted<PublishPlant>,
) -> Result<GameAsset<'a>, ModelError> {
    // Stage 0: strict upfront validation — never silently ignore a misspelled part name, duplicate
    // target, or malformed feature region.
    if parts.is_empty() {
        return Err(ModelError::Argument {
            function: "build_game_asset",
            arg: "parts",
            accepted: "at least one part",
        });
    }
    for (i, a) in parts.iter().enumerate() {
        if let Some(rest) = parts.get(i + 1..) {
            for b in rest {
                if a.name == b.name {
                    return Err(ModelError::DuplicateName { name: a.name });
                }
            }
        }
    }
    for (fi, pf) in o.features.iter().enumerate() {
        if !plant.has(PublishPlant::IgnoreUnknownFeaturePart) && !parts.iter().any(|p| p.name == pf.part) {
            return Err(ModelError::UnknownFeaturePart { part: pf.part, feature: pf.target.name });
        }
        validate_feature_target(pf.part, &pf.target)?;
        if let Some(prevs) = o.features.get(..fi) {
            for prev in prevs {
                if prev.part == pf.part && prev.target.name == pf.target.name {
                    return Err(ModelError::DuplicateFeatureName { part: pf.part, feature: pf.target.name });
                }
            }
        }
    }

    let s = o.scratch;
    let cos_crease = o.cos_crease.unwrap_or_else(|| o.preset.default_cos_crease());
    let default_norm_limit = o
        .fidelity
        .and_then(|fp| fp.max_normal_error)
        .unwrap_or_else(|| o.preset.default_max_normal_error());

    // Stage 1: resolve per-part tessellation settings, per-feature zones, and mesh each part.
    let mut part_tols: Vec<Length, Alloc<'_>> = Vec::new_in(s);
    let mut part_grids: Vec<GridSizing, Alloc<'_>> = Vec::new_in(s);
    let mut part_max_errs: Vec<Option<Length>, Alloc<'_>> = Vec::new_in(s);
    let mut meshes: Vec<Meshed<'_>, Alloc<'_>> = Vec::new_in(s);
    let mut part_feats: Vec<FeatureTarget, Alloc<'_>> = Vec::new_in(s);
    let mut part_zones: Vec<FeatureZone, Alloc<'_>> = Vec::new_in(s);
    let mut feat_ranges: Vec<(usize, usize), Alloc<'_>> = Vec::new_in(s);
    part_tols.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset part tolerances" })?;
    part_grids.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset part grids" })?;
    part_max_errs.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset part ceilings" })?;
    meshes.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset meshes" })?;
    feat_ranges.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset feature ranges" })?;

    for part in parts {
        let tape = part_tape(g, part, Detail::Full, s)?;
        let bounds = tape.aabb();
        let (tol, min_feat, max_err) = match part.budget {
            PartBudget::Detailed { tolerance, min_feature, max_error, .. } => (
                tolerance.unwrap_or_else(|| o.preset.tolerance_for(&bounds)),
                min_feature.or_else(|| o.preset.min_feature()),
                max_error,
            ),
            _ => (o.preset.tolerance_for(&bounds), o.preset.min_feature(), None),
        };
        let f_start = part_feats.len();
        for pf in o.features {
            if pf.part == part.name {
                let zone = pf.target.to_zone(tol, default_norm_limit);
                part_feats.try_push(pf.target).map_err(|_| ModelError::Alloc { what: "asset part features" })?;
                part_zones.try_push(zone).map_err(|_| ModelError::Alloc { what: "asset feature zones" })?;
            }
        }
        let f_end = part_feats.len();
        feat_ranges.try_push((f_start, f_end)).map_err(|_| ModelError::Alloc { what: "asset feature ranges" })?;

        let grid = auto_grid(&bounds, tol, min_feat);
        let mopts = MeshOpts {
            alloc: s,
            scratch: s,
            tolerance: tol,
            piece: grid.piece,
            feature: grid.feature,
            zones: &part_zones[f_start..f_end],
            ..
        };
        let meshed = match cache.as_deref_mut() {
            Some(c) => mesh_cached(&tape, mopts, c),
            None => mesh(&tape, mopts),
        }
        .map_err(ModelError::Mesh)?;
        part_tols.try_push(tol).map_err(|_| ModelError::Alloc { what: "asset part tolerances" })?;
        part_grids.try_push(grid).map_err(|_| ModelError::Alloc { what: "asset part grids" })?;
        part_max_errs.try_push(max_err).map_err(|_| ModelError::Alloc { what: "asset part ceilings" })?;
        meshes.try_push(meshed).map_err(|_| ModelError::Alloc { what: "asset meshes" })?;
    }

    // Stage 2: Phase-1 decimation (assign per-part multi-material regions first so role borders
    // survive simplification, honor explicit per-part cap, preserve marked feature zones, and
    // eliminate flat/coplanar redundancy).
    let mut decimated: Vec<Decimated<'_>, Alloc<'_>> = Vec::new_in(s);
    let mut budget_inputs: Vec<BudgetInput, Alloc<'_>> = Vec::new_in(s);
    let mut part_has_regions: Vec<bool, Alloc<'_>> = Vec::new_in(s);
    decimated.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset phase-1 decimation" })?;
    budget_inputs.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset budget inputs" })?;
    part_has_regions.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset region flags" })?;

    let mut reg_buf: Vec<MaterialRegion, Alloc<'_>> = Vec::new_in(s);
    for (i, (part, m)) in parts.iter().zip(meshes.iter()).enumerate() {
        let tol = part_tols[i];
        let cap = part.budget.explicit_cap();
        let (fs, fe) = feat_ranges[i];
        let target = match cap {
            Some(max_t) => Target::Budget { error: tol, max_triangles: max_t, max_error: part_max_errs[i] },
            None => Target::Error(tol),
        };
        reg_buf.clear();
        for pm in o.material_regions {
            if pm.part == part.name {
                reg_buf.try_push(pm.region).map_err(|_| ModelError::Alloc { what: "asset part material regions" })?;
            }
        }
        let has_regs = !reg_buf.is_empty();
        part_has_regions.try_push(has_regs).map_err(|_| ModelError::Alloc { what: "asset region flags" })?;
        let d = if has_regs {
            let tape = part_tape(g, part, Detail::Full, s)?;
            let assigned = assign_material_regions(g, &m.mesh.view(), part.material, Some(&tape), &reg_buf, s, s)?;
            let checked_assigned = blaze_cad_mesh::check(
                assigned.view(),
                blaze_cad_mesh::CheckOpts { limits: MeshLimits::SHIPPED, scratch: s },
            )
            .ok();
            let src = match checked_assigned {
                Some(v) => Source::Verified(v),
                None => Source::Open(assigned.view()),
            };
            decimate(
                src,
                DecimateOpts { target, cos_crease, zones: &part_zones[fs..fe], limits: MeshLimits::SHIPPED, alloc: s, scratch: s },
            )?
        } else {
            decimate(
                Source::Verified(m.verified()),
                DecimateOpts { target, cos_crease, zones: &part_zones[fs..fe], limits: MeshLimits::SHIPPED, alloc: s, scratch: s },
            )?
        };
        let nat = d.mesh.view().triangle_count();
        budget_inputs
            .try_push(BudgetInput { natural_triangles: nat, explicit_cap: cap, weight: part.budget.weight() })
            .map_err(|_| ModelError::Alloc { what: "asset budget inputs" })?;
        decimated.try_push(d).map_err(|_| ModelError::Alloc { what: "asset phase-1 decimation" })?;
    }

    // Stage 3: Total-budget allocation and incremental Phase-2 decimation continuing directly from
    // Phase-1 `decimated[i]` (50–100× faster than restarting from raw meshed triangles while
    // keeping a sound triangle-inequality Hausdorff bound and hard-pinning `Pinned` features).
    let allocated: Option<Vec<u32, Alloc<'_>>> = match o.total_budget {
        Some(TriangleBudget(total)) => Some(allocate_budgets(&budget_inputs, total, o.min_part_triangles, s)?),
        None => None,
    };
    if let Some(ref alloc_caps) = allocated {
        let mut stage3_zones: Vec<FeatureZone, Alloc<'_>> = Vec::new_in(s);
        for i in 0..parts.len() {
            let cur = decimated[i].mesh.view().triangle_count();
            let target_cap = alloc_caps[i];
            if cur > target_cap {
                let base_err = decimated[i].error_bound;
                let rem_max_err = part_max_errs[i].map(|ce| Length::nm((ce.as_nm() - base_err.as_nm()).max(0)));
                let (fs, fe) = feat_ranges[i];
                stage3_zones.clear();
                for &z in &part_zones[fs..fe] {
                    let mut z3 = z;
                    if z3.protect == FeatureProtect::Pinned {
                        // Phase 1 already reached `z.max_error`; allow zero extra shape error in
                        // Phase 2 so pinned features strictly keep their original error bound.
                        z3.max_error = Some(Length::ZERO);
                    }
                    stage3_zones.try_push(z3).map_err(|_| ModelError::Alloc { what: "asset stage-3 feature zones" })?;
                }
                let src = match decimated[i].verified() {
                    Some(v) => Source::Verified(v),
                    None => Source::Open(decimated[i].mesh.view()),
                };
                let mut d2 = decimate(
                    src,
                    DecimateOpts {
                        target: Target::Budget { error: Length::ZERO, max_triangles: target_cap, max_error: rem_max_err },
                        cos_crease,
                        zones: &stage3_zones,
                        limits: MeshLimits::SHIPPED,
                        alloc: s,
                        scratch: s,
                    },
                )?;
                d2.error_bound = base_err + d2.error_bound;
                decimated[i] = d2;
            }
        }
    }

    // Stage 4 & 5: GPU optimization, LODs, colliders, topology, per-part & per-feature fidelity
    // checks, and preview highlight collection.
    let mut exported_parts: Vec<ExportedPart<'a>, Alloc<'a>> = Vec::new_in(o.alloc);
    exported_parts.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset exported parts" })?;

    let mut ready_lods: Vec<ReadyLod<'_>, Alloc<'_>> = Vec::new_in(s);
    let mut lod_ranges: Vec<(usize, usize), Alloc<'_>> = Vec::new_in(s);
    lod_ranges.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset lod ranges" })?;

    let mut ready_hulls: Vec<Mesh<'_>, Alloc<'_>> = Vec::new_in(s);
    let mut hull_ranges: Vec<(usize, usize), Alloc<'_>> = Vec::new_in(s);
    hull_ranges.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset hull ranges" })?;

    let mut part_reports: Vec<PartReport, Alloc<'a>> = Vec::new_in(o.alloc);
    part_reports.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset part reports" })?;

    let mut feature_reports: Vec<PartFeatureReport, Alloc<'a>> = Vec::new_in(o.alloc);
    let mut missed_highlights: Vec<MeshHighlight, Alloc<'a>> = Vec::new_in(o.alloc);
    let mut all_highlights: Vec<MeshHighlight, Alloc<'a>> = Vec::new_in(o.alloc);

    let mut total_meshed = 0u64;
    let mut total_written = 0u64;
    let mut total_vertices = 0u64;
    let mut all_budgets_ok = true;
    let mut all_fidelity_ok = true;

    let mut next_baked_role: u32 = u32::try_from(o.materials.len()).unwrap_or(0);
    let mut part_decals_buf: Vec<Decal<'_>, Alloc<'_>> = Vec::new_in(s);

    for (i, (part, m)) in parts.iter().zip(meshes.iter()).enumerate() {
        let d = &decimated[i];
        let base_view = d.mesh.view();
        let verified_base = d.verified().unwrap_or_else(|| m.verified());
        let written_tris = base_view.triangle_count();

        let stamp_role = if part_has_regions[i] { None } else { Some(part.material) };
        part_decals_buf.clear();
        for pd in o.decals {
            if pd.part == part.name {
                part_decals_buf.try_push(pd.decal).map_err(|_| ModelError::Alloc { what: "asset part decals" })?;
            }
        }
        let has_matching_finish = o.finishes.iter().any(|fa| {
            if part_has_regions[i] {
                base_view.roles().contains(&fa.role)
            } else {
                fa.role == part.material
            }
        });
        let should_bake = o.bake.is_some() || !part_decals_buf.is_empty() || has_matching_finish;

        let (opt_mesh, baked, baked_role, texels_per_metre) = if should_bake {
            let bake_cfg = o.bake.unwrap_or(AssetBakeOpts::DEFAULT);
            let pre = prepare_gpu_mesh(&base_view, stamp_role, false, o.vertex_cache_size, s, s)?;
            let unwrapped = unwrap_uvs(
                &pre.view(),
                UvOpts {
                    alloc: s,
                    scratch: s,
                    gutter_px: bake_cfg.gutter_px,
                    atlas_size: bake_cfg.size,
                    limits: MeshLimits::SHIPPED,
                    ..
                },
            )?;
            let hi_tape = if bake_cfg.bake_cad_normals {
                Some(part_tape(g, part, Detail::Full, s)?)
            } else {
                None
            };
            let maps = bake_surface_maps(
                &unwrapped.mesh.view(),
                o.materials,
                hi_tape.as_ref(),
                &part_decals_buf,
                o.finishes,
                BakeOpts {
                    size: bake_cfg.size,
                    gutter_px: bake_cfg.gutter_px,
                    bake_cad_normals: bake_cfg.bake_cad_normals,
                    normal_strength: bake_cfg.normal_strength,
                    limits: o.limits,
                    alloc: o.alloc,
                    scratch: s,
                },
            )?;
            let brole = MaterialRole(next_baked_role);
            next_baked_role += 1;
            let opt = prepare_gpu_mesh(&unwrapped.mesh.view(), Some(brole), o.optimize_gpu, o.vertex_cache_size, o.alloc, s)?;
            (opt, Some(maps), Some(brole), Some(unwrapped.texels_per_metre))
        } else {
            let opt = prepare_gpu_mesh(&base_view, stamp_role, o.optimize_gpu, o.vertex_cache_size, o.alloc, s)?;
            (opt, None, None, None)
        };
        let written_verts = opt_mesh.view().vertex_count();

        let lod_start = ready_lods.len();
        if let Some(lo) = o.lods {
            let chain = lod_chain(
                &verified_base,
                LodOpts {
                    ratio: lo.ratio,
                    fewest_triangles: lo.fewest_triangles,
                    projection: lo.projection,
                    cos_crease,
                    limits: MeshLimits::SHIPPED,
                    alloc: s,
                    scratch: s,
                },
            )?;
            for level in chain.levels.iter().skip(1) {
                let lv_mesh = prepare_gpu_mesh(&level.mesh.view(), stamp_role, o.optimize_gpu, o.vertex_cache_size, s, s)?;
                ready_lods
                    .try_push(ReadyLod { mesh: lv_mesh, error: level.error, switch_distance: level.switch_distance })
                    .map_err(|_| ModelError::Alloc { what: "asset lod meshes" })?;
            }
        }
        let lod_end = ready_lods.len();
        lod_ranges.try_push((lod_start, lod_end)).map_err(|_| ModelError::Alloc { what: "asset lod ranges" })?;

        let hull_start = ready_hulls.len();
        if let Some(mode) = o.colliders {
            match mode {
                ColliderMode::ConvexHull => {
                    let hull = convex_hull(base_view.positions(), HullOpts { limits: MeshLimits::SHIPPED, alloc: s, scratch: s })?;
                    let hm = hull_to_mesh(&hull, s)?;
                    ready_hulls.try_push(hm).map_err(|_| ModelError::Alloc { what: "asset collider hulls" })?;
                }
                ColliderMode::Decompose { concavity, rays } => {
                    let decomp = decompose(
                        &verified_base,
                        DecomposeOpts { concavity, rays, limits: MeshLimits::SHIPPED, alloc: s, scratch: s },
                    )?;
                    for piece in decomp.pieces.iter() {
                        let hm = hull_to_mesh(&piece.hull, s)?;
                        ready_hulls.try_push(hm).map_err(|_| ModelError::Alloc { what: "asset collider hulls" })?;
                    }
                }
            }
        }
        let hull_end = ready_hulls.len();
        hull_ranges.try_push((hull_start, hull_end)).map_err(|_| ModelError::Alloc { what: "asset hull ranges" })?;

        let effective_budget = match (part.budget.explicit_cap(), allocated.as_ref().map(|a| a[i])) {
            (Some(c), _) => Some(c),
            (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        let budget_status = match effective_budget {
            None => BudgetStatus::Unbounded,
            Some(cap) => {
                if written_tris <= cap {
                    if d.error_bound <= part_tols[i] + Length::nm(1) {
                        BudgetStatus::WithinBudget
                    } else {
                        BudgetStatus::AtBudget
                    }
                } else {
                    match d.stop {
                        Stop::ErrorCeiling => BudgetStatus::OverErrorCeiling,
                        Stop::NoValidCollapse | Stop::Reached => BudgetStatus::OverNoValidCollapse,
                    }
                }
            }
        };
        if !budget_status.ok() {
            all_budgets_ok = false;
        }

        let (fs, fe) = feat_ranges[i];
        let (fidelity, holes) = if let Some(fp) = o.fidelity {
            let tol = part_tols[i];
            let err_budget = d.error_bound + m.vertex_bound;
            let default_pos_tol = (tol * 2.5).max(err_budget + Length::mm(1.0)).max(Length::mm(2.0));
            let hole_tolerance = fp.hole_tolerance.unwrap_or(default_pos_tol);
            let surface_tolerance = fp.surface_tolerance.unwrap_or(default_pos_tol);
            // Sub-triangle bevel offset probe is at least the mesh's own guaranteed geometric
            // error bound so legal `tol`-bounded chords on fillets are not flagged as lost bevels.
            let bevel_probe_tolerance = Some(err_budget.max(tol));
            let max_normal_error = fp.max_normal_error.unwrap_or_else(|| o.preset.default_max_normal_error());
            // Scale minimum hole subdivision feature to the part's grid/extent so meter-scale
            // parts settle cleanly while still resolving every opening/feature.
            let min_hole_feature = fp.min_hole_feature.unwrap_or_else(|| (part_grids[i].feature * 0.5).max(tol * 6.0).max(Length::mm(4.0)));
            let tol_m = tol.as_m();
            let min_bevel_area_m2 = Some((tol_m * 4.0) * (tol_m * 4.0));
            let rep = check_mesh_fidelity_report(
                g,
                part,
                &opt_mesh.view(),
                Some(&m.mesh.view()),
                FidelityOpts {
                    hole_tolerance,
                    surface_tolerance,
                    bevel_probe_tolerance,
                    max_normal_error,
                    min_hole_feature,
                    min_bevel_area_m2,
                    max_bevel_loss: fp.max_bevel_loss,
                    features: &part_feats[fs..fe],
                    most_subdivisions: fp.most_subdivisions,
                    scratch: s,
                    ..
                },
                s,
            )?;
            let fc = rep.checked;
            if !matches!(fc.verdict, Verdict::Pass) {
                all_fidelity_ok = false;
            }
            for fr in rep.features.iter() {
                feature_reports
                    .try_push(PartFeatureReport { part: part.name, report: *fr })
                    .map_err(|_| ModelError::Alloc { what: "asset feature reports" })?;
                if fr.missed() {
                    let hl = MeshHighlight {
                        part_index: i as u32,
                        region: fr.region,
                        style: HighlightStyle::Missed { colour: MISSED_HIGHLIGHT_RGB },
                    };
                    missed_highlights.try_push(hl).map_err(|_| ModelError::Alloc { what: "asset missed highlights" })?;
                    all_highlights.try_push(hl).map_err(|_| ModelError::Alloc { what: "asset all highlights" })?;
                } else {
                    all_highlights
                        .try_push(MeshHighlight {
                            part_index: i as u32,
                            region: fr.region,
                            style: HighlightStyle::Outline { colour: PASS_OUTLINE_RGB },
                        })
                        .map_err(|_| ModelError::Alloc { what: "asset all highlights" })?;
                }
            }
            (Some(fc), fc.metrics.holes)
        } else {
            let topo = topology(&opt_mesh.view(), s).map_err(|_| ModelError::Alloc { what: "asset part topology" })?;
            (None, topo.genus)
        };

        total_meshed += u64::from(m.stats.triangles);
        total_written += u64::from(written_tris);
        total_vertices += u64::from(written_verts);

        part_reports
            .try_push(PartReport {
                name: part.name,
                material: part.material,
                meshed_triangles: m.stats.triangles,
                written_triangles: written_tris,
                written_vertices: written_verts,
                budget: effective_budget,
                budget_status,
                tolerance_used: part_tols[i],
                feature_grid: part_grids[i].feature,
                simplified_within: d.error_bound,
                vertex_bound: m.vertex_bound,
                lod_count: (lod_end - lod_start) as u32,
                collider_hulls: (hull_end - hull_start) as u32,
                holes,
                fidelity,
            })
            .map_err(|_| ModelError::Alloc { what: "asset part reports" })?;

        exported_parts
            .try_push(ExportedPart {
                name: part.name,
                parent: part.parent,
                placement: part.placement,
                material: part.material,
                mesh: opt_mesh,
                baked,
                baked_role,
                texels_per_metre,
            })
            .map_err(|_| ModelError::Alloc { what: "asset exported parts" })?;
    }

    // Stage 6: Build GLB scene from the exact retained exported meshes (using `glb_textured` when
    // any part baked PBR maps, otherwise byte-identical `glb`).
    let mut lod_views: Vec<Lod<'_>, Alloc<'_>> = Vec::new_in(s);
    lod_views.try_reserve(ready_lods.len()).map_err(|_| ModelError::Alloc { what: "asset lod views" })?;
    for lv in ready_lods.iter() {
        lod_views
            .try_push(Lod { mesh: lv.mesh.view(), error: lv.error, switch_distance: lv.switch_distance })
            .map_err(|_| ModelError::Alloc { what: "asset lod views" })?;
    }

    let mut hull_views: Vec<MeshView<'_>, Alloc<'_>> = Vec::new_in(s);
    hull_views.try_reserve(ready_hulls.len()).map_err(|_| ModelError::Alloc { what: "asset hull views" })?;
    for hm in ready_hulls.iter() {
        hull_views.try_push(hm.view()).map_err(|_| ModelError::Alloc { what: "asset hull views" })?;
    }

    let mut nodes: Vec<blaze_cad_out::Part<'_>, Alloc<'_>> = Vec::new_in(s);
    nodes.try_reserve(parts.len()).map_err(|_| ModelError::Alloc { what: "asset glb nodes" })?;
    for (i, (ep, m)) in exported_parts.iter().zip(meshes.iter()).enumerate() {
        let (ls, le) = lod_ranges[i];
        let (hs, he) = hull_ranges[i];
        let mass = o.density.map(|d| {
            let mp = mass_properties(&m.verified(), d);
            MassFacts {
                mass: mp.mass.as_kg(),
                centre: [mp.centre[0].as_m(), mp.centre[1].as_m(), mp.centre[2].as_m()],
                inertia: mp.inertia,
            }
        });
        let collider = if hs < he { Some(Collider::Hulls(&hull_views[hs..he])) } else { None };
        nodes
            .try_push(blaze_cad_out::Part {
                name: ep.name.as_str(),
                parent: ep.parent.map(|p| p.0),
                transform: transform_of(&ep.placement),
                mesh: Some(ep.mesh.view()),
                lods: &lod_views[ls..le],
                mass,
                joint: None,
                collider,
            })
            .map_err(|_| ModelError::Alloc { what: "asset glb nodes" })?;
    }

    let has_any_baked = exported_parts.iter().any(|ep| ep.baked.is_some());
    let glb_bytes = if has_any_baked {
        let normal_scale = o.bake.map_or(1.0, |b| b.normal_strength);
        let mut all_mats: Vec<Material<'_>, Alloc<'_>> = Vec::new_in(s);
        let mut mat_tex: Vec<MaterialTextures, Alloc<'_>> = Vec::new_in(s);
        let mut png_images: Vec<&'_ [u8], Alloc<'_>> = Vec::new_in(s);
        for &mat in o.materials {
            all_mats.try_push(mat).map_err(|_| ModelError::Alloc { what: "asset materials" })?;
            mat_tex.try_push(MaterialTextures::NONE).map_err(|_| ModelError::Alloc { what: "asset material textures" })?;
        }
        for ep in exported_parts.iter() {
            if let Some(ref baked) = ep.baked {
                let idx_base = u32::try_from(png_images.len()).unwrap_or(0);
                png_images.try_push(&baked.base_colour.png).map_err(|_| ModelError::Alloc { what: "asset pngs" })?;
                png_images.try_push(&baked.metallic_roughness.png).map_err(|_| ModelError::Alloc { what: "asset pngs" })?;
                png_images.try_push(&baked.normal.png).map_err(|_| ModelError::Alloc { what: "asset pngs" })?;
                all_mats
                    .try_push(baked_material(ep.name.as_str()))
                    .map_err(|_| ModelError::Alloc { what: "asset baked materials" })?;
                mat_tex
                    .try_push(MaterialTextures {
                        base_colour: Some(idx_base),
                        metallic_roughness: Some(idx_base + 1),
                        normal: Some(idx_base + 2),
                        normal_scale,
                    })
                    .map_err(|_| ModelError::Alloc { what: "asset material textures" })?;
            }
        }
        let scene = blaze_cad_out::Scene { parts: &nodes, materials: &all_mats };
        let pack = TexturePack { png_images: &png_images, material_textures: &mat_tex };
        glb_textured(&scene, &pack, GlbOpts { encoding: o.encoding, up: o.up, limits: o.limits, alloc: o.alloc, scratch: s })
            .map_err(ModelError::Glb)?
    } else {
        let scene = blaze_cad_out::Scene { parts: &nodes, materials: o.materials };
        glb(&scene, GlbOpts { encoding: o.encoding, up: o.up, limits: o.limits, alloc: o.alloc, scratch: s })
            .map_err(ModelError::Glb)?
    };

    let total_budget_u32 = o.total_budget.map(|b| b.0);
    if let Some(tb) = total_budget_u32
        && total_written > u64::from(tb)
    {
        all_budgets_ok = false;
    }

    let requested_features = u32::try_from(o.features.len()).unwrap_or(u32::MAX);
    let checked_features = u32::try_from(feature_reports.len()).unwrap_or(u32::MAX);
    let passed_features = u32::try_from(
        feature_reports
            .iter()
            .filter(|pf| matches!(pf.report.verdict, Verdict::Pass))
            .count(),
    )
    .unwrap_or(u32::MAX);
    if checked_features != requested_features || passed_features != requested_features {
        all_fidelity_ok = false;
    }

    let report = AssetReport {
        parts: part_reports,
        features: feature_reports,
        requested_features,
        checked_features,
        passed_features,
        total_meshed,
        total_written,
        total_vertices,
        total_budget: total_budget_u32,
        glb_bytes: glb_bytes.len() as u64,
        budget_passed: all_budgets_ok,
        fidelity_passed: all_fidelity_ok,
    };

    Ok(GameAsset { glb: glb_bytes, report, parts: exported_parts, missed_highlights, all_highlights })
}
