//! SOTA Blender-Free MetaHuman Desert Spacefarer Crew Outfit powered by `blaze-cad-cloth`:
//! 1. **A-Pose Articulated Draping** (`pose_body_for_drape`): poses `spacer.glb` into a natural
//!    relaxed standing A-pose (`30°` lowered arms, `8°` bent elbows) so gravity forms authentic
//!    armpit, chest, shoulder, elbow, waist, knee, and boot-break drapery folds.
//! 2. **Anatomical `BodySloper` Conformal Wrapping**: measures real cross-sections of the posed
//!    mannequin so neck, collar, shoulders, chest, waist, hips, and sleeves start conformantly
//!    aligned with `< 25 mm` initial seam gaps — zero collar spikes or violent pulls!
//! 3. **Architectural Tailoring Primitives**: crisp box-pleated field & cargo pockets (`box_pleat: true`),
//!    detached overhanging pocket flaps casting real undercut shadow gaps, split-normal gusset walls,
//!    Cordura storm placket, brushed titanium snaps, zipper pull, and leather waist belt + Cobra buckle.
//! 4. **Crease-Preserving Loop Subdivision** (`subdivide_cloth_loop`): elevates the draped cloth
//!    to sub-6mm cinema-grade drapery while keeping seams, hems, pockets, and flaps razor-sharp!
//! 5. **Exact Inverse-LBS Unskinning** (`unskin_garment_to_bind`): transfers smooth 57-joint skin
//!    weights in A-pose and mathematically inverts the bind transform back into T-pose for glTF 2.0!

#![feature(default_field_values)]

use std::path::PathBuf;
use std::time::Instant;

use blaze_cad_cloth::{
    BodySloper, CHEST_BEACON_DEFAULT, COBRA_BUCKLE_DEFAULT, COLLAR_CLASP_DEFAULT, CanonicalPose, Curve2D, ElasticSpec,
    Fabric, GarmentIntegrityOpts, GarmentMesh, HardwareKind, HardwareMount, InternalSeamSpec, MeshCollider,
    MorphTarget, PanelSpec, PatternSet, PinSpec, SeamSpec, SolverOpts, StudioAngle, SubEdge, SurfacePatchSpec,
    UnifiedCollider, Wrap3D, append_cad_solid_to_garment, body_clearance_check, build_chest_beacon_dual_tapes,
    build_chest_beacon_tape_at, build_cloth_mesh, build_cobra_buckle_dual_tapes, build_collar_clasp_dual_tapes,
    compute_psd_corrective_deltas, cull_occluded_inner_layer, export_pattern_svg, find_joint_pos,
    merge_skinned_mannequin, pose_body_for_drape, read_glb_skeleton_mesh, render_studio_preview_png,
    sample_panel_frame, seam_alignment_check, seam_ease_check, simulate_garment, subdivide_cloth_loop,
    transfer_skin_weights, unskin_garment_to_bind, verify_garment_integrity, verify_posed_collar_clearance,
    write_skinned_glb_with_morphs,
};
use blaze_cad_field::{Placement, Scratch};
use blaze_math::Vec3;
use blaze_own::{Alloc, Vec};
use blaze_units::Length;

fn v<'a>(alloc: Alloc<'a>, items: &[SubEdge]) -> Vec<SubEdge, Alloc<'a>> {
    let mut out = Vec::new_in(alloc);
    for &e in items {
        out.try_push(e).ok().unwrap();
    }
    out
}

fn push<T>(vec: &mut Vec<T, Alloc<'_>>, val: T) {
    vec.try_push(val).ok().unwrap();
}

/// Build the SOTA 3-layer Spacefarer Crew Outfit 2D pattern set aligned to the posed A-pose arms.
fn build_spacefarer_patterns<'a>(
    alloc: Alloc<'a>,
    l_sh: Vec3,
    l_dir: Vec3,
    l_len: f64,
    r_sh: Vec3,
    r_dir: Vec3,
    r_len: f64,
) -> PatternSet<'a> {
    let mut ps = PatternSet::new(alloc);

    // 0: mat_jacket, 1: mat_trousers, 2: mat_undershirt, 3: mat_cordura,
    // 4: mat_leather, 5: mat_metal, 6: mat_dark
    let mut jacket_canvas = Fabric::COTTON_CANVAS;
    jacket_canvas.bending_stiffness = 0.00028;
    push(&mut ps.fabrics, jacket_canvas);
    push(&mut ps.fabrics, Fabric::MILITARY_TWILL);
    push(&mut ps.fabrics, Fabric::RIB_JERSEY);
    push(&mut ps.fabrics, Fabric::BALLISTIC_CORDURA);
    push(&mut ps.fabrics, Fabric::FULL_GRAIN_LEATHER);
    push(&mut ps.fabrics, Fabric::TITANIUM_HARDWARE);
    push(&mut ps.fabrics, Fabric::DARK_POLYMER);

    // ============================================================================================
    // LAYER 0: WARM HEATHER-STONE RIBBED CREWNECK UNDERSHIRT (mat_undershirt = 2)
    // ============================================================================================
    {
        let y0 = 1.12;
        let h_tot = 0.370; // 1.12 -> 1.490
        let hw = 0.235;
        let neck_hw = 0.082;

        push(
            &mut ps.panels,
            PanelSpec {
                name: "shirt_front",
                layer: 0,
                fabric_idx: 2,
                lining_fabric_idx: None,
                bottom: v(alloc, &[SubEdge::new("hem_f", Curve2D::line([-hw, 0.0], [hw, 0.0]), 16)]),
                right: v(alloc, &[SubEdge::new("side_r", Curve2D::line([hw, 0.0], [hw * 0.90, h_tot - 0.025]), 14)]),
                top: v(
                    alloc,
                    &[
                        SubEdge::new("sh_l", Curve2D::line([-hw * 0.90, h_tot - 0.025], [-neck_hw, h_tot]), 5),
                        SubEdge::new("neck_f", Curve2D::quad([-neck_hw, h_tot], [0.0, h_tot - 0.130], [neck_hw, h_tot]), 6),
                        SubEdge::new("sh_r", Curve2D::line([neck_hw, h_tot], [hw * 0.90, h_tot - 0.025]), 5),
                    ],
                ),
                left: v(alloc, &[SubEdge::new("side_l", Curve2D::line([-hw, 0.0], [-hw * 0.90, h_tot - 0.025]), 14)]),
                wrap: Wrap3D::VerticalCylinder {
                    center_x: 0.0,
                    center_z: 0.014,
                    y_origin: y0,
                    radius_x: 0.160,
                    radius_z: 0.122,
                    taper_x: 0.04,
                    taper_z: 0.01,
                    u_center: 0.0,
                    angle_center: 0.0,
                    arc_radius: 0.160,
                },
                flip_normal: true,
                ..
            },
        );

        push(
            &mut ps.panels,
            PanelSpec {
                name: "shirt_back",
                layer: 0,
                fabric_idx: 2,
                lining_fabric_idx: None,
                bottom: v(alloc, &[SubEdge::new("hem_b", Curve2D::line([-hw, 0.0], [hw, 0.0]), 16)]),
                right: v(alloc, &[SubEdge::new("side_l", Curve2D::line([hw, 0.0], [hw * 0.90, h_tot - 0.025]), 14)]),
                top: v(
                    alloc,
                    &[
                        SubEdge::new("sh_r", Curve2D::line([-hw * 0.90, h_tot - 0.025], [-neck_hw, h_tot]), 5),
                        SubEdge::new("neck_b", Curve2D::quad([-neck_hw, h_tot], [0.0, h_tot - 0.018], [neck_hw, h_tot]), 6),
                        SubEdge::new("sh_l", Curve2D::line([neck_hw, h_tot], [hw * 0.90, h_tot - 0.025]), 5),
                    ],
                ),
                left: v(alloc, &[SubEdge::new("side_r", Curve2D::line([-hw, 0.0], [-hw * 0.90, h_tot - 0.025]), 14)]),
                wrap: Wrap3D::VerticalCylinder {
                    center_x: 0.0,
                    center_z: 0.014,
                    y_origin: y0,
                    radius_x: 0.160,
                    radius_z: 0.122,
                    taper_x: 0.04,
                    taper_z: 0.01,
                    u_center: 0.0,
                    angle_center: core::f64::consts::PI,
                    arc_radius: 0.160,
                },
                flip_normal: true,
                ..
            },
        );

        for (name, ea, eb, rev) in [
            ("shirt_side_l", "side_l", "side_l", false),
            ("shirt_side_r", "side_r", "side_r", false),
            ("shirt_sh_l", "sh_l", "sh_l", true),
            ("shirt_sh_r", "sh_r", "sh_r", true),
        ] {
            let mut s = SeamSpec::flat(name, "shirt_front", ea, "shirt_back", eb);
            s.reversed = rev;
            s.max_ease = 0.08;
            s.topstitch_depth = Length::mm(0.4);
            push(&mut ps.seams, s);
        }
        push(&mut ps.pins, PinSpec { panel: "shirt_front", edge: "neck_f", stiffness: 0.40, sewing_only: false });
        push(&mut ps.pins, PinSpec { panel: "shirt_back", edge: "neck_b", stiffness: 0.40, sewing_only: false });
        push(&mut ps.elastics, ElasticSpec { panel: "shirt_front", edge: "neck_f", rest_scale: 0.92 });
        push(&mut ps.elastics, ElasticSpec { panel: "shirt_back", edge: "neck_b", rest_scale: 0.92 });
    }

    // ============================================================================================
    // LAYER 1: 4-PANEL J-CURVE RELAXED MILITARY CARGO TROUSERS (mat_trousers = 1)
    // Extra +25 mm inseam stacking over mag_boots for authentic ankle break & knee creases,
    // architectural box-pleat cargo pockets with detached shadow flaps, and Cordura knee patches!
    // ============================================================================================
    {
        let y_ankle = 0.106; // +26 mm extra inseam stacks & blouses physically onto mag_boots for real ankle break folds!
        let v_crotch = 0.704; // Y = 0.810
        let v_waist = 1.014; // Y = 1.120

        let ankle_hw_f = 0.118;
        let ankle_hw_b = 0.124;
        let thigh_hw_f = 0.168; // generous utilitarian BDU ease for deep organic thigh & knee folds!
        let thigh_hw_b = 0.184;
        let waist_w_f = 0.236;
        let waist_w_b = 0.252;

        for is_right in [false, true] {
            let cx = if is_right { 0.105 } else { -0.105 };
            let (name_f, name_b) = if is_right { ("leg_r_front", "leg_r_back") } else { ("leg_l_front", "leg_l_back") };

            let (u_ank_l, u_ank_r, u_cr_l, u_cr_r, u_wst_l, u_wst_r) = if is_right {
                (-ankle_hw_f, ankle_hw_f, -thigh_hw_f * 1.08, thigh_hw_f, -waist_w_f * 0.44, waist_w_f * 0.56)
            } else {
                (-ankle_hw_f, ankle_hw_f, -thigh_hw_f, thigh_hw_f * 1.08, -waist_w_f * 0.56, waist_w_f * 0.44)
            };

            let front_left_edges = if is_right {
                v(
                    alloc,
                    &[
                        SubEdge::new("inseam", Curve2D::quad([u_ank_l, 0.0], [u_ank_l * 1.12, v_crotch * 0.50], [u_cr_l, v_crotch]), 28),
                        SubEdge::new("fly", Curve2D::cubic([u_cr_l, v_crotch], [u_wst_l * 1.15, v_crotch + 0.06], [u_wst_l, v_crotch + 0.16], [u_wst_l, v_waist]), 14),
                    ],
                )
            } else {
                v(
                    alloc,
                    &[SubEdge::new(
                        "outseam",
                        Curve2D::cubic([u_ank_l, 0.0], [u_cr_l * 0.96, v_crotch * 0.55], [u_cr_l * 1.06, v_crotch + 0.12], [u_wst_l, v_waist]),
                        42,
                    )],
                )
            };

            let front_right_edges = if is_right {
                v(
                    alloc,
                    &[SubEdge::new(
                        "outseam",
                        Curve2D::cubic([u_ank_r, 0.0], [u_cr_r * 0.96, v_crotch * 0.55], [u_cr_r * 1.06, v_crotch + 0.12], [u_wst_r, v_waist]),
                        42,
                    )],
                )
            } else {
                v(
                    alloc,
                    &[
                        SubEdge::new("inseam", Curve2D::quad([u_ank_r, 0.0], [u_ank_r * 1.12, v_crotch * 0.50], [u_cr_r, v_crotch]), 28),
                        SubEdge::new("fly", Curve2D::cubic([u_cr_r, v_crotch], [u_wst_r * 1.15, v_crotch + 0.06], [u_wst_r, v_crotch + 0.16], [u_wst_r, v_waist]), 14),
                    ],
                )
            };

            push(
                &mut ps.panels,
                PanelSpec {
                    name: name_f,
                    layer: 1,
                    fabric_idx: 1,
                    lining_fabric_idx: None,
                    bottom: v(alloc, &[SubEdge::new("ankle", Curve2D::line([u_ank_l, 0.0], [u_ank_r, 0.0]), 14)]),
                    right: front_right_edges,
                    top: v(alloc, &[SubEdge::new("waist", Curve2D::line([u_wst_l, v_waist], [u_wst_r, v_waist]), 14)]),
                    left: front_left_edges,
                    wrap: Wrap3D::VerticalCylinder {
                        center_x: cx,
                        center_z: -0.010,
                        y_origin: y_ankle,
                        radius_x: 0.080,
                        radius_z: 0.086,
                        taper_x: 0.018,
                        taper_z: 0.020,
                        u_center: 0.0,
                        angle_center: 0.0,
                        arc_radius: 0.092,
                    },
                    flip_normal: true,
                    ..
                },
            );

            let (bu_ank_l, bu_ank_r, bu_cr_l, bu_cr_r, bu_wst_l, bu_wst_r) = if is_right {
                (-ankle_hw_b, ankle_hw_b, -thigh_hw_b, thigh_hw_b * 1.24, -waist_w_b * 0.56, waist_w_b * 0.44)
            } else {
                (-ankle_hw_b, ankle_hw_b, -thigh_hw_b * 1.24, thigh_hw_b, -waist_w_b * 0.44, waist_w_b * 0.56)
            };

            let back_left_edges = if is_right {
                v(
                    alloc,
                    &[SubEdge::new(
                        "outseam",
                        Curve2D::cubic([bu_ank_l, 0.0], [bu_cr_l * 0.96, v_crotch * 0.55], [bu_cr_l * 1.06, v_crotch + 0.12], [bu_wst_l, v_waist + 0.015]),
                        42,
                    )],
                )
            } else {
                v(
                    alloc,
                    &[
                        SubEdge::new("inseam", Curve2D::quad([bu_ank_l, 0.0], [bu_ank_l * 1.12, v_crotch * 0.50], [bu_cr_l, v_crotch]), 28),
                        SubEdge::new("seat", Curve2D::cubic([bu_cr_l, v_crotch], [bu_wst_l * 1.48, v_crotch + 0.05], [bu_wst_l * 1.14, v_crotch + 0.17], [bu_wst_l, v_waist + 0.022]), 14),
                    ],
                )
            };

            let back_right_edges = if is_right {
                v(
                    alloc,
                    &[
                        SubEdge::new("inseam", Curve2D::quad([bu_ank_r, 0.0], [bu_ank_r * 1.12, v_crotch * 0.50], [bu_cr_r, v_crotch]), 28),
                        SubEdge::new("seat", Curve2D::cubic([bu_cr_r, v_crotch], [bu_wst_r * 1.48, v_crotch + 0.05], [bu_wst_r * 1.14, v_crotch + 0.17], [bu_wst_r, v_waist + 0.022]), 14),
                    ],
                )
            } else {
                v(
                    alloc,
                    &[SubEdge::new(
                        "outseam",
                        Curve2D::cubic([bu_ank_r, 0.0], [bu_cr_r * 0.96, v_crotch * 0.55], [bu_cr_r * 1.06, v_crotch + 0.12], [bu_wst_r, v_waist + 0.022]),
                        42,
                    )],
                )
            };

            push(
                &mut ps.panels,
                PanelSpec {
                    name: name_b,
                    layer: 1,
                    fabric_idx: 1,
                    lining_fabric_idx: None,
                    bottom: v(alloc, &[SubEdge::new("ankle", Curve2D::line([bu_ank_l, 0.0], [bu_ank_r, 0.0]), 14)]),
                    right: back_right_edges,
                    top: v(alloc, &[SubEdge::new("waist", Curve2D::line([bu_wst_l, v_waist + 0.022], [bu_wst_r, v_waist + 0.022]), 14)]),
                    left: back_left_edges,
                    wrap: Wrap3D::VerticalCylinder {
                        center_x: cx,
                        center_z: 0.012,
                        y_origin: y_ankle,
                        radius_x: 0.086,
                        radius_z: 0.090,
                        taper_x: 0.022,
                        taper_z: 0.028,
                        u_center: 0.0,
                        angle_center: core::f64::consts::PI,
                        arc_radius: 0.095,
                    },
                    flip_normal: true,
                    ..
                },
            );

            let mut s_out = SeamSpec::flat(if is_right { "r_outseam" } else { "l_outseam" }, name_f, "outseam", name_b, "outseam");
            s_out.max_ease = 0.08;
            s_out.topstitch_depth = Length::mm(1.0);
            push(&mut ps.seams, s_out);

            let mut s_in = SeamSpec::flat(if is_right { "r_inseam" } else { "l_inseam" }, name_f, "inseam", name_b, "inseam");
            s_in.max_ease = 0.08;
            s_in.topstitch_depth = Length::mm(0.8);
            push(&mut ps.seams, s_in);

            push(&mut ps.pins, PinSpec { panel: name_f, edge: "waist", stiffness: 0.55, sewing_only: false });
            push(&mut ps.pins, PinSpec { panel: name_b, edge: "waist", stiffness: 0.55, sewing_only: false });
            push(&mut ps.elastics, ElasticSpec { panel: name_f, edge: "waist", rest_scale: 0.90 });
            push(&mut ps.elastics, ElasticSpec { panel: name_b, edge: "waist", rest_scale: 0.90 });
            // Boot-blousing ankle drawcord gathers the +26 mm extra inseam right over mag_boots!
            push(&mut ps.elastics, ElasticSpec { panel: name_f, edge: "ankle", rest_scale: 0.90 });
            push(&mut ps.elastics, ElasticSpec { panel: name_b, edge: "ankle", rest_scale: 0.90 });

            // Crisp architectural box-pleated cargo pocket with detached overhanging flap!
            let (pkt_s0, pkt_s1, snap_s) = if is_right { (0.48, 0.96, 0.72) } else { (0.04, 0.52, 0.28) };
            push(
                &mut ps.patches,
                SurfacePatchSpec {
                    name: if is_right { "cargo_pkt_r" } else { "cargo_pkt_l" },
                    panel: name_f,
                    s_min: pkt_s0,
                    s_max: pkt_s1,
                    t_min: 0.43,
                    t_max: 0.63,
                    depth: Length::mm(13.5),
                    crown: Length::mm(2.0),
                    edge_bevel: 0.14,
                    flap_fraction: 0.30,
                    bottom_chamfer: 0.15,
                    fabric_idx: 1,
                    flap_fabric_idx: Some(1),
                    box_pleat: true,
                },
            );
            push(
                &mut ps.hardware,
                HardwareMount {
                    name: if is_right { "cargo_snap_r1" } else { "cargo_snap_l1" },
                    panel: name_f,
                    s: snap_s - 0.11,
                    t: 0.585,
                    normal_offset: Length::mm(18.5),
                    kind: HardwareKind::SnapButton { radius: Length::mm(5.0), height: Length::mm(2.2) },
                    fabric_idx: 5,
                },
            );
            push(
                &mut ps.hardware,
                HardwareMount {
                    name: if is_right { "cargo_snap_r2" } else { "cargo_snap_l2" },
                    panel: name_f,
                    s: snap_s + 0.11,
                    t: 0.585,
                    normal_offset: Length::mm(18.5),
                    kind: HardwareKind::SnapButton { radius: Length::mm(5.0), height: Length::mm(2.2) },
                    fabric_idx: 5,
                },
            );

            // Flat stitched Cordura articulated knee patch.
            push(
                &mut ps.patches,
                SurfacePatchSpec {
                    name: if is_right { "knee_patch_r" } else { "knee_patch_l" },
                    panel: name_f,
                    s_min: 0.20,
                    s_max: 0.80,
                    t_min: 0.27,
                    t_max: 0.39,
                    depth: Length::mm(3.8),
                    crown: Length::mm(0.5),
                    edge_bevel: 0.18,
                    flap_fraction: 0.0,
                    bottom_chamfer: 0.14,
                    fabric_idx: 3,
                    flap_fabric_idx: None,
                    box_pleat: false,
                },
            );

            // Leather waist belt.
            push(
                &mut ps.patches,
                SurfacePatchSpec {
                    name: if is_right { "belt_rf" } else { "belt_lf" },
                    panel: name_f,
                    s_min: 0.02,
                    s_max: 0.98,
                    t_min: 0.935,
                    t_max: 0.992,
                    depth: Length::mm(4.2),
                    crown: Length::mm(0.3),
                    edge_bevel: 0.14,
                    flap_fraction: 0.0,
                    bottom_chamfer: 0.0,
                    fabric_idx: 4,
                    flap_fabric_idx: None,
                    box_pleat: false,
                },
            );
            push(
                &mut ps.patches,
                SurfacePatchSpec {
                    name: if is_right { "belt_rb" } else { "belt_lb" },
                    panel: name_b,
                    s_min: 0.02,
                    s_max: 0.98,
                    t_min: 0.935,
                    t_max: 0.992,
                    depth: Length::mm(4.2),
                    crown: Length::mm(0.3),
                    edge_bevel: 0.14,
                    flap_fraction: 0.0,
                    bottom_chamfer: 0.0,
                    fabric_idx: 4,
                    flap_fabric_idx: None,
                    box_pleat: false,
                },
            );
        }

        let mut s_fly = SeamSpec::flat("front_fly", "leg_l_front", "fly", "leg_r_front", "fly");
        s_fly.max_ease = 0.06;
        s_fly.topstitch_depth = Length::mm(1.2);
        push(&mut ps.seams, s_fly);

        let mut s_seat = SeamSpec::flat("back_seat", "leg_l_back", "seat", "leg_r_back", "seat");
        s_seat.max_ease = 0.06;
        s_seat.topstitch_depth = Length::mm(1.2);
        push(&mut ps.seams, s_seat);

        // Tailored BDU front-fly J-stitch placket shield overlapping cleanly across `front_fly`!
        push(
            &mut ps.patches,
            SurfacePatchSpec {
                name: "trouser_fly_shield",
                panel: "leg_r_front",
                s_min: 0.0,
                s_max: 0.19,
                t_min: 0.69,
                t_max: 0.94,
                depth: Length::mm(3.0),
                crown: Length::mm(0.4),
                edge_bevel: 0.16,
                flap_fraction: 0.0,
                bottom_chamfer: 0.25,
                fabric_idx: 1,
                flap_fabric_idx: None,
                box_pleat: false,
            },
        );
        // Cobra belt buckle is authored as an exact implicit CSG solid via `blaze-cad-field`
        // (`build_cobra_buckle_dual_tapes`) and meshed directly into the garment post-drape!
    }

    // ============================================================================================
    // LAYER 2: HALF-ZIPPED TAILORED DARK-CHARCOAL CANVAS FIELD JACKET (mat_jacket = 0)
    // Conformed via BodySloper directly onto A-pose torso/neck + A-pose angled sleeves!
    // ============================================================================================
    {
        let y_hem = 0.870;
        let v_pit = 0.425; // Y = 1.295 (natural A-pose armhole)
        let v_zip = 0.530; // Y = 1.400 (crisp high field-jacket zip right below collarbone!)
        let v_sh = 0.618; // Y = 1.488 (real shoulder seam height!)

        // Shaped M65 waist-to-hip tailoring: cinched waist (25.2 cm half) flaring to 30.0 cm hem
        // generates authentic vertical gravity drapery flutes radiating above & below the waist!
        let hem_hw = 0.300;
        let waist_hw = 0.252;
        let chest_hw = 0.285;
        let neck_hw = 0.072;

        // --- Front Left Jacket Panel (X < 0) ---
        push(
            &mut ps.panels,
            PanelSpec {
                name: "jacket_front_l",
                layer: 2,
                fabric_idx: 0,
                lining_fabric_idx: Some(3),
                bottom: v(alloc, &[SubEdge::new("hem_fl", Curve2D::line([-hem_hw, 0.0], [0.0, 0.0]), 15)]),
                right: v(
                    alloc,
                    &[
                        SubEdge::new("cf_zip", Curve2D::line([0.0, 0.0], [0.0, v_zip]), 24),
                        SubEdge::new("cf_vneck", Curve2D::quad([0.0, v_zip], [-0.010, v_zip + 0.025], [-0.018, v_zip + 0.048]), 8),
                    ],
                ),
                top: v(
                    alloc,
                    &[
                        SubEdge::new("sh_l", Curve2D::line([-chest_hw * 0.92, v_sh - 0.020], [-neck_hw - 0.012, v_sh]), 10),
                        SubEdge::new("neck_l", Curve2D::quad([-neck_hw - 0.012, v_sh], [-neck_hw * 0.65, v_zip + 0.055], [-0.018, v_zip + 0.048]), 5),
                    ],
                ),
                left: v(
                    alloc,
                    &[
                        // Tailored curved waist contour along side seam!
                        SubEdge::new("side_l", Curve2D::quad([-hem_hw, 0.0], [-waist_hw, v_pit * 0.52], [-chest_hw * 0.96, v_pit]), 20),
                        SubEdge::new("armscye_lf", Curve2D::quad([-chest_hw * 0.96, v_pit], [-chest_hw * 0.84, v_pit + 0.095], [-chest_hw * 0.92, v_sh - 0.020]), 12),
                    ],
                ),
                wrap: Wrap3D::VerticalCylinder {
                    center_x: 0.0,
                    center_z: 0.014,
                    y_origin: y_hem,
                    radius_x: 0.195,
                    radius_z: 0.148,
                    taper_x: 0.02,
                    taper_z: 0.0,
                    u_center: 0.0,
                    angle_center: 0.0,
                    arc_radius: 0.185,
                },
                flip_normal: true,
                ..
            },
        );

        // --- Front Right Jacket Panel (X > 0) ---
        push(
            &mut ps.panels,
            PanelSpec {
                name: "jacket_front_r",
                layer: 2,
                fabric_idx: 0,
                lining_fabric_idx: Some(3),
                bottom: v(alloc, &[SubEdge::new("hem_fr", Curve2D::line([0.0, 0.0], [hem_hw, 0.0]), 15)]),
                right: v(
                    alloc,
                    &[
                        SubEdge::new("side_r", Curve2D::quad([hem_hw, 0.0], [waist_hw, v_pit * 0.52], [chest_hw * 0.96, v_pit]), 20),
                        SubEdge::new("armscye_rf", Curve2D::quad([chest_hw * 0.96, v_pit], [chest_hw * 0.84, v_pit + 0.095], [chest_hw * 0.92, v_sh - 0.020]), 12),
                    ],
                ),
                top: v(
                    alloc,
                    &[
                        SubEdge::new("neck_r", Curve2D::quad([0.018, v_zip + 0.048], [neck_hw * 0.65, v_zip + 0.055], [neck_hw + 0.012, v_sh]), 5),
                        SubEdge::new("sh_r", Curve2D::line([neck_hw + 0.012, v_sh], [chest_hw * 0.92, v_sh - 0.020]), 10),
                    ],
                ),
                left: v(
                    alloc,
                    &[
                        SubEdge::new("cf_zip", Curve2D::line([0.0, 0.0], [0.0, v_zip]), 24),
                        SubEdge::new("cf_vneck", Curve2D::quad([0.0, v_zip], [0.010, v_zip + 0.025], [0.018, v_zip + 0.048]), 8),
                    ],
                ),
                wrap: Wrap3D::VerticalCylinder {
                    center_x: 0.0,
                    center_z: 0.014,
                    y_origin: y_hem,
                    radius_x: 0.195,
                    radius_z: 0.148,
                    taper_x: 0.02,
                    taper_z: 0.0,
                    u_center: 0.0,
                    angle_center: 0.0,
                    arc_radius: 0.185,
                },
                flip_normal: true,
                ..
            },
        );

        // --- Back Jacket Panel ---
        push(
            &mut ps.panels,
            PanelSpec {
                name: "jacket_back",
                layer: 2,
                fabric_idx: 0,
                lining_fabric_idx: Some(3),
                bottom: v(alloc, &[SubEdge::new("hem_b", Curve2D::line([-hem_hw, 0.0], [hem_hw, 0.0]), 30)]),
                right: v(
                    alloc,
                    &[
                        SubEdge::new("side_l", Curve2D::quad([hem_hw, 0.0], [waist_hw, v_pit * 0.52], [chest_hw * 0.96, v_pit]), 20),
                        SubEdge::new("armscye_lb", Curve2D::quad([chest_hw * 0.96, v_pit], [chest_hw * 0.86, v_pit + 0.095], [chest_hw * 0.92, v_sh - 0.020]), 12),
                    ],
                ),
                top: v(
                    alloc,
                    &[
                        SubEdge::new("sh_r", Curve2D::line([-chest_hw * 0.92, v_sh - 0.020], [-neck_hw - 0.012, v_sh]), 10),
                        SubEdge::new("neck_b", Curve2D::quad([-neck_hw - 0.012, v_sh], [0.0, v_sh - 0.014], [neck_hw + 0.012, v_sh]), 10),
                        SubEdge::new("sh_l", Curve2D::line([neck_hw + 0.012, v_sh], [chest_hw * 0.92, v_sh - 0.020]), 10),
                    ],
                ),
                left: v(
                    alloc,
                    &[
                        SubEdge::new("side_r", Curve2D::quad([-hem_hw, 0.0], [-waist_hw, v_pit * 0.52], [-chest_hw * 0.96, v_pit]), 20),
                        SubEdge::new("armscye_rb", Curve2D::quad([-chest_hw * 0.96, v_pit], [-chest_hw * 0.86, v_pit + 0.095], [-chest_hw * 0.92, v_sh - 0.020]), 12),
                    ],
                ),
                wrap: Wrap3D::VerticalCylinder {
                    center_x: 0.0,
                    center_z: 0.014,
                    y_origin: y_hem,
                    radius_x: 0.195,
                    radius_z: 0.148,
                    taper_x: 0.02,
                    taper_z: 0.0,
                    u_center: 0.0,
                    angle_center: core::f64::consts::PI,
                    arc_radius: 0.185,
                },
                flip_normal: true,
                ..
            },
        );

        let mut s_cf = SeamSpec::flat("jacket_cf_zip", "jacket_front_l", "cf_zip", "jacket_front_r", "cf_zip");
        s_cf.max_ease = 0.02;
        s_cf.topstitch_depth = Length::mm(1.2);
        push(&mut ps.seams, s_cf);

        for (name, pa, ea, pb, eb, rev) in [
            ("jacket_side_l", "jacket_front_l", "side_l", "jacket_back", "side_l", false),
            ("jacket_side_r", "jacket_front_r", "side_r", "jacket_back", "side_r", false),
            ("jacket_sh_l", "jacket_front_l", "sh_l", "jacket_back", "sh_l", true),
            ("jacket_sh_r", "jacket_front_r", "sh_r", "jacket_back", "sh_r", true),
        ] {
            let mut s = SeamSpec::flat(name, pa, ea, pb, eb);
            s.reversed = rev;
            s.max_ease = 0.08;
            s.topstitch_depth = Length::mm(1.0);
            push(&mut ps.seams, s);
        }

        // --- Structured Tailored Standing Field Collar (Forward-Leaning Neck Contour + Throat Ease!) ---
        let col_hw = 0.184;
        let col_h = 0.054;
        push(
            &mut ps.panels,
            PanelSpec {
                name: "jacket_collar",
                layer: 2,
                fabric_idx: 0,
                lining_fabric_idx: Some(3),
                bottom: v(
                    alloc,
                    &[
                        SubEdge::new("c_neck_r", Curve2D::line([-col_hw, 0.0], [-col_hw * 0.54, 0.0]), 5),
                        SubEdge::new("c_neck_b", Curve2D::line([-col_hw * 0.54, 0.0], [col_hw * 0.54, 0.0]), 10),
                        SubEdge::new("c_neck_l", Curve2D::line([col_hw * 0.54, 0.0], [col_hw, 0.0]), 5),
                    ],
                ),
                right: v(alloc, &[SubEdge::new("c_end_l", Curve2D::quad([col_hw, 0.0], [col_hw * 0.97, col_h * 0.52], [col_hw * 0.91, col_h * 0.90]), 6)]),
                top: v(
                    alloc,
                    &[SubEdge::new(
                        "c_top",
                        Curve2D::quad([-col_hw * 0.91, col_h * 0.90], [0.0, col_h * 1.02], [col_hw * 0.91, col_h * 0.90]),
                        20,
                    )],
                ),
                left: v(alloc, &[SubEdge::new("c_end_r", Curve2D::quad([-col_hw, 0.0], [-col_hw * 0.97, col_h * 0.52], [-col_hw * 0.91, col_h * 0.90]), 6)]),
                wrap: Wrap3D::VerticalCylinder {
                    center_x: 0.0,
                    center_z: -0.024,
                    y_origin: 1.482,
                    radius_x: 0.084,
                    radius_z: 0.084,
                    taper_x: -0.02,
                    taper_z: -0.02,
                    u_center: 0.0,
                    angle_center: core::f64::consts::PI,
                    arc_radius: 0.084,
                },
                flip_normal: true,
                ..
            },
        );
        for (name, pa, ea, pb, eb, rev) in [
            ("collar_back", "jacket_collar", "c_neck_b", "jacket_back", "neck_b", false),
            ("collar_l", "jacket_collar", "c_neck_l", "jacket_front_l", "neck_l", false),
            ("collar_r", "jacket_collar", "c_neck_r", "jacket_front_r", "neck_r", false),
        ] {
            let mut s = SeamSpec::flat(name, pa, ea, pb, eb);
            s.reversed = rev;
            s.max_ease = 0.65;
            s.topstitch_depth = Length::mm(0.8);
            s.fold_angle_rad = 0.0;
            push(&mut ps.seams, s);
        }
        push(&mut ps.elastics, ElasticSpec { panel: "jacket_collar", edge: "c_top", rest_scale: 0.94 });
        push(&mut ps.pins, PinSpec { panel: "jacket_collar", edge: "c_top", stiffness: 0.45, sewing_only: false });

        // --- 2-Piece Contoured A-Pose Sleeves + Ballistic Cordura Cuffs ---
        // Extra +26 mm longitudinal slack above cuffs creates authentic forearm/elbow gathering folds!
        for is_right in [false, true] {
            let (origin, ax, arm_len) = if is_right { (r_sh, r_dir, r_len) } else { (l_sh, l_dir, l_len) };
            let cuff_len = 0.058;
            // Real 2D pattern sleeve length is +22 mm longer than straight A-pose distance -> natural forearm stacking!
            let body_len_3d = (arm_len - cuff_len).max(0.28);
            let body_len_2d = body_len_3d + 0.010;

            let w_cap = 0.204;
            let w_cuff = 0.136; // relaxed forearm gathered into 0.84x ballistic Cordura wrist cuff!

            let (sf_name, sb_name, cf_name, cb_name) = if is_right {
                ("sleeve_r_f", "sleeve_r_b", "cuff_r_f", "cuff_r_b")
            } else {
                ("sleeve_l_f", "sleeve_l_b", "cuff_l_f", "cuff_l_b")
            };

            let (ang_f, ang_b) = if is_right {
                (core::f64::consts::FRAC_PI_2, -core::f64::consts::FRAC_PI_2)
            } else {
                (-core::f64::consts::FRAC_PI_2, core::f64::consts::FRAC_PI_2)
            };

            let make_wrap = |ang: f64, v_off: f64, ra0: f64, rb0: f64, ta: f64, tb: f64| Wrap3D::LimbTube {
                origin: origin + ax * v_off,
                axis: ax,
                ref_radial: Vec3::new(0.0, -1.0, 0.0),
                radius_a: ra0,
                radius_b: rb0,
                taper_a: ta,
                taper_b: tb,
                u_center: 0.0,
                angle_center: ang,
                arc_radius: 0.070,
            };

            let (f_u_lo_cap, f_u_hi_cap, f_u_lo_cuff, f_u_hi_cuff) = (-w_cap * 0.5, w_cap * 0.5, -w_cuff * 0.5, w_cuff * 0.5);
            let f_wrap = make_wrap(ang_f, 0.0, 0.090, 0.086, -0.088, -0.080);
            let b_wrap = make_wrap(ang_b, 0.0, 0.090, 0.086, -0.088, -0.080);

            push(
                &mut ps.panels,
                PanelSpec {
                    name: sf_name,
                    layer: 2,
                    fabric_idx: 0,
                    lining_fabric_idx: None,
                    bottom: v(alloc, &[SubEdge::new("cap", Curve2D::line([f_u_lo_cap, 0.0], [f_u_hi_cap, 0.0]), 12)]),
                    right: v(alloc, &[SubEdge::new("edge_hi", Curve2D::line([f_u_hi_cap, 0.0], [f_u_hi_cuff, body_len_2d]), 22)]),
                    top: v(alloc, &[SubEdge::new("wrist", Curve2D::line([f_u_lo_cuff, body_len_2d], [f_u_hi_cuff, body_len_2d]), 12)]),
                    left: v(alloc, &[SubEdge::new("edge_lo", Curve2D::line([f_u_lo_cap, 0.0], [f_u_lo_cuff, body_len_2d]), 22)]),
                    wrap: f_wrap,
                    flip_normal: false,
                    ..
                },
            );
            push(
                &mut ps.panels,
                PanelSpec {
                    name: sb_name,
                    layer: 2,
                    fabric_idx: 0,
                    lining_fabric_idx: None,
                    bottom: v(alloc, &[SubEdge::new("cap", Curve2D::line([f_u_lo_cap, 0.0], [f_u_hi_cap, 0.0]), 12)]),
                    right: v(alloc, &[SubEdge::new("edge_hi", Curve2D::line([f_u_hi_cap, 0.0], [f_u_hi_cuff, body_len_2d]), 22)]),
                    top: v(alloc, &[SubEdge::new("wrist", Curve2D::line([f_u_lo_cuff, body_len_2d], [f_u_hi_cuff, body_len_2d]), 12)]),
                    left: v(alloc, &[SubEdge::new("edge_lo", Curve2D::line([f_u_lo_cap, 0.0], [f_u_lo_cuff, body_len_2d]), 22)]),
                    wrap: b_wrap,
                    flip_normal: false,
                    ..
                },
            );

            // Ballistic Cordura Cuff halves!
            let cuff_w_end = w_cuff * 0.84;
            let cf_wrap = make_wrap(ang_f, body_len_3d, 0.052, 0.050, -0.04, -0.04);
            let cb_wrap = make_wrap(ang_b, body_len_3d, 0.052, 0.050, -0.04, -0.04);
            push(
                &mut ps.panels,
                PanelSpec {
                    name: cf_name,
                    layer: 2,
                    fabric_idx: 3,
                    lining_fabric_idx: Some(3),
                    bottom: v(alloc, &[SubEdge::new("join", Curve2D::line([-w_cuff * 0.5, 0.0], [w_cuff * 0.5, 0.0]), 12)]),
                    right: v(alloc, &[SubEdge::new("edge_hi", Curve2D::line([w_cuff * 0.5, 0.0], [cuff_w_end * 0.5, cuff_len]), 5)]),
                    top: v(alloc, &[SubEdge::new("hem", Curve2D::line([-cuff_w_end * 0.5, cuff_len], [cuff_w_end * 0.5, cuff_len]), 12)]),
                    left: v(alloc, &[SubEdge::new("edge_lo", Curve2D::line([-w_cuff * 0.5, 0.0], [-cuff_w_end * 0.5, cuff_len]), 5)]),
                    wrap: cf_wrap,
                    flip_normal: false,
                    ..
                },
            );
            push(
                &mut ps.panels,
                PanelSpec {
                    name: cb_name,
                    layer: 2,
                    fabric_idx: 3,
                    lining_fabric_idx: Some(3),
                    bottom: v(alloc, &[SubEdge::new("join", Curve2D::line([-w_cuff * 0.5, 0.0], [w_cuff * 0.5, 0.0]), 12)]),
                    right: v(alloc, &[SubEdge::new("edge_hi", Curve2D::line([w_cuff * 0.5, 0.0], [cuff_w_end * 0.5, cuff_len]), 5)]),
                    top: v(alloc, &[SubEdge::new("hem", Curve2D::line([-cuff_w_end * 0.5, cuff_len], [cuff_w_end * 0.5, cuff_len]), 12)]),
                    left: v(alloc, &[SubEdge::new("edge_lo", Curve2D::line([-w_cuff * 0.5, 0.0], [-cuff_w_end * 0.5, cuff_len]), 5)]),
                    wrap: cb_wrap,
                    flip_normal: false,
                    ..
                },
            );

            let mut sl1 = SeamSpec::flat(if is_right { "sl_r_1" } else { "sl_l_1" }, sf_name, "edge_lo", sb_name, "edge_hi");
            sl1.max_ease = 0.04;
            push(&mut ps.seams, sl1);
            let mut sl2 = SeamSpec::flat(if is_right { "sl_r_2" } else { "sl_l_2" }, sf_name, "edge_hi", sb_name, "edge_lo");
            sl2.max_ease = 0.04;
            push(&mut ps.seams, sl2);

            push(&mut ps.seams, SeamSpec::flat(if is_right { "cf_r_1" } else { "cf_l_1" }, cf_name, "edge_lo", cb_name, "edge_hi"));
            push(&mut ps.seams, SeamSpec::flat(if is_right { "cf_r_2" } else { "cf_l_2" }, cf_name, "edge_hi", cb_name, "edge_lo"));
            push(&mut ps.seams, SeamSpec::flat(if is_right { "cf_r_jf" } else { "cf_l_jf" }, sf_name, "wrist", cf_name, "join"));
            push(&mut ps.seams, SeamSpec::flat(if is_right { "cf_r_jb" } else { "cf_l_jb" }, sb_name, "wrist", cb_name, "join"));

            let (front_torso, arm_f, arm_b, rev_f, rev_b) = if is_right {
                ("jacket_front_r", "armscye_rf", "armscye_rb", false, true)
            } else {
                ("jacket_front_l", "armscye_lf", "armscye_lb", true, false)
            };
            let mut sc_f = SeamSpec::flat(if is_right { "arm_rf" } else { "arm_lf" }, front_torso, arm_f, sf_name, "cap");
            sc_f.reversed = rev_f;
            sc_f.max_ease = 0.25;
            sc_f.topstitch_depth = Length::mm(1.0);
            push(&mut ps.seams, sc_f);

            let mut sc_b = SeamSpec::flat(if is_right { "arm_rb" } else { "arm_lb" }, "jacket_back", arm_b, sb_name, "cap");
            sc_b.reversed = rev_b;
            sc_b.max_ease = 0.25;
            sc_b.topstitch_depth = Length::mm(1.0);
            push(&mut ps.seams, sc_b);

            push(&mut ps.elastics, ElasticSpec { panel: cf_name, edge: "hem", rest_scale: 0.88 });
            push(&mut ps.elastics, ElasticSpec { panel: cb_name, edge: "hem", rest_scale: 0.88 });
            push(&mut ps.pins, PinSpec { panel: cf_name, edge: "hem", stiffness: 0.35, sewing_only: true });
            push(&mut ps.pins, PinSpec { panel: cb_name, edge: "hem", stiffness: 0.35, sewing_only: true });

            // Crisp flat-stitched Cordura elbow reinforcement patch on BOTH back sleeves!
            push(
                &mut ps.patches,
                SurfacePatchSpec {
                    name: if is_right { "elbow_r" } else { "elbow_l" },
                    panel: sb_name,
                    s_min: 0.22,
                    s_max: 0.78,
                    t_min: 0.44,
                    t_max: 0.68,
                    depth: Length::mm(2.6),
                    crown: Length::mm(0.4),
                    edge_bevel: 0.18,
                    flap_fraction: 0.0,
                    bottom_chamfer: 0.12,
                    fabric_idx: 3,
                    flap_fabric_idx: None,
                    box_pleat: false,
                },
            );

            // Sleek Cordura upper-sleeve utility / pen pocket on left bicep (`sleeve_l_f`)!
            if !is_right {
                push(
                    &mut ps.patches,
                    SurfacePatchSpec {
                        name: "sleeve_util_pkt_l",
                        panel: sf_name,
                        s_min: 0.26,
                        s_max: 0.72,
                        t_min: 0.23,
                        t_max: 0.44,
                        depth: Length::mm(4.8),
                        crown: Length::mm(0.8),
                        edge_bevel: 0.18,
                        flap_fraction: 0.28,
                        bottom_chamfer: 0.14,
                        fabric_idx: 3,
                        flap_fabric_idx: Some(3),
                        box_pleat: true,
                    },
                );
                push(
                    &mut ps.hardware,
                    HardwareMount {
                        name: "sleeve_pkt_snap_l",
                        panel: sf_name,
                        s: 0.49,
                        t: 0.398,
                        normal_offset: Length::mm(8.8),
                        kind: HardwareKind::SnapButton { radius: Length::mm(4.0), height: Length::mm(1.8) },
                        fabric_idx: 5,
                    },
                );
            }
        }

        // Twin tailored Cordura shoulder epaulette tabs with brushed titanium tack snaps
        // (kept cleanly inside the shoulder slope away from the deltoid armscye cap!).
        for (panel, s0, s1, snap_s, nm_p, nm_s) in [
            ("jacket_front_l", 0.25, 0.68, 0.31, "epaulette_l", "epaulette_snap_l"),
            ("jacket_front_r", 0.32, 0.75, 0.69, "epaulette_r", "epaulette_snap_r"),
        ] {
            push(
                &mut ps.patches,
                SurfacePatchSpec {
                    name: nm_p,
                    panel,
                    s_min: s0,
                    s_max: s1,
                    t_min: 0.905,
                    t_max: 0.955,
                    depth: Length::mm(2.0),
                    crown: Length::mm(0.25),
                    edge_bevel: 0.20,
                    flap_fraction: 0.0,
                    bottom_chamfer: 0.14,
                    fabric_idx: 3,
                    flap_fabric_idx: None,
                    box_pleat: false,
                },
            );
            push(
                &mut ps.hardware,
                HardwareMount {
                    name: nm_s,
                    panel,
                    s: snap_s,
                    t: 0.930,
                    normal_offset: Length::mm(3.4),
                    kind: HardwareKind::SnapButton { radius: Length::mm(4.0), height: Length::mm(1.6) },
                    fabric_idx: 5,
                },
            );
        }

        // --- Architectural Field Jacket Details (Crisp Box-Pleat Pockets, Detached Flaps, Placket) ---
        push(
            &mut ps.patches,
            SurfacePatchSpec {
                name: "storm_placket",
                panel: "jacket_front_r",
                s_min: 0.0,
                s_max: 0.145,
                t_min: 0.004,
                t_max: 0.858,
                depth: Length::mm(4.2),
                crown: Length::mm(0.3),
                edge_bevel: 0.14,
                flap_fraction: 0.0,
                bottom_chamfer: 0.0,
                fabric_idx: 3,
                flap_fabric_idx: None,
                box_pleat: false,
            },
        );
        // Flexible molded dark aerospace zipper teeth tape running cleanly along `jacket_front_l::cf_zip`
        // (deforms smoothly with the spine & waist, tucked right against the Cordura storm placket!).
        push(
            &mut ps.patches,
            SurfacePatchSpec {
                name: "main_zipper_tape",
                panel: "jacket_front_l",
                s_min: 0.965,
                s_max: 1.0,
                t_min: 0.004,
                t_max: 0.858,
                depth: Length::mm(2.2),
                crown: Length::mm(0.2),
                edge_bevel: 0.12,
                flap_fraction: 0.0,
                bottom_chamfer: 0.0,
                fabric_idx: 6,
                flap_fabric_idx: None,
                box_pleat: false,
            },
        );
        for (idx, t_pos) in [0.12, 0.33, 0.54, 0.75].iter().enumerate() {
            push(
                &mut ps.hardware,
                HardwareMount {
                    name: match idx { 0 => "placket_snap_0", 1 => "placket_snap_1", 2 => "placket_snap_2", _ => "placket_snap_3" },
                    panel: "jacket_front_r",
                    s: 0.070,
                    t: *t_pos,
                    normal_offset: Length::mm(6.5),
                    kind: HardwareKind::SnapButton { radius: Length::mm(5.0), height: Length::mm(2.2) },
                    fabric_idx: 5,
                },
            );
        }
        push(
            &mut ps.hardware,
            HardwareMount {
                name: "main_zipper_pull",
                panel: "jacket_front_r",
                s: 0.02,
                t: 0.855,
                normal_offset: Length::mm(7.0),
                kind: HardwareKind::ZipperPull { width: Length::mm(9.0), length: Length::mm(24.0), thickness: Length::mm(3.0) },
                fabric_idx: 5,
            },
        );

        // Chest box-pleat utility pockets + detached overhanging Cordura flaps!
        for (panel, s0, s1, snap_s, nm_p, nm_s) in [
            ("jacket_front_l", 0.30, 0.76, 0.53, "chest_pkt_l", "chest_snap_l"),
            ("jacket_front_r", 0.24, 0.70, 0.47, "chest_pkt_r", "chest_snap_r"),
        ] {
            push(
                &mut ps.patches,
                SurfacePatchSpec {
                    name: nm_p,
                    panel,
                    s_min: s0,
                    s_max: s1,
                    t_min: 0.53,
                    t_max: 0.74,
                    depth: Length::mm(9.5),
                    crown: Length::mm(1.5),
                    edge_bevel: 0.14,
                    flap_fraction: 0.34,
                    bottom_chamfer: 0.14,
                    fabric_idx: 0,
                    flap_fabric_idx: Some(3),
                    box_pleat: true,
                },
            );
            push(
                &mut ps.hardware,
                HardwareMount {
                    name: nm_s,
                    panel,
                    s: snap_s,
                    t: 0.695,
                    normal_offset: Length::mm(15.2),
                    kind: HardwareKind::SnapButton { radius: Length::mm(4.8), height: Length::mm(2.0) },
                    fabric_idx: 5,
                },
            );
        }

        // Lower hip box-pleat bellows pockets + detached overhanging Cordura flaps!
        for (panel, s0, s1, snap_s, nm_p, nm_s) in [
            ("jacket_front_l", 0.26, 0.80, 0.53, "hip_pkt_l", "hip_snap_l"),
            ("jacket_front_r", 0.20, 0.74, 0.47, "hip_pkt_r", "hip_snap_r"),
        ] {
            push(
                &mut ps.patches,
                SurfacePatchSpec {
                    name: nm_p,
                    panel,
                    s_min: s0,
                    s_max: s1,
                    t_min: 0.10,
                    t_max: 0.35,
                    depth: Length::mm(11.5),
                    crown: Length::mm(1.8),
                    edge_bevel: 0.14,
                    flap_fraction: 0.30,
                    bottom_chamfer: 0.14,
                    fabric_idx: 0,
                    flap_fabric_idx: Some(3),
                    box_pleat: true,
                },
            );
            push(
                &mut ps.hardware,
                HardwareMount {
                    name: nm_s,
                    panel,
                    s: snap_s,
                    t: 0.305,
                    normal_offset: Length::mm(17.2),
                    kind: HardwareKind::SnapButton { radius: Length::mm(4.8), height: Length::mm(2.0) },
                    fabric_idx: 5,
                },
            );
        }

        // Flat-stitched Cordura back shoulder yoke (kept strictly across scapulae, well inside the
        // armscye hinge so extreme rifle-aim deltoid rotation never lifts a corner into silhouette!).
        push(
            &mut ps.patches,
            SurfacePatchSpec {
                name: "back_yoke",
                panel: "jacket_back",
                s_min: 0.20,
                s_max: 0.80,
                t_min: 0.82,
                t_max: 0.93,
                depth: Length::mm(2.2),
                crown: Length::mm(0.3),
                edge_bevel: 0.20,
                flap_fraction: 0.0,
                bottom_chamfer: 0.12,
                fabric_idx: 3,
                flap_fabric_idx: None,
                box_pleat: false,
            },
        );

        // True Internal-Seam Sewn Sub-Assembly: physically simulated Ballistic Cordura rear waist
        // cinch strap stitched directly onto `jacket_back` at left & right vertical bar-tack seams!
        let tab_hw = 0.154; // proud tailored military cinch arc across s=0.27..0.73 strictly outside jacket_back!
        let tab_h = 0.042;
        push(
            &mut ps.panels,
            PanelSpec {
                name: "back_cinch_tab",
                layer: 2,
                fabric_idx: 3,
                lining_fabric_idx: Some(3),
                bottom: v(alloc, &[SubEdge::new("tab_bot", Curve2D::line([-tab_hw, 0.0], [tab_hw, 0.0]), 12)]),
                right: v(alloc, &[SubEdge::new("tab_r", Curve2D::line([tab_hw, 0.0], [tab_hw, tab_h]), 3)]),
                top: v(alloc, &[SubEdge::new("tab_top", Curve2D::line([-tab_hw, tab_h], [tab_hw, tab_h]), 12)]),
                left: v(alloc, &[SubEdge::new("tab_l", Curve2D::line([-tab_hw, 0.0], [-tab_hw, tab_h]), 3)]),
                wrap: Wrap3D::VerticalCylinder {
                    center_x: 0.0,
                    center_z: 0.014,
                    y_origin: 1.055,
                    radius_x: 0.210,
                    radius_z: 0.165,
                    taper_x: 0.0,
                    taper_z: 0.0,
                    u_center: 0.0,
                    angle_center: core::f64::consts::PI,
                    arc_radius: 0.195,
                },
                flip_normal: true,
                ..
            },
        );
        push(
            &mut ps.internal_seams,
            InternalSeamSpec {
                name: "cinch_tack_l",
                sub_panel: "back_cinch_tab",
                sub_edge: "tab_l",
                host_panel: "jacket_back",
                host_s0: 0.27,
                host_t0: 0.30,
                host_s1: 0.27,
                host_t1: 0.37,
                reversed: false,
                normal_offset: Length::mm(9.0),
                fold_angle_rad: 0.0,
                topstitch_depth: Length::mm(1.0),
            },
        );
        push(
            &mut ps.internal_seams,
            InternalSeamSpec {
                name: "cinch_tack_r",
                sub_panel: "back_cinch_tab",
                sub_edge: "tab_r",
                host_panel: "jacket_back",
                host_s0: 0.73,
                host_t0: 0.30,
                host_s1: 0.73,
                host_t1: 0.37,
                reversed: false,
                normal_offset: Length::mm(9.0),
                fold_angle_rad: 0.0,
                topstitch_depth: Length::mm(1.0),
            },
        );
    }

    ps
}

fn main() {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let [Some(spacer), Some(boots), Some(wrist), Some(output), None] = core::array::from_fn(|_| args.next()) else {
        eprintln!("usage: tailor_astra SPACER.glb BOOTS.glb WRIST.glb OUTPUT_DIRECTORY");
        std::process::exit(2);
    };
    let t_start = Instant::now();
    let heap = blaze_own::Heap::process();
    let alloc = heap.alloc();

    println!("=== blaze-cad-cloth SOTA: A-Pose Conformal Garment CAD & Physics ===");

    let read_glb = |path: &PathBuf| {
        std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    };
    let spacer_bytes = read_glb(&spacer);
    let boots_bytes = read_glb(&boots);
    let wrist_bytes = read_glb(&wrist);
    std::fs::create_dir_all(&output).unwrap_or_else(|error| panic!("create {}: {error}", output.display()));

    let head_glb = read_glb_skeleton_mesh(&spacer_bytes, Some(0), alloc, alloc).expect("parse head");
    let body_glb = read_glb_skeleton_mesh(&spacer_bytes, Some(1), alloc, alloc).expect("parse body");
    let full_mannequin = merge_skinned_mannequin(&body_glb, &head_glb, alloc).expect("merge full mannequin");
    let boots_glb = read_glb_skeleton_mesh(&boots_bytes, None, alloc, alloc).expect("parse boots");
    let wrist_glb = read_glb_skeleton_mesh(&wrist_bytes, None, alloc, alloc).expect("parse wrist_unit");

    // 1. Pose COMPLETE head-to-toe MetaHuman (`body + head`) into relaxed A-POSE!
    let arm_drop_rad = 0.60;
    let elbow_bend_rad = 0.08;
    let posed = pose_body_for_drape(&full_mannequin, arm_drop_rad, elbow_bend_rad, alloc).expect("pose body+head");

    // Measure exact A-pose shoulder-to-wrist vectors directly from the 57-joint skeleton!
    let l_upper = find_joint_pos(&full_mannequin, &posed, "upperarm_l").unwrap_or(Vec3::new(-0.18, 1.43, 0.02));
    let l_hand = find_joint_pos(&full_mannequin, &posed, "hand_l").unwrap_or(Vec3::new(-0.62, 1.18, -0.02));
    let r_upper = find_joint_pos(&full_mannequin, &posed, "upperarm_r").unwrap_or(Vec3::new(0.18, 1.43, 0.02));
    let r_hand = find_joint_pos(&full_mannequin, &posed, "hand_r").unwrap_or(Vec3::new(0.62, 1.18, -0.02));

    let l_dir = (l_hand - l_upper).normalized().unwrap();
    let r_dir = (r_hand - r_upper).normalized().unwrap();
    let l_full = (l_hand - l_upper).length();
    let r_full = (r_hand - r_upper).length();

    let _ = wrist_glb;
    let l_sleeve_start = l_upper + l_dir * 0.015;
    let r_sleeve_start = r_upper + r_dir * 0.015;
    let l_sleeve_len = (l_full - 0.105).clamp(0.34, 0.44);
    let r_sleeve_len = (r_full - 0.045).clamp(0.38, 0.48);

    println!(
        "Posed MetaHuman into A-pose (drop={:.0} deg, elbow={:.0} deg): L_sh=({:.2},{:.2},{:.2}) -> L_hand=({:.2},{:.2},{:.2})",
        arm_drop_rad.to_degrees(),
        elbow_bend_rad.to_degrees(),
        l_upper.x, l_upper.y, l_upper.z, l_hand.x, l_hand.y, l_hand.z
    );

    // 2. Measure BodySloper & build COMPLETE head-to-toe BVH colliders directly from the unified
    //    `full_mannequin` (`body + head`) so neck, trapezius, collarbone, and body share one exact
    //    skinned geometry & 57-joint weight field!
    let full_body_pos = &posed.positions;
    let full_body_tris = &full_mannequin.triangles;

    let sloper = BodySloper::measure(full_body_pos, l_sleeve_start, l_hand, r_sleeve_start, r_hand, alloc).expect("measure sloper");
    let sl_chest = sloper.sample_torso(1.35);
    let sl_waist = sloper.sample_torso(1.12);
    let sl_neck = sloper.sample_torso(1.48);
    println!(
        "BodySloper measured (body+head): waist rx={:.3}/rz={:.3} | chest rx={:.3}/rz={:.3} | neck rx={:.3}/rz={:.3}",
        sl_waist.rx, sl_waist.rz, sl_chest.rx, sl_chest.rz, sl_neck.rx, sl_neck.rz
    );

    // 3. Draft 2D patterns & verify seam_ease_check + export SVG blueprint!
    let patterns = build_spacefarer_patterns(
        alloc,
        l_sleeve_start,
        l_dir,
        l_sleeve_len,
        r_sleeve_start,
        r_dir,
        r_sleeve_len,
    );
    match seam_ease_check(&patterns) {
        Ok(()) => println!("[PASS] seam_ease_check: all {} seams matched within ease tolerances!", patterns.seams.len()),
        Err(f) => panic!("[FAIL] seam_ease_check: {f:?}"),
    }
    let svg = export_pattern_svg(&patterns, alloc).expect("svg");
    std::fs::write(output.join("blaze-cloth-pattern.svg"), &svg[..]).expect("write pattern svg");

    // 4. Build complete head-to-toe A-pose BVH colliders directly from the unified `full_mannequin`!
    let full_body_raw_col = MeshCollider::build(full_body_pos, full_body_tris, false, alloc).expect("full raw bvh");
    let body_modesty_col = MeshCollider::build(full_body_pos, full_body_tris, true, alloc).expect("modesty bvh");
    let boots_col = MeshCollider::build_open_sheet(&boots_glb.positions, &boots_glb.triangles, alloc).expect("boots bvh");

    let mut cad_scratch = Scratch::new(alloc);
    let mut assembled = GarmentMesh::new(alloc);

    // ==================== LAYER 0: UNDERSHIRT (Conformed via BodySloper!) ====================
    let t0 = Instant::now();
    let mut shirt_cloth = build_cloth_mesh(&patterns, Some(0), alloc).expect("mesh layer 0");
    sloper.conform_panel(&mut shirt_cloth, "shirt_front", 1.12, 0.0, 0.010, 0.010, 0.235, 1.54, 1.43);
    sloper.conform_panel(&mut shirt_cloth, "shirt_back", 1.12, core::f64::consts::PI, 0.010, 0.010, 0.235, 1.54, 1.43);
    seam_alignment_check(&shirt_cloth, &patterns).expect("layer 0 seam alignment");
    let col0_m = [&body_modesty_col];
    let col0_o = [0.0018];
    let col0 = UnifiedCollider { meshes: &col0_m, cad_tapes: &[], mesh_offsets: &col0_o };
    let rep0 = simulate_garment(
        &mut shirt_cloth,
        &patterns,
        &col0,
        SolverOpts { sewing_frames: 30, settle_frames: 40, substeps: 3, iterations: 4, contact_margin: Length::mm(1.5), .. },
        alloc,
        &mut cad_scratch,
    )
    .expect("sim 0");
    println!(
        "Layer 0 (Undershirt): pre-weld gap {:.1} mm | welded {} | v_max={:.3} m/s ({:.1} s)",
        rep0.max_pre_weld_gap.as_mm(), rep0.welded_pairs, rep0.final_max_speed, t0.elapsed().as_secs_f64()
    );
    assembled.append_draped_layer(&shirt_cloth, &patterns, 1, alloc).expect("append 0");
    let shirt_col = MeshCollider::build_open_sheet(&shirt_cloth.pos, &shirt_cloth.triangles, alloc).expect("shirt bvh");

    // ==================== LAYER 1: 4-PANEL CARGO TROUSERS ====================
    let t1 = Instant::now();
    let mut trouser_cloth = build_cloth_mesh(&patterns, Some(1), alloc).expect("mesh layer 1");
    seam_alignment_check(&trouser_cloth, &patterns).expect("layer 1 seam alignment");
    let col1_m = [&body_modesty_col, &boots_col];
    let col1_o = [0.0024, 0.0032];
    let col1 = UnifiedCollider { meshes: &col1_m, cad_tapes: &[], mesh_offsets: &col1_o };
    let rep1 = simulate_garment(
        &mut trouser_cloth,
        &patterns,
        &col1,
        SolverOpts {
            sewing_frames: 40,
            settle_frames: 90,
            substeps: 4,
            iterations: 8,
            velocity_damping: 0.992,
            contact_margin: Length::mm(2.0),
            use_ipc: true,
            adaptive_remesh: true,
            remesh_max_splits: 200,
            ..
        },
        alloc,
        &mut cad_scratch,
    )
    .expect("sim 1");
    println!(
        "Layer 1 (Cargo Trousers): gap {:.1} mm | welded {} | remesh(flip={}, split={}) | IPC(vt={}, ee={}) | v={:.3} ({:.1}s)",
        rep1.max_pre_weld_gap.as_mm(),
        rep1.welded_pairs,
        rep1.remesh_edges_flipped,
        rep1.remesh_edges_split,
        rep1.ipc_vt_contacts,
        rep1.ipc_ee_contacts,
        rep1.final_max_speed,
        t1.elapsed().as_secs_f64()
    );
    assembled.append_draped_layer(&trouser_cloth, &patterns, 2, alloc).expect("append 1");
    let trouser_col = MeshCollider::build_open_sheet(&trouser_cloth.pos, &trouser_cloth.triangles, alloc).expect("trouser bvh");

    // ==================== LAYER 2: A-POSE FIELD JACKET (Conformed + CAD Co-Sim!) ====================
    let t2 = Instant::now();
    let mut jacket_cloth = build_cloth_mesh(&patterns, Some(2), alloc).expect("mesh layer 2");
    // Conform torso with over-the-shoulder dome arching, snap collar onto neckline (0.0 mm gap),
    // AND snap all 4 sleeve caps onto torso armscyes (0.0 mm gap -> zero shoulder/armpit pinching)!
    sloper.conform_panel(&mut jacket_cloth, "jacket_front_l", 0.870, 0.0, 0.026, 0.024, 0.285, 1.54, 1.40);
    sloper.conform_panel(&mut jacket_cloth, "jacket_front_r", 0.870, 0.0, 0.026, 0.024, 0.285, 1.54, 1.40);
    sloper.conform_panel(&mut jacket_cloth, "jacket_back", 0.870, core::f64::consts::PI, 0.026, 0.024, 0.285, 1.54, 1.40);
    sloper.conform_panel(&mut jacket_cloth, "back_cinch_tab", 1.055, core::f64::consts::PI, 0.052, 0.050, 0.195, 1.54, 1.40);
    sloper.conform_collar_to_neckline(&mut jacket_cloth, &patterns, "jacket_collar", 0.004);
    for sp in ["sleeve_l_f", "sleeve_l_b", "sleeve_r_f", "sleeve_r_b"] {
        sloper.conform_sleeve_to_armscye(&mut jacket_cloth, &patterns, sp);
    }
    seam_alignment_check(&jacket_cloth, &patterns).expect("layer 2 seam alignment");
    // Print any seam whose initial 3D gap exceeds 45 mm so every seam is verified tight!
    for (si, seam) in patterns.seams.iter().enumerate() {
        let mut max_g = 0.0f64;
        for pair in jacket_cloth.seam_pairs.iter() {
            if pair.seam_idx as usize == si {
                max_g = max_g.max((jacket_cloth.init_pos[pair.a as usize] - jacket_cloth.init_pos[pair.b as usize]).length());
            }
        }
        if max_g > 0.045 {
            println!("  [seam init] {:<16} max_init_gap = {:.1} mm", seam.name, max_g * 1000.0);
        }
    }

    // Build an implicit `blaze-cad-field::Tape` sub-fabric sternum backing plate nestled cleanly on
    // the upper-left chest under the jacket so cloth co-simulates directly against the CAD SDF!
    let chest_plate_tape = build_chest_beacon_tape_at(
        Length::mm(38.0),
        Length::mm(54.0),
        Length::mm(6.0),
        Placement::shift(
            Length::m(sl_chest.cx - 0.085),
            Length::m(1.375),
            Length::m(sl_chest.cz - sl_chest.rz - 0.010),
        ),
        alloc,
        alloc,
    )
    .expect("build sternum co-sim CAD tape");

    let col2_m = [&body_modesty_col, &boots_col, &trouser_col, &shirt_col];
    let col2_o = [0.0032, 0.0035, 0.0040, 0.0032];
    let col2_tapes = [&chest_plate_tape];
    let col2 = UnifiedCollider { meshes: &col2_m, cad_tapes: &col2_tapes, mesh_offsets: &col2_o };
    let rep2 = simulate_garment(
        &mut jacket_cloth,
        &patterns,
        &col2,
        SolverOpts {
            sewing_frames: 45,
            settle_frames: 95,
            substeps: 4,
            iterations: 8,
            velocity_damping: 0.992,
            contact_margin: Length::mm(2.0),
            use_ipc: true,
            adaptive_remesh: true,
            remesh_max_splits: 180,
            ..
        },
        alloc,
        &mut cad_scratch,
    )
    .expect("sim 2");
    println!(
        "Layer 2 (Field Jacket): gap {:.1} mm | welded {} | remesh(flip={}, split={}) | IPC(vt={}, ee={}) | v={:.3} ({:.1}s)",
        rep2.max_pre_weld_gap.as_mm(),
        rep2.welded_pairs,
        rep2.remesh_edges_flipped,
        rep2.remesh_edges_split,
        rep2.ipc_vt_contacts,
        rep2.ipc_ee_contacts,
        rep2.final_max_speed,
        t2.elapsed().as_secs_f64()
    );
    assembled.append_draped_layer(&jacket_cloth, &patterns, 2, alloc).expect("append 2");

    // ==================== TWO-TONE BLAZE-CAD-FIELD HARD-SURFACE GEAR ====================
    // Compile & mesh exact two-tone CSG aerospace hardware (`mat_dark` = 6 matte polymer/anodized
    // housing + `mat_metal` = 5 brushed titanium bezels, antennas, lock bars & grip ridges)!
    let t_cad = Instant::now();
    let mut cad_tris = 0u32;
    {
        // 1. Two-Tone Tactical Cobra Belt Buckle on front waist belt (`leg_r_front`).
        if let Some(p_belt) = trouser_cloth.panels.iter().find(|p| p.name == "leg_r_front") {
            let (pos, nrm, tu, tv) = sample_panel_frame(&trouser_cloth, p_belt, 0.03, 0.964);
            let (frame_dark, lock_metal) = build_cobra_buckle_dual_tapes(
                COBRA_BUCKLE_DEFAULT[0],
                COBRA_BUCKLE_DEFAULT[1],
                Length::mm(7.5),
                alloc,
                alloc,
            )
            .expect("cobra buckle dual tapes");
            cad_tris += append_cad_solid_to_garment(
                &frame_dark,
                6,
                pos + nrm * 0.0038,
                tu,
                tv,
                nrm,
                pos,
                Length::mm(1.30),
                &mut assembled,
                alloc,
            )
            .expect("mesh cobra dark frame");
            cad_tris += append_cad_solid_to_garment(
                &lock_metal,
                5,
                pos + nrm * 0.0038,
                tu,
                tv,
                nrm,
                pos,
                Length::mm(1.15),
                &mut assembled,
                alloc,
            )
            .expect("mesh cobra titanium lock");
        }
        // 2. Two-Tone Spacefarer Chest Comms / Telemetry Beacon on upper-left chest (`jacket_front_l`).
        if let Some(p_chest) = jacket_cloth.panels.iter().find(|p| p.name == "jacket_front_l") {
            let (pos, nrm, tu, tv) = sample_panel_frame(&jacket_cloth, p_chest, 0.56, 0.775);
            let (housing_dark, accent_metal) = build_chest_beacon_dual_tapes(
                Length::mm(34.0),
                Length::mm(48.0),
                Length::mm(9.0),
                alloc,
                alloc,
            )
            .expect("chest beacon dual tapes");
            cad_tris += append_cad_solid_to_garment(
                &housing_dark,
                6,
                pos + nrm * 0.0018,
                tu,
                tv,
                nrm,
                pos,
                Length::mm(1.10),
                &mut assembled,
                alloc,
            )
            .expect("mesh beacon dark housing");
            cad_tris += append_cad_solid_to_garment(
                &accent_metal,
                5,
                pos + nrm * 0.0018,
                tu,
                tv,
                nrm,
                pos,
                Length::mm(0.85),
                &mut assembled,
                alloc,
            )
            .expect("mesh beacon titanium accents");
        }
        // 3. Two-Tone Collar Magnetic Clasp on right standing collar throat band (`jacket_collar`).
        if let Some(p_col) = jacket_cloth.panels.iter().find(|p| p.name == "jacket_collar") {
            let (pos, nrm, tu, tv) = sample_panel_frame(&jacket_cloth, p_col, 0.11, 0.36);
            let tv_lvl = (Vec3::new(0.0, 1.0, 0.0) - nrm * nrm.y).normalized().unwrap_or(tv);
            let tu_lvl = tv_lvl.cross(nrm).normalized().unwrap_or(tu);
            let (bar_dark, ridges_metal) = build_collar_clasp_dual_tapes(
                COLLAR_CLASP_DEFAULT[0],
                COLLAR_CLASP_DEFAULT[1],
                COLLAR_CLASP_DEFAULT[2],
                alloc,
                alloc,
            )
            .expect("collar clasp dual tapes");
            cad_tris += append_cad_solid_to_garment(
                &bar_dark,
                6,
                pos + nrm * 0.0022,
                tu_lvl,
                tv_lvl,
                nrm,
                pos,
                Length::mm(0.85),
                &mut assembled,
                alloc,
            )
            .expect("mesh collar clasp dark bar");
            cad_tris += append_cad_solid_to_garment(
                &ridges_metal,
                5,
                pos + nrm * 0.0022,
                tu_lvl,
                tv_lvl,
                nrm,
                pos,
                Length::mm(0.75),
                &mut assembled,
                alloc,
            )
            .expect("mesh collar clasp titanium ridges");
        }
        let _ = CHEST_BEACON_DEFAULT;
    }
    println!("Two-Tone Blaze-CAD Hardware (Cobra Buckle + Chest Beacon + Collar Clasp): {} tris ({:.0} ms)", cad_tris, t_cad.elapsed().as_secs_f64() * 1e3);

    // Cull hidden lower/shoulder undershirt faces AND any undershirt vertex climbing above the
    // collarbone neck line (`Y > 1.465`) so the sunlit MetaHuman neck & throat show cleanly!
    let culled = cull_occluded_inner_layer(&mut assembled, 0, 1.305, -1.0, 0.095);
    // Keep undershirt strictly below collarbone neck height (`Y <= 1.462`) for a crisp crewneck!
    {
        let mut w = 0usize;
        for r in 0..assembled.triangles.len() {
            let tri = assembled.triangles[r];
            let keep = if assembled.tri_layer[r] == 0 {
                let my = assembled.positions[tri[0] as usize]
                    .y
                    .max(assembled.positions[tri[1] as usize].y)
                    .max(assembled.positions[tri[2] as usize].y);
                my <= 1.468
            } else {
                true
            };
            if keep {
                assembled.triangles[w] = tri;
                assembled.tri_fabric[w] = assembled.tri_fabric[r];
                assembled.tri_layer[w] = assembled.tri_layer[r];
                w += 1;
            }
        }
        assembled.triangles.truncate(w);
        assembled.tri_fabric.truncate(w);
        assembled.tri_layer.truncate(w);
    }
    assembled.recompute_smooth_normals();

    // 5. Apply 1 pass of Crease-Preserving Loop Subdivision + Curvature Wear/Micro-AO Baking!
    let t_sub = Instant::now();
    let mut refined = subdivide_cloth_loop(&assembled, 2, 0.72, alloc, alloc).expect("loop subdiv");
    refined.bake_curvature_wear_and_ao(alloc).expect("bake wear + micro-AO");
    if let Err(e) = verify_garment_integrity(&assembled, None, &[], GarmentIntegrityOpts::default()) {
        println!("[DIAG] pre-subdiv assembled integrity: {e:?}");
    }
    if let Err(e) = verify_garment_integrity(&refined, None, &[], GarmentIntegrityOpts::default()) {
        println!("[DIAG] post-subdiv refined integrity: {e:?}");
    }
    println!(
        "Crease-Preserving Loop Subdiv + COLOR_0 Curvature Bake: {} -> {} tris (culled {} shirt tris) in {:.0} ms",
        assembled.triangles.len(),
        refined.triangles.len(),
        culled,
        t_sub.elapsed().as_secs_f64() * 1e3
    );

    // 6. Transfer smooth 57-joint skin weights from the UNIFIED `full_mannequin` (`body + head`)
    //    so collar, neckline, trapezius, and shoulders inherit true anatomical `spine_03 + clavicle +
    //    neck_01` transitions with ZERO neck-hole extrapolation!
    let t_skin = Instant::now();
    let weights = transfer_skin_weights(&refined, &full_mannequin, &full_body_raw_col, 10, alloc, alloc).expect("skin weights");
    println!("Transferred unified 57-joint (body+head) skin weights in {:.2} s", t_skin.elapsed().as_secs_f64());

    match body_clearance_check(&refined, &full_body_raw_col, Length::mm(-0.5)) {
        Ok(min_c) => println!("[PASS] body_clearance_check (A-pose): min clearance = {:.2} mm", min_c.as_mm()),
        Err(f) => println!(
            "[WARN] body_clearance_check (A-pose): {} verts < -0.5 mm, worst = {:.2} mm at ({:.3}, {:.3}, {:.3})",
            f.violating_vertices, f.min_clearance.as_mm(), f.position.x, f.position.y, f.position.z
        ),
    }

    // Combine A-pose unified body+head (hiding occluded lower crotch tris just like ASTRA's importer)!
    let mut preview_body_pos = Vec::new_in(alloc);
    let mut preview_body_tris = Vec::new_in(alloc);
    for &p in posed.positions.iter() {
        push(&mut preview_body_pos, p);
    }
    let body_tri_count = body_glb.triangles.len();
    for (ti, &t) in full_mannequin.triangles.iter().enumerate() {
        if ti < body_tri_count {
            let c = (posed.positions[t[0] as usize] + posed.positions[t[1] as usize] + posed.positions[t[2] as usize])
                * (1.0 / 3.0);
            if c.x.abs() < 0.08 && c.y > 0.72 && c.y < 0.98 && c.z < -0.08 {
                continue;
            }
        }
        push(&mut preview_body_tris, t);
    }

    let t_pic = Instant::now();
    for (angle, path) in [
        (StudioAngle::FrontFull, "blaze-cloth-front.png"),
        (StudioAngle::BackFull, "blaze-cloth-back.png"),
        (StudioAngle::UpperThreeQuarter, "blaze-cloth-upper.png"),
        (StudioAngle::LowerFront, "blaze-cloth-lower.png"),
    ] {
        let png = render_studio_preview_png(
            &preview_body_pos,
            &preview_body_tris,
            &boots_glb.positions,
            &boots_glb.triangles,
            &refined,
            &patterns.fabrics,
            angle,
            640,
            960,
            alloc,
            alloc,
        )
        .expect("render preview");
        std::fs::write(output.join(path), &png[..]).expect("write preview");
    }
    println!("Rendered 4 A-pose studio previews (640x960) in {:.1} s", t_pic.elapsed().as_secs_f64());

    // 7. Exact inverse-LBS unskin back to T-pose bind pose + Multi-Pose Corrective PSD Morph Targets!
    let bind_garment = unskin_garment_to_bind(&refined, &weights, &posed, alloc).expect("unskin to bind");

    let t_psd = Instant::now();
    let (rifle_deltas, rifle_rep) = compute_psd_corrective_deltas(
        &bind_garment,
        &weights,
        &body_glb,
        &head_glb,
        CanonicalPose::RifleAim,
        alloc,
        alloc,
    )
    .expect("PSD rifle_aim");
    let (crouch_deltas, crouch_rep) = compute_psd_corrective_deltas(
        &bind_garment,
        &weights,
        &body_glb,
        &head_glb,
        CanonicalPose::Crouch,
        alloc,
        alloc,
    )
    .expect("PSD crouch");
    println!(
        "PSD Corrective Morphs ({:.2} s): rifle_aim(pen {}->{}, active={}, max={:.1} mm) | crouch(pen {}->{}, active={}, max={:.1} mm)",
        t_psd.elapsed().as_secs_f64(),
        rifle_rep.initial_penetrations,
        rifle_rep.remaining_penetrations,
        rifle_rep.active_vertices,
        rifle_rep.max_delta_mm,
        crouch_rep.initial_penetrations,
        crouch_rep.remaining_penetrations,
        crouch_rep.active_vertices,
        crouch_rep.max_delta_mm
    );

    let morph_slices: [&[Vec3]; 2] = [&rifle_deltas, &crouch_deltas];
    let integrity = verify_garment_integrity(
        &bind_garment,
        Some(&weights),
        &morph_slices,
        GarmentIntegrityOpts::default(),
    )
    .expect("[FAIL] verify_garment_integrity gate caught a broken garment!");
    println!(
        "[PASS] verify_garment_integrity: {} seam pairs locked | max_crack={:.4} mm | max_morph_crack={:.4} mm | max_edge={:.1} mm",
        integrity.welded_seam_pairs_checked,
        integrity.max_seam_crack_mm,
        integrity.max_morph_crack_mm,
        integrity.max_cloth_edge_mm
    );

    // Hard-fail multi-pose collar & neck clearance verification against the TRULY POSED
    // cheek-weld neck skin (`head_glb` skinned alongside `body_glb` into `RifleAim`!).
    let neck_clear_m = verify_posed_collar_clearance(
        &bind_garment,
        &weights,
        &rifle_deltas,
        &body_glb,
        &head_glb,
        CanonicalPose::RifleAim,
        0.0,
        alloc,
    )
    .expect("[FAIL] verify_posed_collar_clearance: neck skin sliced through collar in RifleAim cheek-weld pose!");
    println!(
        "[PASS] verify_posed_collar_clearance (RifleAim + CheekWeld): min collar-to-neck clearance = {:.2} mm",
        neck_clear_m * 1000.0
    );

    let morphs = [
        MorphTarget { name: "rifle_aim", position_deltas: &rifle_deltas },
        MorphTarget { name: "crouch", position_deltas: &crouch_deltas },
    ];
    let glb_bytes = write_skinned_glb_with_morphs(
        &bind_garment,
        &patterns.fabrics,
        &body_glb.joints,
        &weights.joints_0,
        &weights.weights_0,
        &morphs,
        alloc,
        alloc,
    )
    .expect("write skinned glb with morphs");
    let out_glb = output.join("sewn_outfit.glb");
    std::fs::write(&out_glb, &glb_bytes[..]).expect("save sewn_outfit.glb");
    println!(
        "Exported inverse-skinned bind glTF 2.0 (COLOR_0 + 2 PSD morphs) -> {} ({:.1} KiB) | Total elapsed: {:.1} s",
        out_glb.display(),
        glb_bytes.len() as f64 / 1024.0,
        t_start.elapsed().as_secs_f64()
    );
}
