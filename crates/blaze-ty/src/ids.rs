//! The handles: each is 4 bytes, `Copy`, and means something only in the arena that made it
//! (a record holds the portable form, `crate::portable`, never these).
//!
//! A [`Node`] is any handle with its class in its two low bits: a type (or predicate), a
//! region, a const, or a list. A generic argument ([`Arg`]) is a node of the first three
//! classes; a list's elements are `Arg`s. Child columns of every arena hold nodes, so one walk
//! serves every class.

use blaze_int::{Cast, Int};

/// The class of a node, in its two low bits.
pub const TAG_TY: u32 = 0;
/// A region.
pub const TAG_REGION: u32 = 1;
/// A const.
pub const TAG_CONST: u32 = 2;
/// A list (never an `Arg`).
pub const TAG_LIST: u32 = 3;

/// The most rows of any column: ids keep two bits for the class.
pub const MOST_ROWS: u32 = (1 << 30) - 1;

/// A type, or a predicate (the same arena, kinds of their own: `crate::kind`).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Ty(pub(crate) u32);

impl Ty {
    pub const BOOL: Ty = Ty(0);
    pub const CHAR: Ty = Ty(1);
    pub const STR: Ty = Ty(2);
    pub const NEVER: Ty = Ty(3);
    pub const ERROR: Ty = Ty(4);
    /// `()`.
    pub const UNIT: Ty = Ty(5);
    pub(crate) const INT0: u32 = 6;
    pub(crate) const UINT0: u32 = 12;
    pub(crate) const FLOAT0: u32 = 18;
    /// How many types every arena starts with (the constants above, the ints and floats).
    pub(crate) const PRESET: u32 = 22;

    /// The type of a signed integer, without a probe.
    pub const fn int(t: IntTy) -> Ty {
        Ty(Ty::INT0 + t.code())
    }

    /// The type of an unsigned integer, without a probe.
    pub const fn uint(t: UintTy) -> Ty {
        Ty(Ty::UINT0 + t.code())
    }

    /// The type of a float, without a probe.
    pub const fn float(t: FloatTy) -> Ty {
        Ty(Ty::FLOAT0 + t.code())
    }

    /// Its row in the arena.
    pub const fn index(self) -> u32 {
        self.0
    }

    pub const fn arg(self) -> Arg {
        Arg(self.0 << 2 | TAG_TY)
    }

    pub const fn node(self) -> Node {
        Node(self.0 << 2 | TAG_TY)
    }
}

/// A predicate (a clause or a goal): a row of the type arena with a predicate kind.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Pred(pub(crate) u32);

impl Pred {
    pub const fn index(self) -> u32 {
        self.0
    }

    pub const fn node(self) -> Node {
        Node(self.0 << 2 | TAG_TY)
    }
}

/// A region.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Region(pub(crate) u32);

impl Region {
    pub const STATIC: Region = Region(0);
    pub const ERASED: Region = Region(1);
    pub const ERROR: Region = Region(2);
    pub(crate) const PRESET: u32 = 3;

    pub const fn index(self) -> u32 {
        self.0
    }

    pub const fn arg(self) -> Arg {
        Arg(self.0 << 2 | TAG_REGION)
    }

    pub const fn node(self) -> Node {
        Node(self.0 << 2 | TAG_REGION)
    }
}

/// A const.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Const(pub(crate) u32);

impl Const {
    pub const ERROR: Const = Const(0);
    pub(crate) const PRESET: u32 = 1;

    pub const fn index(self) -> u32 {
        self.0
    }

    pub const fn arg(self) -> Arg {
        Arg(self.0 << 2 | TAG_CONST)
    }

    pub const fn node(self) -> Node {
        Node(self.0 << 2 | TAG_CONST)
    }
}

/// An interned list of generic arguments (also tuple fields, a fn signature's inputs then its
/// output, a `dyn`'s predicates). `List::EMPTY` is the empty one.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct List(pub(crate) u32);

impl List {
    pub const EMPTY: List = List(0);

    pub const fn index(self) -> u32 {
        self.0
    }

    pub const fn node(self) -> Node {
        Node(self.0 << 2 | TAG_LIST)
    }
}

/// A generic argument: a type, a region or a const, tagged in its two low bits.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Arg(pub(crate) u32);

/// What an argument is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgKind {
    Ty(Ty),
    Region(Region),
    Const(Const),
}

impl Arg {
    pub const fn kind(self) -> ArgKind {
        match self.0 & 3 {
            TAG_TY => ArgKind::Ty(Ty(self.0 >> 2)),
            TAG_REGION => ArgKind::Region(Region(self.0 >> 2)),
            _ => ArgKind::Const(Const(self.0 >> 2)),
        }
    }

    pub const fn ty(self) -> Option<Ty> {
        if self.0 & 3 == TAG_TY { Some(Ty(self.0 >> 2)) } else { None }
    }

    pub const fn region(self) -> Option<Region> {
        if self.0 & 3 == TAG_REGION { Some(Region(self.0 >> 2)) } else { None }
    }

    pub const fn konst(self) -> Option<Const> {
        if self.0 & 3 == TAG_CONST { Some(Const(self.0 >> 2)) } else { None }
    }

    pub const fn node(self) -> Node {
        Node(self.0)
    }

    /// The word: what a list holds.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// Any handle with its class: what child columns, walks and memo keys hold.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Node(pub(crate) u32);

impl Node {
    pub const fn tag(self) -> u32 {
        self.0 & 3
    }

    /// The row in its class's arena.
    pub const fn index(self) -> u32 {
        self.0 >> 2
    }

    /// The argument, unless it is a list.
    pub const fn arg(self) -> Option<Arg> {
        if self.0 & 3 == TAG_LIST { None } else { Some(Arg(self.0)) }
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    /// The type (or predicate) this node is, if it is of the type class.
    pub const fn as_ty(self) -> Option<Ty> {
        if self.0 & 3 == TAG_TY { Some(Ty(self.0 >> 2)) } else { None }
    }

    /// The region this node is, if it is of the region class.
    pub const fn as_region(self) -> Option<Region> {
        if self.0 & 3 == TAG_REGION { Some(Region(self.0 >> 2)) } else { None }
    }

    /// The const this node is, if it is of the const class.
    pub const fn as_const(self) -> Option<Const> {
        if self.0 & 3 == TAG_CONST { Some(Const(self.0 >> 2)) } else { None }
    }

    /// The list this node is, if it is of the list class.
    pub const fn as_list(self) -> Option<List> {
        if self.0 & 3 == TAG_LIST { Some(List(self.0 >> 2)) } else { None }
    }

    pub(crate) const fn ty(self) -> Ty {
        Ty(self.0 >> 2)
    }

    pub(crate) const fn region(self) -> Region {
        Region(self.0 >> 2)
    }

    pub(crate) const fn konst(self) -> Const {
        Const(self.0 >> 2)
    }

    pub(crate) const fn list(self) -> List {
        List(self.0 >> 2)
    }
}

/// An item, by its number in the arena's item column (`Tys::def`, `Tys::item_key`).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Def(pub(crate) u32);

impl Def {
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// A universe: placeholders and variables of a higher one cannot be named from a lower one
/// (`rustc_type_ir::UniverseIndex`).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Universe(pub u32);

impl Universe {
    pub const ROOT: Universe = Universe(0);

    /// Whether a variable of `self` can name something of `other`.
    pub const fn can_name(self, other: Universe) -> bool {
        self.0 >= other.0
    }
}

/// `mut` or not.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Mutability {
    Not,
    Mut,
}

macro_rules! codes {
    ($(#[$m:meta])* $name:ident { $($v:ident = $c:literal),* $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub enum $name { $($v),* }

        impl $name {
            /// Its code in a type's payload column.
            pub const fn code(self) -> u32 {
                match self { $($name::$v => $c),* }
            }

            /// The variant of a code, if one has it.
            pub const fn of_code(code: u32) -> Option<$name> {
                match code { $($c => Some($name::$v),)* _ => None }
            }
        }
    };
}

codes!(
    /// Signed integers, in `rustc_type_ir::IntTy`'s order.
    IntTy { Isize = 0, I8 = 1, I16 = 2, I32 = 3, I64 = 4, I128 = 5 }
);
codes!(
    /// Unsigned integers, in `rustc_type_ir::UintTy`'s order.
    UintTy { Usize = 0, U8 = 1, U16 = 2, U32 = 3, U64 = 4, U128 = 5 }
);
codes!(
    /// Floats, in `rustc_type_ir::FloatTy`'s order.
    FloatTy { F16 = 0, F32 = 1, F64 = 2, F128 = 3 }
);

/// How a relation passes through a position (`rustc_type_ir::Variance`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Variance {
    Covariant,
    Invariant,
    Contravariant,
    Bivariant,
}

impl Variance {
    /// The variance of a position of variance `inner` inside one of `self`
    /// (`Variance::xform`, rustc_type_ir/src/ty_kind.rs).
    pub const fn xform(self, inner: Variance) -> Variance {
        match (self, inner) {
            (Variance::Covariant, x) => x,
            (Variance::Invariant, _) => Variance::Invariant,
            (Variance::Contravariant, Variance::Covariant) => Variance::Contravariant,
            (Variance::Contravariant, Variance::Contravariant) => Variance::Covariant,
            (Variance::Contravariant, Variance::Invariant) => Variance::Invariant,
            (Variance::Contravariant, Variance::Bivariant) => Variance::Bivariant,
            (Variance::Bivariant, _) => Variance::Bivariant,
        }
    }

    pub(crate) const fn code(self) -> u32 {
        match self {
            Variance::Covariant => 0,
            Variance::Invariant => 1,
            Variance::Contravariant => 2,
            Variance::Bivariant => 3,
        }
    }
}

/// An inference variable of a type.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum InferTy {
    TyVar(u32),
    IntVar(u32),
    FloatVar(u32),
}

/// The kinds of a variable table.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum VarKind {
    Ty,
    Int,
    Float,
    Region,
    Const,
}

/// An index as a column position.
#[inline(always)]
pub(crate) fn ix(i: u32) -> usize {
    blaze_int::idx(i)
}

/// A column length as a row id, refused past [`MOST_ROWS`].
#[inline(always)]
pub(crate) fn row(len: usize) -> Option<u32> {
    match blaze_int::len32(len) {
        Ok(n) if n <= MOST_ROWS => Some(n),
        _ => None,
    }
}

/// A small count as a `u8` (a rank, a kind), saturating.
#[inline(always)]
pub(crate) fn small(x: u32) -> u8 {
    x.saturate::<u8>()
}

/// A `u8` as a column position.
#[inline(always)]
pub(crate) const fn ix8(i: u8) -> usize {
    Cast::<u8, usize>::widen(i)
}
