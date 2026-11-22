//! End-to-end game-asset workflow with **per-feature fidelity targets**: a wall-mounted mirror
//! assembly (`frame`, `mirror_panel` with 4 keyhole mounting slots, `cleat_upper`, `cleat_lower`
//! with 45° French-cleat mating bevels).
//!
//! Demonstrates:
//! - Marking critical keyholes (`FeatureKind::Keyhole`, `Pinned`), frame bevels
//!   (`FeatureKind::Bevel`, `Critical`), and 45° French-cleat mating faces
//!   (`FeatureKind::MatingSurface`, `Pinned`) on a [`Model`].
//! - Local octree refinement & protected QEM decimation preserving extra geometry right around
//!   those features while keeping the whole build fast (seconds, even under tight settings).
//! - Structured per-part + per-feature fidelity report (`AssetReport::show()`).
//! - Exported-mesh preview automatically highlighting any feature that misses tolerance.
//!
//! Run:
//!   cargo +nightly-2026-10-27 run --release -p blaze-cad --example mirror -- target/mirror

#![feature(default_field_values)]

use std::{path::PathBuf, process::exit};
use std::time::Instant;

use blaze_cad::prelude::*;
use blaze_math::{Aabb3, Vec3};

const MATERIALS: [Material<'static>; 3] = [
    // 0: Warm beveled oak frame
    Material {
        name: "oak_frame",
        base_colour: [0.38, 0.22, 0.10, 1.0],
        metallic: 0.0,
        roughness: 0.55,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    },
    // 1: Brushed backing / silvered mirror panel
    Material {
        name: "mirror_panel",
        base_colour: [0.76, 0.80, 0.84, 1.0],
        metallic: 0.85,
        roughness: 0.14,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    },
    // 2: Machined hardwood/aluminum French cleats
    Material {
        name: "french_cleat",
        base_colour: [0.56, 0.52, 0.44, 1.0],
        metallic: 0.20,
        roughness: 0.48,
        emissive: [0.0; 3],
        emissive_strength: None,
        clearcoat: None,
        double_sided: false,
    },
];

fn box_region(lo_mm: [f64; 3], hi_mm: [f64; 3]) -> FeatureRegion {
    FeatureRegion::Box(Aabb3::new(
        Vec3::new(lo_mm[0] / 1000.0, lo_mm[1] / 1000.0, lo_mm[2] / 1000.0),
        Vec3::new(hi_mm[0] / 1000.0, hi_mm[1] / 1000.0, hi_mm[2] / 1000.0),
    ))
}

/// Build a 45° French cleat rail along X (`width × depth × height`), beveled on its top-rear
/// mating face (`+Y / +Z`) with two countersunk mounting holes along Y.
fn make_french_cleat(g: &mut Graph<'_, blaze_own::planted::NoPlant>, width_mm: f64, depth_mm: f64, height_mm: f64) -> Solid<Bound> {
    let raw = g
        .cuboid(BoxOpts { size: mm3(width_mm, depth_mm, height_mm), .. })
        .expect("cleat body");
    // Cut the 45-degree mating bevel across the full width using a rotated cutting block.
    let cutter_box = g
        .cuboid(BoxOpts { size: mm3(width_mm + 20.0, depth_mm * 2.0, height_mm * 2.0), .. })
        .expect("cleat cutter");
    let cutter = g
        .place(
            cutter_box,
            Placement::shift(Length::ZERO, Length::mm(depth_mm * 0.5 + 4.0), Length::mm(height_mm * 0.5 + 4.0))
                .then_after(&Placement::rotation(Axis::X, Angle::degrees(45.0))),
        )
        .expect("place cleat cutter");
    let beveled = g.difference(raw, cutter).expect("bevel cleat");

    // Two mounting screw through-holes along Y.
    let mut cleat = beveled.weaken::<Bound>();
    for sx in [-width_mm * 0.34, width_mm * 0.34] {
        let bore = g
            .cylinder(CylinderOpts { radius: Length::mm(4.0), height: Length::mm(depth_mm + 16.0), .. })
            .expect("cleat bore");
        let bore = g
            .place(
                bore,
                Placement::shift(Length::mm(sx), Length::ZERO, Length::mm(-height_mm * 0.12))
                    .then_after(&Placement::rotation(Axis::X, Angle::degrees(90.0))),
            )
            .expect("place cleat bore");
        cleat = g.difference(cleat, bore).expect("cut cleat bore").weaken::<Bound>();
    }
    cleat
}

fn main() {
    let out_dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "target/mirror".into()));
    std::fs::create_dir_all(&out_dir).expect("create output dir");

    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let mut model = Model::new(ModelOpts { alloc, .. });

    // 1. Beveled rectangular mirror frame (`760 × 1040 × 44 mm`) with inner rabbet & opening.
    let frame_outer = model
        .graph()
        .cuboid(BoxOpts { size: mm3(760.0, 44.0, 1040.0), round: Length::mm(6.0), .. })
        .expect("frame outer");
    let window_cut = model
        .graph()
        .cuboid(BoxOpts { size: mm3(640.0, 80.0, 920.0), .. })
        .expect("frame opening");
    let rabbet_cut = model
        .graph()
        .cuboid(BoxOpts { size: mm3(690.0, 26.0, 970.0), .. })
        .expect("frame rabbet");
    let rabbet_cut = model
        .graph()
        .place(rabbet_cut, Placement::shift(Length::ZERO, Length::mm(12.0), Length::ZERO))
        .expect("place rabbet");
    let frame_hollow = model.graph().difference(frame_outer, window_cut).expect("hollow frame");
    let frame_solid = model.graph().difference(frame_hollow, rabbet_cut).expect("rabbet frame");
    let (frame_id, _) = model.add(PartOpts { solid: frame_solid, name: "frame" }).expect("add frame");
    model.set_material(frame_id, MaterialRole(0)).expect("frame mat");

    // Mark the front outer top bevel edge of the frame as a critical Bevel feature.
    model
        .mark_feature(
            frame_id,
            FeatureTarget::new(FeatureTargetOpts {
                name: "frame_top_bevel",
                kind: FeatureKind::Bevel,
                region: FeatureRegion::Capsule {
                    a: Vec3::new(-0.374, -0.018, 0.514),
                    b: Vec3::new(0.374, -0.018, 0.514),
                    radius: Length::mm(10.0),
                },
                max_error: Some(Length::mm(0.25)),
                max_normal_error: Some(Angle::degrees(18.0)),
                protect: FeatureProtect::Critical,
                ..
            }),
        )
        .expect("mark frame bevel");

    // 2. Mirror backing panel (`680 × 18 × 960 mm`) with 4 recessed keyhole mounting slots
    //    on the rear (+Y) face.
    let panel_box = model
        .graph()
        .cuboid(BoxOpts { size: mm3(680.0, 18.0, 960.0), .. })
        .expect("mirror panel");
    let keyhole_coords: [(&str, f64, f64); 4] = [
        ("keyhole_tl", -250.0, 360.0),
        ("keyhole_tr", 250.0, 360.0),
        ("keyhole_bl", -250.0, -360.0),
        ("keyhole_br", 250.0, -360.0),
    ];
    let mut panel_solid = panel_box.weaken::<Bound>();
    for &(_, kx, kz) in &keyhole_coords {
        // Keyhole = 14 mm diameter entry bore + 7 mm × 20 mm upper slot, cut cleanly through
        // the backing panel so all 4 keyholes are exact topological through-holes (genus 4).
        let bore = model
            .graph()
            .cylinder(CylinderOpts { radius: Length::mm(7.0), height: Length::mm(30.0), .. })
            .expect("keyhole bore");
        let bore = model
            .graph()
            .place(
                bore,
                Placement::shift(Length::mm(kx), Length::ZERO, Length::mm(kz))
                    .then_after(&Placement::rotation(Axis::X, Angle::degrees(90.0))),
            )
            .expect("place keyhole bore");
        let slot = model
            .graph()
            .cuboid(BoxOpts { size: mm3(7.0, 30.0, 20.0), .. })
            .expect("keyhole slot");
        let slot = model
            .graph()
            .place(slot, Placement::shift(Length::mm(kx), Length::ZERO, Length::mm(kz + 10.0)))
            .expect("place keyhole slot");
        let keyhole = model.graph().union(bore, slot).expect("keyhole shape");
        panel_solid = model.graph().difference(panel_solid, keyhole).expect("cut keyhole").weaken::<Bound>();
    }
    let panel_placed = model
        .graph()
        .place(panel_solid, Placement::shift(Length::ZERO, Length::mm(8.0), Length::ZERO))
        .expect("place panel");
    let (panel_id, _) = model.add(PartOpts { solid: panel_placed, name: "mirror_panel" }).expect("add mirror panel");
    model.set_material(panel_id, MaterialRole(1)).expect("panel mat");

    for &(kname, kx, kz) in &keyhole_coords {
        model
            .mark_feature(
                panel_id,
                FeatureTarget::new(FeatureTargetOpts {
                    name: kname,
                    kind: FeatureKind::Keyhole,
                    region: box_region([kx - 11.0, -12.0, kz - 11.0], [kx + 11.0, 12.0, kz + 24.0]),
                    max_error: Some(Length::mm(0.6)),
                    max_normal_error: Some(Angle::degrees(20.0)),
                    min_feature: Some(Length::mm(3.5)),
                    protect: FeatureProtect::Pinned,
                }),
            )
            .expect("mark keyhole");
    }

    // 3. Upper & lower 45-degree French cleats mounted on the back of the mirror assembly.
    let cleat_raw = make_french_cleat(model.graph(), 500.0, 20.0, 64.0);
    let cleat_top = model
        .graph()
        .place(cleat_raw, Placement::shift(Length::ZERO, Length::mm(28.0), Length::mm(260.0)))
        .expect("place upper cleat");
    let (cleat_top_id, _) = model.add(PartOpts { solid: cleat_top, name: "cleat_upper" }).expect("add cleat_upper");
    model.set_material(cleat_top_id, MaterialRole(2)).expect("cleat mat");
    model
        .mark_feature(
            cleat_top_id,
            FeatureTarget::new(FeatureTargetOpts {
                name: "cleat_mating_45",
                kind: FeatureKind::MatingSurface,
                region: box_region([-252.0, -12.0, 8.0], [252.0, 12.0, 34.0]),
                max_error: Some(Length::mm(0.5)),
                max_normal_error: Some(Angle::degrees(12.0)),
                protect: FeatureProtect::Pinned,
                ..
            }),
        )
        .expect("mark cleat mating");

    let cleat_bot = model
        .graph()
        .place(cleat_raw, Placement::shift(Length::ZERO, Length::mm(28.0), Length::mm(-260.0)))
        .expect("place lower cleat");
    let (cleat_bot_id, _) = model.add(PartOpts { solid: cleat_bot, name: "cleat_lower" }).expect("add cleat_lower");
    model.set_material(cleat_bot_id, MaterialRole(2)).expect("cleat mat");

    // ---- Run A: Tight HeroAsset build with per-feature protection ----
    let t0 = Instant::now();
    let mut cache = PieceCache::new(PieceCacheOpts { alloc, .. });
    let asset = build_game_asset(
        model.graph_ref(),
        model.parts(),
        GameAssetOpts {
            alloc,
            scratch: alloc,
            preset: AssetPreset::HeroAsset,
            total_budget: Some(TriangleBudget(14_000)),
            materials: &MATERIALS,
            features: model.features(),
            ..
        },
        Some(&mut cache),
    )
    .expect("build mirror asset");
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;

    println!("=== MIRROR ASSEMBLY (HeroAsset + Per-Feature Targets) ===");
    println!("build_time: {build_ms:.1} ms");
    println!("{}", Shown(&asset.report));
    let proof = match asset.check_publish(PublishCheckOpts { require_non_empty_features: true, .. }) {
        Ok(proof) => proof,
        Err(err) => {
            eprintln!("mirror assembly failed strict publish gate: {}", Shown(&err));
            exit(1);
        }
    };
    println!(
        "publish_gate: PASS ({} parts, {}/{} features, {} tris)",
        proof.parts(),
        proof.passed_features(),
        proof.requested_features(),
        proof.written_triangles()
    );

    std::fs::write(out_dir.join("mirror.glb"), asset.publish_glb(proof)).expect("write mirror.glb");

    let png_opts = blaze_cad_out::PngOpts {
        level: blaze_deflate::Level::Default,
        limits: blaze_cad_out::Limits::SHIPPED,
        alloc,
        scratch: alloc,
    };

    let t_prev = Instant::now();
    let front_view = View {
        eye: [Length::mm(620.0), Length::mm(-1450.0), Length::mm(380.0)],
        target: [Length::ZERO, Length::ZERO, Length::ZERO],
        projection: Projection::Perspective { fov: Angle::degrees(42.0) },
        width: 560,
        height: 680,
        ..
    };
    let back_view = View {
        eye: [Length::mm(-580.0), Length::mm(1450.0), Length::mm(320.0)],
        target: [Length::ZERO, Length::ZERO, Length::ZERO],
        projection: Projection::Perspective { fov: Angle::degrees(42.0) },
        width: 560,
        height: 680,
        ..
    };

    let pic_front = asset
        .preview(&MATERIALS, front_view, MeshPictureOpts { alloc, scratch: alloc, supersample: 2, .. })
        .expect("render front");
    std::fs::write(out_dir.join("mirror_front.png"), &pic_front.png(png_opts).expect("png")[..]).expect("write front");

    let pic_back_fid = asset
        .preview_fidelity(&MATERIALS, back_view, MeshPictureOpts { alloc, scratch: alloc, supersample: 2, .. })
        .expect("render back fidelity");
    std::fs::write(out_dir.join("mirror_back_fidelity.png"), &pic_back_fid.png(png_opts).expect("png")[..])
        .expect("write back");

    // ---- Run B: Intentionally starved budget to demonstrate missed-feature highlight in preview ----
    let tight_feats = [PartFeature::new(
        "frame",
        FeatureTarget::new(FeatureTargetOpts {
            name: "frame_top_bevel_strict",
            kind: FeatureKind::Bevel,
            region: FeatureRegion::Capsule {
                a: Vec3::new(-0.374, -0.018, 0.514),
                b: Vec3::new(0.374, -0.018, 0.514),
                radius: Length::mm(14.0),
            },
            max_error: Some(Length::mm(0.05)),
            max_normal_error: Some(Angle::degrees(3.0)),
            protect: FeatureProtect::Critical,
            ..
        }),
    )];
    let starved = build_game_asset(
        model.graph_ref(),
        model.parts(),
        GameAssetOpts {
            alloc,
            scratch: alloc,
            preset: AssetPreset::Environment,
            total_budget: Some(TriangleBudget(900)),
            materials: &MATERIALS,
            features: &tight_feats,
            ..
        },
        None,
    )
    .expect("build starved demo");
    println!("=== STARVED DEMO (Missed Feature Auto-Highlight) ===");
    println!("missed_highlights: {}", starved.missed_highlights.len());
    println!("{}", Shown(&starved.report));
    let blocked = starved
        .check_publish(PublishCheckOpts { .. })
        .expect_err("starved demo must be blocked by publish gate");
    println!("publish_gate: BLOCKED ({})", Shown(&blocked));

    let pic_missed = starved
        .preview(&MATERIALS, front_view, MeshPictureOpts { alloc, scratch: alloc, supersample: 2, .. })
        .expect("render missed highlight");
    std::fs::write(out_dir.join("mirror_missed_highlight.png"), &pic_missed.png(png_opts).expect("png")[..])
        .expect("write missed highlight");

    println!("preview_time: {:.1} ms", t_prev.elapsed().as_secs_f64() * 1000.0);
    println!("wrote {}", out_dir.display());
}
