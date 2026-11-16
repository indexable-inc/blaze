//! x0-adt-10: `repr(packed)` aggregates, loaded and stored at the packed alignment (no_core;
//! success 42, else the failed check's number). Expected: `PASS x0-adt-10-packed`.
//! The Reference, "Type layout", `[layout.repr.alignment.intro]`: "The align and packed
//! modifiers can be used to respectively raise or lower the alignment of structs and unions.
//! packed may also alter the padding between fields (although it will not alter the padding
//! inside of any field)."; `[layout.repr.alignment.packed-fields]`: "The alignments of each
//! field, for the purpose of positioning fields, is the smaller of the specified alignment and
//! the alignment of the field's type." core's `ptr::read_unaligned` reads through
//! `#[repr(Rust, packed)] struct Unaligned<T>(T)`.
//! 1. `size_of::<P>() == 7`, `align_of::<P>() == 1`.
//! 2. `P` stored whole at byte 1 of a buffer, `{ (*ptr).b }` read back by value.
//! 3. its `c`, and its `a`.
//! 4. a packed struct inside a plain `repr(C)` one (`Outer { x: u8, p: P }`: `p.b` at byte 2),
//!    written field by field and read through the nested place.
//! 5. the same nested read in a constant: the LIR evaluator refuses an access that claims more
//!    alignment than its address has, so a fix that drops the refusal without lowering the
//!    claimed alignment fails here (or fails to build).
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

#[repr(packed)]
struct P {
    a: u8,
    b: u32,
    c: u16,
}

#[repr(C, packed)]
struct Buf {
    pad: u8,
    p: P,
    rest: [u8; 8],
}

#[repr(C)]
struct Outer {
    x: u8,
    p: P,
}

fn nested(b: u32) -> u32 {
    let mut o = Outer { x: 7, p: P { a: 0, b: 0, c: 0 } };
    o.p.b = b;
    o.p.c = 0x0506;
    if o.x != 7 || { o.p.c } != 0x0506 {
        return 0;
    }
    o.p.b
}

const NESTED: u32 = {
    let o = Outer { x: 1, p: P { a: 2, b: 0x0A0B_0C0D, c: 3 } };
    o.p.b
};

fn check() -> i32 {
    if size_of::<P>() != 7 || align_of::<P>() != 1 {
        return 1;
    }
    let mut buf = Buf { pad: 0, p: P { a: 0, b: 0, c: 0 }, rest: [0; 8] };
    let ptr = &raw mut buf.p;
    let p = P { a: 1, b: 0x0102_0304, c: 9 };
    unsafe {
        *ptr = p;
        if { (*ptr).b } != 0x0102_0304 {
            return 2;
        }
        if { (*ptr).c } != 9 || { (*ptr).a } != 1 {
            return 3;
        }
    }
    if nested(0xDEAD_BEEF) != 0xDEAD_BEEF {
        return 4;
    }
    if NESTED != 0x0A0B_0C0D {
        return 5;
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
