//@ expect: accept
//@ crate: bin
//@ edition: 2021
// float constants: arithmetic, comparison, and saturating float-to-int casts (NaN to 0, out of range to the edge)
const A: f64 = 1.5 * 2.0;
const B: f32 = 0.1 + 0.2;
const C: i32 = 3.9f64 as i32;
const D: u8 = -1.0f64 as u8;
const E: i32 = f64::NAN as i32;
const F: u8 = 300.7f32 as u8;
const G: f64 = 7u32 as f64 / 2.0;
const H: bool = 0.1 + 0.2 == 0.3;
const I: i64 = f64::INFINITY as i64;
const J: f32 = -0.0;

fn main() {
    println!("{} {} {} {} {} {} {} {} {} {:?}", A, B, C, D, E, F, G, H, I, J);
}
