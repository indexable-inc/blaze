//! The drop glue of a slice `[T]` and of `str`: the body of `drop_glue::<[T]>` (core's
//! `ptr/mod.rs`: `#[lang = "drop_glue"] pub(crate) const unsafe fn drop_glue<T: PointeeSized>(_:
//! &mut T)`), whose one parameter at `T = [U]` or `str` is a wide `&mut [U]`.
//!
//! The Reference, "Destructors" (`destructors.operation`): "The destructor of a type T consists
//! of: If T: Drop, calling <T as std::ops::Drop>::drop; Recursively running the destructor of all
//! of its fields", and "The elements of an array or owned slice are dropped from the first
//! element to the last." A slice has no `Drop` impl of its own and `str` holds bytes, so the glue is
//! the element loop and nothing else.
//!
//! The parameter is the wide pointer, an aggregate of two words passed by reference (walk.rs
//! `rust_pass`: `Repr::Agg` is `Pass::Ref`): its address at 0 and its length at 8, as every
//! other reader of a slice's wide pointer in the walk takes it (`index_place`). The length is a
//! run-time value, so the loop is a LIR loop, never unrolled: `i in 0..len`, the element's glue
//! (`drop_in_place::<U>`, a thin pointer) on `data + i * stride`. A `str`, or an element that
//! needs no drop, builds an empty body.
//!
//! A panic inside one element's drop would, in Rust, go on to drop the rest of the elements
//! while unwinding; the build does not unwind (a panic aborts), so no landing pad is built here
//! and none is guessed.

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::{Flags, IntCond, Op, Ty, Value};
use blaze_typeck::{PNode, PTy};

use super::{TyErr, Walk, malformed};
use crate::BuildError;

/// A grounding or layout refusal as the build's error, as `run` maps a signature's.
fn ty_error(error: TyErr) -> BuildError {
    match error {
        TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
        TyErr::Malformed(what) => malformed(what),
        TyErr::Other(error) => error,
    }
}

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// Whether ground type `g`, `drop_glue`'s argument, is a slice or `str`: the pointees whose
    /// glue [`Self::slice_glue`] builds.
    pub(super) fn is_slice_pointee(&self, g: PTy) -> Result<bool, BuildError> {
        Ok(matches!(self.ground_node(g).map_err(ty_error)?, PNode::Slice(_) | PNode::Str))
    }

    /// The body of `drop_glue::<g>` for `g` a slice `[U]` or `str`, `w` the address of the wide
    /// pointer (data at 0, length at 8): each element's glue, first to last. Ends in the block
    /// after the loop; the caller returns.
    pub(super) fn slice_glue(&mut self, g: PTy, w: Value) -> Result<(), BuildError> {
        let elem = match self.ground_node(g).map_err(ty_error)? {
            PNode::Slice(elem) => elem,
            // `str` is bytes: nothing to drop.
            PNode::Str => return Ok(()),
            _ => return Err(malformed("the slice glue asked of a type that is neither a slice nor `str`")),
        };
        if !self.needs_drop(elem).map_err(ty_error)? {
            return Ok(());
        }
        let repr = self.repr(elem).map_err(ty_error)?;
        // A Rust type's size is a multiple of its alignment, so the size is the stride.
        let (stride, _) = self.size_align(repr);
        let data = self.b.load(Ty::PTR, Flags::NONE, w, 0, MemAttrs::aligned(3));
        let len = self.b.load(Ty::I64, Flags::NONE, w, 8, MemAttrs::aligned(3));
        let header = self.create();
        let body = self.create();
        let exit = self.create();
        let zero = self.b.iconst(Ty::I64, 0);
        self.jump(header, &[zero]);
        self.begin(header);
        let i = self.b.param(Ty::I64);
        let more = self.b.icmp(IntCond::Ult, i, len);
        self.br(more, (body, &[]), (exit, &[]));
        self.begin(body);
        let s = self.b.iconst(Ty::I64, u128::from(stride));
        let off = self.b.binary(Op::Mul, Flags::NONE, i, s);
        let ep = self.b.ptr_add(Flags::NONE, data, off);
        self.drop_at(elem, (ep, 0))?;
        let one = self.b.iconst(Ty::I64, 1);
        let next = self.b.binary(Op::Add, Flags::NONE, i, one);
        self.jump(header, &[next]);
        self.begin(exit);
        Ok(())
    }
}
