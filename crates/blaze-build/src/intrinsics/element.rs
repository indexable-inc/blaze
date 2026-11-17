//! Which generic argument names the element a pointer-stepping intrinsic steps by.
//!
//! Each intrinsic's generic arguments are read by its own signature, stated in its table row,
//! never by the arm that builds it. In the pinned core (library/core/src/intrinsics/mod.rs):
//!
//! - 1157: `pub const unsafe fn offset<Ptr: bounds::BuiltinDeref, Delta>(dst: Ptr, offset: Delta) -> Ptr;`
//!   generic 0 is the pointer type; the element is its pointee.
//! - 1176: `pub const unsafe fn arith_offset<T>(dst: *const T, offset: isize) -> *const T;`
//!   generic 0 is the element type itself.
//!
//! Two intrinsics that share an arm can thus not share a reading of their generics by accident:
//! a row that names no element is refused by name when the arm asks for one.

use core::alloc::Allocator;

use blaze_lir::Sink;
use blaze_typeck::PTy;

use super::{B, Call, Entry, IntrinsicError, ty_arg};

/// Where an intrinsic's element type is among its generic arguments.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElementArg {
    /// The intrinsic does not step a pointer by an element.
    None,
    /// Generic argument `i` is the element type (`arith_offset<T>`).
    Is(u8),
    /// Generic argument `i` is a pointer or reference type; its pointee is the element
    /// (`offset<Ptr: BuiltinDeref, Delta>`).
    PointeeOf(u8),
}

impl Entry {
    /// This row with its element argument stated.
    pub const fn stepping(self, element: ElementArg) -> Entry {
        Entry { element, ..self }
    }
}

impl<LA: Allocator + Clone, S: Sink, A: Allocator + Clone> B<'_, '_, '_, '_, LA, S, A> {
    /// The element type call `c` steps by, read where its table row says.
    pub(super) fn element(&self, c: &Call<'_>) -> Result<PTy, IntrinsicError> {
        match c.which.entry().element {
            ElementArg::None => Err(self.shape("its table row names no generic argument as the element it steps by")),
            ElementArg::Is(i) => ty_arg(c, usize::from(i)).ok_or_else(|| self.shape("a missing type argument")),
            ElementArg::PointeeOf(i) => {
                let ptr = ty_arg(c, usize::from(i)).ok_or_else(|| self.shape("a missing type argument"))?;
                self.pointee(ptr)
            }
        }
    }
}
