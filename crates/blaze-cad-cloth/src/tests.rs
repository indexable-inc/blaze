use blaze_cad_field::Scratch;
use blaze_math::Vec3;
use blaze_own::{Alloc, Vec};
use blaze_units::Length;

use crate::{
    CHEST_BEACON_DEFAULT, COBRA_BUCKLE_DEFAULT, COLLAR_CLASP_DEFAULT, Curve2D, Fabric, GarmentIntegrityFinding,
    GarmentIntegrityOpts, GarmentMesh, GrainPlant, InternalSeamSpec, IpcOpts, JointDef, KneeGatherOpts, LoadedGlb,
    MeshCollider, MorphTarget, PanelKind, PanelSpec, PatternSet, PinSpec, PoseSweepOpts, PoseSweepPlant, RemeshOpts,
    SeamEaseFinding, SeamSpec, SkinnedWeights, SolverOpts, StudioAngle, SubEdge, SurfacePatchSpec, SweepFailureKind,
    SweepMotion, UnifiedCollider, Wrap3D, adaptive_refine_cloth, append_cad_solid_to_garment, barrier_energy,
    barrier_grad, barrier_hess, body_clearance_check, build_chest_beacon_tape, build_cloth_mesh,
    build_cobra_buckle_tape, build_collar_clasp_tape, export_pattern_svg, inspect_garment_ray, measure_cloth_strain,
    measure_cloth_strain_planted, pose_sweep_check, pose_sweep_check_planted, project_ipc_barriers,
    read_glb_panel_metadata, read_glb_skeleton_mesh, relax_compression_zones, render_studio_preview_png,
    seam_ease_check, set_garment_texture_uv_transform, simulate_garment, subdivide_cloth_loop,
    verify_garment_integrity, write_skinned_glb, write_skinned_glb_with_morphs,
};

fn v1<'a>(alloc: Alloc<'a>, e: SubEdge) -> Vec<SubEdge, Alloc<'a>> {
    let mut v = Vec::new_in(alloc);
    v.try_push(e).unwrap();
    v
}

/// Build a simple 2-panel cylindrical sleeve tube wrapped around a cylinder collider to verify
/// sewing, seam ease check, topological welding, continuum membrane + bending stability,
/// determinism, studio preview PNG, and GLB round-trip!
fn make_test_tube_pattern<'a>(alloc: Alloc<'a>, mismatch_ease: bool) -> PatternSet<'a> {
    let mut ps = PatternSet::new(alloc);
    ps.fabrics.try_push(Fabric::COTTON_CANVAS).unwrap();
    ps.fabrics.try_push(Fabric::BALLISTIC_CORDURA).unwrap();

    let w_a = 0.2356;
    let w_b = if mismatch_ease { 0.36 } else { 0.2356 };
    let h = 0.30;

    // Front half-panel
    ps.panels
        .try_push(PanelSpec {
            name: "front",
            layer: 0,
            fabric_idx: 0,
            lining_fabric_idx: Some(1),
            bottom: v1(alloc, SubEdge::new("hem_f", Curve2D::line([-w_a * 0.5, 0.0], [w_a * 0.5, 0.0]), 8)),
            right: v1(alloc, SubEdge::new("side_r", Curve2D::line([w_a * 0.5, 0.0], [w_a * 0.5, h]), 10)),
            top: v1(alloc, SubEdge::new("top_f", Curve2D::line([-w_a * 0.5, h], [w_a * 0.5, h]), 8)),
            left: v1(alloc, SubEdge::new("side_l", Curve2D::line([-w_a * 0.5, 0.0], [-w_a * 0.5, h]), 10)),
            wrap: Wrap3D::VerticalCylinder {
                center_x: 0.0,
                center_z: 0.0,
                y_origin: 0.0,
                radius_x: 0.075,
                radius_z: 0.075,
                taper_x: 0.0,
                taper_z: 0.0,
                u_center: 0.0,
                angle_center: 0.0,
                arc_radius: 0.075,
            },
            flip_normal: true,
            ..
        })
        .ok()
        .unwrap();

    // Back half-panel
    ps.panels
        .try_push(PanelSpec {
            name: "back",
            layer: 0,
            fabric_idx: 0,
            lining_fabric_idx: Some(1),
            bottom: v1(alloc, SubEdge::new("hem_b", Curve2D::line([-w_b * 0.5, 0.0], [w_b * 0.5, 0.0]), 8)),
            right: v1(alloc, SubEdge::new("side_l", Curve2D::line([w_b * 0.5, 0.0], [w_b * 0.5, h]), 10)),
            top: v1(alloc, SubEdge::new("top_b", Curve2D::line([-w_b * 0.5, h], [w_b * 0.5, h]), 8)),
            left: v1(alloc, SubEdge::new("side_r", Curve2D::line([-w_b * 0.5, 0.0], [-w_b * 0.5, h]), 10)),
            wrap: Wrap3D::VerticalCylinder {
                center_x: 0.0,
                center_z: 0.0,
                y_origin: 0.0,
                radius_x: 0.075,
                radius_z: 0.075,
                taper_x: 0.0,
                taper_z: 0.0,
                u_center: 0.0,
                angle_center: core::f64::consts::PI,
                arc_radius: 0.075,
            },
            flip_normal: true,
            ..
        })
        .ok()
        .unwrap();

    // Sew left and right side seams (and check ease on top/side seams).
    let mut s_r = SeamSpec::flat("seam_r", "front", "side_r", "back", "side_r");
    s_r.max_ease = 0.05;
    let mut s_l = SeamSpec::flat("seam_l", "front", "side_l", "back", "side_l");
    s_l.max_ease = 0.05;
    ps.seams.try_push(s_r).unwrap();
    ps.seams.try_push(s_l).unwrap();

    if mismatch_ease {
        let mut bad = SeamSpec::flat("seam_top_bad", "front", "top_f", "back", "top_b");
        bad.max_ease = 0.10;
        ps.seams.try_push(bad).unwrap();
    }

    ps.pins.try_push(PinSpec { panel: "front", edge: "top_f", stiffness: 0.9, sewing_only: false }).unwrap();
    ps.pins.try_push(PinSpec { panel: "back", edge: "top_b", stiffness: 0.9, sewing_only: false }).unwrap();
    ps.patches
        .try_push(SurfacePatchSpec {
            name: "test_pocket",
            panel: "front",
            s_min: 0.25,
            s_max: 0.75,
            t_min: 0.30,
            t_max: 0.70,
            depth: Length::mm(10.0),
            crown: Length::mm(3.0),
            edge_bevel: 0.2,
            flap_fraction: 0.30,
            bottom_chamfer: 0.15,
            fabric_idx: 0,
            flap_fabric_idx: Some(1),
            box_pleat: true,
        })
        .ok()
        .unwrap();
    ps
}

fn make_cylinder_mesh<'a>(radius: f64, y0: f64, y1: f64, alloc: Alloc<'a>) -> (Vec<Vec3, Alloc<'a>>, Vec<[u32; 3], Alloc<'a>>) {
    let segs = 24usize;
    let rings = 12usize;
    let mut pos = Vec::new_in(alloc);
    let mut tris = Vec::new_in(alloc);
    for j in 0..=rings {
        let y = y0 + (y1 - y0) * (j as f64) / (rings as f64);
        for i in 0..segs {
            let a = (i as f64) * core::f64::consts::TAU / (segs as f64);
            pos.try_push(Vec3::new(radius * blaze_math::sin(a), y, -radius * blaze_math::cos(a))).unwrap();
        }
    }
    for j in 0..rings {
        for i in 0..segs {
            let i1 = (i + 1) % segs;
            let v00 = (j * segs + i) as u32;
            let v10 = (j * segs + i1) as u32;
            let v01 = ((j + 1) * segs + i) as u32;
            let v11 = ((j + 1) * segs + i1) as u32;
            tris.try_push([v00, v11, v10]).unwrap();
            tris.try_push([v00, v01, v11]).unwrap();
        }
    }
    (pos, tris)
}

#[test]
fn seam_ease_check_catches_mismatch() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let good = make_test_tube_pattern(alloc, false);
    assert_eq!(seam_ease_check(&good), Ok(()));

    let bad = make_test_tube_pattern(alloc, true);
    let res = seam_ease_check(&bad);
    assert!(matches!(res, Err(SeamEaseFinding::ExcessiveEase { seam: "seam_top_bad", .. })), "got {res:?}");
}

#[test]
fn simulate_weld_clearance_determinism_glb_and_preview() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();

    let patterns = make_test_tube_pattern(alloc, false);
    assert_eq!(seam_ease_check(&patterns), Ok(()));

    let svg = export_pattern_svg(&patterns, alloc).unwrap();
    let svg_str = core::str::from_utf8(&svg).unwrap();
    assert!(svg_str.contains("<svg") && svg_str.contains("front") && svg_str.contains("back"));

    let (cyl_p, cyl_t) = make_cylinder_mesh(0.062, -0.05, 0.35, alloc);
    let cyl_col = MeshCollider::build(&cyl_p, &cyl_t, false, alloc).unwrap();
    let meshes = [&cyl_col];
    let offsets = [0.001];
    let unified = UnifiedCollider { meshes: &meshes, cad_tapes: &[], mesh_offsets: &offsets };

    let opts = SolverOpts { sewing_frames: 20, settle_frames: 25, substeps: 3, iterations: 3, .. };

    // Run twice to prove bit-for-bit determinism!
    let mut mesh1 = build_cloth_mesh(&patterns, Some(0), alloc).unwrap();
    let mut cad_scratch = Scratch::new(alloc);
    let rep1 = simulate_garment(&mut mesh1, &patterns, &unified, opts, alloc, &mut cad_scratch).unwrap();

    let mut mesh2 = build_cloth_mesh(&patterns, Some(0), alloc).unwrap();
    let rep2 = simulate_garment(&mut mesh2, &patterns, &unified, opts, alloc, &mut cad_scratch).unwrap();

    assert_eq!(rep1, rep2, "simulation must be bit-for-bit deterministic");
    assert!(rep1.welded_pairs >= 20, "expected welded seam pairs, got {}", rep1.welded_pairs);
    assert!(rep1.max_pre_weld_gap.as_mm() < 8.0, "seams should pull close before welding: {:?}", rep1.max_pre_weld_gap);
    for i in 0..mesh1.pos.len() {
        assert_eq!(mesh1.pos[i].x.to_bits(), mesh2.pos[i].x.to_bits());
        assert_eq!(mesh1.pos[i].y.to_bits(), mesh2.pos[i].y.to_bits());
        assert_eq!(mesh1.pos[i].z.to_bits(), mesh2.pos[i].z.to_bits());
    }

    // Post-drape tailoring + rim solidify + 3D pocket.
    let mut garment = GarmentMesh::new(alloc);
    garment.append_draped_layer(&mesh1, &patterns, 2, alloc).unwrap();
    garment.recompute_smooth_normals();

    // Verify zero body penetration against raw cylinder!
    let clear = body_clearance_check(&garment, &cyl_col, Length::mm(-0.5));
    assert!(clear.is_ok(), "garment penetrated body collider: {clear:?}");

    // Export deterministic GLB and read it back via our own sans-IO reader!
    let glb1 = write_skinned_glb(&garment, &patterns.fabrics, &[], &[], &[], alloc, alloc).unwrap();
    let glb2 = write_skinned_glb(&garment, &patterns.fabrics, &[], &[], &[], alloc, alloc).unwrap();
    assert_eq!(glb1.as_slice(), glb2.as_slice(), "GLB export must be byte-identical");

    let parsed = read_glb_skeleton_mesh(&glb1, Some(0), alloc, alloc).unwrap();
    assert!(parsed.positions.len() >= garment.positions.len());
    assert_eq!(parsed.triangles.len(), garment.triangles.len());

    // Render deterministic studio preview PNG via blaze-cad-picture!
    let png_bytes = render_studio_preview_png(
        &cyl_p,
        &cyl_t,
        &[],
        &[],
        &garment,
        &patterns.fabrics,
        StudioAngle::FrontFull,
        160,
        240,
        alloc,
        alloc,
    )
    .unwrap();
    assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
}

#[test]
fn c_ipc_barrier_math_and_vt_repulsion() {
    let d_hat = 0.004;
    // Barrier energy, gradient, Hessian sanity + C2 smoothness at d_hat.
    assert_eq!(barrier_energy(d_hat, d_hat), 0.0);
    assert_eq!(barrier_energy(d_hat * 2.0, d_hat), 0.0);
    assert!(barrier_energy(0.001, d_hat) > barrier_energy(0.002, d_hat));
    assert!(barrier_grad(0.001, d_hat) < 0.0);
    assert!(barrier_hess(0.001, d_hat) > 0.0);

    // Two parallel cloth sheets separated by 1.2 mm (< d_hat = 4.0 mm) must repel apart!
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let mut ps = PatternSet::new(alloc);
    ps.fabrics.try_push(Fabric::COTTON_CANVAS).unwrap();
    for (name, z_off) in [("sheet_a", 0.0), ("sheet_b", 0.0012)] {
        ps.panels
            .try_push(PanelSpec {
                name,
                layer: 0,
                fabric_idx: 0,
                lining_fabric_idx: None,
                bottom: v1(alloc, SubEdge::new("b", Curve2D::line([-0.05, 0.0], [0.05, 0.0]), 4)),
                right: v1(alloc, SubEdge::new("r", Curve2D::line([0.05, 0.0], [0.05, 0.10]), 4)),
                top: v1(alloc, SubEdge::new("t", Curve2D::line([-0.05, 0.10], [0.05, 0.10]), 4)),
                left: v1(alloc, SubEdge::new("l", Curve2D::line([-0.05, 0.0], [-0.05, 0.10]), 4)),
                wrap: Wrap3D::Planar {
                    origin: Vec3::new(0.0, 0.0, z_off),
                    u_axis: Vec3::new(1.0, 0.0, 0.0),
                    v_axis: Vec3::new(0.0, 1.0, 0.0),
                },
                flip_normal: false,
                ..
            })
            .ok()
            .unwrap();
    }
    let mut mesh = build_cloth_mesh(&ps, Some(0), alloc).unwrap();
    let mut prev = Vec::new_in(alloc);
    for &p in mesh.pos.iter() {
        prev.try_push(p).unwrap();
    }
    let stats = project_ipc_barriers(&mut mesh, &prev, IpcOpts { d_hat: Length::mm(4.0), .. }, alloc).unwrap();
    assert!(stats.vt_contacts > 0, "expected active VT barrier contacts, got {stats:?}");
    let z_a = mesh.pos[0].z;
    let z_b = mesh.pos[mesh.panels[1].vert_start as usize].z;
    assert!((z_b - z_a).abs() > 0.0012, "sheets should separate under C-IPC barrier: z_a={z_a}, z_b={z_b}");
}

#[test]
fn adaptive_remesh_and_internal_seam_and_cad_gear_and_morph_glb() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();

    // 1. Pattern with a hanging flap attached via InternalSeamSpec + adaptive remeshing.
    let mut ps = make_test_tube_pattern(alloc, false);
    ps.panels
        .try_push(PanelSpec {
            name: "flap",
            layer: 0,
            fabric_idx: 1,
            lining_fabric_idx: None,
            bottom: v1(alloc, SubEdge::new("fb", Curve2D::line([-0.04, 0.0], [0.04, 0.0]), 4)),
            right: v1(alloc, SubEdge::new("fr", Curve2D::line([0.04, 0.0], [0.04, 0.05]), 2)),
            top: v1(alloc, SubEdge::new("ft", Curve2D::line([-0.04, 0.05], [0.04, 0.05]), 4)),
            left: v1(alloc, SubEdge::new("fl", Curve2D::line([-0.04, 0.0], [-0.04, 0.05]), 2)),
            wrap: Wrap3D::VerticalCylinder {
                center_x: 0.0,
                center_z: 0.0,
                y_origin: 0.14,
                radius_x: 0.082,
                radius_z: 0.082,
                taper_x: 0.0,
                taper_z: 0.0,
                u_center: 0.0,
                angle_center: 0.0,
                arc_radius: 0.082,
            },
            flip_normal: true,
            ..
        })
        .ok()
        .unwrap();
    ps.internal_seams
        .try_push(InternalSeamSpec {
            name: "flap_attach",
            sub_panel: "flap",
            sub_edge: "ft",
            host_panel: "front",
            host_s0: 0.30,
            host_t0: 0.65,
            host_s1: 0.70,
            host_t1: 0.65,
            reversed: false,
            normal_offset: Length::mm(2.0),
            fold_angle_rad: 0.0,
            topstitch_depth: Length::mm(0.8),
        })
        .ok()
        .unwrap();

    let mut mesh = build_cloth_mesh(&ps, Some(0), alloc).unwrap();
    assert_eq!(mesh.internal_stitches.len(), 5);

    // Induce a sharp bend in the front panel and verify ARCSim adaptive refinement splits & flips!
    for i in 0..mesh.pos.len() {
        if mesh.vert_panel[i] == 0 && mesh.panel_st[i].y > 0.45 && mesh.panel_st[i].y < 0.55 {
            mesh.pos[i].z -= 0.014;
        }
    }
    let rem_rep = adaptive_refine_cloth(&mut mesh, &ps, RemeshOpts { max_splits: 32, .. }, alloc).unwrap();
    assert!(rem_rep.edges_split > 0, "expected adaptive splits on creased cloth: {rem_rep:?}");
    assert!(rem_rep.new_verts > 0 && rem_rep.new_tris > 0);
    // Verify EVERY refined triangle preserves its panel's exact 2D winding sign (zero inverted tris!).
    for (ti, &[a, b, c]) in mesh.triangles.iter().enumerate() {
        let ua = mesh.rest_uv_m[a as usize];
        let ub = mesh.rest_uv_m[b as usize];
        let uc = mesh.rest_uv_m[c as usize];
        let det = (ub - ua).cross(uc - ua);
        let expect_neg = mesh.panels[mesh.tri_panel[ti] as usize].flip_normal;
        if expect_neg {
            assert!(det < -1e-10, "inverted refined tri {ti} on flipped panel: det={det}");
        } else {
            assert!(det > 1e-10, "inverted refined tri {ti} on normal panel: det={det}");
        }
    }

    // 2. Direct blaze-cad-field tactical hardware -> GarmentMesh + COLOR_0 curvature baking.
    let mut garment = GarmentMesh::new(alloc);
    garment.append_draped_layer(&mesh, &ps, 2, alloc).unwrap();
    let buckle = build_cobra_buckle_tape(
        COBRA_BUCKLE_DEFAULT[0],
        COBRA_BUCKLE_DEFAULT[1],
        COBRA_BUCKLE_DEFAULT[2],
        alloc,
        alloc,
    )
    .unwrap();
    let beacon = build_chest_beacon_tape(
        CHEST_BEACON_DEFAULT[0],
        CHEST_BEACON_DEFAULT[1],
        CHEST_BEACON_DEFAULT[2],
        alloc,
        alloc,
    )
    .unwrap();
    let clasp = build_collar_clasp_tape(
        COLLAR_CLASP_DEFAULT[0],
        COLLAR_CLASP_DEFAULT[1],
        COLLAR_CLASP_DEFAULT[2],
        alloc,
        alloc,
    )
    .unwrap();

    let mut added_tris = 0u32;
    for (tape, y) in [(&buckle, 0.10), (&beacon, 0.20), (&clasp, 0.28)] {
        added_tris += append_cad_solid_to_garment(
            tape,
            1,
            Vec3::new(0.0, y, -0.09),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(0.0, 0.0, -1.0),
            Vec3::new(0.0, y, -0.08),
            Length::mm(1.2),
            &mut garment,
            alloc,
        )
        .unwrap();
    }
    assert!(added_tris > 100, "expected manifold CAD gear triangles, got {added_tris}");
    assert_eq!(garment.colors.len(), garment.positions.len());

    garment.recompute_smooth_normals();
    garment.bake_curvature_wear_and_ao(alloc).unwrap();

    // 3. glTF 2.0 export with COLOR_0 + dual PSD morph targets (deterministic round-trip!).
    let mut d_rifle = Vec::new_in(alloc);
    let mut d_crouch = Vec::new_in(alloc);
    for i in 0..garment.positions.len() {
        d_rifle.try_push(Vec3::new(0.001, (i % 3) as f64 * 0.0005, 0.0)).unwrap();
        d_crouch.try_push(Vec3::new(0.0, -0.001, 0.0005)).unwrap();
    }
    let morphs = [
        MorphTarget { name: "rifle_aim", position_deltas: &d_rifle },
        MorphTarget { name: "crouch", position_deltas: &d_crouch },
    ];
    let glb1 = write_skinned_glb_with_morphs(&garment, &ps.fabrics, &[], &[], &[], &morphs, alloc, alloc).unwrap();
    let glb2 = write_skinned_glb_with_morphs(&garment, &ps.fabrics, &[], &[], &[], &morphs, alloc, alloc).unwrap();
    assert_eq!(glb1.as_slice(), glb2.as_slice(), "morph GLB export must be byte-identical");
    // Verify JSON chunk advertises COLOR_0, targets, and targetNames!
    let json_len = u32::from_le_bytes([glb1[12], glb1[13], glb1[14], glb1[15]]) as usize;
    let json_str = core::str::from_utf8(&glb1[20..20 + json_len]).unwrap();
    assert!(json_str.contains("\"COLOR_0\""), "missing COLOR_0 in GLB JSON");
    assert!(json_str.contains("\"targets\""), "missing morph targets in GLB JSON");
    assert!(json_str.contains("\"rifle_aim\"") && json_str.contains("\"crouch\""), "missing targetNames");
}

#[test]
fn garment_integrity_passes_subdivided_seams_and_catches_planted_tears_and_spikes() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();

    let patterns = make_test_tube_pattern(alloc, false);
    let (cyl_p, cyl_t) = make_cylinder_mesh(0.062, -0.05, 0.35, alloc);
    let cyl_col = MeshCollider::build(&cyl_p, &cyl_t, false, alloc).unwrap();
    let meshes = [&cyl_col];
    let offsets = [0.001];
    let unified = UnifiedCollider { meshes: &meshes, cad_tapes: &[], mesh_offsets: &offsets };
    let opts = SolverOpts { sewing_frames: 18, settle_frames: 20, substeps: 3, iterations: 3, .. };

    let mut cloth = build_cloth_mesh(&patterns, Some(0), alloc).unwrap();
    let mut cad_scratch = Scratch::new(alloc);
    simulate_garment(&mut cloth, &patterns, &unified, opts, alloc, &mut cad_scratch).unwrap();

    let mut garment = GarmentMesh::new(alloc);
    garment.append_draped_layer(&cloth, &patterns, 1, alloc).unwrap();
    let mut subdiv = subdivide_cloth_loop(&garment, 2, 0.72, alloc, alloc).unwrap();

    // 1. Subdivided UV-split welded garment MUST pass with 0.000 mm seam crack!
    let rep = verify_garment_integrity(&subdiv, None, &[], GarmentIntegrityOpts::default()).unwrap();
    assert!(rep.welded_seam_pairs_checked > 0, "expected checked welded seam pairs");
    assert!(rep.max_seam_crack_mm < 1e-6, "expected bit-exact 0.000 mm seam crack, got {}", rep.max_seam_crack_mm);

    // 2. Plant a 1.5 mm seam tear -> MUST FAIL with SeamSplitCrack!
    let twin = (0..subdiv.positions.len()).find(|&i| subdiv.weld_of(i as u32) as usize != i).unwrap();
    let orig_p = subdiv.positions[twin];
    subdiv.positions[twin] = orig_p + Vec3::new(0.0015, 0.0, 0.0);
    let err_crack = verify_garment_integrity(&subdiv, None, &[], GarmentIntegrityOpts::default());
    assert!(matches!(err_crack, Err(GarmentIntegrityFinding::SeamSplitCrack { .. })), "expected SeamSplitCrack, got {err_crack:?}");
    subdiv.positions[twin] = orig_p;

    // 3. Plant a 2.0 mm morph-target seam split -> MUST FAIL with SeamMorphSplit!
    let mut bad_deltas = Vec::new_in(alloc);
    for _ in 0..subdiv.positions.len() {
        bad_deltas.try_push(Vec3::ZERO).unwrap();
    }
    bad_deltas[twin] = Vec3::new(0.0, 0.002, 0.0);
    let err_morph = verify_garment_integrity(&subdiv, None, &[&bad_deltas], GarmentIntegrityOpts::default());
    assert!(matches!(err_morph, Err(GarmentIntegrityFinding::SeamMorphSplit { .. })), "expected SeamMorphSplit, got {err_morph:?}");

    // 4. Plant a 140 mm stray vertex spike -> MUST FAIL with MeshSpikeDetected!
    let root = subdiv.weld_of(twin as u32) as usize;
    subdiv.positions[root] = subdiv.positions[root] + Vec3::new(0.0, -0.14, 0.0);
    subdiv.positions[twin] = subdiv.positions[root];
    let err_spike = verify_garment_integrity(&subdiv, None, &[], GarmentIntegrityOpts::default());
    assert!(matches!(err_spike, Err(GarmentIntegrityFinding::MeshSpikeDetected { .. })), "expected MeshSpikeDetected, got {err_spike:?}");
}

/// Build a synthetic rigged mannequin GLB with standard MetaHuman joint names and exact bind-pose
/// inverseBindMatrices so pose-sweep tests exercise real articulated LBS transforms.
fn make_rigged_test_mannequin<'a>(radius: f64, y0: f64, y1: f64, alloc: Alloc<'a>) -> LoadedGlb<'a> {
    let (positions, triangles) = make_cylinder_mesh(radius, y0, y1, alloc);

    let joint_specs: [(&str, Option<u32>, [f64; 3]); 12] = [
        ("pelvis", None, [0.0, 0.0, 0.0]),
        ("spine_01", Some(0), [0.0, 0.10, 0.0]),
        ("spine_02", Some(1), [0.0, 0.10, 0.0]),
        ("clavicle_l", Some(2), [-0.04, 0.08, 0.0]),
        ("upperarm_l", Some(3), [-0.10, 0.0, 0.0]),
        ("lowerarm_l", Some(4), [-0.22, 0.0, 0.0]),
        ("clavicle_r", Some(2), [0.04, 0.08, 0.0]),
        ("upperarm_r", Some(6), [0.10, 0.0, 0.0]),
        ("lowerarm_r", Some(7), [0.22, 0.0, 0.0]),
        ("thigh_l", Some(0), [-0.08, -0.04, 0.0]),
        ("calf_l", Some(9), [0.0, -0.35, 0.0]),
        ("foot_l", Some(10), [0.0, -0.35, 0.0]),
    ];

    let mut world_pos = [[0.0f64; 3]; 12];
    let mut joints = Vec::new_in(alloc);
    for (idx, &(jname, parent, trans)) in joint_specs.iter().enumerate() {
        let wp = match parent {
            Some(p) => {
                let pp = world_pos[p as usize];
                [pp[0] + trans[0], pp[1] + trans[1], pp[2] + trans[2]]
            }
            None => trans,
        };
        world_pos[idx] = wp;
        let mut name = Vec::new_in(alloc);
        for &b in jname.as_bytes() {
            name.try_push(b).unwrap();
        }
        let ibm = [
            1.0, 0.0, 0.0, 0.0,
            0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            -(wp[0] as f32), -(wp[1] as f32), -(wp[2] as f32), 1.0,
        ];
        joints
            .try_push(JointDef {
                name,
                parent,
                translation: trans,
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0, 1.0, 1.0],
                inverse_bind: ibm,
            })
            .ok()
            .unwrap();
    }

    // Bind all torso cylinder vertices to `spine_01` (index 1) so Sitting/Crouching/DeepSquat
    // articulate the mannequin cleanly without tearing the synthetic cylinder.
    let mut joints_0 = Vec::new_in(alloc);
    let mut weights_0 = Vec::new_in(alloc);
    for _ in 0..positions.len() {
        joints_0.try_push([1, 0, 0, 0]).unwrap();
        weights_0.try_push([1.0, 0.0, 0.0, 0.0]).unwrap();
    }

    LoadedGlb {
        positions,
        triangles,
        joints_0,
        weights_0,
        joints,
    }
}

/// Build a 2-layer front garment (layer 0 inner undershirt + layer 2 outer jacket + chest pocket).
fn make_layered_front_garment<'a>(alloc: Alloc<'a>) -> (PatternSet<'a>, GarmentMesh<'a>) {
    let mut ps = PatternSet::new(alloc);
    ps.fabrics.try_push(Fabric::COTTON_CANVAS).unwrap(); // 0: mat_jacket
    ps.fabrics.try_push(Fabric::BALLISTIC_CORDURA).unwrap(); // 1: mat_cordura
    ps.fabrics.try_push(Fabric::RIB_JERSEY).unwrap(); // 2: mat_undershirt

    let hw = 0.06;
    let h = 0.28;

    // Layer 0: inner lining / undershirt front panel at r = 0.072 m
    ps.panels
        .try_push(PanelSpec {
            name: "undershirt_front",
            layer: 0,
            fabric_idx: 2,
            lining_fabric_idx: None,
            bottom: v1(alloc, SubEdge::new("u_b", Curve2D::line([-hw, 0.0], [hw, 0.0]), 8)),
            right: v1(alloc, SubEdge::new("u_r", Curve2D::line([hw, 0.0], [hw, h]), 8)),
            top: v1(alloc, SubEdge::new("u_t", Curve2D::line([-hw, h], [hw, h]), 8)),
            left: v1(alloc, SubEdge::new("u_l", Curve2D::line([-hw, 0.0], [-hw, h]), 8)),
            wrap: Wrap3D::VerticalCylinder {
                center_x: 0.0,
                center_z: 0.0,
                y_origin: 0.02,
                radius_x: 0.072,
                radius_z: 0.072,
                taper_x: 0.0,
                taper_z: 0.0,
                u_center: 0.0,
                angle_center: 0.0,
                arc_radius: 0.072,
            },
            flip_normal: true,
            ..
        })
        .ok()
        .unwrap();

    // Layer 2: outer jacket front panel at r = 0.082 m (10 mm outside undershirt)
    ps.panels
        .try_push(PanelSpec {
            name: "jacket_front",
            layer: 2,
            fabric_idx: 0,
            lining_fabric_idx: Some(1),
            bottom: v1(alloc, SubEdge::new("j_b", Curve2D::line([-hw, 0.0], [hw, 0.0]), 8)),
            right: v1(alloc, SubEdge::new("j_r", Curve2D::line([hw, 0.0], [hw, h]), 8)),
            top: v1(alloc, SubEdge::new("j_t", Curve2D::line([-hw, h], [hw, h]), 8)),
            left: v1(alloc, SubEdge::new("j_l", Curve2D::line([-hw, 0.0], [-hw, h]), 8)),
            wrap: Wrap3D::VerticalCylinder {
                center_x: 0.0,
                center_z: 0.0,
                y_origin: 0.02,
                radius_x: 0.082,
                radius_z: 0.082,
                taper_x: 0.0,
                taper_z: 0.0,
                u_center: 0.0,
                angle_center: 0.0,
                arc_radius: 0.082,
            },
            flip_normal: true,
            ..
        })
        .ok()
        .unwrap();

    // Attached chest cargo pocket on `jacket_front` (upper half: t in [0.40, 0.78])
    ps.patches
        .try_push(SurfacePatchSpec {
            name: "chest_cargo_pocket",
            panel: "jacket_front",
            s_min: 0.25,
            s_max: 0.75,
            t_min: 0.40,
            t_max: 0.78,
            depth: Length::mm(8.0),
            crown: Length::mm(1.5),
            edge_bevel: 0.15,
            flap_fraction: 0.0,
            bottom_chamfer: 0.0,
            fabric_idx: 1,
            flap_fabric_idx: None,
            box_pleat: false,
        })
        .ok()
        .unwrap();

    let cloth_inner = build_cloth_mesh(&ps, Some(0), alloc).unwrap();
    let cloth_outer = build_cloth_mesh(&ps, Some(2), alloc).unwrap();

    let mut garment = GarmentMesh::new(alloc);
    garment.append_draped_layer(&cloth_inner, &ps, 0, alloc).unwrap();
    garment.append_draped_layer(&cloth_outer, &ps, 0, alloc).unwrap();
    garment.recompute_smooth_normals();
    (ps, garment)
}

#[test]
fn pose_sweep_flags_first_lining_breakthrough_and_pocket_pull_away_and_catches_plants() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();

    let body = make_rigged_test_mannequin(0.060, 0.0, 0.32, alloc);
    let (_ps, garment) = make_layered_front_garment(alloc);

    let mut j0 = Vec::new_in(alloc);
    let mut w0 = Vec::new_in(alloc);
    for _ in 0..garment.positions.len() {
        j0.try_push([1, 0, 0, 0]).unwrap();
        w0.try_push([1.0, 0.0, 0.0, 0.0]).unwrap();
    }
    let weights = SkinnedWeights { joints_0: j0, weights_0: w0 };

    let inner_pid = garment.panels.iter().find(|p| p.name == "undershirt_front").unwrap().id;
    let outer_pid = garment.panels.iter().find(|p| p.name == "jacket_front").unwrap().id;
    let pocket_pid = garment.panels.iter().find(|p| p.name == "chest_cargo_pocket").unwrap().id;

    // 1. Clean layered garment MUST pass all 5 articulated motions!
    let motions = [
        SweepMotion::Sitting,
        SweepMotion::Reaching,
        SweepMotion::Walking,
        SweepMotion::Crouching,
        SweepMotion::DeepSquat,
    ];
    let clean = pose_sweep_check(
        &garment,
        &weights,
        &body,
        &[],
        PoseSweepOpts {
            motions: &motions,
            frames_per_motion: 8,
            alloc,
            scratch: alloc,
            ..
        },
    )
    .unwrap();
    assert!(clean.passed, "clean garment failed sweep: {:?}", clean.first_failure);
    assert!(clean.first_failure.is_none());
    assert_eq!(clean.summaries.len(), 5);
    assert!(clean.summaries.iter().all(|s| s.passed && s.frames_checked == 8));

    // Helper: mark vertices owned by a given panel ID.
    let mut is_inner_vert = Vec::new_in(alloc);
    let mut is_pocket_vert = Vec::new_in(alloc);
    for _ in 0..garment.positions.len() {
        is_inner_vert.try_push(false).unwrap();
        is_pocket_vert.try_push(false).unwrap();
    }
    for (ti, &[a, b, c]) in garment.triangles.iter().enumerate() {
        if garment.tri_panel[ti] == inner_pid {
            for v in [a as usize, b as usize, c as usize] {
                is_inner_vert[v] = true;
            }
        }
        if garment.tri_panel[ti] == pocket_pid {
            for v in [a as usize, b as usize, c as usize] {
                is_pocket_vert[v] = true;
            }
        }
    }

    // 2. Planted lining breakthrough: inner undershirt pushes outward -Z by 24 mm * phase
    //    (10 mm gap -> frame 3 = 9.0 mm still inside, frame 4 = 12.0 mm -> +2.0 mm breakthrough!).
    let mut bt_deltas = Vec::new_in(alloc);
    for i in 0..garment.positions.len() {
        let d = if is_inner_vert[i] { Vec3::new(0.0, 0.0, -0.024) } else { Vec3::ZERO };
        bt_deltas.try_push(d).unwrap();
    }
    let rep_bt = pose_sweep_check(
        &garment,
        &weights,
        &body,
        &[(&bt_deltas, 1.0)],
        PoseSweepOpts {
            motions: &[SweepMotion::Sitting],
            frames_per_motion: 8,
            alloc,
            scratch: alloc,
            ..
        },
    )
    .unwrap();
    assert!(!rep_bt.passed, "expected LayerBreakthrough failure");
    let hit_bt = rep_bt.first_failure.expect("first breakthrough hit");
    assert_eq!(hit_bt.motion, SweepMotion::Sitting);
    assert_eq!(hit_bt.frame, 4, "expected exact first failing frame 4, got {hit_bt:?}");
    match hit_bt.kind {
        SweepFailureKind::LayerBreakthrough {
            inner_layer,
            outer_layer,
            inner_panel,
            outer_panel,
            breakthrough,
            ..
        } => {
            assert_eq!(inner_layer, 0);
            assert_eq!(outer_layer, 2);
            assert_eq!(inner_panel, inner_pid);
            assert_eq!(outer_panel, outer_pid);
            assert!(breakthrough.as_mm() > 0.5, "breakthrough={:?}", breakthrough);
        }
        other => panic!("expected LayerBreakthrough, got {other:?}"),
    }
    // Plant control: IgnoreLayerBreakthrough MUST miss this planted breakthrough!
    let planted_bt = pose_sweep_check_planted(
        &garment,
        &weights,
        &body,
        &[(&bt_deltas, 1.0)],
        PoseSweepOpts {
            motions: &[SweepMotion::Sitting],
            frames_per_motion: 8,
            alloc,
            scratch: alloc,
            ..
        },
        PoseSweepPlant::IgnoreLayerBreakthrough,
    )
    .unwrap();
    assert!(planted_bt.passed, "IgnoreLayerBreakthrough control should mask breakthrough");

    // 3. Planted pocket pull-away: pocket pulls outward -Z by 9.6 mm * phase
    //    (max_pocket_pull_away = 2.5 mm -> frame 2 = 2.4 mm clean, frame 3 = 3.6 mm fails!).
    let mut pk_deltas = Vec::new_in(alloc);
    for i in 0..garment.positions.len() {
        let d = if is_pocket_vert[i] { Vec3::new(0.0, 0.0, -0.0096) } else { Vec3::ZERO };
        pk_deltas.try_push(d).unwrap();
    }
    let rep_pk = pose_sweep_check(
        &garment,
        &weights,
        &body,
        &[(&pk_deltas, 1.0)],
        PoseSweepOpts {
            motions: &[SweepMotion::Reaching],
            frames_per_motion: 8,
            alloc,
            scratch: alloc,
            ..
        },
    )
    .unwrap();
    assert!(!rep_pk.passed, "expected PocketPullAway failure");
    let hit_pk = rep_pk.first_failure.expect("first pocket pull-away hit");
    assert_eq!(hit_pk.motion, SweepMotion::Reaching);
    assert_eq!(hit_pk.frame, 3, "expected exact first failing frame 3, got {hit_pk:?}");
    match hit_pk.kind {
        SweepFailureKind::PocketPullAway {
            gap,
            rest_gap,
            pocket_panel,
            host_panel,
            ..
        } => {
            assert_eq!(pocket_panel, pocket_pid);
            assert_eq!(host_panel, outer_pid);
            assert!((gap.as_mm() - rest_gap.as_mm()) > 2.5, "gap={gap:?} rest={rest_gap:?}");
        }
        other => panic!("expected PocketPullAway, got {other:?}"),
    }
    // Plant control: IgnorePocketPullAway MUST miss this planted pull-away!
    let planted_pk = pose_sweep_check_planted(
        &garment,
        &weights,
        &body,
        &[(&pk_deltas, 1.0)],
        PoseSweepOpts {
            motions: &[SweepMotion::Reaching],
            frames_per_motion: 8,
            alloc,
            scratch: alloc,
            ..
        },
        PoseSweepPlant::IgnorePocketPullAway,
    )
    .unwrap();
    assert!(planted_pk.passed, "IgnorePocketPullAway control should mask pull-away");

    // 4. Planted body penetration: inner undershirt pushes inward +Z by 28 mm * phase
    //    (12 mm clearance -> frame 3 = 10.5 mm clean, frame 4 = 14.0 mm -> 2.0 mm penetration!).
    let mut pen_deltas = Vec::new_in(alloc);
    for i in 0..garment.positions.len() {
        let d = if is_inner_vert[i] { Vec3::new(0.0, 0.0, 0.028) } else { Vec3::ZERO };
        pen_deltas.try_push(d).unwrap();
    }
    let rep_pen = pose_sweep_check(
        &garment,
        &weights,
        &body,
        &[(&pen_deltas, 1.0)],
        PoseSweepOpts {
            motions: &[SweepMotion::Crouching],
            frames_per_motion: 8,
            alloc,
            scratch: alloc,
            ..
        },
    )
    .unwrap();
    assert!(!rep_pen.passed, "expected BodyPenetration failure");
    let hit_pen = rep_pen.first_failure.expect("first penetration hit");
    assert_eq!(hit_pen.motion, SweepMotion::Crouching);
    assert_eq!(hit_pen.frame, 4, "expected exact first failing frame 4, got {hit_pen:?}");
    assert!(matches!(
        hit_pen.kind,
        SweepFailureKind::BodyPenetration { panel_id, .. } if panel_id == inner_pid
    ));
    // Plant control: IgnoreBodyPenetration MUST miss this planted penetration!
    let planted_pen = pose_sweep_check_planted(
        &garment,
        &weights,
        &body,
        &[(&pen_deltas, 1.0)],
        PoseSweepOpts {
            motions: &[SweepMotion::Crouching],
            frames_per_motion: 8,
            alloc,
            scratch: alloc,
            ..
        },
        PoseSweepPlant::IgnoreBodyPenetration,
    )
    .unwrap();
    assert!(planted_pen.passed, "IgnoreBodyPenetration control should mask body penetration");
}

#[test]
fn named_panels_ray_click_inspector_and_glb_round_trip() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();

    let (ps, garment) = make_layered_front_garment(alloc);

    // 1. Click below the pocket (y = 0.08): hits outer `jacket_front` (layer 2) first, then
    //    `undershirt_front` (layer 0) ~10 mm underneath -> no layer inversion.
    let click_clean = inspect_garment_ray(
        &garment,
        None,
        &ps.fabrics,
        Vec3::new(0.0, 0.08, -0.50),
        Vec3::new(0.0, 0.0, 1.0),
        alloc,
    )
    .unwrap();
    let vis = click_clean.visible.expect("visible outer hit");
    let under = click_clean.underneath.expect("underneath inner hit");
    assert_eq!(vis.panel_name, "jacket_front");
    assert_eq!(vis.fabric_name, "mat_jacket");
    assert_eq!(vis.kind, PanelKind::PatternPiece);
    assert_eq!(vis.layer, 2);
    assert_eq!(under.panel_name, "undershirt_front");
    assert_eq!(under.fabric_name, "mat_undershirt");
    assert_eq!(under.layer, 0);
    let sep_mm = click_clean.separation.expect("separation").as_mm();
    assert!((sep_mm - 10.0).abs() < 1.0, "expected ~10 mm separation, got {sep_mm}");
    assert!(!click_clean.layer_inversion);
    assert_eq!(click_clean.source_pattern_piece, Some("jacket_front"));

    // 2. Click directly on the chest pocket (y = 0.18): visible is `chest_cargo_pocket`
    //    (`mat_cordura`), underneath is `jacket_front`, and `source_pattern_piece` jumps straight
    //    to the host CAD pattern piece `"jacket_front"`!
    let click_pkt = inspect_garment_ray(
        &garment,
        None,
        &ps.fabrics,
        Vec3::new(0.0, 0.18, -0.50),
        Vec3::new(0.0, 0.0, 1.0),
        alloc,
    )
    .unwrap();
    let vis_p = click_pkt.visible.expect("pocket hit");
    assert_eq!(vis_p.panel_name, "chest_cargo_pocket");
    assert_eq!(vis_p.kind, PanelKind::SurfacePatch);
    assert_eq!(vis_p.fabric_name, "mat_cordura");
    assert_eq!(vis_p.host_panel, Some("jacket_front"));
    assert_eq!(click_pkt.source_pattern_piece, Some("jacket_front"));

    // 3. Breakthrough / layer-inversion case: pose undershirt 4 mm in front of outer jacket!
    let inner_pid = garment.panels.iter().find(|p| p.name == "undershirt_front").unwrap().id;
    let mut posed = Vec::new_in(alloc);
    for &p in garment.positions.iter() {
        posed.try_push(p).unwrap();
    }
    let mut moved = Vec::new_in(alloc);
    for _ in 0..garment.positions.len() {
        moved.try_push(false).unwrap();
    }
    for (ti, &[a, b, c]) in garment.triangles.iter().enumerate() {
        if garment.tri_panel[ti] == inner_pid {
            for v in [a as usize, b as usize, c as usize] {
                if !moved[v] {
                    moved[v] = true;
                    posed[v].z -= 0.014; // -0.072 -> -0.086 (4 mm in front of -0.082 jacket!)
                }
            }
        }
    }
    let click_inv = inspect_garment_ray(
        &garment,
        Some(&posed),
        &ps.fabrics,
        Vec3::new(0.0, 0.08, -0.50),
        Vec3::new(0.0, 0.0, 1.0),
        alloc,
    )
    .unwrap();
    let vis_inv = click_inv.visible.expect("inverted visible hit");
    let under_inv = click_inv.underneath.expect("inverted underneath hit");
    assert_eq!(vis_inv.panel_name, "undershirt_front");
    assert_eq!(vis_inv.fabric_name, "mat_undershirt");
    assert_eq!(under_inv.panel_name, "jacket_front");
    assert_eq!(under_inv.fabric_name, "mat_jacket");
    assert!(click_inv.layer_inversion, "expected layer_inversion == true");
    let inv_sep_mm = click_inv.separation.expect("inv separation").as_mm();
    assert!((inv_sep_mm - 4.0).abs() < 1.0, "expected ~4 mm separation, got {inv_sep_mm}");

    // 4. GLB export + `read_glb_panel_metadata` round-trip verification!
    let glb = write_skinned_glb_with_morphs(&garment, &ps.fabrics, &[], &[], &[], &[], alloc, alloc).unwrap();
    let panel_map = read_glb_panel_metadata(&glb, alloc, alloc).unwrap();
    assert_eq!(panel_map.panels.len(), garment.panels.len());
    assert_eq!(panel_map.tri_panel.len(), garment.triangles.len());
    for (orig, loaded) in garment.panels.iter().zip(panel_map.panels.iter()) {
        assert_eq!(orig.id, loaded.id);
        assert_eq!(orig.name, loaded.name_str());
        assert_eq!(orig.kind, loaded.kind);
        assert_eq!(orig.layer, loaded.layer);
        assert_eq!(orig.fabric_idx, loaded.fabric_idx);
        assert_eq!(orig.host_panel, loaded.host_panel_str());
    }
    for tri_idx in 0..panel_map.tri_panel.len() {
        assert!(panel_map.panel_for_triangle(tri_idx).is_some());
    }
}

#[test]
fn fabric_grain_rest_strain_independent_of_texture_uvs_and_knee_gather() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();

    // Build a 2-panel knee trouser cylinder (`front_knee` + `back_knee`, radius = 0.070 m, height = 0.32 m).
    let mut ps = PatternSet::new(alloc);
    ps.fabrics.try_push(Fabric::MILITARY_TWILL).unwrap();
    let half_circ = core::f64::consts::PI * 0.070;
    let h = 0.32;
    for (name, ang) in [("front_knee", 0.0), ("back_knee", core::f64::consts::PI)] {
        ps.panels
            .try_push(PanelSpec {
                name,
                layer: 1,
                fabric_idx: 0,
                lining_fabric_idx: None,
                bottom: v1(alloc, SubEdge::new("b", Curve2D::line([-half_circ * 0.5, 0.0], [half_circ * 0.5, 0.0]), 8)),
                right: v1(alloc, SubEdge::new("r", Curve2D::line([half_circ * 0.5, 0.0], [half_circ * 0.5, h]), 12)),
                top: v1(alloc, SubEdge::new("t", Curve2D::line([-half_circ * 0.5, h], [half_circ * 0.5, h]), 8)),
                left: v1(alloc, SubEdge::new("l", Curve2D::line([-half_circ * 0.5, 0.0], [-half_circ * 0.5, h]), 12)),
                wrap: Wrap3D::VerticalCylinder {
                    center_x: 0.0,
                    center_z: 0.0,
                    y_origin: 0.0,
                    radius_x: 0.070,
                    radius_z: 0.070,
                    taper_x: 0.0,
                    taper_z: 0.0,
                    u_center: 0.0,
                    angle_center: ang,
                    arc_radius: 0.070,
                },
                flip_normal: true,
                ..
            })
            .ok()
            .unwrap();
    }
    let cloth = build_cloth_mesh(&ps, Some(1), alloc).unwrap();
    let mut garment = GarmentMesh::new(alloc);
    garment.append_draped_layer(&cloth, &ps, 0, alloc).unwrap();
    garment.recompute_smooth_normals();

    // Smoothly bend a knee cylinder by 70° around the knee hinge at y = 0.16 (front = -Z stretches
    // taut over patella, back = +Z compresses behind the knee!).
    let bend_knee = |p: Vec3, rear_bulge_m: f64| -> Vec3 {
        let knee_y = 0.16;
        let band = 0.12;
        let s = ((p.y - knee_y) / band).clamp(-1.0, 1.0);
        // Smooth bend angle: 0 at y >= 0.28, bends calf backward (+Z) below y = 0.16!
        let frac = 0.5 * (1.0 - s);
        let theta = 1.22 * frac * frac;
        let dy = p.y - knee_y;
        // Rotate (dy, z) backward around +X by -theta so front (-Z) arcs longer and back (+Z) compresses!
        let c = blaze_math::cos(theta);
        let sn = blaze_math::sin(theta);
        let mut out = Vec3::new(p.x, knee_y + dy * c + p.z * sn, -dy * sn + p.z * c);
        if rear_bulge_m > 0.0 && p.z > 0.02 {
            let bell = (1.0 - s * s).max(0.0);
            let rear_w = ((p.z - 0.02) / 0.05).clamp(0.0, 1.0);
            out.z += rear_bulge_m * bell * rear_w;
        }
        out
    };

    // Build bent knee body cylinder collider (radius = 0.052 m) and bent trouser positions.
    let (raw_body_p, body_t) = make_cylinder_mesh(0.052, 0.0, 0.32, alloc);
    let mut bent_body_p = Vec::new_in(alloc);
    for &p in raw_body_p.iter() {
        bent_body_p.try_push(bend_knee(p, 0.0)).unwrap();
    }
    let bent_body_col = MeshCollider::build(&bent_body_p, &body_t, false, alloc).unwrap();

    let mut bent_pos = Vec::new_in(alloc);
    for &p in garment.positions.iter() {
        bent_pos.try_push(bend_knee(p, 0.012)).unwrap();
    }

    // 1. Measure baseline cloth strain from grain-aligned physical `rest_pos_m`.
    let rep1 = measure_cloth_strain(&garment, Some(&bent_pos), 1.03, 0.95, alloc).unwrap();
    assert!(rep1.taut_count > 0, "expected taut front-of-knee triangles, got {rep1:?}", rep1 = (rep1.taut_count, rep1.max_warp_stretch));
    assert!(rep1.compressed_count > 0, "expected compressed rear-of-knee triangles");
    assert!(rep1.max_warp_stretch > 1.05, "expected taut warp_stretch > 1.05, got {}", rep1.max_warp_stretch);
    assert!(rep1.min_principal_stretch < 0.90, "expected compressed principal_min < 0.90, got {}", rep1.min_principal_stretch);

    let front_sum = rep1.panels.iter().find(|p| p.panel_name == "front_knee").unwrap();
    let back_sum = rep1.panels.iter().find(|p| p.panel_name == "back_knee").unwrap();
    assert!(front_sum.max_warp_stretch > 1.05, "front of knee must pull taut: {front_sum:?}");
    assert!(back_sum.min_principal_stretch < 0.90, "back of knee must compress: {back_sum:?}");

    // 2. Wildly transform texture UVs (7.5x scale + 37° rotation + offset) and prove
    //    `measure_cloth_strain` is BIT-FOR-BIT IDENTICAL!
    set_garment_texture_uv_transform(
        &mut garment,
        None,
        [7.5, 7.5],
        37.0 * core::f64::consts::PI / 180.0,
        [1.25, -0.75],
    );
    let rep2 = measure_cloth_strain(&garment, Some(&bent_pos), 1.03, 0.95, alloc).unwrap();
    assert_eq!(rep1.triangles.len(), rep2.triangles.len());
    assert_eq!(rep1.taut_count, rep2.taut_count);
    assert_eq!(rep1.compressed_count, rep2.compressed_count);
    assert_eq!(rep1.max_warp_stretch.to_bits(), rep2.max_warp_stretch.to_bits());
    assert_eq!(rep1.min_principal_stretch.to_bits(), rep2.min_principal_stretch.to_bits());
    for (t1, t2) in rep1.triangles.iter().zip(rep2.triangles.iter()) {
        assert_eq!(t1.warp_stretch.to_bits(), t2.warp_stretch.to_bits());
        assert_eq!(t1.weft_stretch.to_bits(), t2.weft_stretch.to_bits());
        assert_eq!(t1.bias_shear.to_bits(), t2.bias_shear.to_bits());
        assert_eq!(t1.principal_max.to_bits(), t2.principal_max.to_bits());
        assert_eq!(t1.principal_min.to_bits(), t2.principal_min.to_bits());
        assert_eq!(t1.area_ratio.to_bits(), t2.area_ratio.to_bits());
        assert_eq!(t1.compression_dir_world.x.to_bits(), t2.compression_dir_world.x.to_bits());
        assert_eq!(t1.compression_dir_world.y.to_bits(), t2.compression_dir_world.y.to_bits());
        assert_eq!(t1.compression_dir_world.z.to_bits(), t2.compression_dir_world.z.to_bits());
    }

    // 3. Planted-bug control: `GrainPlant::UseTextureUvsInsteadOfRest` MUST be caught!
    let rep_planted = measure_cloth_strain_planted(
        &garment,
        Some(&bent_pos),
        1.03,
        0.95,
        alloc,
        GrainPlant::UseTextureUvsInsteadOfRest,
    )
    .unwrap();
    assert!(
        (rep_planted.max_warp_stretch - rep1.max_warp_stretch).abs() > 0.25,
        "planted UV bug must dramatically corrupt strain: clean={}, planted={}",
        rep1.max_warp_stretch,
        rep_planted.max_warp_stretch
    );

    // 4. Verify `_REST_POS_M` is emitted in exported `.glb` JSON!
    let glb = write_skinned_glb_with_morphs(&garment, &ps.fabrics, &[], &[], &[], &[], alloc, alloc).unwrap();
    let json_len = u32::from_le_bytes([glb[12], glb[13], glb[14], glb[15]]) as usize;
    let json_str = core::str::from_utf8(&glb[20..20 + json_len]).unwrap();
    assert!(json_str.contains("\"_REST_POS_M\""), "missing _REST_POS_M attribute in exported GLB");

    // 5. Verify `relax_compression_zones` reduces rear-knee bulk while keeping front taut & body-clear!
    let relaxed = relax_compression_zones(
        &garment,
        &bent_pos,
        Some(&bent_body_col),
        KneeGatherOpts {
            iterations: 14,
            alloc,
            scratch: alloc,
            ..
        },
    )
    .unwrap();
    let mut before_rear_bulk = 0.0;
    let mut after_rear_bulk = 0.0;
    let mut rear_cnt = 0u32;
    for i in 0..garment.positions.len() {
        let p0 = garment.positions[i];
        if p0.z > 0.04 && (p0.y - 0.16).abs() < 0.06 {
            before_rear_bulk += bent_body_col.query(bent_pos[i]).signed_dist;
            after_rear_bulk += bent_body_col.query(relaxed[i]).signed_dist;
            rear_cnt += 1;
        }
        // Every relaxed vertex MUST clear the bent knee body collider!
        let sd = bent_body_col.query(relaxed[i]).signed_dist;
        assert!(sd >= -1e-4, "relaxed vertex {i} penetrated bent knee body: sd={sd}");
    }
    assert!(rear_cnt > 0);
    assert!(
        after_rear_bulk < before_rear_bulk,
        "expected rear-knee bulk to decrease after gathering: before={}, after={}",
        before_rear_bulk / (rear_cnt as f64),
        after_rear_bulk / (rear_cnt as f64)
    );
}
