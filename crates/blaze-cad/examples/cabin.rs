//! End-to-end game-asset cabin workflow: builds a meter-scale multi-part cabin with named parts,
//! material roles, beveled edges, window/door/flue openings, per-part & bundle-wide triangle
//! budgets (27,000 tris), sharper lost-hole & lost-bevel fidelity checks, a per-part report,
//! GPU-ready `.glb` export, and WYSIWYG previews rendered directly from the **exported meshes**.
//!
//! Run with:
//! `cargo run --release -p blaze-cad --example cabin -- target/cabin`

#![feature(default_field_values)]

use std::time::Instant;

use blaze_cad::prelude::*;

const WOOD_WALL: Material<'static> = Material {
    name: "wood.wall",
    base_colour: [0.44, 0.27, 0.14, 1.0],
    metallic: 0.0,
    roughness: 0.78,
    emissive: [0.0; 3],
    emissive_strength: None,
    clearcoat: None,
    double_sided: false,
};

const SLATE_ROOF: Material<'static> = Material {
    name: "slate.roof",
    base_colour: [0.16, 0.19, 0.24, 1.0],
    metallic: 0.06,
    roughness: 0.62,
    emissive: [0.0; 3],
    emissive_strength: None,
    clearcoat: None,
    double_sided: false,
};

const STONE: Material<'static> = Material {
    name: "stone",
    base_colour: [0.40, 0.39, 0.37, 1.0],
    metallic: 0.0,
    roughness: 0.90,
    emissive: [0.0; 3],
    emissive_strength: None,
    clearcoat: None,
    double_sided: false,
};

const TRIM: Material<'static> = Material {
    name: "trim.frame",
    base_colour: [0.76, 0.70, 0.60, 1.0],
    metallic: 0.0,
    roughness: 0.58,
    emissive: [0.0; 3],
    emissive_strength: None,
    clearcoat: None,
    double_sided: false,
};

const DOOR_WOOD: Material<'static> = Material {
    name: "wood.door",
    base_colour: [0.28, 0.15, 0.08, 1.0],
    metallic: 0.0,
    roughness: 0.55,
    emissive: [0.0; 3],
    emissive_strength: None,
    clearcoat: None,
    double_sided: false,
};

const MATERIALS: [Material<'static>; 5] = [WOOD_WALL, SLATE_ROOF, STONE, TRIM, DOOR_WOOD];

fn mm(v: f64) -> Length {
    Length::mm(v)
}

fn shift(x: f64, y: f64, z: f64) -> Placement {
    Placement::shift(mm(x), mm(y), mm(z))
}

fn build_cabin(model: &mut Model<'_>) -> Result<(), ModelError> {
    let g = model.graph();

    // 1) Foundation plinth + front porch deck (5.8 x 4.4 x 0.36 m, beveled edges).
    let plinth = g.cuboid(BoxOpts { size: mm3(5200.0, 3800.0, 360.0), round: mm(25.0), name: Some("plinth") })?;
    let porch = g.cuboid(BoxOpts { size: mm3(5200.0, 1100.0, 260.0), round: mm(20.0), name: Some("porch") })?;
    let porch = g.place(porch, shift(0.0, -2300.0, -50.0))?;
    let foundation = g.union(plinth, porch)?;
    let foundation = g.place(foundation, shift(0.0, 0.0, 180.0))?;

    // 2) Beveled timber cabin walls (4.8 x 3.4 x 2.4 m, 220 mm thick) with 2 front windows,
    // 1 side window, and 1 front doorway cutout (open shell + 4 openings -> genus 5).
    let outer = g.cuboid(BoxOpts { size: mm3(4800.0, 3400.0, 2400.0), round: mm(28.0), name: Some("wall outer") })?;
    let inner = g.cuboid(BoxOpts { size: mm3(4360.0, 2960.0, 2600.0), name: Some("wall inner"), .. })?;
    let shell = g.difference(outer, inner)?;

    let front_win_l = g.cuboid(BoxOpts { size: mm3(780.0, 700.0, 820.0), round: mm(20.0), .. })?;
    let front_win_l = g.place(front_win_l, shift(-1350.0, -1700.0, 160.0))?;
    let front_win_r = g.cuboid(BoxOpts { size: mm3(780.0, 700.0, 820.0), round: mm(20.0), .. })?;
    let front_win_r = g.place(front_win_r, shift(1350.0, -1700.0, 160.0))?;
    let side_win = g.cuboid(BoxOpts { size: mm3(700.0, 860.0, 820.0), round: mm(20.0), .. })?;
    let side_win = g.place(side_win, shift(2400.0, 0.0, 160.0))?;
    let doorway = g.cuboid(BoxOpts { size: mm3(920.0, 700.0, 1850.0), round: mm(20.0), .. })?;
    let doorway = g.place(doorway, shift(0.0, -1700.0, -280.0))?;
    let cutouts = union_all(g, &[front_win_l, front_win_r, side_win, doorway])?;
    let walls = g.difference(shell, cutouts)?;
    let walls = g.place(walls, shift(0.0, 0.0, 1560.0))?;

    // 3) Pitched beveled gabled roof (two pitched slopes joined at the ridge, overhanging eaves).
    let slope_len = 2550.0;
    let slope_raw = g.cuboid(BoxOpts { size: mm3(5500.0, slope_len, 220.0), round: mm(28.0), name: Some("roof slope") })?;
    let pitch = Angle::degrees(28.0);
    let slope_front = g.place(
        slope_raw,
        shift(0.0, -1020.0, 3240.0).then_after(&Placement::rotation(Axis::X, pitch)),
    )?;
    let slope_back = g.place(
        slope_raw,
        shift(0.0, 1020.0, 3240.0).then_after(&Placement::rotation(Axis::X, Angle::degrees(-28.0))),
    )?;
    let ridge = g.cuboid(BoxOpts { size: mm3(5550.0, 320.0, 260.0), round: mm(30.0), name: Some("ridge cap") })?;
    let ridge = g.place(ridge, shift(0.0, 0.0, 3810.0))?;
    let roof = union_all(g, &[slope_front, slope_back, ridge])?;

    // 4) Recessed window frames (left and right named parts — sharing one local solid so the
    // PieceCache reuses the frame's triangles across both placements!).
    let frame_out = g.cuboid(BoxOpts { size: mm3(860.0, 260.0, 900.0), round: mm(18.0), .. })?;
    let frame_in = g.cuboid(BoxOpts { size: mm3(680.0, 420.0, 720.0), .. })?;
    let single_frame = g.difference(frame_out, frame_in)?;
    let wf_l = g.place(single_frame, shift(-1350.0, -1690.0, 1720.0))?;
    let wf_r = g.place(single_frame, shift(1350.0, -1690.0, 1720.0))?;

    // 5) Paneled front door with beveled slab.
    let door_slab = g.cuboid(BoxOpts { size: mm3(880.0, 120.0, 1810.0), round: mm(18.0), name: Some("door") })?;
    let door = g.place(door_slab, shift(0.0, -1660.0, 1280.0))?;

    // 6) Stone chimney stack with cylindrical flue hole (genus 1) and beveled crown.
    let chim_shaft = g.cuboid(BoxOpts { size: mm3(680.0, 680.0, 2400.0), round: mm(25.0), .. })?;
    let chim_crown = g.cuboid(BoxOpts { size: mm3(820.0, 820.0, 180.0), round: mm(25.0), .. })?;
    let chim_crown = g.place(chim_crown, shift(0.0, 0.0, 1180.0))?;
    let chim_outer = g.union(chim_shaft, chim_crown)?;
    let flue = g.cylinder(CylinderOpts { radius: mm(160.0), height: mm(2900.0), .. })?;
    let chimney = g.difference(chim_outer, flue)?;
    let chimney = g.place(chimney, shift(1450.0, 900.0, 3550.0))?;

    let (found_id, _) = model.add(PartOpts { solid: foundation, name: "foundation" })?;
    let (walls_id, _) = model.add(PartOpts { solid: walls, name: "walls" })?;
    let (roof_id, _) = model.add(PartOpts { solid: roof, name: "roof" })?;
    let (wf_l_id, _) = model.add(PartOpts { solid: wf_l, name: "window_frame_left" })?;
    let (wf_r_id, _) = model.add(PartOpts { solid: wf_r, name: "window_frame_right" })?;
    let (door_id, _) = model.add(PartOpts { solid: door, name: "door" })?;
    let (chim_id, _) = model.add(PartOpts { solid: chimney, name: "chimney" })?;

    model.set_material(found_id, MaterialRole(2))?;
    model.set_material(walls_id, MaterialRole(0))?;
    model.set_material(roof_id, MaterialRole(1))?;
    model.set_material(wf_l_id, MaterialRole(3))?;
    model.set_material(wf_r_id, MaterialRole(3))?;
    model.set_material(door_id, MaterialRole(4))?;
    model.set_material(chim_id, MaterialRole(2))?;

    // Per-part budgets: explicit caps on simpler parts, higher allocation weight on hero walls.
    model.set_budget(found_id, PartBudget::max_triangles(1_500))?;
    model.set_budget(
        walls_id,
        PartBudget::Detailed {
            max_triangles: None,
            tolerance: Some(mm(2.5)),
            min_feature: Some(mm(64.0)),
            max_error: Some(mm(6.0)),
            weight: 1.5,
        },
    )?;
    model.set_budget(door_id, PartBudget::max_triangles(1_200))?;

    Ok(())
}

fn write_out(dir: &str, name: &str, bytes: &[u8]) {
    std::fs::write(format!("{dir}/{name}"), bytes).expect(name);
}

fn main() -> Result<(), ModelError> {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let dir = std::env::args().nth(1).unwrap_or_else(|| String::from("target/cabin"));
    std::fs::create_dir_all(&dir).expect("create output dir");

    let mut model = Model::new(ModelOpts { alloc, .. });
    build_cabin(&mut model)?;

    let mut cache = PieceCache::new(PieceCacheOpts { alloc, .. });
    let t0 = Instant::now();
    let asset = build_game_asset(
        model.graph_ref(),
        model.parts(),
        GameAssetOpts {
            alloc,
            scratch: alloc,
            preset: AssetPreset::Environment,
            total_budget: Some(TriangleBudget(27_000)),
            materials: &MATERIALS,
            lods: Some(ExportLodOpts { ratio: 0.5, fewest_triangles: 64, .. }),
            colliders: Some(ColliderMode::ConvexHull),
            ..
        },
        Some(&mut cache),
    )?;
    let build_ms = t0.elapsed().as_secs_f64() * 1e3;

    println!("=== Blaze CAD Game-Asset Cabin Report ({build_ms:.1} ms) ===");
    print!("{}", Shown(&asset.report));

    write_out(&dir, "cabin.glb", &asset.glb[..]);

    // WYSIWYG previews rendered directly from the ACTUAL exported, decimated, GPU-ready meshes!
    let view = View {
        eye: mm3(8200.0, -9600.0, 5600.0),
        target: mm3(0.0, -200.0, 1900.0),
        projection: Projection::Perspective { fov: Angle::degrees(36.0) },
        width: 640,
        height: 440,
        ..
    };

    let t1 = Instant::now();
    let mesh_pic = asset.preview(
        &MATERIALS,
        view,
        MeshPictureOpts { alloc, scratch: alloc, supersample: 2, .. },
    )?;
    let mesh_png = mesh_pic.png(blaze_cad_out::PngOpts {
        level: blaze_deflate::Level::Default,
        limits: blaze_cad_out::Limits::SHIPPED,
        alloc,
        scratch: alloc,
    })?;
    write_out(&dir, "cabin_mesh.png", &mesh_png[..]);

    let wire_pic = asset.preview(
        &MATERIALS,
        view,
        MeshPictureOpts {
            alloc,
            scratch: alloc,
            wireframe: Wireframe::Overlay { width_px: 1.0, colour: [22, 26, 34] },
            supersample: 2,
            ..
        },
    )?;
    let wire_png = wire_pic.png(blaze_cad_out::PngOpts {
        level: blaze_deflate::Level::Default,
        limits: blaze_cad_out::Limits::SHIPPED,
        alloc,
        scratch: alloc,
    })?;
    write_out(&dir, "cabin_wireframe.png", &wire_png[..]);

    let parts_pic = asset.preview_part_colors(
        view,
        MeshPictureOpts {
            alloc,
            scratch: alloc,
            wireframe: Wireframe::Overlay { width_px: 0.9, colour: [30, 32, 38] },
            ..
        },
    )?;
    let parts_png = parts_pic.png(blaze_cad_out::PngOpts {
        level: blaze_deflate::Level::Default,
        limits: blaze_cad_out::Limits::SHIPPED,
        alloc,
        scratch: alloc,
    })?;
    write_out(&dir, "cabin_parts.png", &parts_png[..]);
    let preview_ms = t1.elapsed().as_secs_f64() * 1e3;

    println!(
        "Wrote {dir}/cabin.glb ({} bytes, {} tris) + 3 exported-mesh previews in {preview_ms:.1} ms",
        asset.glb.len(),
        asset.triangles(),
    );
    Ok(())
}
