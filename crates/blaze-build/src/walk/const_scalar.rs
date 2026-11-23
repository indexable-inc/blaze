//! A relocation-free scalar constant built by its scalar kind (walk-AJ P4).
//!
//! An evaluated constant whose root allocation holds no relocation is its bytes, read
//! little-endian at its type's width (walk.rs `const_record_value`). Which LIR constant those
//! bits become is the type's scalar kind, by Rust's rules:
//!
//! - an integer (`bool`, `char` and every integer type are integers in LIR) is `iconst`;
//! - a pointer is an address with no provenance of an allocation. The Reference, "Type cast
//!   expressions", "Pointer to address cast" and "Address to pointer cast": "Casting from an
//!   integer to a raw pointer interprets the integer as a memory address and produces a pointer
//!   referencing that memory" (the library's `ptr::with_exposed_provenance`: "Converts an
//!   address back to a pointer, picking up some previously 'exposed' provenance"). Address `0`
//!   is `null()`; any other is `from_exposed` of the address as an `i64`. Sample: libc's
//!   `pub const MAP_FAILED: *mut c_void = !0 as *mut c_void;`, compared in blaze-mem os.rs
//!   `reserve_with`;
//! - a float is its IEEE 754 bits. The Reference, "Numeric types", "Floating-point types":
//!   "The IEEE 754-2008 "binary32" and "binary64" floating-point types are `f32` and `f64`,
//!   respectively" (and `f16`, `f128` the binary16 and binary128 formats): the constant is
//!   `fconst` of those bits, any bit past the type's width refused by name;
//! - any other LIR type (a vector) has no scalar constant form and is refused by name.
//!
//! A second site builds the same pointer form: walk.rs `write_tag`'s `Prim::Ptr` arms (a
//! pointer niche). It returns `()` and is not changed by this piece; making this function its
//! one home is a follow-up for the integrator.

use core::alloc::Allocator;

use blaze_lir::{Flags, Op, Scalar, Ty, Value};

use super::Walk;
use crate::BuildError;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The constant of scalar type `ty` whose bits are `value` (little-endian, the bits past
    /// the type's width 0): an integer, a pointer's address or a float's IEEE bits.
    pub(super) fn const_scalar(&mut self, ty: Ty, value: u128) -> Result<Value, BuildError> {
        if !ty.is_scalar() {
            return Err(BuildError::Malformed { what: "a scalar constant of a type with no constant form" });
        }
        match ty.elem() {
            Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64 | Scalar::I128 => {
                Ok(self.b.iconst(ty, value))
            }
            Scalar::Ptr => {
                if value == 0 {
                    return Ok(self.b.null());
                }
                if value > u128::from(u64::MAX) {
                    return Err(BuildError::Malformed { what: "a pointer constant's address past 64 bits" });
                }
                let address = self.b.iconst(Ty::I64, value);
                Ok(self.b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, address))
            }
            Scalar::F16 | Scalar::F32 | Scalar::F64 | Scalar::F128 => {
                let width = match ty.elem() {
                    Scalar::F16 => 16,
                    Scalar::F32 => 32,
                    Scalar::F64 => 64,
                    _ => 128,
                };
                if width < 128 && value >> width != 0 {
                    return Err(BuildError::Malformed { what: "a float constant's bits past its type's width" });
                }
                Ok(self.b.fconst(ty, value))
            }
            Scalar::None => Err(BuildError::Malformed { what: "a scalar constant of a type with no constant form" }),
        }
    }
}
