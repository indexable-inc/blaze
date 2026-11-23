//! Sharper geometric & topological fidelity checks between an exact CAD solid ([`Graph`] +
//! [`Part`]) and its exported/decimated triangle mesh ([`MeshView`]), including **per-feature
//! fidelity targets** ([`FeatureTarget`]) for critical keyholes, bevels, and mating surfaces.
//!
//! Position-only Hausdorff bounds miss three failure modes that break game assets:
//!
//! 1. **Lost / bridged holes**: a coarse octree cell or aggressive collapse bridges flat across a
//!    window, doorway, slot, or keyhole — even inside a huge 2-triangle wall where vertex and
//!    centroid probes miss the opening completely. [`check_mesh_fidelity`] certifies every exported
//!    triangle with adaptive longest-edge bisection guided by `blaze_cad_field::eval_box` and
//!    1-Lipschitz ball pruning: any sub-triangle whose interval reaches empty air zooms straight
//!    onto the bridged void and reports [`FidelityFinding::LostHole`] with its exact 3D coordinate
//!    and void depth. When a pre-decimation reference mesh is provided, exact Euler-characteristic
//!    genus (`blaze_cad_mesh::topology`) also flags any lost through-hole
//!    ([`FidelityFinding::TopologyHoleLost`]) or vanished shell component.
//! 2. **Lost / flattened bevels & chamfers**: deleting a 2–4 mm chamfer moves vertices by only
//!    `0.29 × width` (often below a game asset's position tolerance), yet bends the surface normal
//!    by 30°–45° and destroys specular edge highlights. Evaluating `eval_grad` at triangle
//!    centroids and interval-flagged edge sub-triangles — with a C0 sharp-crease guard so valid
//!    sharp 90° frames and 45° French cleats do not trigger false positives — catches normal
//!    collapse directly ([`FidelityFinding::LostBevel`]), complemented by dihedral bevel-strip
//!    length retention (`blaze_cad_mesh::crease_profile`) against the pre-decimation reference.
//! 3. **Per-feature critical targets**: callers can mark specific keyholes, bevel edges, or mating
//!    surfaces with [`FeatureTarget`] (tighter local position and normal tolerances). Any triangle
//!    touching a marked feature is checked against that feature's tighter bounds (and subdivided
//!    down to its finer `min_feature`), producing per-feature [`FeatureReport`] metrics and
//!    flagging [`FidelityFinding::FeatureMissed`] whenever a critical feature misses tolerance.

#![blaze::sans_io]

use blaze_cad_field::{CadPlant, Detail, Ex, Graph, Name, Scratch, eval_box, eval_grad, eval_point};
use blaze_cad_mesh::{
    CreaseProfileOpts, FeatureProtect, FeatureRegion, FeatureZone, MeshView, crease_profile, topology,
};
use blaze_math::{Aabb3, Vec3};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, Vec};
use blaze_text::{Put, Show, Writes};
use blaze_units::{Angle, Length};

use crate::check::{UAS_PER_DEGREE, put_deg, put_mm, put_point};
use crate::{ModelError, Part, Unsettled, Verdict, part_tape};

/// Semantic kind of a user-marked critical CAD feature (used for sensible tolerance defaults,
/// meshing refinement, and structured fidelity reports).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FeatureKind {
    /// Through-hole, keyhole mounting slot, bore, or recessed pocket.
    Keyhole,
    /// Chamfer, fillet, or specular highlight edge.
    Bevel,
    /// Precision contact, seating, or alignment face (e.g. French cleat 45° face or rabbet).
    MatingSurface,
    /// General critical detail region.
    Detail,
}

impl FeatureKind {
    /// Short human-readable label for tables and diagnostics.
    #[inline]
    pub const fn tag(self) -> &'static str {
        match self {
            FeatureKind::Keyhole => "keyhole",
            FeatureKind::Bevel => "bevel",
            FeatureKind::MatingSurface => "mating",
            FeatureKind::Detail => "detail",
        }
    }

    /// Default per-feature position tolerance derived from a part's baseline `part_tol`.
    #[inline]
    pub fn default_max_error(self, part_tol: Length) -> Length {
        let base_mm = part_tol.as_mm().max(0.05);
        let mm = match self {
            FeatureKind::Keyhole => (base_mm * 0.50).clamp(0.15, 0.75),
            FeatureKind::Bevel => (base_mm * 0.50).clamp(0.15, 0.75),
            FeatureKind::MatingSurface => (base_mm * 0.40).clamp(0.10, 0.50),
            FeatureKind::Detail => (base_mm * 0.60).clamp(0.20, 1.00),
        };
        Length::mm(mm.min(base_mm))
    }

    /// Default per-feature normal-deviation ceiling derived from a part's baseline `part_norm`.
    #[inline]
    pub fn default_max_normal_error(self, part_norm: Angle) -> Angle {
        let cap_deg = match self {
            FeatureKind::Keyhole => 18.0,
            FeatureKind::Bevel => 14.0,
            FeatureKind::MatingSurface => 10.0,
            FeatureKind::Detail => 16.0,
        };
        let cap = Angle::degrees(cap_deg);
        if cap < part_norm { cap } else { part_norm }
    }
}

/// Options for [`FeatureTarget::new`] (rule 83: one options-struct constructor).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct FeatureTargetOpts<'n> {
    /// Short name printed in the fidelity report and findings (e.g. `"keyhole_tl"`).
    pub name: &'n str,
    /// Semantic kind (sets defaults for error/normal tolerances when omitted).
    pub kind: FeatureKind,
    /// Spatial region in the part's local coordinate frame.
    pub region: FeatureRegion,
    /// Tighter position / hole / surface tolerance inside this region (`None` derives from `kind`
    /// and the part's tolerance).
    pub max_error: Option<Length> = None,
    /// Tighter normal-deviation ceiling inside this region (`None` derives from `kind` and the
    /// part's normal limit).
    pub max_normal_error: Option<Angle> = None,
    /// Local octree feature ceiling inside this region (`None` derives from `max_error` and region
    /// size so small keyholes/bevels are refined locally without exploding the rest of the part).
    pub min_feature: Option<Length> = None,
    /// Geometry protection policy during meshing & simplification (`Critical` by default; `Pinned`
    /// refuses to sacrifice this feature's error bound even under Phase-2 budget pressure).
    pub protect: FeatureProtect = FeatureProtect::Critical,
}

/// A named per-feature fidelity target on a CAD [`Part`] (in the part's local coordinate frame).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FeatureTarget {
    pub name: Name,
    pub kind: FeatureKind,
    pub region: FeatureRegion,
    pub max_error: Option<Length>,
    pub max_normal_error: Option<Angle>,
    pub min_feature: Option<Length>,
    pub protect: FeatureProtect,
}

/// Authored physical feature dimensions (Priority 11): automatically derive tight local octree
/// `min_feature`, geometric `max_error`, and normal tolerance from a groove's width, a bevel's
/// radius, a panel seam's gap, a slot's width, or a stencil's stroke width.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FeatureSpec {
    /// Narrow groove or gasket channel authored with physical `width` and `depth`.
    Groove { width: Length, depth: Length },
    /// Filleted or chamfered highlight edge authored with physical `radius`.
    Bevel { radius: Length },
    /// Narrow panel perimeter gap / cover seam authored with physical `gap` and `depth`.
    PanelSeam { gap: Length, depth: Length },
    /// Surface-conforming stencil or decal authored with physical `stroke_width`.
    Stencil { stroke_width: Length },
    /// Narrow slot or keyhole authored with physical `width`.
    Slot { width: Length },
}

impl FeatureTarget {
    /// Construct a [`FeatureTarget`] from [`FeatureTargetOpts`].
    pub fn new(o: FeatureTargetOpts<'_>) -> FeatureTarget {
        FeatureTarget {
            name: Name::new(o.name),
            kind: o.kind,
            region: o.region,
            max_error: o.max_error,
            max_normal_error: o.max_normal_error,
            min_feature: o.min_feature,
            protect: o.protect,
        }
    }

    /// Construct a [`FeatureTarget`] directly from authored physical dimensions ([`FeatureSpec`]),
    /// carrying groove width, bevel radius, seam gap, slot width, or stencil stroke width straight
    /// into local octree refinement and protected QEM budgets without manual guesswork.
    pub fn from_spec(name: &str, region: FeatureRegion, spec: FeatureSpec) -> FeatureTarget {
        match spec {
            FeatureSpec::Groove { width, depth } => {
                let min_f = Length::m((width.as_m() / 3.0).max(1e-4));
                let max_e = Length::m((width.as_m() / 8.0).min(depth.as_m() / 5.0).max(5e-5));
                FeatureTarget::new(FeatureTargetOpts {
                    name,
                    kind: FeatureKind::Detail,
                    region,
                    max_error: Some(max_e),
                    max_normal_error: Some(Angle::degrees(12.0)),
                    min_feature: Some(min_f),
                    protect: FeatureProtect::Critical,
                })
            }
            FeatureSpec::Bevel { radius } => {
                let min_f = Length::m((radius.as_m() * 0.5).max(1e-4));
                // 15° chord sagitta `r * (1 - cos(15°)) ≈ 0.0341 * r`.
                let max_e = Length::m((radius.as_m() * 0.035).max(5e-5));
                FeatureTarget::new(FeatureTargetOpts {
                    name,
                    kind: FeatureKind::Bevel,
                    region,
                    max_error: Some(max_e),
                    max_normal_error: Some(Angle::degrees(10.0)),
                    min_feature: Some(min_f),
                    protect: FeatureProtect::Critical,
                })
            }
            FeatureSpec::PanelSeam { gap, depth } => {
                let min_f = Length::m((gap.as_m() / 2.5).max(1e-4));
                let max_e = Length::m((gap.as_m() / 6.0).min(depth.as_m() / 4.0).max(5e-5));
                FeatureTarget::new(FeatureTargetOpts {
                    name,
                    kind: FeatureKind::MatingSurface,
                    region,
                    max_error: Some(max_e),
                    max_normal_error: Some(Angle::degrees(12.0)),
                    min_feature: Some(min_f),
                    protect: FeatureProtect::Pinned,
                })
            }
            FeatureSpec::Stencil { stroke_width } => {
                let min_f = Length::m((stroke_width.as_m() * 0.5).max(1e-4));
                let max_e = Length::m((stroke_width.as_m() / 8.0).max(5e-5));
                FeatureTarget::new(FeatureTargetOpts {
                    name,
                    kind: FeatureKind::Detail,
                    region,
                    max_error: Some(max_e),
                    max_normal_error: Some(Angle::degrees(14.0)),
                    min_feature: Some(min_f),
                    protect: FeatureProtect::Critical,
                })
            }
            FeatureSpec::Slot { width } => {
                let min_f = Length::m((width.as_m() / 3.0).max(1e-4));
                let max_e = Length::m((width.as_m() / 8.0).max(5e-5));
                FeatureTarget::new(FeatureTargetOpts {
                    name,
                    kind: FeatureKind::Keyhole,
                    region,
                    max_error: Some(max_e),
                    max_normal_error: Some(Angle::degrees(16.0)),
                    min_feature: Some(min_f),
                    protect: FeatureProtect::Pinned,
                })
            }
        }
    }

    /// Resolve effective `(max_error, max_normal_error, min_feature)` against a part's baseline
    /// tolerance and normal limit.
    pub fn resolved(&self, part_tol: Length, part_norm: Angle) -> (Length, Angle, Length) {
        let max_err = self.max_error.unwrap_or_else(|| self.kind.default_max_error(part_tol));
        let max_norm = self.max_normal_error.unwrap_or_else(|| self.kind.default_max_normal_error(part_norm));
        let min_feat = self.min_feature.unwrap_or_else(|| {
            let minor = self.region.minor_extent_m();
            let from_region = if minor > 0.0 { Length::m(minor * 0.25) } else { max_err * 6.0 };
            from_region.min(max_err * 8.0).max(max_err * 2.0)
        });
        (max_err, max_norm, min_feat)
    }

    /// Convert this target into a low-level [`FeatureZone`] consumed by `blaze-cad-mesh`'s
    /// local octree refinement (`MeshOpts::zones`) and protected QEM decimation
    /// (`DecimateOpts::zones`).
    pub fn to_zone(&self, part_tol: Length, part_norm: Angle) -> FeatureZone {
        let (max_err, max_norm, min_feat) = self.resolved(part_tol, part_norm);
        // Guard single-step QEM normal turns to ~55% of the cumulative normal budget so chained
        // collapses along a curved bevel stay within `max_norm`.
        let step_rad = (max_norm.as_radians() * 0.55).clamp(0.08, core::f64::consts::FRAC_PI_2 - 0.05);
        let c = blaze_math::cos(step_rad);
        let min_cos_sq = (c * c).clamp(0.0, 0.995);
        let mut zone = FeatureZone::new(self.region, self.protect);
        zone.max_error = Some(max_err);
        // Only force a local octree `min_feature` cell ceiling when explicitly requested OR for
        // `Keyhole`/`Detail` regions (small holes/slots that might fall between coarse lattice
        // samples). Flat `MatingSurface` faces and curved `Bevel`s are already refined adaptively
        // by `settles()` against `max_error` without exploding adjacent flat walls!
        zone.min_feature = match self.min_feature {
            Some(explicit) => Some(explicit),
            None => match self.kind {
                FeatureKind::Keyhole | FeatureKind::Detail => Some(min_feat.max(Length::mm(2.0))),
                FeatureKind::Bevel | FeatureKind::MatingSurface => None,
            },
        };
        zone.min_cos_sq = Some(min_cos_sq);
        zone
    }
}

/// Quantitative fidelity result for one marked [`FeatureTarget`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FeatureReport {
    pub name: Name,
    pub kind: FeatureKind,
    pub region: FeatureRegion,
    pub protect: FeatureProtect,
    /// Effective position tolerance enforced inside this feature.
    pub max_error: Length,
    /// Effective normal-deviation ceiling enforced inside this feature.
    pub max_normal_error: Angle,
    /// Worst positive CAD field depth (`f(p) > 0`, bridged void) measured inside this feature.
    pub measured_outside: Length,
    /// Worst negative CAD field depth (`-f(p) > 0`, clipped solid) measured inside this feature.
    pub measured_inside: Length,
    /// Worst confirmed normal deviation measured inside this feature.
    pub measured_normal_error: Angle,
    /// Number of exported triangles overlapping this feature region.
    pub triangles: u32,
    /// Feature verdict (`Verdict::Pass` when within both position and normal targets).
    pub verdict: Verdict<FidelityFinding>,
}

impl FeatureReport {
    /// True when this marked feature missed its tolerance or lost its geometry.
    #[inline]
    pub const fn missed(&self) -> bool {
        matches!(self.verdict, Verdict::Found(_))
    }

    /// Worst positional deviation (`max(measured_outside, measured_inside)`).
    #[inline]
    pub fn measured_error(&self) -> Length {
        self.measured_outside.max(self.measured_inside)
    }
}

/// Planted bugs for [`check_mesh_fidelity_planted`], caught by the test controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FidelityPlant {
    None,
    /// Skips interval-guided triangle subdivision and probes only each triangle's single centroid
    /// (misses a small bridged hole hidden off-center inside a large wall triangle).
    SkipVoidSubdivision,
    /// Ignores `eval_grad` normal deviation and bevel-strip retention (misses a flattened bevel
    /// whose positional error stays under `hole_tolerance`).
    IgnoreNormalDeviation,
    /// Ignores [`FidelityOpts::features`] (misses a tighter per-feature keyhole/bevel/mating
    /// violation that stays inside the looser part-wide tolerance).
    IgnoreFeatureTargets,
}

impl Plants for FidelityPlant {}

/// Options for [`check_mesh_fidelity`] and [`check_mesh_fidelity_report`].
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct FidelityOpts<'s, 'f> {
    /// Maximum positive CAD field depth `f(p) > 0` (empty air) a mesh triangle may span before
    /// flagging [`FidelityFinding::LostHole`].
    pub hole_tolerance: Length = Length::nm(2_500_000),
    /// Maximum negative CAD field depth `f(p) < 0` (cutting inside the solid) a mesh triangle may
    /// penetrate before flagging [`FidelityFinding::SurfaceClipped`].
    pub surface_tolerance: Length = Length::nm(2_500_000),
    /// Positional probe threshold above which interval subdivision zooms in to test bevel normal
    /// fidelity (defaults to `0.25 × min(hole_tolerance, surface_tolerance)` when `None`).
    pub bevel_probe_tolerance: Option<Length> = None,
    /// Maximum allowed angular deviation between the exported triangle normal and the exact CAD
    /// field gradient `eval_grad` before flagging [`FidelityFinding::LostBevel`].
    pub max_normal_error: Angle = Angle::microarcsec(18 * UAS_PER_DEGREE),
    /// Smallest hole or bevel feature size to resolve during triangle subdivision (holes wider
    /// than this are guaranteed to be sampled).
    pub min_hole_feature: Length = Length::nm(4_000_000),
    /// Minimum triangle or sub-triangle area required before a normal-angle mismatch triggers
    /// [`FidelityFinding::LostBevel`] (`None` derives a tight area floor from `min_hole_feature`).
    pub min_bevel_area_m2: Option<f64> = None,
    /// Minimum edge length of a bevel strip in the reference mesh before bevel-retention loss is
    /// enforced (prevents noise on tiny parts with no real bevels).
    pub min_reference_bevel_length: Length = Length::nm(20_000_000),
    /// Maximum allowed fraction (`0.0..=1.0`) of reference bevel edge length lost during
    /// decimation before flagging [`FidelityFinding::LostBevel`].
    pub max_bevel_loss: f64 = 0.45,
    /// Optional per-feature critical targets (keyholes, bevels, mating surfaces) enforced with
    /// tighter local position, normal, and subdivision tolerances.
    pub features: &'f [FeatureTarget] = &[],
    /// Hard cap on total triangle subdivisions per part (rule 18: bounded progress).
    pub most_subdivisions: u64 = 1 << 18,
    /// Scratch allocator for the compiled tape, subdivision stack, and topology tables.
    pub scratch: Alloc<'s>,
}

/// First / worst fidelity defect discovered between the exact CAD solid and its exported mesh.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FidelityFinding {
    /// An exported triangle spans across empty space (`f(p) > tolerance`) — a bridged or missed
    /// hole, window, doorway, or recess.
    LostHole {
        at: [Length; 3],
        void_depth: Length,
        tolerance: Length,
    },
    /// Exact Euler-characteristic genus dropped relative to the pre-decimation mesh: at least one
    /// through-hole was lost.
    TopologyHoleLost {
        expected_holes: u32,
        actual_holes: u32,
    },
    /// Connected shell component count changed relative to the pre-decimation mesh (e.g. a small
    /// detached trim piece or internal cavity vanished).
    ComponentCountChanged {
        expected: u32,
        actual: u32,
    },
    /// A bevel, chamfer, or curved highlight transition was flattened or lost (either by exact
    /// CAD normal deviation `eval_grad`, or by dihedral bevel-strip length collapse vs reference).
    LostBevel {
        at: [Length; 3],
        normal_error: Angle,
        max_normal_error: Angle,
        surface_offset: Length,
        bevel_retention: Option<f64>,
    },
    /// An exported triangle cuts into solid material deeper than `surface_tolerance`.
    SurfaceClipped {
        at: [Length; 3],
        depth: Length,
        tolerance: Length,
    },
    /// A user-marked critical feature (keyhole, bevel, mating surface, detail) missed its tighter
    /// local tolerance or lost its geometry.
    FeatureMissed {
        feature: Name,
        kind: FeatureKind,
        at: [Length; 3],
        measured_error: Length,
        max_error: Length,
        normal_error: Angle,
        max_normal_error: Angle,
    },
}

/// Quantitative fidelity metrics gathered across the whole exported part mesh.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FidelityMetrics {
    /// Worst positive field value (`f(p) > 0`, bridged void depth) measured on the mesh.
    pub max_outside_error: Length,
    /// Worst negative field value (`-f(p) > 0`, shaved solid depth) measured on the mesh.
    pub max_inside_error: Length,
    /// Worst angular deviation between triangle normal and CAD field gradient.
    pub max_normal_error: Angle,
    /// Area-weighted mean normal deviation across sampled triangles.
    pub mean_normal_error: Angle,
    /// Exact topological through-hole count (genus) of the exported mesh (`None` if non-manifold/open).
    pub holes: Option<u32>,
    /// Exact topological through-hole count of the pre-decimation reference mesh, if supplied.
    pub reference_holes: Option<u32>,
    /// Connected shell component count of the exported mesh.
    pub components: u32,
    /// Retained fraction (`0.0..=1.0+`) of dihedral bevel edge length vs reference mesh, if
    /// reference had meaningful bevels.
    pub bevel_retention: Option<f64>,
    /// Number of sampled triangles/sub-triangles exceeding `hole_tolerance`.
    pub bridged_void_samples: u32,
    /// Number of sampled triangles/sub-triangles exceeding `max_normal_error`.
    pub bevel_violation_samples: u32,
    /// Number of marked [`FeatureTarget`]s evaluated on this part.
    pub features_checked: u32,
    /// Number of marked [`FeatureTarget`]s that missed their target tolerance.
    pub features_failed: u32,
    /// Total adaptive triangle subdivisions performed.
    pub subdivisions: u64,
}

/// Complete fidelity check outcome: three-way [`Verdict`] plus full quantitative metrics.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FidelityChecked {
    pub verdict: Verdict<FidelityFinding>,
    pub metrics: FidelityMetrics,
}

/// Fidelity check outcome together with per-feature [`FeatureReport`] entries.
#[derive(Debug)]
pub struct FidelityReport<'a> {
    pub checked: FidelityChecked,
    pub features: Vec<FeatureReport, Alloc<'a>>,
}

// ---- blaze_text::Show formatting ---------------------------------------------------------

impl Show for FidelityFinding {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match *self {
            FidelityFinding::LostHole { at, void_depth, tolerance } => {
                out.put_str("lost/bridged hole: mesh spans ");
                put_mm(out, void_depth.as_nm(), 2);
                out.put_str(" mm into empty space (limit ");
                put_mm(out, tolerance.as_nm(), 2);
                out.put_str(" mm) at ");
                put_point(out, &at, 2);
            }
            FidelityFinding::TopologyHoleLost { expected_holes, actual_holes } => {
                out.put_str("lost through-hole: expected ");
                out.int(expected_holes);
                out.put_str(" holes (genus), exported mesh has ");
                out.int(actual_holes);
            }
            FidelityFinding::ComponentCountChanged { expected, actual } => {
                out.put_str("component count changed: expected ");
                out.int(expected);
                out.put_str(" shells, exported mesh has ");
                out.int(actual);
            }
            FidelityFinding::LostBevel { at, normal_error, max_normal_error, surface_offset, bevel_retention } => {
                out.put_str("lost/flattened bevel: normal off by ");
                put_deg(out, normal_error.as_microarcsec(), 1);
                out.put_str(" deg (limit ");
                put_deg(out, max_normal_error.as_microarcsec(), 1);
                out.put_str(" deg, offset ");
                put_mm(out, surface_offset.as_nm(), 2);
                out.put_str(" mm)");
                if let Some(ret) = bevel_retention {
                    let pct = (ret.clamp(0.0, 2.0) * 100.0 + 0.5) as u64;
                    out.put_str(", bevel retention ");
                    out.int(pct);
                    out.put_byte(b'%');
                }
                out.put_str(" at ");
                put_point(out, &at, 2);
            }
            FidelityFinding::SurfaceClipped { at, depth, tolerance } => {
                out.put_str("surface clipped ");
                put_mm(out, depth.as_nm(), 2);
                out.put_str(" mm inside solid (limit ");
                put_mm(out, tolerance.as_nm(), 2);
                out.put_str(" mm) at ");
                put_point(out, &at, 2);
            }
            FidelityFinding::FeatureMissed {
                feature,
                kind,
                at,
                measured_error,
                max_error,
                normal_error,
                max_normal_error,
            } => {
                out.put_str("feature ");
                out.put_str(feature.as_str());
                out.put_str(" [");
                out.put_str(kind.tag());
                out.put_str("] missed target: err ");
                put_mm(out, measured_error.as_nm(), 2);
                out.put_str("/");
                put_mm(out, max_error.as_nm(), 2);
                out.put_str(" mm, norm ");
                put_deg(out, normal_error.as_microarcsec(), 1);
                out.put_str("/");
                put_deg(out, max_normal_error.as_microarcsec(), 1);
                out.put_str(" deg at ");
                put_point(out, &at, 2);
            }
        }
    }
}

// ---- Core checker ------------------------------------------------------------------------

struct FeatureAcc {
    target: FeatureTarget,
    max_err_m: f64,
    max_norm_rad: f64,
    cos_max_norm: f64,
    min_edge_m: f64,
    max_outside: f64,
    max_inside: f64,
    max_normal_rad: f64,
    triangles: u32,
    worst_pos: Option<(Vec3, f64)>,
    worst_norm: Option<(Vec3, f64, f64)>,
}

/// Verify that `mesh` (in `part`'s own local frame) preserves the CAD solid's holes, bevels,
/// normals, surface envelope, and any marked [`FeatureTarget`]s.
pub fn check_mesh_fidelity<K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    mesh: &MeshView<'_>,
    reference: Option<&MeshView<'_>>,
    o: FidelityOpts<'_, '_>,
) -> Result<FidelityChecked, ModelError> {
    check_mesh_fidelity_planted(g, part, mesh, reference, o, NoPlant)
}

/// [`check_mesh_fidelity`] with a planted bug, for the test controls.
pub fn check_mesh_fidelity_planted<K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    mesh: &MeshView<'_>,
    reference: Option<&MeshView<'_>>,
    o: FidelityOpts<'_, '_>,
    plant: impl Planted<FidelityPlant>,
) -> Result<FidelityChecked, ModelError> {
    Ok(check_mesh_fidelity_report_planted(g, part, mesh, reference, o, o.scratch, plant)?.checked)
}

/// Like [`check_mesh_fidelity`], additionally returning a per-feature [`FeatureReport`] for every
/// [`FeatureTarget`] in `o.features`.
pub fn check_mesh_fidelity_report<'a, K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    mesh: &MeshView<'_>,
    reference: Option<&MeshView<'_>>,
    o: FidelityOpts<'_, '_>,
    alloc: Alloc<'a>,
) -> Result<FidelityReport<'a>, ModelError> {
    check_mesh_fidelity_report_planted(g, part, mesh, reference, o, alloc, NoPlant)
}

/// [`check_mesh_fidelity_report`] with a planted bug, for the test controls.
pub fn check_mesh_fidelity_report_planted<'a, K: Ex, P: Planted<CadPlant>>(
    g: &Graph<'_, P>,
    part: &Part<K>,
    mesh: &MeshView<'_>,
    reference: Option<&MeshView<'_>>,
    o: FidelityOpts<'_, '_>,
    alloc: Alloc<'a>,
    plant: impl Planted<FidelityPlant>,
) -> Result<FidelityReport<'a>, ModelError> {
    if o.hole_tolerance <= Length::ZERO {
        return Err(ModelError::Argument {
            function: "check_mesh_fidelity",
            arg: "hole_tolerance",
            accepted: "a length above zero",
        });
    }
    if o.surface_tolerance <= Length::ZERO {
        return Err(ModelError::Argument {
            function: "check_mesh_fidelity",
            arg: "surface_tolerance",
            accepted: "a length above zero",
        });
    }
    if o.min_hole_feature <= Length::ZERO {
        return Err(ModelError::Argument {
            function: "check_mesh_fidelity",
            arg: "min_hole_feature",
            accepted: "a length above zero",
        });
    }
    if !(o.max_normal_error.as_radians() > 0.0 && o.max_normal_error.as_radians() < core::f64::consts::PI) {
        return Err(ModelError::Argument {
            function: "check_mesh_fidelity",
            arg: "max_normal_error",
            accepted: "an angle strictly between 0 and 180 degrees",
        });
    }

    let s = o.scratch;
    let tape = part_tape(g, part, Detail::Full, s)?;
    let mut sc = Scratch::new(s);

    let topo = topology(mesh, s).map_err(|_| ModelError::Alloc { what: "fidelity topology" })?;
    let ref_topo = match reference {
        Some(r) => Some(topology(r, s).map_err(|_| ModelError::Alloc { what: "fidelity reference topology" })?),
        None => None,
    };

    // Bevel dihedral strip retention when a pre-decimation reference is available.
    let bevel_retention = if let Some(r) = reference
        && !plant.has(FidelityPlant::IgnoreNormalDeviation)
    {
        let ref_prof = crease_profile(r, CreaseProfileOpts { scratch: s, .. })
            .map_err(|_| ModelError::Alloc { what: "reference crease profile" })?;
        if ref_prof.bevel_edge_length >= o.min_reference_bevel_length {
            let cur_prof = crease_profile(mesh, CreaseProfileOpts { scratch: s, .. })
                .map_err(|_| ModelError::Alloc { what: "exported crease profile" })?;
            let r_m = ref_prof.bevel_edge_length.as_m();
            if r_m > 0.0 {
                Some(cur_prof.bevel_edge_length.as_m() / r_m)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let hole_tol = o.hole_tolerance.as_m();
    let surf_tol = o.surface_tolerance.as_m();
    let bevel_probe = o
        .bevel_probe_tolerance
        .map_or((hole_tol.min(surf_tol) * 0.25).max(5e-5), |l| l.as_m().max(1e-6));
    let min_edge = o.min_hole_feature.as_m();
    let max_norm_rad = o.max_normal_error.as_radians();
    let cos_max_norm = blaze_math::cos(max_norm_rad);
    // Sliver area floor so microscopic corner artifacts don't trigger false bevel alarms.
    let min_tri_area = o.min_bevel_area_m2.unwrap_or_else(|| (min_edge * min_edge * 0.01).min(2.5e-7));

    let skip_features = plant.has(FidelityPlant::IgnoreFeatureTargets);
    let base_part_tol = o.hole_tolerance.min(o.surface_tolerance);
    let mut feat_accs: Vec<FeatureAcc, Alloc<'_>> = Vec::new_in(s);
    if !skip_features && !o.features.is_empty() {
        feat_accs.try_reserve(o.features.len()).map_err(|_| ModelError::Alloc { what: "fidelity feature targets" })?;
        for &ft in o.features {
            let (fe, fnm, fmin) = ft.resolved(base_part_tol, o.max_normal_error);
            if fe <= Length::ZERO {
                return Err(ModelError::Argument {
                    function: "check_mesh_fidelity",
                    arg: "features.max_error",
                    accepted: "a length above zero",
                });
            }
            let fn_rad = fnm.as_radians();
            if !(fn_rad > 0.0 && fn_rad < core::f64::consts::PI) {
                return Err(ModelError::Argument {
                    function: "check_mesh_fidelity",
                    arg: "features.max_normal_error",
                    accepted: "an angle strictly between 0 and 180 degrees",
                });
            }
            feat_accs
                .try_push(FeatureAcc {
                    target: ft,
                    max_err_m: fe.as_m(),
                    max_norm_rad: fn_rad,
                    cos_max_norm: blaze_math::cos(fn_rad),
                    min_edge_m: fmin.as_m().min(min_edge).max(5e-4),
                    max_outside: 0.0,
                    max_inside: 0.0,
                    max_normal_rad: 0.0,
                    triangles: 0,
                    worst_pos: None,
                    worst_norm: None,
                })
                .map_err(|_| ModelError::Alloc { what: "fidelity feature targets" })?;
        }
    }
    let mut active_feats: Vec<usize, Alloc<'_>> = Vec::new_in(s);

    let mut max_outside = 0.0f64;
    let mut max_inside = 0.0f64;
    let mut max_normal_rad = 0.0f64;
    let mut normal_area_sum = 0.0f64;
    let mut area_sum = 0.0f64;
    let mut bridged_void_samples = 0u32;
    let mut bevel_violation_samples = 0u32;
    let mut subdivisions = 0u64;
    let mut budget_hit = false;

    let mut worst_hole: Option<(Vec3, f64)> = None;
    let mut worst_clip: Option<(Vec3, f64)> = None;
    let mut worst_bevel: Option<(Vec3, f64, f64)> = None;

    let mut stack: Vec<([Vec3; 3], u8), Alloc<'_>> = Vec::new_in(s);
    let skip_subdiv = plant.has(FidelityPlant::SkipVoidSubdivision);
    let skip_normal = plant.has(FidelityPlant::IgnoreNormalDeviation);

    let pos = mesh.positions();
    let nrms = mesh.normals();
    let has_nrms = nrms.len() == pos.len();
    for tri in mesh.triangles() {
        let (i0, i1, i2) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
        let a = to_v3(pos[i0]);
        let b = to_v3(pos[i1]);
        let c = to_v3(pos[i2]);
        let cross = (b - a).cross(c - a);
        let twice_area = cross.length();
        if !(twice_area > 0.0 && twice_area.is_finite()) {
            continue;
        }
        let tri_area = 0.5 * twice_area;
        let tri_n = cross * (1.0 / twice_area);
        let (n0, n1, n2) = if has_nrms {
            (
                to_v3(nrms[i0]).normalized().unwrap_or(tri_n),
                to_v3(nrms[i1]).normalized().unwrap_or(tri_n),
                to_v3(nrms[i2]).normalized().unwrap_or(tri_n),
            )
        } else {
            (tri_n, tri_n, tri_n)
        };
        let shade_at = |p: Vec3| -> Vec3 {
            if !has_nrms {
                return tri_n;
            }
            let (w0, w1, w2) = barycentric(a, b, c, p);
            (n0 * w0 + n1 * w1 + n2 * w2).normalized().unwrap_or(tri_n)
        };
        let centroid = (a + b + c) * (1.0 / 3.0);

        // Identify marked features touching this triangle.
        active_feats.clear();
        let mut tri_pos_bound = if skip_normal { hole_tol } else { hole_tol.min(bevel_probe) };
        let mut tri_surf_bound = surf_tol;
        let mut tri_min_edge = min_edge;
        let mut tri_min_cos_norm = cos_max_norm;
        if !feat_accs.is_empty() {
            for (fi, fa) in feat_accs.iter_mut().enumerate() {
                if fa.target.region.intersects_triangle(a, b, c) {
                    fa.triangles += 1;
                    active_feats.try_push(fi).map_err(|_| ModelError::Alloc { what: "fidelity active features" })?;
                    tri_pos_bound = tri_pos_bound.min(fa.max_err_m * 0.5);
                    tri_surf_bound = tri_surf_bound.min(fa.max_err_m);
                    tri_min_edge = tri_min_edge.min(fa.min_edge_m);
                    tri_min_cos_norm = tri_min_cos_norm.max(fa.cos_max_norm);
                }
            }
        }

        // 1) Baseline centroid sample (position + CAD gradient normal vs exported shading normal).
        let (f_c, g_c) = eval_grad(&tape, centroid, &mut sc);
        if f_c.is_finite() {
            if f_c > 0.0 {
                max_outside = max_outside.max(f_c);
                if f_c > hole_tol {
                    bridged_void_samples += 1;
                    if worst_hole.is_none_or(|(_, d)| f_c > d) {
                        worst_hole = Some((centroid, f_c));
                    }
                }
            } else {
                let depth = -f_c;
                max_inside = max_inside.max(depth);
                if depth > surf_tol && worst_clip.is_none_or(|(_, d)| depth > d) {
                    worst_clip = Some((centroid, depth));
                }
            }
            for &fi in active_feats.iter() {
                let fa = &mut feat_accs[fi];
                if fa.target.region.contains(centroid) {
                    record_feature_pos(fa, centroid, f_c);
                }
            }
        }
        if !skip_normal
            && let Some(n_cad) = g_c.normalized()
        {
            let n_mesh = (n0 + n1 + n2).normalized().unwrap_or(tri_n);
            let cos_a = n_mesh.dot(n_cad).clamp(-1.0, 1.0);
            let ang = blaze_math::acos(cos_a);
            area_sum += tri_area;
            normal_area_sum += ang * tri_area;
            if tri_area >= min_tri_area {
                let needs_confirm = cos_a < tri_min_cos_norm;
                let confirmed_ang = if needs_confirm {
                    confirm_normal_mismatch(&tape, &shade_at, a, b, c, centroid, f_c, bevel_probe, ang, &mut sc)
                } else {
                    ang
                };
                max_normal_rad = max_normal_rad.max(confirmed_ang);
                if confirmed_ang > max_norm_rad {
                    bevel_violation_samples += 1;
                    if worst_bevel.is_none_or(|(_, a0, _)| confirmed_ang > a0) {
                        worst_bevel = Some((centroid, confirmed_ang, f_c.abs()));
                    }
                }
                for &fi in active_feats.iter() {
                    let fa = &mut feat_accs[fi];
                    if fa.target.region.contains(centroid) {
                        record_feature_norm(fa, centroid, confirmed_ang, f_c.abs());
                    }
                }
            }
        }

        if skip_subdiv {
            continue;
        }

        // 2) Fast 1-Lipschitz ball certificate: because every blaze-cad-field tape satisfies
        // `|f(p) - f(q)| <= |p - q|`, if `f(c) + R_tri <= tri_pos_bound` and
        // `f(c) - R_tri >= -tri_surf_bound`, NO point on the triangle can violate any position
        // bound! (Skip early-out only if a marked feature overlaps a large triangle needing finer
        // normal checks inside that feature.)
        let r_tri = tri_radius(a, b, c, centroid);
        if active_feats.is_empty() && f_c.is_finite() && f_c + r_tri <= tri_pos_bound && f_c - r_tri >= -tri_surf_bound {
            continue;
        }

        // 3) Interval-certified triangle subdivision: proves NO bridged hole, flattened bevel, or
        // missed critical feature hides anywhere inside even a giant wall triangle.
        let tri_box = tri_aabb(a, b, c);
        let (iv, _) = eval_box(&tape, &tri_box, &mut sc);
        if active_feats.is_empty() && iv.hi() <= tri_pos_bound && iv.lo() >= -tri_surf_bound {
            continue;
        }
        let max_e = max_tri_edge(a, b, c);
        if max_e <= tri_min_edge {
            continue;
        }

        stack.clear();
        stack.try_push(([a, b, c], 0u8)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;

        while let Some(([p0, p1, p2], depth)) = stack.pop() {
            if depth > 0 {
                subdivisions += 1;
                if subdivisions > o.most_subdivisions {
                    budget_hit = true;
                    break;
                }
                let sb = tri_aabb(p0, p1, p2);
                let mut sub_has_feat = false;
                let mut sub_pos_bound = if skip_normal { hole_tol } else { hole_tol.min(bevel_probe) };
                let mut sub_surf_bound = surf_tol;
                let mut sub_min_edge = min_edge;
                let mut sub_min_cos = cos_max_norm;
                for &fi in active_feats.iter() {
                    let fa = &feat_accs[fi];
                    if fa.target.region.intersects_aabb(&sb) {
                        sub_has_feat = true;
                        sub_pos_bound = sub_pos_bound.min(fa.max_err_m * 0.5);
                        sub_surf_bound = sub_surf_bound.min(fa.max_err_m);
                        sub_min_edge = sub_min_edge.min(fa.min_edge_m);
                        sub_min_cos = sub_min_cos.max(fa.cos_max_norm);
                    }
                }
                let (siv, _) = eval_box(&tape, &sb, &mut sc);
                if !sub_has_feat && siv.hi() <= sub_pos_bound && siv.lo() >= -sub_surf_bound {
                    continue;
                }
                let mid = (p0 + p1 + p2) * (1.0 / 3.0);
                let f_m = eval_point(&tape, mid, &mut sc);
                if f_m.is_finite() {
                    let r_sub = tri_radius(p0, p1, p2, mid);
                    if !sub_has_feat && f_m + r_sub <= sub_pos_bound && f_m - r_sub >= -sub_surf_bound {
                        continue;
                    }
                    if f_m > 0.0 {
                        max_outside = max_outside.max(f_m);
                        if f_m > hole_tol {
                            bridged_void_samples += 1;
                            if worst_hole.is_none_or(|(_, d)| f_m > d) {
                                worst_hole = Some((mid, f_m));
                            }
                        }
                    } else {
                        let d_in = -f_m;
                        max_inside = max_inside.max(d_in);
                        if d_in > surf_tol && worst_clip.is_none_or(|(_, d)| d_in > d) {
                            worst_clip = Some((mid, d_in));
                        }
                    }
                    let mut in_any_feat = false;
                    for &fi in active_feats.iter() {
                        let fa = &mut feat_accs[fi];
                        if fa.target.region.contains(mid) {
                            in_any_feat = true;
                            record_feature_pos(fa, mid, f_m);
                        }
                    }
                    if !skip_normal && (f_m > bevel_probe || (in_any_feat && f_m.abs() > sub_pos_bound)) {
                        let (_, gm) = eval_grad(&tape, mid, &mut sc);
                        if let Some(n_cad) = gm.normalized() {
                            let n_mesh = shade_at(mid);
                            let cos_a = n_mesh.dot(n_cad).clamp(-1.0, 1.0);
                            let ang = blaze_math::acos(cos_a);
                            let sub_area = 0.5 * (p1 - p0).cross(p2 - p0).length();
                            if (sub_area >= min_tri_area || in_any_feat) && cos_a < sub_min_cos {
                                let confirmed_ang = confirm_normal_mismatch(
                                    &tape,
                                    &shade_at,
                                    p0,
                                    p1,
                                    p2,
                                    mid,
                                    f_m,
                                    bevel_probe,
                                    ang,
                                    &mut sc,
                                );
                                if f_m > bevel_probe && sub_area >= min_tri_area {
                                    max_normal_rad = max_normal_rad.max(confirmed_ang);
                                    if confirmed_ang > max_norm_rad {
                                        bevel_violation_samples += 1;
                                        if worst_bevel.is_none_or(|(_, a0, _)| confirmed_ang > a0) {
                                            worst_bevel = Some((mid, confirmed_ang, f_m));
                                        }
                                    }
                                }
                                if in_any_feat {
                                    for &fi in active_feats.iter() {
                                        let fa = &mut feat_accs[fi];
                                        if fa.target.region.contains(mid) {
                                            record_feature_norm(fa, mid, confirmed_ang, f_m.abs());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Stop subdividing cleanly once a sub-triangle inside a feature is also
                    // 1-Lipschitz certified within that feature's tighter error & below min_edge.
                    if sub_has_feat
                        && f_m + r_sub <= sub_pos_bound
                        && f_m - r_sub >= -sub_surf_bound
                        && max_tri_edge(p0, p1, p2) <= sub_min_edge * 2.0
                    {
                        continue;
                    }
                }
            }
            // Longest-edge 2-way bisection: avoids the 4^d explosion on high-aspect-ratio
            // architectural triangles while shrinking every sub-triangle below its target edge.
            if depth < 18 {
                let e01 = (p1 - p0).length();
                let e12 = (p2 - p1).length();
                let e20 = (p0 - p2).length();
                let max_e = e01.max(e12).max(e20);
                let sb = tri_aabb(p0, p1, p2);
                let mut local_min_edge = min_edge;
                for &fi in active_feats.iter() {
                    if feat_accs[fi].target.region.intersects_aabb(&sb) {
                        local_min_edge = local_min_edge.min(feat_accs[fi].min_edge_m);
                    }
                }
                if max_e > local_min_edge {
                    let nd = depth + 1;
                    if e01 >= e12 && e01 >= e20 {
                        let m = (p0 + p1) * 0.5;
                        stack.try_push(([p0, m, p2], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                        stack.try_push(([m, p1, p2], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                    } else if e12 >= e20 {
                        let m = (p1 + p2) * 0.5;
                        stack.try_push(([p0, p1, m], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                        stack.try_push(([p0, m, p2], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                    } else {
                        let m = (p2 + p0) * 0.5;
                        stack.try_push(([p0, p1, m], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                        stack.try_push(([m, p1, p2], nd)).map_err(|_| ModelError::Alloc { what: "fidelity subdivision stack" })?;
                    }
                }
            }
        }
        if budget_hit {
            break;
        }
    }

    // Build per-feature reports and find the first missed critical feature.
    let mut feature_reports: Vec<FeatureReport, Alloc<'a>> = Vec::new_in(alloc);
    feature_reports
        .try_reserve(feat_accs.len())
        .map_err(|_| ModelError::Alloc { what: "fidelity feature reports" })?;
    let mut features_failed = 0u32;
    let mut first_feature_missed: Option<FidelityFinding> = None;

    for fa in feat_accs.iter() {
        let max_err = Length::m(fa.max_err_m);
        let max_norm = Angle::radians(fa.max_norm_rad);
        let meas_out = Length::m(fa.max_outside);
        let meas_in = Length::m(fa.max_inside);
        let meas_norm = Angle::radians(fa.max_normal_rad);
        let f_verdict = if fa.triangles == 0 {
            let c = (fa.target.region.aabb().lo + fa.target.region.aabb().hi) * 0.5;
            let finding = FidelityFinding::FeatureMissed {
                feature: fa.target.name,
                kind: fa.target.kind,
                at: [Length::m(c.x), Length::m(c.y), Length::m(c.z)],
                measured_error: Length::m(fa.target.region.minor_extent_m()),
                max_error: max_err,
                normal_error: Angle::ZERO,
                max_normal_error: max_norm,
            };
            Verdict::Found(finding)
        } else if let Some((p, err_m)) = fa.worst_pos {
            let finding = FidelityFinding::FeatureMissed {
                feature: fa.target.name,
                kind: fa.target.kind,
                at: [Length::m(p.x), Length::m(p.y), Length::m(p.z)],
                measured_error: Length::m(err_m),
                max_error: max_err,
                normal_error: meas_norm,
                max_normal_error: max_norm,
            };
            Verdict::Found(finding)
        } else if let Some((p, ang_rad, off_m)) = fa.worst_norm {
            let finding = FidelityFinding::FeatureMissed {
                feature: fa.target.name,
                kind: fa.target.kind,
                at: [Length::m(p.x), Length::m(p.y), Length::m(p.z)],
                measured_error: Length::m(off_m.max(fa.max_outside.max(fa.max_inside))),
                max_error: max_err,
                normal_error: Angle::radians(ang_rad),
                max_normal_error: max_norm,
            };
            Verdict::Found(finding)
        } else {
            Verdict::Pass
        };
        if let Verdict::Found(finding) = f_verdict {
            features_failed += 1;
            if first_feature_missed.is_none() {
                first_feature_missed = Some(finding);
            }
        }
        feature_reports
            .try_push(FeatureReport {
                name: fa.target.name,
                kind: fa.target.kind,
                region: fa.target.region,
                protect: fa.target.protect,
                max_error: max_err,
                max_normal_error: max_norm,
                measured_outside: meas_out,
                measured_inside: meas_in,
                measured_normal_error: meas_norm,
                triangles: fa.triangles,
                verdict: f_verdict,
            })
            .map_err(|_| ModelError::Alloc { what: "fidelity feature reports" })?;
    }

    let mean_normal_rad = if area_sum > 0.0 { normal_area_sum / area_sum } else { 0.0 };
    let metrics = FidelityMetrics {
        max_outside_error: Length::m(max_outside),
        max_inside_error: Length::m(max_inside),
        max_normal_error: Angle::radians(max_normal_rad),
        mean_normal_error: Angle::radians(mean_normal_rad),
        holes: topo.genus,
        reference_holes: ref_topo.and_then(|r| r.genus),
        components: topo.components,
        bevel_retention,
        bridged_void_samples,
        bevel_violation_samples,
        features_checked: feat_accs.len() as u32,
        features_failed,
        subdivisions,
    };

    // Priority order of findings:
    // 1. User-marked critical feature missed its target tolerance
    // 2. Exact topological hole/component regression vs reference mesh
    // 3. Bridged / lost hole (`f(p) > hole_tolerance`)
    // 4. Lost / flattened bevel (normal deviation or bevel-strip retention drop)
    // 5. Shaved / clipped surface (`f(p) < -surface_tolerance`)
    // 6. Budget exhausted -> Unknown (never falsely Pass!)
    let verdict = if let Some(finding) = first_feature_missed {
        Verdict::Found(finding)
    } else if let (Some(exp_g), Some(act_g)) = (ref_topo.and_then(|r| r.genus), topo.genus)
        && act_g < exp_g
    {
        Verdict::Found(FidelityFinding::TopologyHoleLost { expected_holes: exp_g, actual_holes: act_g })
    } else if let Some(rt) = ref_topo
        && topo.components != rt.components
    {
        Verdict::Found(FidelityFinding::ComponentCountChanged { expected: rt.components, actual: topo.components })
    } else if let Some((p, depth)) = worst_hole {
        Verdict::Found(FidelityFinding::LostHole {
            at: [Length::m(p.x), Length::m(p.y), Length::m(p.z)],
            void_depth: Length::m(depth),
            tolerance: o.hole_tolerance,
        })
    } else if let Some((p, ang, off)) = worst_bevel {
        Verdict::Found(FidelityFinding::LostBevel {
            at: [Length::m(p.x), Length::m(p.y), Length::m(p.z)],
            normal_error: Angle::radians(ang),
            max_normal_error: o.max_normal_error,
            surface_offset: Length::m(off),
            bevel_retention,
        })
    } else if let Some(ret) = bevel_retention
        && ret < (1.0 - o.max_bevel_loss.clamp(0.0, 1.0))
    {
        Verdict::Found(FidelityFinding::LostBevel {
            at: [Length::ZERO; 3],
            normal_error: Angle::radians(max_normal_rad),
            max_normal_error: o.max_normal_error,
            surface_offset: Length::m(max_outside.max(max_inside)),
            bevel_retention: Some(ret),
        })
    } else if let Some((p, depth)) = worst_clip {
        Verdict::Found(FidelityFinding::SurfaceClipped {
            at: [Length::m(p.x), Length::m(p.y), Length::m(p.z)],
            depth: Length::m(depth),
            tolerance: o.surface_tolerance,
        })
    } else if budget_hit {
        Verdict::Unknown(Unsettled::Budget { boxes: subdivisions })
    } else {
        Verdict::Pass
    };

    Ok(FidelityReport { checked: FidelityChecked { verdict, metrics }, features: feature_reports })
}

#[inline]
fn record_feature_pos(fa: &mut FeatureAcc, p: Vec3, f: f64) {
    if f > 0.0 {
        fa.max_outside = fa.max_outside.max(f);
        if f > fa.max_err_m && fa.worst_pos.is_none_or(|(_, d)| f > d) {
            fa.worst_pos = Some((p, f));
        }
    } else {
        let d_in = -f;
        fa.max_inside = fa.max_inside.max(d_in);
        if d_in > fa.max_err_m && fa.worst_pos.is_none_or(|(_, d)| d_in > d) {
            fa.worst_pos = Some((p, d_in));
        }
    }
}

#[inline]
fn record_feature_norm(fa: &mut FeatureAcc, p: Vec3, ang: f64, off: f64) {
    fa.max_normal_rad = fa.max_normal_rad.max(ang);
    if ang > fa.max_norm_rad && fa.worst_norm.is_none_or(|(_, a0, _)| ang > a0) {
        fa.worst_norm = Some((p, ang, off));
    }
}

#[inline]
fn to_v3(p: [f32; 3]) -> Vec3 {
    Vec3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2]))
}

#[inline]
fn tri_aabb(a: Vec3, b: Vec3, c: Vec3) -> Aabb3 {
    Aabb3::new(a.min(b).min(c), a.max(b).max(c))
}

#[inline]
fn max_tri_edge(a: Vec3, b: Vec3, c: Vec3) -> f64 {
    (b - a).length().max((c - b).length()).max((a - c).length())
}

#[inline]
fn tri_radius(a: Vec3, b: Vec3, c: Vec3, mid: Vec3) -> f64 {
    (a - mid).length().max((b - mid).length()).max((c - mid).length())
}

/// Barycentric weights of `p` projected onto triangle `(a, b, c)`.
#[inline]
fn barycentric(a: Vec3, b: Vec3, c: Vec3, p: Vec3) -> (f64, f64, f64) {
    let v0 = b - a;
    let v1 = c - a;
    let v2 = p - a;
    let d00 = v0.dot(v0);
    let d01 = v0.dot(v1);
    let d11 = v1.dot(v1);
    let d20 = v2.dot(v0);
    let d21 = v2.dot(v1);
    let denom = d00 * d11 - d01 * d01;
    if !(denom.abs() > 1e-24 && denom.is_finite()) {
        return (1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0);
    }
    let v = ((d11 * d20 - d01 * d21) / denom).clamp(0.0, 1.0);
    let w = ((d00 * d21 - d01 * d20) / denom).clamp(0.0, 1.0);
    let u = (1.0 - v - w).clamp(0.0, 1.0);
    (u, v, w)
}

/// Guard against a C0 sharp-crease gradient discontinuity right on a valid 90° frame edge,
/// rabbet, or 45° French cleat edge without masking genuinely lost/flattened bevels:
///
/// 1. Probe 3 midpoint insets `0.5 * (v_i + mid)` and 3 deeper interior points `0.8 * v_i + 0.2 * mid`
///    inside the triangle: on a valid flat face meeting a sharp crease, at least one interior point
///    sits strictly on the flat face where `eval_grad` agrees with the mesh's shading normal.
/// 2. When the sample sits right on the CAD surface (`|f_mid| <= 0.35 * bevel_probe`, i.e., NOT
///    bridging empty air over a missing bevel!), also probe a tiny step inward along `-n_mesh` to
///    clear the exterior convex-edge SDF rounding zone (where `f > 0` makes `eval_grad` point at
///    45° even on an exact sharp 90° box corner). On a genuinely flattened bevel bridging empty air
///    (`f_mid > bevel_probe`), this surface-on-crease guard never triggers, and all interior probes
///    still see the missing bevel's tilted gradient.
#[allow(clippy::too_many_arguments)]
fn confirm_normal_mismatch(
    tape: &blaze_cad_field::Tape<'_>,
    shade_at: &impl Fn(Vec3) -> Vec3,
    p0: Vec3,
    p1: Vec3,
    p2: Vec3,
    mid: Vec3,
    f_mid: f64,
    bevel_probe: f64,
    mid_ang: f64,
    sc: &mut Scratch<'_>,
) -> f64 {
    let mut best_ang = mid_ang;
    let near_surface = f_mid.is_finite() && f_mid.abs() <= bevel_probe * 0.35;
    let n_mid = shade_at(mid);
    let inset_shift = if near_surface { (f_mid.abs() + 1.5e-4).min(bevel_probe * 0.5) } else { 0.0 };

    if near_surface {
        // Step just inside the solid along `-n_mid` (and just outside along `+n_mid` for concave
        // sharp corners) to move off the 0-thickness C0 crease Voronoi boundary.
        for sign in [-1.0f64, 1.0f64] {
            let q = mid + n_mid * (sign * inset_shift);
            let (fq, gq) = eval_grad(tape, q, sc);
            if fq.is_finite()
                && fq.abs() <= bevel_probe
                && let Some(n_q) = gq.normalized()
            {
                let ang = blaze_math::acos(n_mid.dot(n_q).clamp(-1.0, 1.0));
                best_ang = best_ang.min(ang);
            }
        }
    }

    for v in [p0, p1, p2] {
        for alpha in [0.5f64, 0.8f64] {
            let q = v * alpha + mid * (1.0 - alpha);
            let n_m = shade_at(q);
            let (fq, gq) = eval_grad(tape, q, sc);
            if let Some(n_q) = gq.normalized() {
                let ang = blaze_math::acos(n_m.dot(n_q).clamp(-1.0, 1.0));
                best_ang = best_ang.min(ang);
            }
            if near_surface && fq.is_finite() && fq.abs() <= bevel_probe * 0.5 {
                let q_in = q - n_m * (fq.abs() + 1.5e-4).min(bevel_probe * 0.5);
                let (_, g_in) = eval_grad(tape, q_in, sc);
                if let Some(n_q) = g_in.normalized() {
                    let ang = blaze_math::acos(n_m.dot(n_q).clamp(-1.0, 1.0));
                    best_ang = best_ang.min(ang);
                }
            }
        }
    }
    best_ang
}
