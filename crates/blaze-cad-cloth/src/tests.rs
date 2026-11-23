use blaze_cad_field::Scratch;
use blaze_math::Vec3;
use blaze_own::{Alloc, Vec};
use blaze_units::Length;

use crate::{
    CHEST_BEACON_DEFAULT, COBRA_BUCKLE_DEFAULT, COLLAR_CLASP_DEFAULT, Curve2D, Fabric, GarmentIntegrityFinding,
    GarmentIntegrityOpts, GarmentMesh, InternalSeamSpec, IpcOpts, MeshCollider, MorphTarget, PanelSpec, PatternSet,
    PinSpec, RemeshOpts, SeamEaseFinding, SeamSpec, SolverOpts, StudioAngle, SubEdge, SurfacePatchSpec,
    UnifiedCollider, Wrap3D, adaptive_refine_cloth, append_cad_solid_to_garment, barrier_energy, barrier_grad,
    barrier_hess, body_clearance_check, build_chest_beacon_tape, build_cloth_mesh, build_cobra_buckle_tape,
    build_collar_clasp_tape, export_pattern_svg, project_ipc_barriers, read_glb_skeleton_mesh,
    render_studio_preview_png, seam_ease_check, simulate_garment, subdivide_cloth_loop, verify_garment_integrity,
    write_skinned_glb, write_skinned_glb_with_morphs,
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
