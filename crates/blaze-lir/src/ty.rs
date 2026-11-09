//! LIR's types: [`Ty`], a `u16`, machine types with a pointer distinct from its integer.
//!
//! - Integers `i1` (comparisons, `bool` as a value) to `i128`; the arbitrary widths of
//!   `#![blaze::arbitrary_ints]` stay in their storage integer, normalized by the builder.
//! - Floats `f16`, `f32`, `f64`, `f128`.
//! - `ptr`, the target's pointer, which carries provenance.
//! - Fixed vectors `<n x T>`, `n` a power of two, at most 512 bits.
//! - Scalable vectors `<vscale x n x T>` (SVE), at most 512 bits per `vscale`.
//! - Compounds `{T, U, ..}`, a row of the function's `compounds` table: a register aggregate
//!   in a signature (rustc's `CastTarget` and `Uniform`, homogeneous or not, so the consumer
//!   assigns registers by the target's rule) and the result of an operation that makes several
//!   values (a call returning several, `uadd_overflow`, `cmpxchg`), read by [`Op::Project`].
//!
//! The encoding: bit 15 set is a compound, its row in the low 15 bits. Otherwise bits 0..4 are
//! the [`Scalar`] (0 is no type, [`Ty::NONE`]), bits 4..6 the form (scalar, fixed vector,
//! scalable vector) and bits 6..10 the log2 of the lane count; bits 10..15 are 0.
//!
//! [`Op::Project`]: crate::Op::Project

use std::fmt;

/// A LIR type. See the module for its encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
#[repr(transparent)]
pub struct Ty(pub u16);

/// The element of a type: a scalar, or a vector's lane.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Scalar {
    None = 0,
    I1,
    I8,
    I16,
    I32,
    I64,
    I128,
    F16,
    F32,
    F64,
    F128,
    Ptr,
}

/// How a [`Ty`] is shaped.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Form {
    Scalar,
    Vector,
    Scalable,
    Compound,
}

const COMPOUND: u16 = 1 << 15;
const ELEM_MASK: u16 = 0xf;
const FORM_SHIFT: u16 = 4;
const FORM_MASK: u16 = 0x3;
const LANES_SHIFT: u16 = 6;
const LANES_MASK: u16 = 0xf;
const HIGH_MASK: u16 = !((1 << 10) - 1) & !COMPOUND;

/// The most bits a vector holds (per `vscale` for a scalable one): AVX-512's.
pub const VECTOR_BITS: u32 = 512;

impl Scalar {
    pub const ALL: [Scalar; 12] = [
        Scalar::None,
        Scalar::I1,
        Scalar::I8,
        Scalar::I16,
        Scalar::I32,
        Scalar::I64,
        Scalar::I128,
        Scalar::F16,
        Scalar::F32,
        Scalar::F64,
        Scalar::F128,
        Scalar::Ptr,
    ];

    pub const fn from_bits(bits: u16) -> Option<Scalar> {
        match (bits as usize) < Scalar::ALL.len() {
            true => Some(Scalar::ALL[bits as usize]),
            false => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Scalar::None => "none",
            Scalar::I1 => "i1",
            Scalar::I8 => "i8",
            Scalar::I16 => "i16",
            Scalar::I32 => "i32",
            Scalar::I64 => "i64",
            Scalar::I128 => "i128",
            Scalar::F16 => "f16",
            Scalar::F32 => "f32",
            Scalar::F64 => "f64",
            Scalar::F128 => "f128",
            Scalar::Ptr => "ptr",
        }
    }

    pub fn from_name(name: &str) -> Option<Scalar> {
        Scalar::ALL.into_iter().find(|it| it.name() == name)
    }

    /// Its width in bits, a pointer's being `ptr_bits`.
    pub const fn bits(self, ptr_bits: u32) -> u32 {
        match self {
            Scalar::None => 0,
            Scalar::I1 => 1,
            Scalar::I8 => 8,
            Scalar::I16 | Scalar::F16 => 16,
            Scalar::I32 | Scalar::F32 => 32,
            Scalar::I64 | Scalar::F64 => 64,
            Scalar::I128 | Scalar::F128 => 128,
            Scalar::Ptr => ptr_bits,
        }
    }

    pub const fn is_int(self) -> bool {
        matches!(
            self,
            Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64 | Scalar::I128
        )
    }

    pub const fn is_float(self) -> bool {
        matches!(self, Scalar::F16 | Scalar::F32 | Scalar::F64 | Scalar::F128)
    }
}

impl Ty {
    /// No type: the result of an instruction that makes no value.
    pub const NONE: Ty = Ty(0);
    pub const I1: Ty = Ty::scalar(Scalar::I1);
    pub const I8: Ty = Ty::scalar(Scalar::I8);
    pub const I16: Ty = Ty::scalar(Scalar::I16);
    pub const I32: Ty = Ty::scalar(Scalar::I32);
    pub const I64: Ty = Ty::scalar(Scalar::I64);
    pub const I128: Ty = Ty::scalar(Scalar::I128);
    pub const F16: Ty = Ty::scalar(Scalar::F16);
    pub const F32: Ty = Ty::scalar(Scalar::F32);
    pub const F64: Ty = Ty::scalar(Scalar::F64);
    pub const F128: Ty = Ty::scalar(Scalar::F128);
    pub const PTR: Ty = Ty::scalar(Scalar::Ptr);

    pub const fn scalar(scalar: Scalar) -> Ty {
        Ty(scalar as u16)
    }

    /// The integer of `bits` bits, if LIR has one.
    pub const fn int(bits: u32) -> Option<Ty> {
        Some(match bits {
            1 => Ty::I1,
            8 => Ty::I8,
            16 => Ty::I16,
            32 => Ty::I32,
            64 => Ty::I64,
            128 => Ty::I128,
            _ => return None,
        })
    }

    /// `<lanes x elem>`: `None` unless `lanes` is a power of two and the vector at most
    /// [`VECTOR_BITS`] (a pointer lane counted as 64 bits, the widest).
    pub const fn vector(elem: Scalar, lanes: u32) -> Option<Ty> {
        Ty::lanes_of(elem, lanes, 1)
    }

    /// `<vscale x lanes x elem>`, as [`Ty::vector`] per `vscale`.
    pub const fn scalable(elem: Scalar, lanes: u32) -> Option<Ty> {
        Ty::lanes_of(elem, lanes, 2)
    }

    const fn lanes_of(elem: Scalar, lanes: u32, form: u16) -> Option<Ty> {
        if matches!(elem, Scalar::None) || !lanes.is_power_of_two() {
            return None;
        }
        let log2 = lanes.trailing_zeros();
        if log2 > LANES_MASK as u32 || lanes * elem.bits(64) > VECTOR_BITS {
            return None;
        }
        Some(Ty(elem as u16 | (form << FORM_SHIFT) | ((log2 as u16) << LANES_SHIFT)))
    }

    /// The compound type of row `row` of the function's `compounds` table.
    pub const fn compound(row: u32) -> Option<Ty> {
        match row < COMPOUND as u32 {
            true => Some(Ty(COMPOUND | row as u16)),
            false => None,
        }
    }

    /// Whether the bits are a type of this encoding: a compound, or a known scalar in a known
    /// form with a lane count its form allows. A compound's row is checked against its table
    /// by the verifier.
    pub const fn is_valid(self) -> bool {
        if self.0 & COMPOUND != 0 {
            return true;
        }
        if self.0 & HIGH_MASK != 0 {
            return false;
        }
        let Some(elem) = Scalar::from_bits(self.0 & ELEM_MASK) else { return false };
        let log2 = (self.0 >> LANES_SHIFT) & LANES_MASK;
        match (self.0 >> FORM_SHIFT) & FORM_MASK {
            0 => log2 == 0 && (!matches!(elem, Scalar::None) || self.0 == 0),
            1 | 2 => {
                !matches!(elem, Scalar::None) && (1u32 << log2) * elem.bits(64) <= VECTOR_BITS
            }
            _ => false,
        }
    }

    pub const fn form(self) -> Form {
        if self.0 & COMPOUND != 0 {
            return Form::Compound;
        }
        match (self.0 >> FORM_SHIFT) & FORM_MASK {
            1 => Form::Vector,
            2 => Form::Scalable,
            _ => Form::Scalar,
        }
    }

    /// The scalar, or a vector's lane; [`Scalar::None`] for no type and a compound.
    pub const fn elem(self) -> Scalar {
        if self.0 & COMPOUND != 0 {
            return Scalar::None;
        }
        match Scalar::from_bits(self.0 & ELEM_MASK) {
            Some(it) => it,
            None => Scalar::None,
        }
    }

    /// The lane count (per `vscale` for a scalable vector), 1 for a scalar.
    pub const fn lanes(self) -> u32 {
        match self.form() {
            Form::Vector | Form::Scalable => 1 << ((self.0 >> LANES_SHIFT) & LANES_MASK),
            Form::Scalar | Form::Compound => 1,
        }
    }

    /// The row of a compound type in its function's `compounds` table.
    pub const fn compound_row(self) -> Option<u32> {
        match self.0 & COMPOUND != 0 {
            true => Some((self.0 & !COMPOUND) as u32),
            false => None,
        }
    }

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Whether this is a scalar or a vector of integers.
    pub const fn is_int(self) -> bool {
        self.elem().is_int()
    }

    /// Whether this is a scalar or a vector of floats.
    pub const fn is_float(self) -> bool {
        self.elem().is_float()
    }

    /// Whether this is a scalar or a vector of pointers.
    pub const fn is_ptr(self) -> bool {
        matches!(self.elem(), Scalar::Ptr)
    }

    pub const fn is_vector(self) -> bool {
        matches!(self.form(), Form::Vector | Form::Scalable)
    }

    pub const fn is_scalar(self) -> bool {
        matches!(self.form(), Form::Scalar) && self.0 != 0
    }

    /// The type of one lane (the type itself for a scalar).
    pub const fn lane(self) -> Ty {
        Ty::scalar(self.elem())
    }

    /// This shape with lanes of `elem`: a comparison's `<n x i1>` of `<n x f32>` operands.
    pub const fn with_elem(self, elem: Scalar) -> Ty {
        match self.form() {
            Form::Scalar => Ty::scalar(elem),
            Form::Vector | Form::Scalable => {
                Ty((self.0 & !ELEM_MASK) | elem as u16)
            }
            Form::Compound => self,
        }
    }

    /// The width in bits (per `vscale` for a scalable vector), a pointer's being `ptr_bits`;
    /// 0 for no type and a compound.
    pub const fn bits(self, ptr_bits: u32) -> u32 {
        self.elem().bits(ptr_bits) * self.lanes()
    }

    /// How many `u32` words a constant of this type takes in `extra`: a scalar's width or a
    /// fixed vector's, in words, rounded up. 0 for a type that has no constants (a pointer, a
    /// scalable vector, a compound).
    pub const fn const_words(self) -> u32 {
        match (self.form(), self.elem()) {
            (_, Scalar::Ptr | Scalar::None) | (Form::Scalable | Form::Compound, _) => 0,
            (Form::Scalar | Form::Vector, elem) => (elem.bits(64) * self.lanes()).div_ceil(32),
        }
    }
}

impl fmt::Display for Ty {
    /// `i64`, `ptr`, `<4 x i32>`, `<vscale x 4 x i32>`, `t3` for row 3 of the
    /// compounds, `none` for no type.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.form() {
            Form::Scalar => f.write_str(self.elem().name()),
            Form::Vector => write!(f, "<{} x {}>", self.lanes(), self.elem().name()),
            Form::Scalable => write!(f, "<vscale x {} x {}>", self.lanes(), self.elem().name()),
            Form::Compound => write!(f, "t{}", self.0 & !COMPOUND),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Form, Scalar, Ty};

    #[test]
    fn every_valid_type_decodes_to_what_made_it() {
        for bits in 0..=u16::MAX {
            let ty = Ty(bits);
            if !ty.is_valid() {
                continue;
            }
            let made = match ty.form() {
                Form::Scalar => Some(Ty::scalar(ty.elem())),
                Form::Vector => Ty::vector(ty.elem(), ty.lanes()),
                Form::Scalable => Ty::scalable(ty.elem(), ty.lanes()),
                Form::Compound => ty.compound_row().and_then(Ty::compound),
            };
            assert_eq!(made, Some(ty), "{bits:#06x} decodes to {ty}, which does not make it back");
        }
    }

    #[test]
    fn vectors_are_at_most_512_bits() {
        assert!(Ty::vector(Scalar::I8, 64).is_some());
        assert!(Ty::vector(Scalar::I8, 128).is_none());
        assert!(Ty::vector(Scalar::F64, 8).is_some());
        assert!(Ty::vector(Scalar::F64, 3).is_none());
        assert!(Ty::vector(Scalar::None, 4).is_none());
        assert_eq!(Ty::vector(Scalar::I32, 4).map(|it| it.const_words()), Some(4));
        assert_eq!(Ty::vector(Scalar::I1, 8).map(|it| it.const_words()), Some(1));
    }
}
