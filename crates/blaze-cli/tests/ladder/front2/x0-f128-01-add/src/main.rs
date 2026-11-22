//! x0-f128-01-add: an `f128` add and a cast to `i32` run through the program's own
//! compiler_builtins routines (port27/tempo-run/INTRINSICS.md 3.5, P5b): `1.5 + 1.5` is `3.0`,
//! and the process exits with status 3.
//!
//! The pinned core (library/core/src/primitive_docs.rs): `f128` is "A 128-bit floating-point type
//! (specifically, the "binary128" type defined in IEEE 754-2008)"; 1.5 and 3.0 are exact in it.
//! The Reference, "Numeric cast": "Casting from a float to an integer will round the float
//! towards zero", so `3.0 as i32` is 3. blaze's Reference tier has no `f128` arithmetic: legalize
//! makes the add a call of `__addtf3` and the cast a call of `__fixtfdi` (with its saturating
//! comparisons), each bound to compiler_builtins' `#[no_mangle]` export (front2/builtins_shim.rs).
//! A wrong binding exits otherwise or is refused naming the routine.
#![feature(f128)]

fn main() {
    let a: f128 = 1.5;
    std::process::exit((a + a) as i32)
}
