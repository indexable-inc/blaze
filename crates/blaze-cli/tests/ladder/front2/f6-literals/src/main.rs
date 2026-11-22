// Own program f6-literals (front2): an `f64` computation from float literals (each read from
// its text by the seam's `float_lit`, at the type the check gave it) decides the status, and
// a C string literal's bytes (from the seam's strings; the builder adds the nul) are checked
// through the slice its `CStr` wraps: index 2 in bounds is the length with its nul, 3.
// `no_core`: `CStr` is declared here as core declares it, by its lang item.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for u8 {}
impl Copy for f64 {}

#[lang = "CStr"]
struct CStr {
    inner: [u8],
}

#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let x: f64 = 1.5;
    let y = x * 4.0 + 0.25;
    // The `CStr`'s own slice, by a cast of its pointer (the field of an unsized struct is
    // not what this rung tests).
    let bytes: &[u8] = unsafe { &*(c"ab" as *const CStr as *const [u8]) };
    let text = bytes[0] == 97 && bytes[1] == 98 && bytes[2] == 0;
    // 6.25 lies between; a literal read as 0 or as an integer's bits would not.
    if y > 6.2 && y < 6.3 && text { 42 } else { 1 }
}
