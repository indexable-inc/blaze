//! What writing a picture costs, for the README's table: PNG bytes and megapixels a second by
//! size, content and compression level. (A glTF binary's cost is measured from real meshes in
//! blaze-cad's `cost` example.) Run with `cargo run --release -p blaze-cad-out --example cost`.
//!
//! Two contents: a flat background with a shaded disc (what a CAD picture is: most of it one
//! colour) and noise (the worst case for the compressor).

use std::time::Instant;

use blaze_cad_out::{Image, Limits, PngKind, PngOpts, png};

fn picture(side: u32, noise: bool) -> Vec<u8> {
    let mut px = Vec::with_capacity((side * side * 4) as usize);
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let c = f64::from(side) / 2.0;
    for y in 0..side {
        for x in 0..side {
            if noise {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                px.extend_from_slice(&state.to_le_bytes()[..4]);
            } else {
                let (dx, dy) = (f64::from(x) - c, f64::from(y) - c);
                let r = (dx * dx + dy * dy).sqrt() / c;
                if r < 0.8 {
                    let shade = (200.0 * (1.0 - r)) as u8;
                    px.extend_from_slice(&[shade, shade, shade.saturating_add(20), 255]);
                } else {
                    px.extend_from_slice(&[246, 246, 244, 255]);
                }
            }
        }
    }
    px
}

fn main() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    println!("| pixels | content | level | bytes | Mpx/s |");
    println!("|---|---|---|---|---|");
    for side in [256u32, 1024, 2048] {
        for noise in [false, true] {
            let px = picture(side, noise);
            for (name, level) in [("store", blaze_deflate::Level::Store), ("fast", blaze_deflate::Level::Fast), ("default", blaze_deflate::Level::Default)] {
                let t = Instant::now();
                let out = png(
                    Image { width: side, height: side, kind: PngKind::Rgba8, pixels: &px },
                    PngOpts { level, limits: Limits::SHIPPED, alloc, scratch: alloc },
                )
                .expect("a png");
                let s = t.elapsed().as_secs_f64();
                let mpx = f64::from(side) * f64::from(side) / 1e6;
                println!("| {side} x {side} | {} | {name} | {} | {:.1} |", if noise { "noise" } else { "disc" }, out.len(), mpx / s);
            }
        }
    }
}
