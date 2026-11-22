//! What a picture costs, for the README's table: segments looked at (each one `eval_box`),
//! nanoseconds a segment, rays and hits a second, by picture size and model size; and the PNG
//! written at the end. Run with `cargo run --release -p blaze-cad-picture --example cost`.
//!
//! The model is a row of `n` frames (rounded boxes) on a keel with a sphere at each end.

#![feature(default_field_values)]

use std::time::Instant;

use blaze_cad_field::{BoxOpts, CompileOpts, Graph, GraphOpts, Outside, Placement, Solid, SphereOpts, Tape, compile};
use blaze_cad_picture::{PictureOpts, Projection, View, render};
use blaze_own::Alloc;
use blaze_units::{Angle, Length};

fn model<'a>(alloc: Alloc<'a>, frames: usize) -> Tape<'a> {
    let mut g = Graph::new(GraphOpts { alloc, .. });
    let keel = g.cuboid(BoxOpts { size: [Length::mm(40.0 * frames as f64), Length::mm(20.0), Length::mm(20.0)], .. }).expect("a keel");
    let mut acc: Solid<Outside> = keel.weaken::<Outside>();
    for i in 0..frames {
        let x = Length::mm(40.0 * i as f64 - 20.0 * frames as f64 + 20.0);
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

fn main() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    println!("| frames | instructions | pixels | segments | ns/segment | rays/s | hits | most segments a ray |");
    println!("|---|---|---|---|---|---|---|---|");
    let mut last = None;
    for frames in [4usize, 16, 64] {
        let tape = model(alloc, frames);
        let reach = 30.0 * frames as f64 + 200.0;
        for side in [128u32, 512] {
            let view = View {
                eye: [Length::mm(reach), Length::mm(-reach), Length::mm(reach * 0.6)],
                target: [Length::ZERO; 3],
                projection: Projection::Perspective { fov: Angle::degrees(35.0) },
                width: side,
                height: side,
                ..
            };
            let t0 = Instant::now();
            let pic = render(&tape, view, PictureOpts { alloc, scratch: alloc, .. }).expect("a picture");
            let ns = t0.elapsed().as_nanos() as f64;
            let s = pic.stats;
            println!(
                "| {frames} | {} | {} | {} | {:.0} | {:.3e} | {} | {} |",
                tape.len(),
                u64::from(side) * u64::from(side),
                s.boxes,
                ns / s.boxes.max(1) as f64,
                s.rays as f64 / (ns / 1e9),
                s.hits,
                s.most_steps,
            );
            last = Some(pic.png(blaze_cad_out::PngOpts { level: blaze_deflate::Level::Default, limits: blaze_cad_out::Limits::SHIPPED, alloc, scratch: alloc }).expect("a png").len());
        }
    }
    println!("the last picture's PNG: {} bytes", last.unwrap_or(0));
}
