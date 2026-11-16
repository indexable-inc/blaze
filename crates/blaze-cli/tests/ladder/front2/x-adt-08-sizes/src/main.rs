//! x-adt-08: the sizes the Reference fixes: primitives, arrays, the unit type, char and bool.
//!
//! The Reference, "Type layout": the primitive table "bool 1, u8/i8 1, u16/i16 2, u32/i32 4,
//! u64/i64 8, u128/i128 16, f32 4, f64 8, char 4"; "`usize` and `isize` have a size big enough
//! to contain every address on the target platform"; "Array Layout": "An array of `[T; N]` has
//! a size of `size_of::<T>() * N` and the same alignment of `T`"; "Tuple Layout": "the unit
//! tuple (`()`) ... is guaranteed as a zero-sized type to have a size of 0 and an alignment
//! of 1".
//! 1. bool 1, char 4, u128 16, f64 8.
//! 2. [u16; 3]: 2 * 3 = 6; align 2.        3. [u32; 0]: 0.
//! 4. (): size 0, align 1.
//! 5. [[u8; 3]; 5]: 3 * 5 = 15.
//! A wrong builder that pads arrays to a power of two exits 2 (8).

use core::mem::{align_of, size_of};

fn main() -> std::entry::Status {
    let status = if size_of::<bool>() != 1 || size_of::<char>() != 4 || size_of::<u128>() != 16 || size_of::<f64>() != 8 {
        1
    } else if size_of::<[u16; 3]>() != 6 || align_of::<[u16; 3]>() != 2 {
        2
    } else if size_of::<[u32; 0]>() != 0 {
        3
    } else if size_of::<()>() != 0 || align_of::<()>() != 1 {
        4
    } else if size_of::<[[u8; 3]; 5]>() != 15 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
