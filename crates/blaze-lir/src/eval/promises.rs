//! The flags the evaluator models, and how it checks each one's promise.
//!
//! A promise flag (crate::flags: "opt-in UB the builder proved from Rust's semantics") does
//! not change the value an instruction computes when the program has no undefined
//! behaviour; it only adds a case that is undefined. So the evaluator computes the value as
//! without the flag and, where it can check the promise from what it holds, ends the run as
//! [`super::Outcome::Undefined`] naming the broken promise. A flag whose promise it cannot
//! check is refused by name, never dropped.
//!
//! Modelled: `ptr_add inbounds`. The evaluator's pointer is `Val::Ptr { slot, offset }`, an
//! offset into one stack slot (one allocation: "in Rust, every (stack-allocated) variable is
//! considered a separate allocation", library/core/src/ptr/mod.rs:25 in the pinned core), so
//! the promise is checkable. Its text, library/core/src/ptr/docs/offset.md (the pinned core's
//! `offset`, which the builder lowers to `ptr_add inbounds`):
//!
//! > If the computed offset is non-zero, then `self` must be derived from a pointer to some
//! > allocation, and the entire memory range between `self` and `result` (i.e.,
//! > `min(self.addr(), result)..max(self.addr(), result)`) must be in bounds of that
//! > allocation.
//!
//! So a zero step is always defined; a non-zero one needs a live slot and both ends in
//! `0..=size` (one past the end is in bounds: `vec.as_ptr().offset(vec.len() as isize)` "is
//! always safe", the same file).
//!
//! Refused by name: `ptr_add nuw` (its promise is about the absolute address, which the
//! evaluator does not hold), and every flag on every other operation.

use crate::{Flags, Op};

/// Why instruction flags `flags` on `op` are outside the modelled core, or `None` when the
/// evaluator models every one of them.
pub(super) fn unmodelled(op: Op, flags: Flags) -> Option<&'static str> {
    if flags == Flags::NONE {
        return None;
    }
    match op {
        Op::PtrAdd if flags.contains(Flags::NUW) => {
            Some("`nuw` on a `ptr_add` (its promise is about the absolute address, which a run does not hold)")
        }
        Op::PtrAdd if flags.without(Flags::INBOUNDS) == Flags::NONE => None,
        _ => Some("an instruction with flags"),
    }
}

/// The offset `ptr_add inbounds` of a pointer `offset` bytes into a slot of `size` bytes, live
/// or not, steps to by `by` bytes; or the promise it breaks, as undefined behaviour.
pub(super) fn inbounds_step(offset: i128, by: i128, size: i128, live: bool) -> Result<i128, &'static str> {
    if by == 0 {
        return Ok(offset);
    }
    if !live {
        return Err("an `inbounds` step of a pointer into a dead slot");
    }
    if offset < 0 || offset > size {
        return Err("an `inbounds` step from a pointer outside its slot");
    }
    match offset.checked_add(by) {
        Some(result) if (0..=size).contains(&result) => Ok(result),
        _ => Err("an `inbounds` step past its slot's bounds"),
    }
}
