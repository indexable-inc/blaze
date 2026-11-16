//! x-adt-04: `Option<&T>` is one pointer wide and its `None` is the null pointer.
//!
//! The Reference, "Type layout": "Pointers to sized types have the same size and alignment as
//! `usize`." core's `option` module docs, "Representation": "Rust guarantees to optimize the
//! following types `T` such that `Option<T>` has the same size, alignment, and function call
//! ABI as `T`": `&U`, `&mut U`, `Box<U>`, `NonNull<U>`, `NonZero*`, ...; "In some of these
//! cases, Rust further guarantees the following: `transmute::<_, Option<T>>([0u8;
//! size_of::<T>()])` is sound and produces `Option::<T>::None`".
//! 1. size_of::<&u8>() == size_of::<usize>().
//! 2. size_of::<Option<&u8>>() == size_of::<&u8>().
//! 3. size_of::<Option<core::num::NonZeroU32>>() == 4.
//! 4. all-zero bytes read as Option<&u8> are None.
//! 5. Some(&x) is not all zeros: its bits are x's address (nonzero), and it reads back x.
//! A wrong builder that gives `Option<&T>` a separate tag exits 2 (16, not 8).

use core::mem::{size_of, transmute};
use core::num::NonZeroU32;

fn main() -> std::entry::Status {
    let x = 42u8;
    // SAFETY: core's docs above: all-zero bytes of size_of::<&u8>() are `None::<&u8>`.
    let none: Option<&u8> = unsafe { transmute::<usize, Option<&u8>>(0) };
    let some = Some(&x);
    // SAFETY: Option<&u8> has the size of usize (check 2); the bits of Some(&x) are &x's.
    let bits: usize = unsafe { transmute::<Option<&u8>, usize>(some) };
    let status = if size_of::<&u8>() != size_of::<usize>() {
        1
    } else if size_of::<Option<&u8>>() != size_of::<&u8>() {
        2
    } else if size_of::<Option<NonZeroU32>>() != 4 {
        3
    } else if none.is_some() {
        4
    } else if bits == 0 || bits != &x as *const u8 as usize || some != Some(&42) {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
