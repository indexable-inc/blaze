// Own program x0-int-15-add-wraps-release (front2), written by the wrong-answers lane
// (2026-11-16, WRONG-NOTES.md "Runner 6", decision D101): x0-int-14's `255u8 + 1` with
// `overflow-checks = false` under this package's `[profile.dev]`, the release setting.
//
// The Reference, "Overflow" ([expr.operator.int-overflow]): "When the programmer has enabled
// debug_assert! assertions (for example, by enabling a non-optimized build), implementations
// must insert dynamic checks that panic on overflow. Other kinds of builds may result in panics
// or silently wrapped values on overflow, at the implementation's discretion." rustc with
// `-C overflow-checks=off` wraps (two's complement): 255u8 + 1 == 0, so `main` returns 42.
// A panic at `panic_const_add_overflow` is the wrong answer here: the manifest's setting was
// not taken.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u8 {}
impl Copy for i32 {}

#[lang = "add"]
trait Add<Rhs = Self> {
    type Output;
    fn add(self, rhs: Rhs) -> Self::Output;
}

impl Add for u8 {
    type Output = u8;
    fn add(self, rhs: u8) -> u8 {
        self + rhs
    }
}

impl Add for i32 {
    type Output = i32;
    fn add(self, rhs: i32) -> i32 {
        self + rhs
    }
}

unsafe extern "C" {
    fn _Unwind_RaiseException(exception: *mut u8) -> i32;
}

#[lang = "panic_const_add_overflow"]
fn panic_const_add_overflow() -> ! {
    unsafe {
        _Unwind_RaiseException(0 as *mut u8);
    }
    loop {}
}

fn l() -> u8 {
    255
}

fn r() -> u8 {
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    (l() + r()) as i32 + 42
}
