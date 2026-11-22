//! A hull section built, checked, weighed, pictured and written out: the library end to end.
//! Run with `cargo run --release -p blaze-cad --example hull -- <output directory>`; it writes
//! `hull.png` and `hull.glb` there (default `target/hull`).
//!
//! The checks: every structural part's walls at least 6 mm (pass), the skin's at least 14 mm
//! (it is 10: a finding), the hatch cover 5 mm clear of the deck (it is 10 in from the opening),
//! and the section inside an 800 x 800 x 500 mm transport envelope.

#![feature(default_field_values)]

#[path = "shared/hull_model.rs"]
mod hull_model;

use std::time::Instant;

use blaze_cad::prelude::*;
use blaze_cad::{Checked, ThinWall};

fn report(what: &str, c: &Checked<ThinWall>, ms: f64) {
    println!("{what}: {} ({} boxes, {} samples, {} points, {ms:.1} ms)", Shown(&c.verdict), c.boxes, c.samples, c.points);
}

fn main() -> Result<(), ModelError> {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let dir = std::env::args().nth(1).unwrap_or_else(|| String::from("target/hull"));
    std::fs::create_dir_all(&dir).expect("the output directory");

    let mut model = Model::new(ModelOpts { alloc, .. });
    let hull = hull_model::build(&mut model, 10.0)?;

    // Walls.
    let walls: [(&str, &Part<Inside>, f64); 4] =
        [("skin, 6 mm", &hull.skin, 6.0), ("frames, 6 mm", &hull.frames, 6.0), ("deck, 6 mm", &hull.deck, 6.0), ("skin, 14 mm", &hull.skin, 14.0)];
    for (name, part, wall) in walls {
        let t = Instant::now();
        let c = min_wall(model.graph_ref(), part, MinWallOpts { wall: Length::mm(wall), scratch: alloc, .. })?;
        report(name, &c, t.elapsed().as_secs_f64() * 1e3);
    }

    // The cover against the deck, where they sit.
    let cover = placed_tape(model.graph(), &hull.cover, Detail::Shape, alloc)?;
    let deck = placed_tape(model.graph(), &hull.deck, Detail::Shape, alloc)?;
    let clear = no_interference(&cover, &deck, InterferenceOpts { clearance: Length::mm(5.0), scratch: alloc, .. })?;
    println!("hatch cover 5 mm clear of the deck: {}", Shown(&clear.verdict));

    // The whole section in the transport envelope.
    let whole = model.assembly()?;
    let envelope = hull_model::envelope(&mut model)?;
    let fit = fits_within(model.graph(), whole, envelope.weaken(), FitOpts { scratch: alloc, tolerance: Length::mm(1.0), .. })?;
    println!("fits the 800 x 800 x 500 mm envelope: {} ({} boxes)", Shown(&fit.verdict), fit.boxes);

    // Mass, part by part, in steel, and each stage timed: the solid compiled to a tape, meshed,
    // and simplified as the export does. Meshing is the octree and the contour in one call: the
    // mesher takes no clock (it is sans-IO), so the two are not timed apart here; its counts say
    // how its work divides (cells looked at, leaves contoured).
    let steel = Density::kg_per_m3(7850.0);
    let tolerance = Length::mm(1.0);
    let ms = |from: Instant, to: Instant| (to - from).as_secs_f64() * 1e3;
    let mut total = 0.0;
    for part in model.parts() {
        let t0 = Instant::now();
        let tape = part_tape(model.graph_ref(), part, Detail::Full, alloc)?;
        let t1 = Instant::now();
        let m = blaze_cad_mesh::mesh(&tape, blaze_cad_mesh::MeshOpts { alloc, scratch: alloc, tolerance, .. }).map_err(ModelError::Mesh)?;
        let t2 = Instant::now();
        let d = blaze_cad_mesh::decimate(
            blaze_cad_mesh::Source::Verified(m.verified()),
            blaze_cad_mesh::DecimateOpts {
                target: blaze_cad_mesh::Target::Error(tolerance),
                cos_crease: blaze_cad_mesh::COS_32_DEGREES,
                limits: blaze_cad_mesh::MeshLimits::SHIPPED,
                alloc,
                scratch: alloc,
                ..
            },
        )?;
        let t3 = Instant::now();
        let mp = mass_properties(&m.verified(), steel);
        total += mp.mass.as_kg();
        println!(
            "{}: {:.3} kg, {:.2} litres, centre ({:.1}, {:.1}, {:.1}) mm in its own frame",
            part.name.as_str(),
            mp.mass.as_kg(),
            mp.volume.as_m3() * 1e3,
            mp.centre[0].as_m() * 1e3,
            mp.centre[1].as_m() * 1e3,
            mp.centre[2].as_m() * 1e3,
        );
        let s = d.stats;
        println!(
            "  compile {:.1} ms; mesh {:.1} ms ({} cells, {} leaves, {} triangles); simplify {:.1} ms ({} triangles within {}: {} collapses, {} maps reading {} corners, at most {} in one, {} merges kept and {} not, {} frozen, {} refused by the error and {} by the region)",
            ms(t0, t1),
            ms(t1, t2),
            m.stats.boxes,
            m.stats.leaves,
            m.stats.triangles,
            ms(t2, t3),
            d.mesh.view().triangle_count(),
            Shown(&d.error_bound),
            s.collapses,
            s.maps,
            s.corners,
            s.most_corners,
            s.merged,
            s.not_merged,
            s.frozen,
            s.over_error,
            s.over_region,
        );
    }
    println!("the section: {total:.2} kg of steel");

    // A picture of it.
    let pic = picture(model.graph_ref(), whole, hull_model::view(960, 640), PictureOpts { alloc, scratch: alloc, .. })?;
    let png = pic.png(blaze_cad_out::PngOpts { level: blaze_deflate::Level::Default, limits: blaze_cad_out::Limits::SHIPPED, alloc, scratch: alloc })?;
    std::fs::write(format!("{dir}/hull.png"), &png[..]).expect("hull.png written");

    // And the file a viewer or a game engine reads (it meshes and simplifies every part again, as
    // above, then writes the glTF).
    let t_write = Instant::now();
    let out = export_glb(model.graph_ref(), model.parts(), ExportOpts { alloc, scratch: alloc, tolerance, density: Some(steel), .. }, None)?;
    println!("export: {:.1} ms", t_write.elapsed().as_secs_f64() * 1e3);
    for (part, c) in model.parts().iter().zip(out.counts.iter()) {
        println!(
            "{}: {} triangles meshed, {} written (simplified within {}, vertices within {})",
            part.name.as_str(),
            c.meshed,
            c.written,
            Shown(&c.simplified_within),
            Shown(&c.vertex_bound),
        );
    }
    std::fs::write(format!("{dir}/hull.glb"), &out.glb[..]).expect("hull.glb written");
    println!(
        "wrote {dir}/hull.png ({} bytes) and {dir}/hull.glb ({} bytes, {} triangles)",
        png.len(),
        out.glb.len(),
        out.triangles()
    );
    Ok(())
}
