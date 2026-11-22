//! What the interpreters cost, for the README's table: points a second by model size and lane
//! count, nanoseconds a point an instruction, boxes a second, and what a simplification costs
//! and saves. Run with `cargo run --release -p blaze-cad-field --example cost`.
//!
//! The model is a balanced union of `2^k` placed spheres, rounded boxes and cylinders in a row,
//! the shape of a part with many features; the points are a regular grid over its box.

#![feature(default_field_values)]

use std::time::Instant;

use blaze_cad_field::{
    BoxOpts, CompileOpts, CylinderOpts, Graph, GraphOpts, Lanes, Placement, Points, Scratch, Solid, SphereOpts, Bound, compile, eval_box,
    eval_points, simplify,
};
use blaze_math::{Aabb3, Vec3};
use blaze_units::Length;

fn model(g: &mut Graph<'_>, parts: usize) -> Solid<Bound> {
    let mut layer: Vec<Solid<Bound>> = Vec::new();
    for i in 0..parts {
        let x = Length::nm(i as i64 * 12_000_000);
        let s = match i % 3 {
            0 => g.sphere(SphereOpts { radius: Length::nm(4_000_000), .. }).expect("a sphere").weaken::<Bound>(),
            1 => g
                .cuboid(BoxOpts { size: [Length::nm(8_000_000); 3], round: Length::nm(1_000_000), .. })
                .expect("a box")
                .weaken::<Bound>(),
            _ => g.cylinder(CylinderOpts { radius: Length::nm(3_000_000), height: Length::nm(9_000_000), .. }).expect("a cylinder").weaken::<Bound>(),
        };
        layer.push(g.place(s, Placement::shift(x, Length::ZERO, Length::ZERO)).expect("a placement"));
    }
    while layer.len() > 1 {
        let mut next = Vec::new();
        for pair in layer.chunks(2) {
            next.push(if pair.len() == 2 { g.union(pair[0], pair[1]).expect("a union") } else { pair[0] });
        }
        layer = next;
    }
    layer[0]
}

fn main() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    println!("| parts | instructions | lanes | points/s | ns/point | ns/point/op |");
    println!("|---|---|---|---|---|---|");
    let n = 1usize << 16;
    for k in [0u32, 2, 4, 6, 8] {
        let mut g = Graph::new(GraphOpts { alloc, .. });
        let root = model(&mut g, 1 << k);
        let tape = compile(&g, root.id(), CompileOpts { alloc, .. }).expect("a tape");
        let b = g.aabb(root.id());
        let (mut xs, mut ys, mut zs) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
        for i in 0..n {
            let u = (i % 64) as f64 / 63.0;
            let v = ((i / 64) % 32) as f64 / 31.0;
            let w = (i / 2048) as f64 / 31.0;
            xs.push(b.lo.x + (b.hi.x - b.lo.x) * u);
            ys.push(b.lo.y + (b.hi.y - b.lo.y) * v);
            zs.push(b.lo.z + (b.hi.z - b.lo.z) * w);
        }
        let pts = Points { x: &xs, y: &ys, z: &zs };
        let mut out = vec![0.0; n];
        let mut s = Scratch::new(alloc);
        for (lanes, name) in [(Lanes::One, "1"), (Lanes::Four, "4"), (Lanes::Eight, "8")] {
            eval_points(&tape, pts, &mut out, lanes, &mut s).expect("columns of one length");
            let reps = 8;
            let t0 = Instant::now();
            for _ in 0..reps {
                eval_points(&tape, pts, &mut out, lanes, &mut s).expect("columns of one length");
            }
            let ns = t0.elapsed().as_nanos() as f64 / (reps * n) as f64;
            println!("| {} | {} | {name} | {:.3e} | {ns:.2} | {:.3} |", 1 << k, tape.len(), 1e9 / ns, ns / tape.len() as f64);
        }
        // Boxes and simplification: an 8 x 8 x 8 grid of cells over the model's box.
        let cells = 8usize;
        let side = (b.hi - b.lo) * (1.0 / cells as f64);
        let t0 = Instant::now();
        let (mut boxes, mut kept, mut surface) = (0usize, 0usize, 0usize);
        let mut simplify_ns = 0u128;
        for i in 0..cells * cells * cells {
            let c = Vec3::new((i % cells) as f64, ((i / cells) % cells) as f64, (i / (cells * cells)) as f64);
            let lo = b.lo + side.mul_each(c);
            let cell = Aabb3::new(lo, lo + side);
            let (iv, choices) = eval_box(&tape, &cell, &mut s);
            boxes += 1;
            if iv.lo() <= 0.0 && iv.hi() >= 0.0 {
                let t1 = Instant::now();
                let small = simplify(&tape, choices, alloc).expect("a simplified tape");
                simplify_ns += t1.elapsed().as_nanos();
                surface += 1;
                kept += small.len();
            }
        }
        let total = t0.elapsed().as_nanos() as f64;
        println!(
            "parts {}: {:.0} ns a box (with simplification), simplify {:.0} ns a surface cell, surface cells keep {:.1}% of {} instructions",
            1 << k,
            total / boxes as f64,
            simplify_ns as f64 / surface.max(1) as f64,
            100.0 * kept as f64 / (surface.max(1) * tape.len()) as f64,
            tape.len(),
        );
    }
}
