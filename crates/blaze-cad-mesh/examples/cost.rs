//! What the mesher costs, for the README's table: cells looked at, nanoseconds a cell, triangles
//! a second from scratch, and what a small edit recomputes with the session's piece cache. Run
//! with `cargo run --release -p blaze-cad-mesh --example cost`.
//!
//! The model is a row of `n` frames (boxes) along a keel (a long box) with a sphere at each end,
//! the shape of a hull's skeleton; the edit moves one frame by a millimetre.

#![feature(default_field_values)]

use std::time::Instant;

use blaze_cad_field::{BoxOpts, CompileOpts, Graph, GraphOpts, Outside, Placement, Solid, SphereOpts, Tape, compile};
use blaze_cad_mesh::{MeshOpts, Meshed, PieceCache, PieceCacheOpts, mesh, mesh_cached};
use blaze_own::Alloc;
use blaze_units::Length;

fn model<'a>(alloc: Alloc<'a>, frames: usize, moved: Option<usize>) -> Tape<'a> {
    let mut g = Graph::new(GraphOpts { alloc, .. });
    let keel = g.cuboid(BoxOpts { size: [Length::mm(40.0 * frames as f64), Length::mm(20.0), Length::mm(20.0)], .. }).expect("a keel");
    let mut acc: Solid<Outside> = keel.weaken::<Outside>();
    for i in 0..frames {
        let dx = if moved == Some(i) { 1.0 } else { 0.0 };
        let x = Length::mm(40.0 * i as f64 - 20.0 * frames as f64 + 20.0 + dx);
        let frame = g.cuboid(BoxOpts { size: [Length::mm(6.0), Length::mm(160.0), Length::mm(90.0)], round: Length::mm(2.0), .. }).expect("a frame");
        let frame = g.place(frame, Placement::shift(x, Length::ZERO, Length::mm(40.0))).expect("a placement");
        acc = g.union(acc, frame).expect("a union");
    }
    for end in [-1.0, 1.0] {
        let ball = g.sphere(SphereOpts { radius: Length::mm(30.0), .. }).expect("a sphere");
        let ball = g.place(ball, Placement::shift(Length::mm(end * 20.0 * frames as f64), Length::ZERO, Length::ZERO)).expect("a placement");
        acc = g.union(acc, ball).expect("a union");
    }
    compile(&g, acc.id(), CompileOpts { alloc, .. }).expect("a tape")
}

fn row(what: &str, m: &Meshed<'_>, ns: f64) {
    let s = m.stats;
    println!(
        "| {what} | {} | {} | {:.0} | {} | {} | {} | {:.3e} | {:?} |",
        s.boxes,
        s.leaves,
        ns / s.boxes.max(1) as f64,
        s.pieces,
        s.recomputed,
        s.triangles,
        f64::from(s.triangles) / (ns / 1e9),
        m.vertex_bound,
    );
}

fn main() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    println!("| run | cells looked at | surface leaves | ns/cell | pieces | recomputed | triangles | triangles/s | vertex bound |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for (frames, tolerance) in [(8usize, 4.0f64), (8, 2.0), (32, 2.0)] {
        let o = MeshOpts { alloc, scratch: alloc, tolerance: Length::mm(tolerance), .. };
        let before = model(alloc, frames, None);
        let after = model(alloc, frames, Some(frames / 2));

        let t0 = Instant::now();
        let scratch = mesh(&before, o).expect("a mesh");
        row(&format!("{frames} frames, {tolerance} mm, from scratch"), &scratch, t0.elapsed().as_nanos() as f64);

        let mut cache = PieceCache::new(PieceCacheOpts { alloc, .. });
        mesh_cached(&before, o, &mut cache).expect("a mesh");
        let t1 = Instant::now();
        let edited = mesh_cached(&after, o, &mut cache).expect("a mesh");
        row(&format!("{frames} frames, {tolerance} mm, one frame moved, cached"), &edited, t1.elapsed().as_nanos() as f64);
    }
}
