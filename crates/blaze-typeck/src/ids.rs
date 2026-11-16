//! The small portable names the driver passes around: item keys, identifiers, node indices,
//! ranges, and the primitive kinds both type forms share.

use crate::hash::{ContentHash, StableHasher};

/// An item's portable identity (docs/v2/plan/design/identity.md). Becomes `blaze_id::ItemKey`
/// when blaze-id is at zero third-party crates; until then a 128-bit content id the caller
/// computes from the key's parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemKey(pub u128);

impl ContentHash for ItemKey {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u128(self.0);
    }
}

/// An identifier, by the stable hash of its text. Portable, and what a question to the world
/// names; the text itself lives in the body's lowering for printing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(pub u64);

impl Name {
    /// The name of `text`.
    pub const fn of(text: &str) -> Name {
        Name(StableHasher::new("name").bytes(text.as_bytes()).finish())
    }
}

impl ContentHash for Name {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u64(self.0);
    }
}

macro_rules! index {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u32);

        impl $name {
            /// The index as a `usize`, for a column.
            pub const fn ix(self) -> usize {
                self.0 as usize
            }
        }

        impl ContentHash for $name {
            fn hash_into(&self, h: &mut StableHasher) {
                h.u32(self.0);
            }
        }
    };
}

index!(
    /// An expression of the body, an index into `BodyView::exprs`.
    ExprId
);
index!(
    /// A pattern of the body, an index into `BodyView::pats`.
    PatId
);
index!(
    /// A statement of the body, an index into `BodyView::stmts`.
    StmtId
);
index!(
    /// A path written in the body, an index into `BodyView::paths`.
    PathIdx
);
index!(
    /// A block scope of the body, as its lowering numbered them in source order: what the
    /// world is asked traits in scope and paths at.
    ScopeId
);

/// A range of a side column: `start..start + len`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct List {
    pub start: u32,
    pub len: u32,
}

impl List {
    /// The empty list.
    pub const EMPTY: List = List { start: 0, len: 0 };

    /// The range as `usize`s.
    pub const fn range(self) -> core::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }
}

/// A byte range relative to the start of the body's item (rule 71): `file:line:col` exists only
/// when printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Range {
    pub start: u32,
    pub end: u32,
}

/// `mut` or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mutbl {
    Not,
    Mut,
}

/// Signed integer types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntTy {
    Isize,
    I8,
    I16,
    I32,
    I64,
    I128,
}

/// Unsigned integer types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UintTy {
    Usize,
    U8,
    U16,
    U32,
    U64,
    U128,
}

/// Float types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FloatTy {
    F16,
    F32,
    F64,
    F128,
}

/// The edition of the body, which binding modes and never-type fallback depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Edition {
    E2015,
    E2018,
    E2021,
    E2024,
}

/// The lang items the driver itself asks for. Each is a question to the world, so a body's
/// read list says which it used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LangItem {
    Sized,
    Copy,
    Deref,
    DerefMut,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    BitAndAssign,
    BitOrAssign,
    BitXorAssign,
    ShlAssign,
    ShrAssign,
    Neg,
    Not,
    PartialEq,
    PartialOrd,
    Index,
    IndexMut,
    FnOnce,
    FnMut,
    Fn,
}

impl ContentHash for LangItem {
    fn hash_into(&self, h: &mut StableHasher) {
        h.tag(*self as u8);
    }
}
