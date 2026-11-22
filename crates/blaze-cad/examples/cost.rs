//! What the library's pieces cost on the hull section, for the README's table: cells (boxes)
//! visited, nanoseconds a cell, pieces recomputed after a small edit, triangles a second.
//! Run with `cargo run --release -p blaze-cad --example cost`.
//!
//! The small edit is the hatch cover made one millimetre smaller all round, in a model rebuilt
//! from scratch (the graph is recomputed from the text, as every run does) against the same
//! piece cache: only the cover's pieces should be recomputed, and moving the cover none.

#![feature(default_field_values)]

#[path = "shared/hull_model.rs"]
mod hull_model;

use std::time::Instant;

use blaze_cad::prelude::*;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn main() -> Result<(), ModelError> {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let tolerance = Length::mm(1.0);
    let mut cache = PieceCache::new(PieceCacheOpts { alloc, .. });

    println!("| run | part | cells | ns/cell | pieces | recomputed | reused | triangles | triangles/s |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for (run, gap) in [("cold", 10.0), ("cover 1 mm smaller", 11.0), ("cover moved 20 mm", 11.0)] {
        let mut model = Model::new(ModelOpts { alloc, .. });
        let hull = hull_model::build(&mut model, gap)?;
        if run == "cover moved 20 mm" {
            let moved = Placement::shift(Length::mm(20.0), Length::ZERO, Length::ZERO).then_after(&hull.cover.placement);
            model.move_part(hull.cover_id, moved)?;
        }
        for part in model.parts() {
            let t = Instant::now();
            let m = mesh_part(model.graph_ref(), part, MeshPartOpts { alloc, scratch: alloc, tolerance, .. }, Some(&mut cache))?;
            let ns = ms(t) * 1e6;
            let s = m.stats;
            println!(
                "| {run} | {} | {} | {:.0} | {} | {} | {} | {} | {:.3e} |",
                part.name.as_str(),
                s.boxes,
                ns / s.boxes.max(1) as f64,
                s.pieces,
                s.recomputed,
                s.reused,
                s.triangles,
                s.triangles as f64 / (ns / 1e9),
            );
        }
    }

    let mut model = Model::new(ModelOpts { alloc, .. });
    let hull = hull_model::build(&mut model, 10.0)?;
    println!();
    println!("| check | boxes | ns/box | ms | verdict |");
    println!("|---|---|---|---|---|");
    let walls: [(&str, &Part<Inside>); 3] = [("min_wall skin", &hull.skin), ("min_wall frames", &hull.frames), ("min_wall deck", &hull.deck)];
    for (name, part) in walls {
        let t = Instant::now();
        let c = min_wall(model.graph_ref(), part, MinWallOpts { wall: Length::mm(6.0), scratch: alloc, .. })?;
        let e = ms(t);
        println!("| {name} | {} | {:.0} | {e:.1} | {} |", c.boxes, e * 1e6 / c.boxes.max(1) as f64, Shown(&c.verdict));
    }
    // A moved part keeps its check's key (the check reads the part's own frame), so its record
    // is read, not recomputed.
    let shifted = Part { placement: Placement::shift(Length::mm(20.0), Length::ZERO, Length::ZERO).then_after(&hull.cover.placement), ..hull.cover };
    let before = min_wall(model.graph_ref(), &hull.cover, MinWallOpts { wall: Length::mm(6.0), scratch: alloc, .. })?;
    let after = min_wall(model.graph_ref(), &shifted, MinWallOpts { wall: Length::mm(6.0), scratch: alloc, .. })?;
    println!("| min_wall cover, key kept after a 20 mm move: {} | {} | - | - | {} |", before.key == after.key, before.boxes, Shown(&before.verdict));
    let t = Instant::now();
    let cover = placed_tape(model.graph(), &hull.cover, Detail::Shape, alloc)?;
    let deck = placed_tape(model.graph(), &hull.deck, Detail::Shape, alloc)?;
    let c = no_interference(&cover, &deck, InterferenceOpts { clearance: Length::mm(5.0), scratch: alloc, .. })?;
    println!("| no_interference cover/deck | {} | - | {:.1} | {} |", c.boxes, ms(t), Shown(&c.verdict));
    let whole = model.assembly()?;
    let envelope = hull_model::envelope(&mut model)?;
    let t = Instant::now();
    let c = fits_within(model.graph(), whole, envelope.weaken(), FitOpts { scratch: alloc, tolerance: Length::mm(1.0), .. })?;
    let e = ms(t);
    println!("| fits_within envelope | {} | {:.0} | {e:.1} | {} |", c.boxes, e * 1e6 / c.boxes.max(1) as f64, Shown(&c.verdict));

    println!();
    println!("| picture | rays/s | segments | ns/segment | ms |");
    println!("|---|---|---|---|---|");
    for side in [256u32, 1024] {
        let t = Instant::now();
        let pic = picture(model.graph_ref(), whole, hull_model::view(side, side), PictureOpts { alloc, scratch: alloc, .. })?;
        let e = ms(t);
        let s = pic.stats;
        println!("| {side} x {side} | {:.3e} | {} | {:.0} | {e:.1} |", s.rays as f64 / (e / 1e3), s.boxes, e * 1e6 / s.boxes.max(1) as f64);
    }

    let t = Instant::now();
    let out = export_glb(model.graph_ref(), model.parts(), ExportOpts { alloc, scratch: alloc, tolerance, .. }, Some(&mut cache))?;
    let meshed: u64 = out.counts.iter().map(|c| u64::from(c.meshed)).sum();
    println!();
    println!(
        "glb from warm pieces: {} bytes, {meshed} triangles meshed, {} written (simplified to the tolerance), in {:.1} ms",
        out.glb.len(),
        out.triangles(),
        ms(t)
    );
    Ok(())
}
