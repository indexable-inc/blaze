//! Common utilities shared by both `rustc_ast` and `rustc_type_ir`.
//!
//! blaze: vendored from crates.io `ra-ap-rustc_ast_ir` 0.179.0 with the integer widths of
//! `#![blaze::arbitrary_ints]` added (`IntWidth`, `IntTy::Bits`, `UintTy::Bits`;
//! vendor/rustc_ast_ir/SOURCE.md).
//!
//! Don't depend on this crate directly; both of those crates should re-export
//! the functionality. Additionally, if you're in scope of `rustc_middle`, then
//! prefer imports via that too, to avoid needing to directly depend on (e.g.)
//! `rustc_type_ir` for a single import.

// tidy-alphabetical-start
#![cfg_attr(feature = "nightly", allow(internal_features))]
#![cfg_attr(feature = "nightly", feature(rustc_attrs))]
// tidy-alphabetical-end

use std::fmt;

#[cfg(feature = "nightly")]
use rustc_macros::{Decodable_NoContext, Encodable_NoContext, StableHash};
#[cfg(feature = "nightly")]
use rustc_span::{Symbol, sym};

pub mod visit;

/// blaze: the width of an integer type of the extension `#![blaze::arbitrary_ints]`
/// (docs/v2/plan/lanes/frontend/arbitrary-ints.md): 1 to 127 bits, never 8, 16, 32 or 64. The
/// standard widths are the standard types (`u8` is [`UintTy::U8`], never `Bits` of 8), so
/// [`IntWidth::new`] refuses them and every type has exactly one spelling.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IntWidth(u8);

impl IntWidth {
    /// The width of `bits` bits, if it is one only the extension has: `None` for 0, for the
    /// standard widths 8, 16, 32, 64 and 128, and above 128.
    pub const fn new(bits: u32) -> Option<IntWidth> {
        match bits {
            0 | 8 | 16 | 32 | 64 | 128.. => None,
            _ => Some(IntWidth(bits as u8)),
        }
    }

    /// How many bits a value has: its range is `0..2^bits` unsigned, `-2^(bits-1)..2^(bits-1)`
    /// signed.
    pub const fn bits(self) -> u32 {
        self.0 as u32
    }

    /// The bits of the standard integer that stores it: the smallest power-of-two byte count
    /// that holds `bits` (`u3` in 8, `u24` in 32, `u65` in 128). Its size and alignment are
    /// that standard type's; the bits above `bits` are the value's sign (signed) or zero
    /// (unsigned), and every other pattern is a niche.
    pub const fn storage_bits(self) -> u32 {
        match self.0 {
            0..=8 => 8,
            9..=16 => 16,
            17..=32 => 32,
            33..=64 => 64,
            _ => 128,
        }
    }
}

impl fmt::Debug for IntWidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// `prefix` and the decimal digits of each width below 129, at its width: the names of the
/// types, with no allocation.
const fn width_names(prefix: u8) -> [[u8; 4]; 129] {
    let mut names = [[0u8; 4]; 129];
    let mut bits = 1;
    while bits < 129 {
        let name = &mut names[bits];
        name[0] = prefix;
        let mut at = 1;
        if bits >= 100 {
            name[at] = b'0' + (bits / 100) as u8;
            at += 1;
        }
        if bits >= 10 {
            name[at] = b'0' + (bits / 10 % 10) as u8;
            at += 1;
        }
        name[at] = b'0' + (bits % 10) as u8;
        bits += 1;
    }
    names
}

static INT_NAMES: [[u8; 4]; 129] = width_names(b'i');
static UINT_NAMES: [[u8; 4]; 129] = width_names(b'u');

fn width_name(names: &'static [[u8; 4]; 129], width: IntWidth) -> &'static str {
    let name = &names[width.bits() as usize];
    let len = name.iter().position(|&it| it == 0).unwrap_or(name.len());
    match std::str::from_utf8(&name[..len]) {
        Ok(name) => name,
        Err(_) => unreachable!("a type name is ASCII"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "nightly", derive(Encodable_NoContext, Decodable_NoContext, StableHash))]
pub enum IntTy {
    Isize,
    I8,
    I16,
    I32,
    I64,
    I128,
    /// blaze: `i1`..`i127` but the standard widths (`#![blaze::arbitrary_ints]`).
    Bits(IntWidth),
}

impl IntTy {
    /// The signed type of `bits` bits: a standard one for 8, 16, 32, 64 and 128 (the extension
    /// adds no second `i8`), else the extension's; `None` for 0 and above 128.
    pub const fn of_width(bits: u32) -> Option<IntTy> {
        Some(match bits {
            8 => IntTy::I8,
            16 => IntTy::I16,
            32 => IntTy::I32,
            64 => IntTy::I64,
            128 => IntTy::I128,
            _ => match IntWidth::new(bits) {
                Some(width) => IntTy::Bits(width),
                None => return None,
            },
        })
    }

    pub fn name_str(&self) -> &'static str {
        match *self {
            IntTy::Isize => "isize",
            IntTy::I8 => "i8",
            IntTy::I16 => "i16",
            IntTy::I32 => "i32",
            IntTy::I64 => "i64",
            IntTy::I128 => "i128",
            IntTy::Bits(width) => width_name(&INT_NAMES, width),
        }
    }

    #[cfg(feature = "nightly")]
    pub fn name(self) -> Symbol {
        match self {
            IntTy::Isize => sym::isize,
            IntTy::I8 => sym::i8,
            IntTy::I16 => sym::i16,
            IntTy::I32 => sym::i32,
            IntTy::I64 => sym::i64,
            IntTy::I128 => sym::i128,
            IntTy::Bits(_) => Symbol::intern(self.name_str()),
        }
    }

    /// How many bits a value has (3 for `i3`, whose storage is 8: [`IntTy::storage`]).
    pub fn bit_width(&self) -> Option<u64> {
        Some(match *self {
            IntTy::Isize => return None,
            IntTy::I8 => 8,
            IntTy::I16 => 16,
            IntTy::I32 => 32,
            IntTy::I64 => 64,
            IntTy::I128 => 128,
            IntTy::Bits(width) => u64::from(width.bits()),
        })
    }

    /// blaze: the standard type whose size, alignment and ABI it has: itself for a standard
    /// type, the one of [`IntWidth::storage_bits`] for the extension's (`i3` is stored as
    /// `i8`).
    pub fn storage(self) -> IntTy {
        match self {
            IntTy::Bits(width) => match width.storage_bits() {
                8 => IntTy::I8,
                16 => IntTy::I16,
                32 => IntTy::I32,
                64 => IntTy::I64,
                _ => IntTy::I128,
            },
            _ => self,
        }
    }

    /// blaze: the width, if the type is one of the extension's.
    pub fn arbitrary_width(self) -> Option<IntWidth> {
        match self {
            IntTy::Bits(width) => Some(width),
            _ => None,
        }
    }

    /// blaze: a stable number for persisted tables and identity hashes: a standard type's
    /// variant number (as `as u8` gave before the extension), an arbitrary width 128 plus
    /// its width, so every number fits a byte.
    pub const fn code(self) -> u8 {
        match self {
            IntTy::Isize => 0,
            IntTy::I8 => 1,
            IntTy::I16 => 2,
            IntTy::I32 => 3,
            IntTy::I64 => 4,
            IntTy::I128 => 5,
            IntTy::Bits(width) => 128 + width.bits() as u8,
        }
    }

    /// blaze: the type of [`IntTy::code`], `None` for a number that names none.
    pub const fn from_code(code: u8) -> Option<IntTy> {
        Some(match code {
            0 => IntTy::Isize,
            1 => IntTy::I8,
            2 => IntTy::I16,
            3 => IntTy::I32,
            4 => IntTy::I64,
            5 => IntTy::I128,
            128.. => match IntWidth::new(code as u32 - 128) {
                Some(width) => IntTy::Bits(width),
                None => return None,
            },
            _ => return None,
        })
    }

    pub fn normalize(&self, target_width: u16) -> Self {
        match self {
            IntTy::Isize => match target_width {
                16 => IntTy::I16,
                32 => IntTy::I32,
                64 => IntTy::I64,
                _ => unreachable!(),
            },
            _ => *self,
        }
    }

    pub fn to_unsigned(self) -> UintTy {
        match self {
            IntTy::Isize => UintTy::Usize,
            IntTy::I8 => UintTy::U8,
            IntTy::I16 => UintTy::U16,
            IntTy::I32 => UintTy::U32,
            IntTy::I64 => UintTy::U64,
            IntTy::I128 => UintTy::U128,
            IntTy::Bits(width) => UintTy::Bits(width),
        }
    }
}

impl fmt::Debug for IntTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name_str())
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Copy)]
#[cfg_attr(feature = "nightly", derive(Encodable_NoContext, Decodable_NoContext, StableHash))]
pub enum UintTy {
    Usize,
    U8,
    U16,
    U32,
    U64,
    U128,
    /// blaze: `u1`..`u127` but the standard widths (`#![blaze::arbitrary_ints]`).
    Bits(IntWidth),
}

impl UintTy {
    /// The unsigned type of `bits` bits: a standard one for 8, 16, 32, 64 and 128, else the
    /// extension's; `None` for 0 and above 128.
    pub const fn of_width(bits: u32) -> Option<UintTy> {
        Some(match bits {
            8 => UintTy::U8,
            16 => UintTy::U16,
            32 => UintTy::U32,
            64 => UintTy::U64,
            128 => UintTy::U128,
            _ => match IntWidth::new(bits) {
                Some(width) => UintTy::Bits(width),
                None => return None,
            },
        })
    }

    pub fn name_str(&self) -> &'static str {
        match *self {
            UintTy::Usize => "usize",
            UintTy::U8 => "u8",
            UintTy::U16 => "u16",
            UintTy::U32 => "u32",
            UintTy::U64 => "u64",
            UintTy::U128 => "u128",
            UintTy::Bits(width) => width_name(&UINT_NAMES, width),
        }
    }

    #[cfg(feature = "nightly")]
    pub fn name(self) -> Symbol {
        match self {
            UintTy::Usize => sym::usize,
            UintTy::U8 => sym::u8,
            UintTy::U16 => sym::u16,
            UintTy::U32 => sym::u32,
            UintTy::U64 => sym::u64,
            UintTy::U128 => sym::u128,
            UintTy::Bits(_) => Symbol::intern(self.name_str()),
        }
    }

    /// How many bits a value has (3 for `u3`, whose storage is 8: [`UintTy::storage`]).
    pub fn bit_width(&self) -> Option<u64> {
        Some(match *self {
            UintTy::Usize => return None,
            UintTy::U8 => 8,
            UintTy::U16 => 16,
            UintTy::U32 => 32,
            UintTy::U64 => 64,
            UintTy::U128 => 128,
            UintTy::Bits(width) => u64::from(width.bits()),
        })
    }

    /// blaze: the standard type whose size, alignment and ABI it has: itself for a standard
    /// type, the one of [`IntWidth::storage_bits`] for the extension's (`u3` is stored as
    /// `u8`).
    pub fn storage(self) -> UintTy {
        match self {
            UintTy::Bits(width) => match width.storage_bits() {
                8 => UintTy::U8,
                16 => UintTy::U16,
                32 => UintTy::U32,
                64 => UintTy::U64,
                _ => UintTy::U128,
            },
            _ => self,
        }
    }

    /// blaze: the width, if the type is one of the extension's.
    pub fn arbitrary_width(self) -> Option<IntWidth> {
        match self {
            UintTy::Bits(width) => Some(width),
            _ => None,
        }
    }

    /// blaze: a stable number for persisted tables and identity hashes: a standard type's
    /// variant number (as `as u8` gave before the extension), an arbitrary width 128 plus
    /// its width, so every number fits a byte.
    pub const fn code(self) -> u8 {
        match self {
            UintTy::Usize => 0,
            UintTy::U8 => 1,
            UintTy::U16 => 2,
            UintTy::U32 => 3,
            UintTy::U64 => 4,
            UintTy::U128 => 5,
            UintTy::Bits(width) => 128 + width.bits() as u8,
        }
    }

    /// blaze: the type of [`UintTy::code`], `None` for a number that names none.
    pub const fn from_code(code: u8) -> Option<UintTy> {
        Some(match code {
            0 => UintTy::Usize,
            1 => UintTy::U8,
            2 => UintTy::U16,
            3 => UintTy::U32,
            4 => UintTy::U64,
            5 => UintTy::U128,
            128.. => match IntWidth::new(code as u32 - 128) {
                Some(width) => UintTy::Bits(width),
                None => return None,
            },
            _ => return None,
        })
    }

    pub fn normalize(&self, target_width: u16) -> Self {
        match self {
            UintTy::Usize => match target_width {
                16 => UintTy::U16,
                32 => UintTy::U32,
                64 => UintTy::U64,
                _ => unreachable!(),
            },
            _ => *self,
        }
    }

    pub fn to_signed(self) -> IntTy {
        match self {
            UintTy::Usize => IntTy::Isize,
            UintTy::U8 => IntTy::I8,
            UintTy::U16 => IntTy::I16,
            UintTy::U32 => IntTy::I32,
            UintTy::U64 => IntTy::I64,
            UintTy::U128 => IntTy::I128,
            UintTy::Bits(width) => IntTy::Bits(width),
        }
    }
}

impl fmt::Debug for UintTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "nightly", derive(Encodable_NoContext, Decodable_NoContext, StableHash))]
pub enum FloatTy {
    F16,
    F32,
    F64,
    F128,
}

impl FloatTy {
    pub fn name_str(self) -> &'static str {
        match self {
            FloatTy::F16 => "f16",
            FloatTy::F32 => "f32",
            FloatTy::F64 => "f64",
            FloatTy::F128 => "f128",
        }
    }

    #[cfg(feature = "nightly")]
    pub fn name(self) -> Symbol {
        match self {
            FloatTy::F16 => sym::f16,
            FloatTy::F32 => sym::f32,
            FloatTy::F64 => sym::f64,
            FloatTy::F128 => sym::f128,
        }
    }

    pub fn bit_width(self) -> u64 {
        match self {
            FloatTy::F16 => 16,
            FloatTy::F32 => 32,
            FloatTy::F64 => 64,
            FloatTy::F128 => 128,
        }
    }
}

impl fmt::Debug for FloatTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name_str())
    }
}

/// The movability of a coroutine / closure literal:
/// whether a coroutine contains self-references, causing it to be `!Unpin`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Copy)]
#[cfg_attr(feature = "nightly", derive(Encodable_NoContext, Decodable_NoContext, StableHash))]
pub enum Movability {
    /// May contain self-references, `!Unpin`.
    Static,
    /// Must not contain self-references, `Unpin`.
    Movable,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Copy)]
#[cfg_attr(feature = "nightly", derive(Encodable_NoContext, Decodable_NoContext, StableHash))]
pub enum Mutability {
    // N.B. Order is deliberate, so that Not < Mut
    Not,
    Mut,
}

impl Mutability {
    pub fn invert(self) -> Self {
        match self {
            Mutability::Mut => Mutability::Not,
            Mutability::Not => Mutability::Mut,
        }
    }

    /// Returns `""` (empty string) or `"mut "` depending on the mutability.
    pub fn prefix_str(self) -> &'static str {
        match self {
            Mutability::Mut => "mut ",
            Mutability::Not => "",
        }
    }

    /// Returns `"&"` or `"&mut "` depending on the mutability.
    pub fn ref_prefix_str(self) -> &'static str {
        match self {
            Mutability::Not => "&",
            Mutability::Mut => "&mut ",
        }
    }

    /// Returns `"const"` or `"mut"` depending on the mutability.
    pub fn ptr_str(self) -> &'static str {
        match self {
            Mutability::Not => "const",
            Mutability::Mut => "mut",
        }
    }

    /// Returns `""` (empty string) or `"mutably "` depending on the mutability.
    pub fn mutably_str(self) -> &'static str {
        match self {
            Mutability::Not => "",
            Mutability::Mut => "mutably ",
        }
    }

    /// Return `true` if self is mutable
    pub fn is_mut(self) -> bool {
        matches!(self, Self::Mut)
    }

    /// Return `true` if self is **not** mutable
    pub fn is_not(self) -> bool {
        matches!(self, Self::Not)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Copy)]
#[cfg_attr(feature = "nightly", derive(Encodable_NoContext, Decodable_NoContext, StableHash))]
pub enum Pinnedness {
    Not,
    Pinned,
}

impl Pinnedness {
    /// Return `true` if self is pinned
    pub fn is_pinned(self) -> bool {
        matches!(self, Self::Pinned)
    }

    /// Returns `""` (empty string), "mut", `"pin mut "` or `"pin const "` depending
    /// on the pinnedness and mutability.
    pub fn prefix_str(self, mutbl: Mutability) -> &'static str {
        match (self, mutbl) {
            (Pinnedness::Pinned, Mutability::Mut) => "pin mut ",
            (Pinnedness::Pinned, Mutability::Not) => "pin const ",
            (Pinnedness::Not, Mutability::Mut) => "mut ",
            (Pinnedness::Not, Mutability::Not) => "",
        }
    }
}
