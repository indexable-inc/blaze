//! Textual values and layout queries the walk builds without a call: a `char` literal, a C
//! string literal and `offset_of!`.
//!
//! - The Reference, "Textual types": "A value of type `char` is a Unicode scalar value (i.e. a
//!   code point that is not a surrogate), represented as a 32-bit unsigned word in the 0x0000 to
//!   0xD7FF or 0xE000 to 0x10FFFF range"; "`char` ... has the same size and alignment as `u32`".
//!   The walk holds a `char` as a `u32` (`Repr::Int { ty: I32, signed: false }`), so `c as u32`,
//!   `b as char` (from `u8`) and comparisons are the integer forms; its valid range is the
//!   layout's (layout.rs), not the value's.
//! - The Reference, "C string literal expressions": "a C string literal expression ... has type
//!   `&'static core::ffi::CStr`"; its bytes are those of the literal "followed by a nul byte",
//!   and it may contain no other nul. The toolchain's library/core/src/ffi/c_str.rs:
//!   `#[repr(transparent)] pub struct CStr { inner: [c_char] }`: a `&CStr` is the wide pointer
//!   of a `[c_char]`, whose length counts the nul (`from_bytes_with_nul_unchecked` casts the
//!   `&[u8]` with its nul to `&CStr`).
//! - `offset_of!(T, a.b)` (`Expr::OffsetOf`): the sum of each step's offset in the layout the
//!   walk already computes. A step through an enum variant (`offset_of_enum`) is refused by
//!   name.

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::{Flags, Ty};
use blaze_typeck::{ExprId, Name};

use super::{Repr, Shape, V, Walk, malformed, unsupported};
use crate::BuildError;

/// The last Unicode scalar value (the Reference, "Textual types").
const CHAR_MAX: u128 = 0x10_FFFF;
/// The surrogates, which are no `char`'s value.
const SURROGATES: core::ops::RangeInclusive<u128> = 0xD800..=0xDFFF;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// `'x'`: its scalar value as a `u32`. A value outside the scalar values is a record the
    /// checker does not make.
    pub(super) fn char_lit(&mut self, e: ExprId) -> Result<V, BuildError> {
        let value = self.adts.int_lit(self.body.owner, e).ok_or(BuildError::NoLiteralValue { expr: e })?;
        if value > CHAR_MAX || SURROGATES.contains(&value) {
            return Err(malformed("a char literal that is not a Unicode scalar value"));
        }
        Ok(V::Scalar(self.b.iconst(Ty::I32, value)))
    }

    /// `c"..."`: its bytes and a nul as the function's data, and the pair of their address and
    /// length, the nul counted. `Adts::byte_str` gives the literal's bytes without the nul.
    pub(super) fn c_str_lit(&mut self, e: ExprId) -> Result<V, BuildError> {
        let bytes = self.adts.byte_str(self.body.owner, e).ok_or(BuildError::NoLiteralValue { expr: e })?;
        if bytes.contains(&0) {
            return Err(malformed("a C string literal with a nul inside"));
        }
        let mut with_nul = blaze_own::Vec::new_in(self.alloc);
        with_nul.extend_from_slice(bytes);
        with_nul.push(0);
        let len = with_nul.len() as u128;
        let data = self.b.data(&with_nul, 0, &[]);
        let p = self.b.data_addr(data);
        let repr = self.expr_repr(e)?;
        let Repr::Agg(_) = repr else {
            return Err(unsupported(e, "a C string literal whose type is not a wide pointer"));
        };
        let at = self.temp(repr);
        self.b.store(Flags::NONE, p, at.0, at.1, MemAttrs::aligned(3));
        let n = self.b.iconst(Ty::I64, len);
        self.b.store(Flags::NONE, n, at.0, at.1 + 8, MemAttrs::aligned(3));
        Ok(V::Place(at.0, at.1))
    }

    /// `offset_of!(ty, a.b.c)`: each step's offset in its container's layout, summed.
    pub(super) fn offset_of(&mut self, e: ExprId, ty: blaze_typeck::PTy, fields: blaze_typeck::List) -> Result<V, BuildError> {
        // `ty` is the body's written type (`BodyView::types`), not the record's table.
        let types = self.body.types;
        let g = self.ground_in(types, ty).map_err(|error| Self::ty_err(e, error))?;
        let mut repr = self.repr(g).map_err(|error| Self::ty_err(e, error))?;
        let steps = self.body.segments.get(fields.range()).ok_or(malformed("an `offset_of!` path past `segments`"))?;
        if steps.is_empty() {
            return Err(malformed("an `offset_of!` with no field"));
        }
        let mut total: u64 = 0;
        for step in steps {
            let name: Name = step.name;
            let l = match repr {
                Repr::Agg(l) => l,
                _ => return Err(unsupported(e, "`offset_of!` through a field of a zero-sized or scalar type")),
            };
            let (first, len) = match self.layouts[l as usize].shape {
                Shape::Fields { first, len } => (first, len),
                Shape::Enum { .. } => return Err(unsupported(e, "`offset_of!` through an enum variant (`offset_of_enum`)")),
                _ => return Err(unsupported(e, "`offset_of!` into an array")),
            };
            let row = (first..first + len)
                .find(|&i| self.layout_fields[i as usize].0 == name)
                .ok_or(malformed("an `offset_of!` step naming a field its container lacks"))?;
            let (_, off, next) = self.layout_fields[row as usize];
            total += u64::from(off);
            repr = next;
        }
        Ok(V::Scalar(self.b.iconst(Ty::I64, u128::from(total))))
    }
}
