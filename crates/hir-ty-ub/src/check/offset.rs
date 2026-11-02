//! `ptr_offset_from` and `ptr_offset_from_unsigned`.
// rustc: compiler/rustc_const_eval/src/interpret/intrinsics.rs, the `ptr_offset_from` arm of
// `eval_intrinsic`, with Miri's `OFFSET_IS_ADDR` (the distance is of the two addresses).

use std::fmt;

use crate::{
    ids::{AllocId, Prov},
    memory::Memory,
};

/// The undefined behavior of `ptr_offset_from`, in rustc's words
/// (`compiler/rustc_const_eval/messages.ftl`, `const_eval_offset_from_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OffsetFromError {
    DifferentAllocations { unsigned: bool },
    OutOfBounds { unsigned: bool },
    TooFarAhead { unsigned: bool },
    TooFarBefore,
    /// `ptr_offset_from_unsigned` with the first address below the second.
    Negative { a: u64, b: u64 },
}

fn name(unsigned: bool) -> &'static str {
    if unsigned { "ptr_offset_from_unsigned" } else { "ptr_offset_from" }
}

impl fmt::Display for OffsetFromError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            OffsetFromError::DifferentAllocations { unsigned } => write!(
                f,
                "`{}` called on two different pointers that are not both derived from the same allocation",
                name(unsigned)
            ),
            OffsetFromError::OutOfBounds { unsigned } => write!(
                f,
                "`{}` called on two different pointers where the memory range between them is not in-bounds of an allocation",
                name(unsigned)
            ),
            OffsetFromError::TooFarAhead { unsigned } => {
                write!(f, "`{}` called when first pointer is too far ahead of second", name(unsigned))
            }
            OffsetFromError::TooFarBefore => {
                f.write_str("`ptr_offset_from` called when first pointer is too far before second")
            }
            OffsetFromError::Negative { a, b } => write!(
                f,
                "`ptr_offset_from_unsigned` called when first pointer has smaller address than second: {a} < {b}"
            ),
        }
    }
}

/// The allocation `ptr_try_get_alloc_id(ptr, 0)` names: the provenance's, or for a wildcard
/// the exposed allocation at the address.
fn alloc_of(mem: &Memory, (addr, prov): (u64, Option<Prov>)) -> Option<AllocId> {
    match prov? {
        Prov::Concrete { alloc, .. } => Some(alloc),
        Prov::Wildcard => mem.exposed_at(addr),
    }
}

/// Checks `a.offset_from(b)` (`unsigned`: `a.offset_from_unsigned(b)`): the distance fits
/// `isize` (and is not negative, unsigned), and the bytes between them are in bounds of one
/// allocation, counted from each end.
pub(crate) fn check(
    mem: &Memory,
    a: (u64, Option<Prov>),
    b: (u64, Option<Prov>),
    unsigned: bool,
) -> Result<(), OffsetFromError> {
    let bits = mem.pointer_size() * 8;
    let min = -(1i128 << (bits - 1));
    let max = (1i128 << (bits - 1)) - 1;
    let diff = i128::from(a.0) - i128::from(b.0);
    if diff < 0 {
        if unsigned {
            return Err(OffsetFromError::Negative { a: a.0, b: b.0 });
        }
        // The wrapped difference read as `isize` is positive or `isize::MIN`.
        if diff <= min {
            return Err(OffsetFromError::TooFarBefore);
        }
    } else if diff > max {
        return Err(OffsetFromError::TooFarAhead { unsigned });
    }
    let dist = diff as i64;
    if mem.offset_inbounds(b.0, b.1, dist).is_err() {
        let same = alloc_of(mem, a).is_some_and(|it| Some(it) == alloc_of(mem, b));
        return Err(if same {
            OffsetFromError::OutOfBounds { unsigned }
        } else {
            OffsetFromError::DifferentAllocations { unsigned }
        });
    }
    if mem.offset_inbounds(a.0, a.1, -dist).is_err() {
        return Err(OffsetFromError::DifferentAllocations { unsigned });
    }
    Ok(())
}
