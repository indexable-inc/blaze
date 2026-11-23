//! SOTA parametric garment CAD, 2D pattern drafting, deterministic continuum shell physics,
//! unified body + Blaze-CAD solid collision, Blender-free tailoring details, smooth 57-joint
//! skinning, automated clearance/fit checks, studio preview rendering, and skinned binary glTF 2.0
//! export — all in `#![no_std]` sans-IO Rust with zero third-party dependencies.
//!
//! # Architecture
//!
//! | module | role |
//! |---|---|
//! | [`fabric`] | Calibrated SI textile mechanics (`g/m²`, warp/weft/shear `N/m`, dihedral bending `N·m`, thickness, friction) |
//! | [`pattern`] | 2D curvilinear Coons pattern IR, seams, pins, elastics, 2D-attached pockets & hardware, [`seam_ease_check`], [`export_pattern_svg`] |
//! | [`mesh2d`] | Deterministic alternating-diagonal Coons triangulator with exact 1-to-1 seam vertex matching and 1:1 flat-pattern metric UVs |
//! | [`collider`] | Deterministic triangle BVH collider with angle-weighted pseudonormals, smooth modesty bridge, and native `blaze-cad-field::Tape` rigid CAD solid composition |
//! | [`solver`] | Two-stage deterministic XPBD continuum membrane + Bridson/Bergou isometric dihedral bending + topological seam welding + spatial-hash self-collision |
//! | [`detail`] | Flat-felled seam trench relief, rim-aware solidify, parametric 3D pockets/flaps/plackets along outer surface normals (zero shrinkwrap origami!), rigid hardware mounting |
//! | [`skin`] | Smooth surface-projected 57-joint skin weight transfer with Laplacian smoothing, occluded inner-layer culling, [`body_clearance_check`], [`pose_fit_check`] |
//! | [`gltf_io`] | Pure `#![no_std]` sans-IO binary `.glb` reader & deterministic skinned `.glb` writer |
//! | [`preview`] | Instant studio PNG previews via `blaze-cad-picture::render_mesh_scene` + `blaze-cad-out::png` |

#![no_std]
#![feature(register_tool, custom_inner_attributes, default_field_values)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

pub mod cad_gear;
pub mod collider;
pub mod detail;
pub mod fabric;
pub mod gltf_io;
pub mod grain;
pub mod inspect;
pub mod ipc;
pub mod mesh2d;
pub mod pattern;
pub mod pose_sweep;
pub mod preview;
pub mod psd;
pub mod remesh;
pub mod skin;
pub mod sloper;
pub mod solver;
pub mod subdiv;

#[cfg(test)]
mod tests;

pub use cad_gear::{
    CHEST_BEACON_DEFAULT, COBRA_BUCKLE_DEFAULT, COLLAR_CLASP_DEFAULT, CadGearError, append_cad_solid_to_garment,
    build_chest_beacon_dual_tapes, build_chest_beacon_tape, build_chest_beacon_tape_at,
    build_cobra_buckle_dual_tapes, build_cobra_buckle_tape, build_cobra_buckle_tape_at,
    build_collar_clasp_dual_tapes, build_collar_clasp_tape, build_collar_clasp_tape_at,
};
pub use collider::{ContactQuery, MeshCollider, UnifiedCollider};
pub use detail::{GarmentMesh, PanelKind, PanelMeta};
pub use fabric::Fabric;
pub use gltf_io::{
    GltfIoError, JointDef, LoadedGlb, LoadedPanelEntry, LoadedPanelMap, MorphTarget, read_glb_panel_metadata,
    read_glb_skeleton_mesh, write_skinned_glb, write_skinned_glb_with_morphs,
};
pub use grain::{
    FabricGrain, GrainPlant, KneeGatherOpts, PanelStrainSummary, StrainReport, TriangleStrain, measure_cloth_strain,
    measure_cloth_strain_planted, relax_compression_zones, set_garment_texture_uv_transform,
};
pub use inspect::{PanelClickReport, PanelHit, inspect_garment_ray};
pub use ipc::{IpcOpts, IpcStats, barrier_energy, barrier_grad, barrier_hess, project_ipc_barriers};
pub use mesh2d::{
    ClothMesh, InternalStitch, Mesh2DError, PanelSlice, build_cloth_mesh, recompute_normals, sample_panel_frame,
};
pub use pattern::{
    Curve2D, ElasticSpec, HardwareKind, HardwareMount, InternalSeamSpec, PanelSpec, PatternSet, PinSpec,
    SeamEaseFinding, SeamSpec, Side, SubEdge, SurfacePatchSpec, Wrap3D, export_pattern_svg, seam_ease_check,
};
pub use pose_sweep::{
    DEFAULT_SWEEP_MOTIONS, JointRot, MotionSweepSummary, PoseSweepHit, PoseSweepOpts, PoseSweepPlant, PoseSweepReport,
    SweepFailureKind, SweepMotion, pose_body_at, pose_body_with_rots, pose_sweep_check, pose_sweep_check_planted,
};
pub use preview::{PreviewError, StudioAngle, render_studio_preview_png};
pub use psd::{
    CanonicalPose, PsdReport, compute_psd_corrective_deltas, pose_body_canonical, verify_posed_collar_clearance,
};
pub use remesh::{RemeshOpts, RemeshReport, adaptive_refine_cloth, flip_fold_edges};
pub use skin::{
    ClearanceFinding, GarmentIntegrityFinding, GarmentIntegrityOpts, GarmentIntegrityReport, PosedMannequin,
    SkinnedWeights, body_clearance_check, cull_occluded_inner_layer, find_joint_pos, merge_skinned_mannequin,
    pose_body_for_drape, pose_fit_check, transfer_skin_weights, unskin_garment_to_bind, verify_garment_integrity,
};
pub use sloper::{BodySlice, BodySloper};
pub use solver::{SeamAlignmentFinding, SimReport, SolverOpts, seam_alignment_check, simulate_garment};
pub use subdiv::subdivide_cloth_loop;
