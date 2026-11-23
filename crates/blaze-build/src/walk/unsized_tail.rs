//! Structs and tuples whose last field is unsized: `struct Packet { tag: u16, body: [u8] }`,
//! core's `CStr { inner: [c_char] }` and `ByteStr(pub [u8])`.
//!
//! The Reference, "Dynamically Sized Types": "Structs may contain a DST as the last field; this
//! makes the struct itself a DST", and "Pointer types to DSTs are sized but have twice the size
//! of the size of pointers to sized types": "Pointers to slices and `str` also store the number
//! of elements". So such a struct is never held by value; a place of it exists only behind a
//! wide pointer, whose second word is its tail's length.
//!
//! "Type layout", "The `Rust` representation" and "The `C` representation": the fields before
//! the tail are placed at fixed offsets, and the tail at the next offset that is a multiple of
//! its alignment; a slice `[T]`'s alignment is `T`'s and `str`'s is 1, both known statically,
//! so the tail's offset is fixed too (`ls`, layout.rs `univariant`, keeps the unsized field last
//! and places it so). A `dyn Trait` tail's alignment is the vtable's, a run-time value: its
//! offset is the prefix's end rounded up to the vtable's word 2 where the tail is reached
//! (crate::dyn_tail); the wide pointer's second word is then the vtable. The size of a value is a
//! function of its length: `Layouts::size_of_val` (the prefix, plus `len` strides, rounded up
//! to the alignment).
//!
//! The walk (walk.rs "Unsized places") holds the place of an unsized value as the memory of the
//! wide pointer that reached it (`V::Unsized`). Through it:
//! - `(*p).f` for a sized field `f` is the ordinary place at the pointer's address plus `f`'s
//!   offset;
//! - `(*p).tail` is again an unsized place: a new wide pointer (the address plus the tail's
//!   offset, the same length) in a fresh slot, so `&(*p).tail` is that wide pointer and
//!   `(*p).tail[i]` indexes it (walk.rs `index_place`);
//! - a `#[repr(transparent)]` wrapper over `[T]` or `str` has its tail at offset 0, so a cast
//!   between `&Wrapper` and `&[T]` keeps both words unchanged.
//!
//! Every by-value use of such a struct is refused BY NAME at the expression (`repr`:
//! `UNSIZED_BY_VALUE`), never laid out as a sized value.

use core::alloc::Allocator;

use blaze_lir::Flags;
use blaze_lir::Ty;
use blaze_lir::builder::MemAttrs;
use blaze_lir::func::SlotKind;
use blaze_typeck::{Expr, ExprId};

use super::{Shape, V, Walk, malformed, unsupported};
use crate::BuildError;

/// The refusal of a struct or tuple with an unsized tail used by value.
pub(super) const UNSIZED_BY_VALUE: &str = "a struct or tuple whose last field is unsized (a slice, `str` or `dyn` tail) held by \
                                           value: the Reference, \"Dynamically Sized Types\": it exists only behind a \
                                           pointer";

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether walk layout row `l` is a struct or tuple whose last field is unsized (its row in
    /// `layout_fields` holds the tail's offset and no representation).
    pub(super) fn has_unsized_tail(&self, l: u32) -> bool {
        self.unsized_tails.contains(&l)
    }

    /// Field expression `e` of the unsized place on top of the value stack (`V::Unsized`, the
    /// memory of the wide pointer that reached it): as a place (`as_place`) or its value.
    pub(super) fn unsized_field(&mut self, e: ExprId, as_place: bool) -> Result<(), BuildError> {
        let base = match self.expr(e)? {
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => base,
            _ => return Err(malformed("a field's task on another expression")),
        };
        let col = self.checked.fields.column::<0>();
        let row = col.binary_search(&e).map_err(|_| malformed("a field access without its record row"))?;
        let at = self.checked.fields.column::<2>()[row];
        let t = self.adjusted_ty(base)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(base, error))?;
        let l = self.layout(g).map_err(|error| Self::ty_err(base, error))?;
        if !self.has_unsized_tail(l) {
            return Err(unsupported(base, "a field of an unsized place that is not a struct or tuple ending in a slice, `str` or `dyn`"));
        }
        // A `dyn` tail's offset is rounded up to its run-time alignment, the vtable's or a
        // nested struct's (crate::dyn_tail).
        let ls_id = self.ls_of(g).map_err(|error| Self::ty_err(base, error))?;
        let dyn_prefix = self.ls.dyn_chain(ls_id);
        let Shape::Fields { first, len } = self.layouts[l as usize].shape else {
            return Err(malformed("an unsized aggregate that is not a struct or a tuple"));
        };
        if at >= len {
            return Err(malformed("a field index past its aggregate's fields"));
        }
        let (_, field_off, repr) = self.layout_fields[(first + at) as usize];
        let field_off = i32::try_from(field_off).map_err(|_| unsupported(e, "a field at 2 GiB or more"))?;
        let V::Unsized(w, woff) = self.pop()? else {
            return Err(malformed("an unsized field without its unsized place"));
        };
        // Word 0 of the wide pointer: the struct's address.
        let addr = self.b.load(Ty::PTR, Flags::NONE, w, woff, MemAttrs::aligned(3));
        let v = if at + 1 == len {
            if !as_place {
                return Err(unsupported(e, "an unsized field (a slice, `str` or `dyn` tail) held by value"));
            }
            // The tail's wide pointer: the address plus the tail's offset, the same metadata
            // (a length, or the vtable).
            let len_word = self.b.load(Ty::I64, Flags::NONE, w, woff + 8, MemAttrs::aligned(3));
            let tail = match dyn_prefix {
                None => self.address((addr, field_off)),
                Some(prefix) => {
                    let vtable = self.b.load(Ty::PTR, Flags::NONE, w, woff + 8, MemAttrs::aligned(3));
                    let off = crate::dyn_tail::tail_offset(&mut self.b, prefix, vtable);
                    self.b.ptr_add(Flags::INBOUNDS, addr, off)
                }
            };
            let slot = self.b.slot(16, 3, SlotKind::Temp);
            let to = self.b.slot_addr(slot);
            self.b.store(Flags::NONE, tail, to, 0, MemAttrs::aligned(3));
            self.b.store(Flags::NONE, len_word, to, 8, MemAttrs::aligned(3));
            V::Unsized(to, 0)
        } else if as_place {
            V::Place(addr, field_off)
        } else {
            // The check refuses a move out of a place behind a reference; a field that needs
            // drop read here would be one.
            if self.repr_needs_drop(repr) {
                return Err(unsupported(e, "a move out of a field behind a pointer whose type needs drop"));
            }
            self.load(repr, (addr, field_off))
        };
        self.push_val(v, None);
        Ok(())
    }
}
