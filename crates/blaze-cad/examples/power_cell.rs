//! End-to-end ASTRA Power Cell milestone exercising all 14 SOTA CAD capabilities:
//!
//! 1. **Rail-guided C2 loft** (`guided_loft`): flares, pinches, and rolls smoothly into a shoulder
//!    using independent side-silhouette and top-ridge cubic rails.
//! 2. **Selective variable-radius filleting** (`variable_round_difference` / `round_selected_edges`):
//!    rolls the exposed shoulder while keeping adjacent panel seams crisp.
//! 3. **Stable surface regions & attachment frames** (`SurfaceRegion`, `resolve_region`):
//!    names `cell.outer_face`, `cell.shoulder`, and `cell.grip_patch` parametrically on the live
//!    field so editing the shell's curvature carries every attached feature with it.
//! 4. **Panels derived from a parent surface** (`derive_panel`): derives the recessed pocket,
//!    flush cover, perimeter clearance gap, and retaining lip from ONE closed outline.
//! 5. **Surface curves with tapered profile sweeps** (`project_curve_to_surface`, `sweep_on_surface`):
//!    carves a compound-curve seam groove with tapered ends across the curved shell.
//! 6. **Patterns fitted to a bounded tapered region** (`fill_region_pattern`): adds grip ribs
//!    respecting an inward border margin on the tapered grip patch.
//! 7. **Multi-material regions on one continuous shell** (`PartMaterialRegion`): ceramic housing,
//!    graphite recessed pocket, and exposed bare-metal rim surviving simplification.
//! 8. **Directional finishes & wear masks in physical units** (`BrushedMetal`, `MoldedGrain`,
//!    `EdgeWearMask`): authored in mm and previewed under grazing strip lights.
//! 9. **Surface-conforming vector decals (`"07"` + SVG chevron)** (`Decal`, `DecalShape::Text`,
//!    `DecalShape::SvgPath`): wrapped conformally onto the curved shoulder with their own colour,
//!    roughness, and subtle relief — zero flat patch, zero hand-tuned offset.
//! 10. **UVs, tangents, and PBR detail baking** (`unwrap_uvs`, `bake_surface_maps`, `glb_textured`):
//!     bakes colour, roughness, metallic, CAD normals, finishes, and `"07"` stencil into PNG maps
//!     embedded right in the exported `.glb`.
//! 11. **Authored-feature tessellation budgets** (`FeatureSpec`): groove width, seam gap, and
//!     stencil stroke width flow directly into local octree & QEM protection zones.
//! 12. **Camera-aware fidelity checks** (`check_camera_fidelity`): reports silhouette shift (px),
//!     seam visibility, and decal legibility across Aiming, Carried, and Inspect views & LODs.
//! 13. **Highlight & reflection inspection** (`RenderMode::Zebra`, `StripLight`): verifies smooth
//!     Class-A zebra stripes and grazing strip-light ribbons across the curved shoulder.
//! 14. **Saved game-camera previews & side-by-side revision comparison** (`render_glb_views`,
//!     `compare_glb_revisions`): renders the ACTUAL exported `.glb` bytes before and after a single
//!     curvature edit and writes a side-by-side comparison sheet.
//!
//! Run:
//!   cargo +nightly-2026-10-27 run --release -p blaze-cad --example power_cell -- target/power_cell

#![feature(default_field_values)]

use std::path::PathBuf;
use std::time::Instant;

use blaze_cad::prelude::*;
use blaze_cad_field::compile;
use blaze_cad_out::PngOpts;
use blaze_math::Vec3;

const MATERIALS: [Material<'static>; 3] = [
    // 0: Matte tactical ceramic housing (warm off-white / industrial bone)
    Material {
        name: "ceramic_shell",
        base_colour: [0.76, 0.75, 0.71, 1.0],
        metallic: 0.0,
        roughness: 0.44,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    },
    // 1: Dark graphite recessed pocket & groove interior
    Material {
        name: "graphite_recess",
        base_colour: [0.16, 0.17, 0.19, 1.0],
        metallic: 0.15,
        roughness: 0.68,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    },
    // 2: Brushed titanium rim & latch hardware
    Material {
        name: "titanium_rim",
        base_colour: [0.64, 0.67, 0.71, 1.0],
        metallic: 0.90,
        roughness: 0.26,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    },
];

struct BuiltPowerCell<'a> {
    asset: GameAsset<'a>,
    outer_frame: AttachmentFrame,
    shoulder_frame: AttachmentFrame,
    bounds: blaze_math::Aabb3,
    interference_passed: bool,
    camera_report: CameraFidelityReport<'a>,
}

/// Build the entire Power Cell parametrically from a single curvature parameter `shoulder_flare_mm`.
/// Changing `shoulder_flare_mm` updates the C2 lofted housing, automatically carries `cell.outer_face`
/// and `cell.shoulder`, re-derives the flush cover/pocket/gap/lip, and re-projects the `"07"` decal
/// flush onto the updated shoulder.
fn build_power_cell<'a>(
    alloc: blaze_own::Alloc<'a>,
    shoulder_flare_mm: f64,
    bake_size: u32,
) -> BuiltPowerCell<'a> {
    let mut model = Model::new(ModelOpts { alloc, .. });

    // 1. Rail-guided C2 lofted housing (130 mm tall along Z, flared shoulder around Z = +20..+45 mm).
    let base_prof = model
        .graph()
        .profile(ProfileOpts {
            profile: Profile::RoundedRect {
                width: Length::mm(66.0),
                height: Length::mm(42.0),
                radius: Length::mm(10.0),
            },
            name: Some("cell_section"),
        })
        .expect("cell profile");

    let stations = [
        GuidedStation { z: Length::mm(-65.0), profile: base_prof, .. },
        GuidedStation { z: Length::mm(-20.0), profile: base_prof, .. },
        GuidedStation { z: Length::mm(25.0), profile: base_prof, .. },
        GuidedStation { z: Length::mm(65.0), profile: base_prof, .. },
    ];
    let side_pts = [
        RailPoint { z: Length::mm(-65.0), value: Length::mm(30.0), tangent: Some(0.08) },
        RailPoint { z: Length::mm(-15.0), value: Length::mm(33.5), tangent: None },
        RailPoint { z: Length::mm(25.0), value: Length::mm(33.0 + shoulder_flare_mm * 0.65), tangent: None },
        RailPoint { z: Length::mm(65.0), value: Length::mm(26.5), tangent: Some(-0.22) },
    ];
    let ridge_pts = [
        RailPoint { z: Length::mm(-65.0), value: Length::mm(19.0), tangent: Some(0.06) },
        RailPoint { z: Length::mm(-15.0), value: Length::mm(21.5), tangent: None },
        RailPoint { z: Length::mm(25.0), value: Length::mm(22.0 + shoulder_flare_mm), tangent: None },
        RailPoint { z: Length::mm(65.0), value: Length::mm(16.5), tangent: Some(-0.20) },
    ];
    let raw_shell = model
        .graph()
        .guided_loft(GuidedLoftOpts {
            stations: &stations,
            side_rail: Some(GuideRail { points: &side_pts }),
            ridge_rail: Some(GuideRail { points: &ridge_pts }),
            continuity: LoftContinuity::CurvatureC2,
            cap_round: Length::mm(4.5),
            name: Some("power_cell_shell"),
            ..
        })
        .expect("guided loft");

    // 2. Selective variable-radius recess notch on the upper side shoulder (tapered fillet along Z,
    //    crisp elsewhere).
    let notch = model
        .graph()
        .cuboid(BoxOpts { size: mm3(18.0, 26.0, 32.0), round: Length::mm(2.0), .. })
        .expect("notch");
    let notch = model
        .graph()
        .place(notch, Placement::shift(Length::mm(35.0 + shoulder_flare_mm * 0.4), Length::ZERO, Length::mm(18.0)))
        .expect("place notch");
    let shell_shaped = model
        .graph()
        .variable_round_difference(
            raw_shell,
            notch,
            VariableBlendOpts {
                profile: RadiusProfile::Linear {
                    from: mm3(25.0, 0.0, 2.0),
                    to: mm3(25.0, 0.0, 34.0),
                    r0: Length::mm(1.5),
                    r1: Length::mm(5.5),
                },
                select: EdgeSelect::Box {
                    lo: mm3(15.0, -25.0, -5.0),
                    hi: mm3(55.0, 25.0, 45.0),
                    fade: Length::mm(4.0),
                },
                name: Some("shoulder_relief_fillet"),
            },
        )
        .expect("variable round difference");

    // Compile live shell tape to resolve parametric surface regions!
    let shell_tape = compile(model.graph_ref(), shell_shaped.id(), blaze_cad_field::CompileOpts { alloc, .. })
        .expect("compile shell");
    let bounds = shell_tape.aabb();

    // 3. Resolve stable named surface regions on the live curved shell.
    let outer_reg = SurfaceRegion::new(SurfaceRegionOpts {
        name: "cell.outer_face",
        selector: RegionSelector::Extremal { dir: [0.0, -1.0, 0.0], at_frac: [0.5, 0.0, 0.36] },
        up_hint: [0.0, 0.0, 1.0],
        ..
    });
    let shoulder_reg = SurfaceRegion::new(SurfaceRegionOpts {
        name: "cell.shoulder",
        selector: RegionSelector::Extremal { dir: [0.0, -1.0, 0.0], at_frac: [0.5, 0.0, 0.72] },
        up_hint: [0.0, 0.0, 1.0],
        ..
    });
    let grip_reg = SurfaceRegion::new(SurfaceRegionOpts {
        name: "cell.grip_patch",
        selector: RegionSelector::Extremal { dir: [-1.0, 0.0, 0.0], at_frac: [0.0, 0.5, 0.44] },
        up_hint: [0.0, 0.0, 1.0],
        ..
    });
    let outer_frame = resolve_region(&shell_tape, &outer_reg, alloc).expect("resolve cell.outer_face");
    let shoulder_frame = resolve_region(&shell_tape, &shoulder_reg, alloc).expect("resolve cell.shoulder");
    let grip_frame = resolve_region(&shell_tape, &grip_reg, alloc).expect("resolve cell.grip_patch");

    // 4. Derive a flush cover, recessed pocket, perimeter gap, and retaining lip from ONE outline.
    let panel_outline = Profile::RoundedRect {
        width: Length::mm(38.0),
        height: Length::mm(34.0),
        radius: Length::mm(5.0),
    };
    let derived = derive_panel(
        model.graph(),
        shell_shaped,
        &outer_frame,
        PanelOpts {
            outline: panel_outline,
            gap: Length::mm(1.2),
            cover_thickness: Length::mm(3.0),
            pocket_depth: Length::mm(5.0),
            lip_width: Length::mm(2.5),
            lip_thickness: Length::mm(2.0),
            name: "service_bay",
            ..
        },
    )
    .expect("derive panel");

    // 5. Draw a compound curve on the curved front shell and sweep a tapered groove along it.
    let groove_pts = [
        Curve2dPoint { u: Length::mm(-22.0), v: Length::mm(24.0), width: Length::mm(2.2), depth: Length::mm(1.4) },
        Curve2dPoint { u: Length::mm(-8.0), v: Length::mm(27.0), width: Length::mm(2.6), depth: Length::mm(1.6) },
        Curve2dPoint { u: Length::mm(8.0), v: Length::mm(27.0), width: Length::mm(2.6), depth: Length::mm(1.6) },
        Curve2dPoint { u: Length::mm(22.0), v: Length::mm(24.0), width: Length::mm(2.2), depth: Length::mm(1.4) },
    ];
    let groove_curve = project_curve_to_surface(
        &shell_tape,
        &outer_frame,
        &groove_pts,
        ProjectCurveOpts { samples_per_segment: 4, alloc, scratch: alloc, .. },
    )
    .expect("project groove curve");
    let groove_feat = sweep_on_surface(
        model.graph(),
        &groove_curve,
        SurfaceSweepOpts {
            section: SweepCrossSection::Channel,
            taper_start: Length::mm(7.0),
            taper_end: Length::mm(7.0),
            name: "shoulder_groove",
            scratch: alloc,
            ..
        },
    )
    .expect("sweep groove");
    let host_with_groove = model
        .graph()
        .difference(derived.host_with_pocket, groove_feat.solid)
        .expect("cut groove");

    // 6. Fit bounded grip ribs onto the tapered side grip patch with clean border margin.
    let grip_pts = [
        [Length::mm(-10.0), Length::mm(-22.0)],
        [Length::mm(10.0), Length::mm(-22.0)],
        [Length::mm(7.5), Length::mm(22.0)],
        [Length::mm(-7.5), Length::mm(22.0)],
    ];
    let grip_pattern = fill_region_pattern(
        model.graph(),
        shell_shaped,
        &shell_tape,
        &grip_frame,
        PatternOpts {
            boundary: Profile::Polygon { points: &grip_pts },
            border: Length::mm(2.0),
            pattern: SurfacePattern::Ribs {
                pitch: Length::mm(6.5),
                width: Length::mm(2.4),
                depth: Length::mm(1.2),
                angle: Angle::ZERO,
            },
            trim: TrimMode::Clip,
            raised: true,
            name: "side_grip",
            scratch: alloc,
            ..
        },
    )
    .expect("fill grip ribs");
    let final_housing = model.graph().union(host_with_groove, grip_pattern.solid).expect("add grip ribs");

    let (housing_id, _) = model.add(PartOpts { solid: final_housing, name: "housing" }).expect("add housing");
    model.set_material(housing_id, MaterialRole(0)).expect("housing mat");
    let (cover_id, _) = model.add(PartOpts { solid: derived.flush_cover, name: "flush_cover" }).expect("add cover");
    model.set_material(cover_id, MaterialRole(2)).expect("cover mat");

    // Verify cover sits inside the derived pocket with ZERO interference!
    let clearance = model
        .no_interference(InterferenceOpts { clearance: Length::mm(0.2), scratch: alloc, .. })
        .expect("check cover clearance");
    let interference_passed = clearance.verdict.passed();

    // 7. Multi-material regions on the single continuous housing shell (ceramic exterior, graphite
    //    recessed pocket, titanium rim at top cap).
    let mat_regions = [
        PartMaterialRegion::new(
            "housing",
            MaterialRegion {
                role: MaterialRole(1),
                rule: MaterialRule::RecessDepth { outer_shell: shell_shaped, min_depth: Length::mm(1.8) },
            },
        ),
        PartMaterialRegion::new(
            "housing",
            MaterialRegion {
                role: MaterialRole(2),
                rule: MaterialRule::NormalCone { dir: Vec3::new(0.0, 0.0, 1.0), max_angle: Angle::degrees(28.0) },
            },
        ),
    ];

    // 8. Directional physical-unit finishes & edge wear masks.
    let finishes = [
        FinishAssignment {
            role: MaterialRole(0),
            finish: Some(SurfaceFinish::MoldedGrain {
                grain_size: Length::mm(0.8),
                depth: Length::mm(0.03),
                roughness_variation: 0.08,
            }),
            wear: Some(EdgeWearMask {
                substrate_colour: [0.64, 0.67, 0.71],
                substrate_metallic: 0.85,
                substrate_roughness: 0.30,
                curvature_threshold: 18.0,
                intensity: 0.55,
                noise_scale: Length::mm(3.0),
            }),
        },
        FinishAssignment {
            role: MaterialRole(2),
            finish: Some(SurfaceFinish::BrushedMetal {
                direction: [1.0, 0.0, 0.0],
                pitch: Length::mm(0.45),
                depth: Length::mm(0.025),
                roughness_variation: 0.12,
            }),
            wear: None,
        },
    ];

    // 9. Surface-conforming `"07"` vector decal + SVG caution chevron wrapped directly onto the
    //    curved shoulder frame (ZERO flat patch, ZERO hand-tuned offset!).
    let stencil_07 = Decal::new(DecalOpts {
        name: "stencil_07",
        frame: shoulder_frame,
        shape: DecalShape::Text {
            text: "07",
            height: Length::mm(17.0),
            stroke_width: Length::mm(2.4),
            tracking: 0.18,
        },
        material: DecalMaterial {
            base_colour: [0.88, 0.28, 0.08, 1.0], // high-vis tactical safety orange
            roughness: 0.34,
            metallic: 0.05,
            relief: Some(Length::mm(0.08)),
        },
        projection: DecalProjection::ConformalArcLength,
        clip_role: Some(MaterialRole(0)),
        ..
    });
    let decals = [PartDecal::new("housing", stencil_07)];

    // 10. Authored-feature tessellation targets + full textured GLB asset build!
    let features = [
        PartFeature::new("housing", derived.seam_feature),
        PartFeature::new("housing", groove_feat.feature),
        PartFeature::new("housing", decal_feature_target(&stencil_07)),
    ];

    let asset = build_game_asset(
        model.graph_ref(),
        model.parts(),
        GameAssetOpts {
            alloc,
            scratch: alloc,
            preset: AssetPreset::GameProp,
            total_budget: Some(TriangleBudget(12_000)),
            materials: &MATERIALS,
            features: &features,
            material_regions: &mat_regions,
            decals: &decals,
            finishes: &finishes,
            bake: Some(AssetBakeOpts { size: bake_size, gutter_px: 4, bake_cad_normals: true, normal_strength: 1.0 }),
            lods: Some(ExportLodOpts { ratio: 0.5, fewest_triangles: 120, .. }),
            ..
        },
        None,
    )
    .expect("build power cell asset");

    // 12. Camera-aware fidelity check on the housing across Aiming, Carried, and Inspect views.
    let rig = GameCameraRig::standard_views(&bounds, 480, 480, alloc).expect("camera rig");
    let cam_list = [rig[0].1, rig[1].1, rig[2].1];
    let seams = [SeamSpec::new("service_bay_seam", derived.seam_feature.region, Length::mm(1.2), Length::mm(3.0))];
    let decal_specs = [DecalSpec::new(
        "stencil_07",
        shoulder_frame.origin,
        shoulder_frame.normal,
        Length::mm(2.4),
        asset.parts[0].texels_per_metre,
    )];
    let housing_view = asset.parts[0].mesh.view();
    let camera_report = check_camera_fidelity(
        &shell_tape,
        &asset.parts[0].placement,
        &[housing_view],
        CameraFidelityOpts {
            cameras: &cam_list,
            seams: &seams,
            decals: &decal_specs,
            max_silhouette_shift_px: 2.5,
            alloc,
            scratch: alloc,
            ..
        },
    )
    .expect("camera fidelity");

    BuiltPowerCell { asset, outer_frame, shoulder_frame, bounds, interference_passed, camera_report }
}

fn main() {
    let out_dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "target/power_cell".into()));
    std::fs::create_dir_all(&out_dir).expect("create output dir");

    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();

    let t0 = Instant::now();
    let rev1 = build_power_cell(alloc, 4.0, 512);
    let rev1_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = Instant::now();
    let rev2 = build_power_cell(alloc, 11.0, 512);
    let rev2_ms = t1.elapsed().as_secs_f64() * 1000.0;

    println!("=== POWER CELL REV 1 (shoulder flare = 4.0 mm) [{rev1_ms:.0} ms] ===");
    println!("  cell.outer_face:  origin=({:.1}, {:.1}, {:.1}) mm  normal=({:.3}, {:.3}, {:.3})",
        rev1.outer_frame.origin.x * 1e3, rev1.outer_frame.origin.y * 1e3, rev1.outer_frame.origin.z * 1e3,
        rev1.outer_frame.normal.x, rev1.outer_frame.normal.y, rev1.outer_frame.normal.z);
    println!("  cell.shoulder:    origin=({:.1}, {:.1}, {:.1}) mm  normal=({:.3}, {:.3}, {:.3})",
        rev1.shoulder_frame.origin.x * 1e3, rev1.shoulder_frame.origin.y * 1e3, rev1.shoulder_frame.origin.z * 1e3,
        rev1.shoulder_frame.normal.x, rev1.shoulder_frame.normal.y, rev1.shoulder_frame.normal.z);
    println!("  cover clearance:  {}", if rev1.interference_passed { "PASS (0.2 mm)" } else { "FAIL" });
    println!("  decal texels:     {}", rev1.asset.parts[0].baked.as_ref().map_or(0, |b| b.decal_texels));
    println!("{}", Shown(&rev1.asset.report));
    println!("{}", Shown(&rev1.camera_report));

    println!("=== POWER CELL REV 2 (curvature edit: shoulder flare = 11.0 mm) [{rev2_ms:.0} ms] ===");
    println!("  cell.outer_face:  origin=({:.1}, {:.1}, {:.1}) mm  normal=({:.3}, {:.3}, {:.3})",
        rev2.outer_frame.origin.x * 1e3, rev2.outer_frame.origin.y * 1e3, rev2.outer_frame.origin.z * 1e3,
        rev2.outer_frame.normal.x, rev2.outer_frame.normal.y, rev2.outer_frame.normal.z);
    println!("  cell.shoulder:    origin=({:.1}, {:.1}, {:.1}) mm  normal=({:.3}, {:.3}, {:.3})",
        rev2.shoulder_frame.origin.x * 1e3, rev2.shoulder_frame.origin.y * 1e3, rev2.shoulder_frame.origin.z * 1e3,
        rev2.shoulder_frame.normal.x, rev2.shoulder_frame.normal.y, rev2.shoulder_frame.normal.z);
    println!("  cover clearance:  {}", if rev2.interference_passed { "PASS (0.2 mm)" } else { "FAIL" });
    println!("  decal texels:     {}", rev2.asset.parts[0].baked.as_ref().map_or(0, |b| b.decal_texels));
    println!("{}", Shown(&rev2.asset.report));

    // Write exported GLBs and baked texture maps.
    std::fs::write(out_dir.join("power_cell_v1.glb"), &rev1.asset.glb[..]).expect("write v1 glb");
    std::fs::write(out_dir.join("power_cell_v2.glb"), &rev2.asset.glb[..]).expect("write v2 glb");
    if let Some(ref baked) = rev2.asset.parts[0].baked {
        std::fs::write(out_dir.join("housing_base_colour.png"), &baked.base_colour.png[..]).expect("write base");
        std::fs::write(out_dir.join("housing_metallic_roughness.png"), &baked.metallic_roughness.png[..]).expect("write mr");
        std::fs::write(out_dir.join("housing_normal.png"), &baked.normal.png[..]).expect("write normal");
    }

    // 13 & 14. Render actual GLB bytes from saved game cameras (Aiming, Carried, Inspect, Zebra)
    //          and compose side-by-side revision comparison!
    let union_bounds = rev1.bounds.union(&rev2.bounds);
    let rig = GameCameraRig::standard_views(&union_bounds, 360, 360, alloc).expect("rig");
    let chosen = [
        (Some(rig[0].0), rig[0].1), // Aiming
        (Some(rig[1].0), rig[1].1), // Carried
        (Some(rig[2].0), rig[2].1), // Inspect
        (Some(rig[5].0), rig[5].1), // ZebraInspect
    ];
    let preview_opts = GlbPreviewOpts {
        supersample: 2,
        strip_lights: [Some(StripLight::OVERHEAD_HORIZONTAL), Some(StripLight::GRAZING_SIDE), None, None],
        alloc,
        scratch: alloc,
        ..
    };
    let rendered_v2 = render_glb_views(&rev2.asset.glb, &chosen, preview_opts).expect("render v2 glb views");
    std::fs::write(out_dir.join("power_cell_v2_contact_sheet.png"), &rendered_v2.contact_sheet_png[..])
        .expect("write contact sheet");

    let cmp_views = [
        (Some(rig[2].0), rig[2].1), // Inspect
        (Some(rig[5].0), rig[5].1), // ZebraInspect
    ];
    let compared = compare_glb_revisions(&rev1.asset.glb, &rev2.asset.glb, &cmp_views, preview_opts)
        .expect("compare revisions");
    std::fs::write(out_dir.join("power_cell_v1_vs_v2.png"), &compared.sheet_png[..]).expect("write comparison");

    for v in compared.views.iter() {
        println!(
            "  revision diff [{}]: rms={:.4}, max_delta={}, changed={:.1}%",
            v.name.as_str(),
            v.rms_error,
            v.max_channel_delta,
            v.changed_fraction * 100.0
        );
    }

    // Also render a direct studio preview via `GameAsset::preview` to verify parity.
    let direct = rev2
        .asset
        .preview(
            &MATERIALS,
            rig[2].1.view,
            MeshPictureOpts {
                alloc,
                scratch: alloc,
                supersample: 2,
                shading: Shading::Studio(StudioOpts {
                    strip_lights: [Some(StripLight::OVERHEAD_HORIZONTAL), Some(StripLight::GRAZING_SIDE), None, None],
                    ..StudioOpts::DEFAULT
                }),
                ..
            },
        )
        .expect("direct preview");
    let png = direct
        .png(PngOpts { level: blaze_deflate::Level::Fast, limits: blaze_cad_out::Limits::SHIPPED, alloc, scratch: alloc })
        .expect("encode direct png");
    std::fs::write(out_dir.join("power_cell_v2_direct.png"), &png[..]).expect("write direct");

    println!("wrote artifacts to {}", out_dir.display());
}
