//! This module defines built-in types.
//!
//! A peculiarity of built-in types is that they are always available and are
//! not associated with any particular crate.

use std::fmt;

use hir_expand::name::{AsName, Name};
use intern::{KnownNames, Symbol, sym, sym::Declared};
pub use rustc_ast_ir::IntWidth;
use rustc_ast_ir::{IntTy, UintTy};

/// Different signed int types.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BuiltinInt {
    Isize,
    I8,
    I16,
    I32,
    I64,
    I128,
    /// blaze: `i1`..`i127` but for the standard widths, named only in a crate with
    /// `#![blaze::arbitrary_ints]` (docs/v2/plan/design/arbitrary-ints.md).
    Bits(IntWidth),
}

/// Different unsigned int types.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BuiltinUint {
    Usize,
    U8,
    U16,
    U32,
    U64,
    U128,
    /// blaze: `u1`..`u127` but for the standard widths, named only in a crate with
    /// `#![blaze::arbitrary_ints]`.
    Bits(IntWidth),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BuiltinFloat {
    F16,
    F32,
    F64,
    F128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinType {
    Char,
    Bool,
    Str,
    Int(BuiltinInt),
    Uint(BuiltinUint),
    Float(BuiltinFloat),
}

impl BuiltinType {
    #[rustfmt::skip]
    pub fn all_builtin_types() -> [(Name, BuiltinType); 19] {
        [
            (Name::new_symbol_root(sym::char), BuiltinType::Char),
            (Name::new_symbol_root(sym::bool), BuiltinType::Bool),
            (Name::new_symbol_root(sym::str),  BuiltinType::Str),

            (Name::new_symbol_root(sym::isize), BuiltinType::Int(BuiltinInt::Isize)),
            (Name::new_symbol_root(sym::i8),    BuiltinType::Int(BuiltinInt::I8)),
            (Name::new_symbol_root(sym::i16),   BuiltinType::Int(BuiltinInt::I16)),
            (Name::new_symbol_root(sym::i32),   BuiltinType::Int(BuiltinInt::I32)),
            (Name::new_symbol_root(sym::i64),   BuiltinType::Int(BuiltinInt::I64)),
            (Name::new_symbol_root(sym::i128),  BuiltinType::Int(BuiltinInt::I128)),

            (Name::new_symbol_root(sym::usize), BuiltinType::Uint(BuiltinUint::Usize)),
            (Name::new_symbol_root(sym::u8),    BuiltinType::Uint(BuiltinUint::U8)),
            (Name::new_symbol_root(sym::u16),   BuiltinType::Uint(BuiltinUint::U16)),
            (Name::new_symbol_root(sym::u32),   BuiltinType::Uint(BuiltinUint::U32)),
            (Name::new_symbol_root(sym::u64),   BuiltinType::Uint(BuiltinUint::U64)),
            (Name::new_symbol_root(sym::u128),  BuiltinType::Uint(BuiltinUint::U128)),

            (Name::new_symbol_root(sym::f16), BuiltinType::Float(BuiltinFloat::F16)),
            (Name::new_symbol_root(sym::f32), BuiltinType::Float(BuiltinFloat::F32)),
            (Name::new_symbol_root(sym::f64), BuiltinType::Float(BuiltinFloat::F64)),
            (Name::new_symbol_root(sym::f128), BuiltinType::Float(BuiltinFloat::F128)),
        ]
    }

    /// The builtin type `name` names: its declared index and one load.
    #[inline]
    pub fn by_name(name: &Name) -> Option<Self> {
        Self::by_symbol(name.symbol())
    }

    #[inline]
    fn by_symbol(symbol: &Symbol) -> Option<Self> {
        BY_SYMBOL.get(symbol).map(|at| BY_INDEX[usize::from(at)])
    }

    /// The builtin numeric type a literal's suffix names: one hash of it and one compare.
    #[inline]
    fn by_suffix(suffix: &str) -> Option<Self> {
        SUFFIX.get(suffix).map(|at| BY_INDEX[NUMERIC + at])
    }

    /// The integer of a width other than 8, 16, 32, 64 and 128 that `text` names under
    /// `#![blaze::arbitrary_ints]`: `u` or `i` and a width of 1 to 127 written in decimal
    /// without a leading zero (`u3`, `i24`; not `u03`, `u0` or `u8`, which is the standard
    /// type). Only a crate that opts in asks, so every other crate's `u3` stays a plain name.
    pub fn arbitrary_by_text(text: &str) -> Option<Self> {
        let (sign, digits) = text.split_at_checked(1)?;
        if digits.is_empty() || digits.len() > 3 || digits.starts_with('0') {
            return None;
        }
        if !digits.bytes().all(|it| it.is_ascii_digit()) {
            return None;
        }
        let width = IntWidth::new(digits.parse().ok()?)?;
        match sign {
            "u" => Some(BuiltinType::Uint(BuiltinUint::Bits(width))),
            "i" => Some(BuiltinType::Int(BuiltinInt::Bits(width))),
            _ => None,
        }
    }

    /// [`BuiltinType::arbitrary_by_text`] of a name.
    #[inline]
    pub fn arbitrary_by_name(name: &Name) -> Option<Self> {
        Self::arbitrary_by_text(name.symbol().as_str())
    }

    /// A stable number for each builtin type, for persisted tables: the place in
    /// [`BuiltinType::all_builtin_types`] for the standard ones, then `i1`..`i127` at
    /// `ARBITRARY + width - 1` and `u1`..`u127` 127 further on.
    pub fn code(self) -> u32 {
        match self {
            BuiltinType::Int(BuiltinInt::Bits(w)) => ARBITRARY + w.bits() - 1,
            BuiltinType::Uint(BuiltinUint::Bits(w)) => ARBITRARY + 127 + w.bits() - 1,
            it => BY_INDEX.iter().position(|x| *x == it).map_or(u32::MAX, |at| at as u32),
        }
    }

    /// The builtin type of [`BuiltinType::code`], `None` for a number that names none.
    pub fn from_code(code: u32) -> Option<Self> {
        if let Some(it) = BY_INDEX.get(code as usize) {
            return Some(*it);
        }
        let at = code.checked_sub(ARBITRARY)?;
        let width = IntWidth::new(at % 127 + 1)?;
        match at / 127 {
            0 => Some(BuiltinType::Int(BuiltinInt::Bits(width))),
            1 => Some(BuiltinType::Uint(BuiltinUint::Bits(width))),
            _ => None,
        }
    }
}

/// Where the arbitrary widths start in [`BuiltinType::code`].
const ARBITRARY: u32 = BY_INDEX.len() as u32;

/// Every builtin type, in the order of [`BuiltinType::all_builtin_types`].
const BY_INDEX: [BuiltinType; 19] = [
    BuiltinType::Char,
    BuiltinType::Bool,
    BuiltinType::Str,
    BuiltinType::Int(BuiltinInt::Isize),
    BuiltinType::Int(BuiltinInt::I8),
    BuiltinType::Int(BuiltinInt::I16),
    BuiltinType::Int(BuiltinInt::I32),
    BuiltinType::Int(BuiltinInt::I64),
    BuiltinType::Int(BuiltinInt::I128),
    BuiltinType::Uint(BuiltinUint::Usize),
    BuiltinType::Uint(BuiltinUint::U8),
    BuiltinType::Uint(BuiltinUint::U16),
    BuiltinType::Uint(BuiltinUint::U32),
    BuiltinType::Uint(BuiltinUint::U64),
    BuiltinType::Uint(BuiltinUint::U128),
    BuiltinType::Float(BuiltinFloat::F16),
    BuiltinType::Float(BuiltinFloat::F32),
    BuiltinType::Float(BuiltinFloat::F64),
    BuiltinType::Float(BuiltinFloat::F128),
];

/// Where the numeric types start in [`BY_INDEX`].
const NUMERIC: usize = 3;

/// Each builtin type's name, member `i` naming `BY_INDEX[i]`.
static BY_SYMBOL: KnownNames = KnownNames::new(&[
    Declared::char,
    Declared::bool,
    Declared::str,
    Declared::isize,
    Declared::i8,
    Declared::i16,
    Declared::i32,
    Declared::i64,
    Declared::i128,
    Declared::usize,
    Declared::u8,
    Declared::u16,
    Declared::u32,
    Declared::u64,
    Declared::u128,
    Declared::f16,
    Declared::f32,
    Declared::f64,
    Declared::f128,
]);

/// The literal suffixes, member `i` naming `BY_INDEX[NUMERIC + i]`.
const SUFFIXES: &[&str] = &[
    "isize", "i8", "i16", "i32", "i64", "i128", "usize", "u8", "u16", "u32", "u64", "u128", "f16",
    "f32", "f64", "f128",
];

blaze_phf::phf_table!(static SUFFIX = SUFFIXES;);

const _: () = assert!(NUMERIC + SUFFIXES.len() == BY_INDEX.len());

impl AsName for BuiltinType {
    fn as_name(&self) -> Name {
        match self {
            BuiltinType::Char => Name::new_symbol_root(sym::char),
            BuiltinType::Bool => Name::new_symbol_root(sym::bool),
            BuiltinType::Str => Name::new_symbol_root(sym::str),
            BuiltinType::Int(it) => match it {
                BuiltinInt::Isize => Name::new_symbol_root(sym::isize),
                BuiltinInt::I8 => Name::new_symbol_root(sym::i8),
                BuiltinInt::I16 => Name::new_symbol_root(sym::i16),
                BuiltinInt::I32 => Name::new_symbol_root(sym::i32),
                BuiltinInt::I64 => Name::new_symbol_root(sym::i64),
                BuiltinInt::I128 => Name::new_symbol_root(sym::i128),
                BuiltinInt::Bits(w) => {
                    Name::new_symbol_root(Symbol::intern(IntTy::Bits(*w).name_str()))
                }
            },
            BuiltinType::Uint(it) => match it {
                BuiltinUint::Usize => Name::new_symbol_root(sym::usize),
                BuiltinUint::U8 => Name::new_symbol_root(sym::u8),
                BuiltinUint::U16 => Name::new_symbol_root(sym::u16),
                BuiltinUint::U32 => Name::new_symbol_root(sym::u32),
                BuiltinUint::U64 => Name::new_symbol_root(sym::u64),
                BuiltinUint::U128 => Name::new_symbol_root(sym::u128),
                BuiltinUint::Bits(w) => {
                    Name::new_symbol_root(Symbol::intern(UintTy::Bits(*w).name_str()))
                }
            },
            BuiltinType::Float(it) => match it {
                BuiltinFloat::F16 => Name::new_symbol_root(sym::f16),
                BuiltinFloat::F32 => Name::new_symbol_root(sym::f32),
                BuiltinFloat::F64 => Name::new_symbol_root(sym::f64),
                BuiltinFloat::F128 => Name::new_symbol_root(sym::f128),
            },
        }
    }
}

impl fmt::Display for BuiltinType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuiltinType::Char => f.write_str("char"),
            BuiltinType::Bool => f.write_str("bool"),
            BuiltinType::Str => f.write_str("str"),
            BuiltinType::Int(it) => it.fmt(f),
            BuiltinType::Uint(it) => it.fmt(f),
            BuiltinType::Float(it) => it.fmt(f),
        }
    }
}

impl BuiltinInt {
    #[inline]
    pub fn from_suffix(suffix: &str) -> Option<BuiltinInt> {
        match BuiltinType::by_suffix(suffix)? {
            BuiltinType::Int(it) => Some(it),
            _ => None,
        }
    }

    #[inline]
    pub fn from_suffix_sym(suffix: &Symbol) -> Option<BuiltinInt> {
        match BuiltinType::by_symbol(suffix)? {
            BuiltinType::Int(it) => Some(it),
            _ => None,
        }
    }
}

impl BuiltinUint {
    #[inline]
    pub fn from_suffix(suffix: &str) -> Option<BuiltinUint> {
        match BuiltinType::by_suffix(suffix)? {
            BuiltinType::Uint(it) => Some(it),
            _ => None,
        }
    }

    #[inline]
    pub fn from_suffix_sym(suffix: &Symbol) -> Option<BuiltinUint> {
        match BuiltinType::by_symbol(suffix)? {
            BuiltinType::Uint(it) => Some(it),
            _ => None,
        }
    }
}

impl BuiltinFloat {
    #[inline]
    pub fn from_suffix(suffix: &str) -> Option<BuiltinFloat> {
        match BuiltinType::by_suffix(suffix)? {
            BuiltinType::Float(it) => Some(it),
            _ => None,
        }
    }
}

impl fmt::Display for BuiltinInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BuiltinInt::Isize => "isize",
            BuiltinInt::I8 => "i8",
            BuiltinInt::I16 => "i16",
            BuiltinInt::I32 => "i32",
            BuiltinInt::I64 => "i64",
            BuiltinInt::I128 => "i128",
            BuiltinInt::Bits(w) => IntTy::Bits(*w).name_str(),
        })
    }
}

impl fmt::Display for BuiltinUint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BuiltinUint::Usize => "usize",
            BuiltinUint::U8 => "u8",
            BuiltinUint::U16 => "u16",
            BuiltinUint::U32 => "u32",
            BuiltinUint::U64 => "u64",
            BuiltinUint::U128 => "u128",
            BuiltinUint::Bits(w) => UintTy::Bits(*w).name_str(),
        })
    }
}

impl fmt::Display for BuiltinFloat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BuiltinFloat::F16 => "f16",
            BuiltinFloat::F32 => "f32",
            BuiltinFloat::F64 => "f64",
            BuiltinFloat::F128 => "f128",
        })
    }
}

#[cfg(test)]
mod tests {
    use hir_expand::name::Name;
    use intern::{Symbol, sym};

    use super::{BY_INDEX, BuiltinFloat, BuiltinInt, BuiltinType, BuiltinUint};

    /// The names and suffixes, the same texts interned at run time, their near misses and
    /// other names.
    fn texts() -> Vec<String> {
        let mut texts = Vec::new();
        for (name, _) in BuiltinType::all_builtin_types() {
            let text = name.symbol().as_str();
            texts.extend([
                text.to_owned(),
                format!("{text}_"),
                format!("{text}0"),
                text.to_uppercase(),
                text[..text.len() - 1].to_owned(),
                text[1..].to_owned(),
            ]);
        }
        texts
            .extend(["", "i", "u", "f", "i256", "f80", "Self", "String", "self"].map(String::from));
        texts
    }

    #[test]
    fn by_name_as_a_linear_search() {
        let all = BuiltinType::all_builtin_types();
        for (at, (name, ty)) in all.iter().enumerate() {
            assert_eq!(BY_INDEX[at], *ty);
            assert_eq!(BuiltinType::by_name(name), Some(*ty), "{name:?}");
        }
        let texts = texts();
        let symbols =
            texts.iter().map(|it| Symbol::intern(it)).chain([sym::self_, sym::Integer::get(8)]);
        for symbol in symbols {
            let name = Name::new_symbol_root(symbol);
            let linear = all.iter().find_map(|(n, ty)| (*n == name).then_some(*ty));
            assert_eq!(BuiltinType::by_name(&name), linear, "{name:?}");
        }
    }

    /// `u3` is a builtin only where a crate opts in, and every width has one spelling: the
    /// standard widths and malformed texts name no arbitrary width.
    #[test]
    fn arbitrary_widths_have_one_spelling() {
        let u3 = BuiltinType::arbitrary_by_text("u3");
        assert_eq!(u3.map(|it| it.to_string()).as_deref(), Some("u3"));
        assert_eq!(BuiltinType::by_name(&Name::new_symbol_root(Symbol::intern("u3"))), None);
        for text in
            ["u0", "u8", "i16", "u32", "i64", "u128", "u129", "u03", "u", "x3", "u3a", "u1000"]
        {
            assert_eq!(BuiltinType::arbitrary_by_text(text), None, "{text}");
        }
        for bits in (1..=127).filter(|it| ![8, 16, 32, 64].contains(it)) {
            for sign in ["u", "i"] {
                let text = format!("{sign}{bits}");
                let ty = BuiltinType::arbitrary_by_text(&text).unwrap();
                assert_eq!(ty.to_string(), text);
                assert_eq!(BuiltinType::from_code(ty.code()), Some(ty), "{text}");
            }
        }
        for (_, ty) in BuiltinType::all_builtin_types() {
            assert_eq!(BuiltinType::from_code(ty.code()), Some(ty));
        }
    }

    #[test]
    fn suffixes_as_the_string_match() {
        let int = |s: &str| match s {
            "isize" => Some(BuiltinInt::Isize),
            "i8" => Some(BuiltinInt::I8),
            "i16" => Some(BuiltinInt::I16),
            "i32" => Some(BuiltinInt::I32),
            "i64" => Some(BuiltinInt::I64),
            "i128" => Some(BuiltinInt::I128),
            _ => None,
        };
        let uint = |s: &str| match s {
            "usize" => Some(BuiltinUint::Usize),
            "u8" => Some(BuiltinUint::U8),
            "u16" => Some(BuiltinUint::U16),
            "u32" => Some(BuiltinUint::U32),
            "u64" => Some(BuiltinUint::U64),
            "u128" => Some(BuiltinUint::U128),
            _ => None,
        };
        let float = |s: &str| match s {
            "f16" => Some(BuiltinFloat::F16),
            "f32" => Some(BuiltinFloat::F32),
            "f64" => Some(BuiltinFloat::F64),
            "f128" => Some(BuiltinFloat::F128),
            _ => None,
        };
        for text in texts() {
            assert_eq!(BuiltinInt::from_suffix(&text), int(&text), "{text}");
            assert_eq!(BuiltinUint::from_suffix(&text), uint(&text), "{text}");
            assert_eq!(BuiltinFloat::from_suffix(&text), float(&text), "{text}");
            let symbol = Symbol::intern(&text);
            assert_eq!(BuiltinInt::from_suffix_sym(&symbol), int(&text), "{text}");
            assert_eq!(BuiltinUint::from_suffix_sym(&symbol), uint(&text), "{text}");
        }
    }
}
