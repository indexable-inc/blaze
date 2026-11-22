//! differentiable: `#[blaze::differentiable]`, a function blaze can be asked the derivative of. A working layer accepts it.
//! expect: accept

#[blaze::differentiable]
fn square(x: f64) -> f64 {
    x * x
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    square(6.0) as i32 + 6
}
