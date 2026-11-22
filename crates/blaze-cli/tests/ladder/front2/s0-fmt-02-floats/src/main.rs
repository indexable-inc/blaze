//! s0-fmt-02: floats through `std`'s formatting.
//!
//! First to need: core's flt2dec (shortest and exact modes; library/core/src/num/flt2dec) and
//! core/src/fmt/float.rs. `Display` is `float_to_decimal_display`: the shortest digits that read
//! back to the same value, never an exponent, no `.0` (min precision 0); `Debug` is
//! `float_to_general_debug`: min precision 1, and the exponent form when `abs != 0.0 && abs < 1e-4`
//! or `abs >= 1e16` (`impl_general_format!`). A negative zero keeps its sign (`Sign::Minus`).
//! `16777217.0f32` rounds to the nearest `f32`, 16777216 (ties to even: the Reference,
//! "Floating-point literal expressions"). Casts, the Reference "Numeric cast": float to integer
//! rounds toward zero, saturates, and NaN gives 0. `sqrt` is correctly rounded (IEEE 754).

fn main() {
    println!("{}", 1.5);
    println!("{}", 1.0);
    println!("{:?}", 1.0);
    println!("{}", 0.1 + 0.2);
    println!("{:.2}", 3.14159);
    println!("{:8.3}|", 2.5);
    println!("{:<8.1}|", -1.24);
    println!("{:e}", 1234.5);
    println!("{}", 1e21);
    println!("{:?}", 1e16);
    println!("{:?}", 1e15);
    println!("{:?}", 0.0001);
    println!("{:?}", 0.00001);
    println!("{}", 0.00001);
    println!("{} {} {}", f64::NAN, f64::INFINITY, -f64::INFINITY);
    println!("{} {:?}", -0.0, -0.0);
    println!("{}", 0.1f32);
    println!("{}", 16777217.0f32);
    println!("{:+.1}", 2.0);
    println!("{}", 2.0f64.sqrt());
    println!("{}", 7.0 / 2.0);
    let big = std::hint::black_box(300.5f64);
    let neg = std::hint::black_box(-5.0f64);
    let nan = std::hint::black_box(f64::NAN);
    println!("{} {} {} {}", 255.9 as u8, big as u8, neg as u8, nan as i32);
}
