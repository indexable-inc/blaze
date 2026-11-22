//! simd: `#![blaze::simd]` turns on `core::simd` with no feature gate, and `simd::native_lanes!`. A working layer accepts it.
//! expect: accept
#![blaze::simd]

fn lanes() -> usize {
    core::simd::native_lanes!(u8)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    lanes() as i32 + 26
}
