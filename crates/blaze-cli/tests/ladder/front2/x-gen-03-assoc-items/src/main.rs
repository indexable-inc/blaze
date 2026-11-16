//! x-gen-03: associated constants and associated types read through a generic bound.
//!
//! The Reference, "Associated Items": "Associated constants are constants associated with a
//! type"; "Associated types are type aliases associated with another type"; through a bound
//! `T: Shape`, `T::SIDES` and `T::Unit` are the impl's.
//! 1. Tri::SIDES = 3, Quad::SIDES = 4; sides_of::<Tri>() + sides_of::<Quad>() = 7.
//! 2. a default associated constant `NAME_LEN` = 1 not overridden by Tri, overridden by Quad
//!    (4): 1 and 4.
//! 3. `per` returns `Self::Unit`: u8 for Tri, computed with `wrapping_mul`: 200 * 3 = 600,
//!    600 - 2 * 256 = 88; u64 for Quad: 200 * 4 = 800.
//! 4. size_of::<T::Unit>(): 1 for Tri, 8 for Quad.
//! A wrong builder that reads the constant of the wrong impl exits 1.

use core::mem::size_of;

trait Shape {
    const SIDES: u32;
    const NAME_LEN: u32 = 1;
    type Unit;
    fn per(side: u8) -> Self::Unit;
}

struct Tri;
struct Quad;

impl Shape for Tri {
    const SIDES: u32 = 3;
    type Unit = u8;
    fn per(side: u8) -> u8 {
        side.wrapping_mul(3)
    }
}

impl Shape for Quad {
    const SIDES: u32 = 4;
    const NAME_LEN: u32 = 4;
    type Unit = u64;
    fn per(side: u8) -> u64 {
        side as u64 * 4
    }
}

fn sides_of<T: Shape>() -> u32 {
    T::SIDES
}

fn unit_size<T: Shape>() -> usize {
    size_of::<T::Unit>()
}

fn main() -> std::entry::Status {
    let status = if sides_of::<Tri>() + sides_of::<Quad>() != 7 {
        1
    } else if Tri::NAME_LEN != 1 || Quad::NAME_LEN != 4 {
        2
    } else if Tri::per(200) != 88 || Quad::per(200) != 800 {
        3
    } else if unit_size::<Tri>() != 1 || unit_size::<Quad>() != 8 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
