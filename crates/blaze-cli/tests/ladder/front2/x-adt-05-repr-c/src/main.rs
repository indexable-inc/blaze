//! x-adt-05: `#[repr(C)]` struct sizes and field offsets, by the Reference's algorithm.
//!
//! The Reference, "Type layout", "The C Representation", reprc structs: "Start with a current
//! offset of 0 bytes. For each field in declaration order in the struct, first determine the
//! size and alignment of the field. If the current offset is not a multiple of the field's
//! alignment, then add padding bytes to the current offset until it is a multiple of the
//! field's alignment. The offset for the field is what the current offset is now. Then
//! increase the current offset by the size of the field. Finally, the size of the struct is
//! the current offset rounded up to the nearest multiple of the struct's alignment" (the
//! largest field alignment). u8, u16, u32 have size and alignment 1, 2, 4 on the hosts in
//! scope (arm64, x86-64).
//! 1. A { a: u8, b: u32, c: u16 }: a 0..1; b pads to 4, 4..8; c 8..10; align 4: size 12.
//! 2. offsets: b at 4, c at 8 (core's `offset_of!`).
//! 3. B { a: u16, b: u8 }: 0..2, 2..3; align 2: size 4.
//! 4. C { a: u8, b: u8, c: u16 }: 0, 1, 2..4: size 4, no padding.
//! 5. D { a: u8, inner: B }: B's align 2: inner at 2, 2..6: size 6.
//! A wrong builder that packs fields (no padding) exits 1 (7).

use core::mem::{align_of, offset_of, size_of};

#[repr(C)]
struct A {
    a: u8,
    b: u32,
    c: u16,
}

#[repr(C)]
struct B {
    a: u16,
    b: u8,
}

#[repr(C)]
struct C {
    a: u8,
    b: u8,
    c: u16,
}

#[repr(C)]
struct D {
    a: u8,
    inner: B,
}

fn main() -> std::entry::Status {
    let status = if size_of::<A>() != 12 || align_of::<A>() != 4 {
        1
    } else if offset_of!(A, b) != 4 || offset_of!(A, c) != 8 {
        2
    } else if size_of::<B>() != 4 {
        3
    } else if size_of::<C>() != 4 || offset_of!(C, c) != 2 {
        4
    } else if size_of::<D>() != 6 || offset_of!(D, inner) != 2 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
