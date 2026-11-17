//! The validity assertions: `assert_inhabited`, `assert_zero_valid` and
//! `assert_mem_uninitialized_valid`, decided at the instance by `T`'s layout.
//!
//! The pinned core, library/core/src/intrinsics/mod.rs:753-782: `assert_inhabited<T>()`: "A
//! guard for unsafe functions that cannot ever be executed if `T` is uninhabited: This will
//! statically either panic, or do nothing"; `assert_zero_valid<T>()`: "if `T` does not permit
//! zero-initialization"; `assert_mem_uninitialized_valid<T>()`: "A guard for
//! `std::mem::uninitialized`" (whose doc, library/core/src/mem/mod.rs, says the memory is
//! "filled with 0x01"). Each is `#[rustc_nounwind]`.
//!
//! So the walk (walk/validity.rs) builds nothing when [`permits`] says yes, and otherwise a
//! call of the `panic_nounwind` lang fn (library/core/src/panicking.rs: `#[lang =
//! "panic_nounwind"] pub const fn panic_nounwind(expr: &'static str) -> !`) with
//! [`Check::message`]'s text around `T`'s name (type_name.rs).
//!
//! The test ([`permits`], validity/exact.rs) is exact for every type, by the layout records:
//! `T` is inhabited (an enum iff some variant has every field inhabited); the value whose
//! every byte is the fill (0, or 0x01) is a valid value of `T` (an enum by the variant the
//! fill selects through its tag encoding and every field of that variant). exact.rs's doc
//! states each rule with the Reference's line, and why the 0x01 check judges the 0x01-filled
//! value and not truly uninitialized memory (the pinned `mem::uninitialized` writes 0x01).

use super::Which;
use crate::layout::Scalar;

/// One of the three assertions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Check {
    /// `assert_inhabited`.
    Inhabited,
    /// `assert_zero_valid`: every byte 0.
    Zero,
    /// `assert_mem_uninitialized_valid`: every byte 0x01.
    Ones,
}

impl Check {
    /// The assertion an intrinsic is, if it is one.
    pub const fn of(which: Which) -> Option<Check> {
        match which {
            Which::AssertInhabited => Some(Check::Inhabited),
            Which::AssertZeroValid => Some(Check::Zero),
            Which::AssertMemUninitializedValid => Some(Check::Ones),
            _ => None,
        }
    }

    /// The panic message's text before and after `T`'s name.
    pub const fn message(self) -> (&'static str, &'static str) {
        match self {
            Check::Inhabited => ("attempted to instantiate uninhabited type `", "`"),
            Check::Zero => ("attempted to zero-initialize type `", "`, which is invalid"),
            Check::Ones => ("attempted to leave type `", "` uninitialized, which is invalid"),
        }
    }

    /// The fill of a scalar `bits` wide.
    const fn fill(self, bits: u32) -> u128 {
        match self {
            Check::Inhabited | Check::Zero => 0,
            Check::Ones => {
                let all = 0x0101_0101_0101_0101_0101_0101_0101_0101u128;
                if bits >= 128 { all } else { all & ((1u128 << bits) - 1) }
            }
        }
    }

    fn holds(self, s: Scalar) -> bool {
        let bits = s.prim.bits();
        s.valid.contains(self.fill(bits), bits)
    }
}

mod exact;

/// Whether `ty` (ground) passes `check`: exact for every type (exact.rs).
pub use exact::permits;

#[cfg(test)]
mod tests;
