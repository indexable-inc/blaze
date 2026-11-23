//! The small portable names the driver passes around: item keys, identifiers, node indices,
//! ranges, and the primitive kinds both type forms share.

use crate::hash::{ContentHash, StableHasher};

/// A fn pointer's ABI and safety: the solver's type, one home for both type forms.
pub use blaze_solve::{Abi, FnPtrHeader, Safety};

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

    /// A tuple field's name: its index in decimal, `Name::of("0")`, as the world names a tuple
    /// struct's and a tuple variant's fields (blaze-ladder's front2 heads.rs, the fixture).
    pub const fn of_index(i: u32) -> Name {
        let mut buf = [0u8; 10];
        let mut at = buf.len();
        let mut n = i;
        loop {
            at -= 1;
            buf[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        let (_, digits) = buf.split_at(at);
        Name(StableHasher::new("name").bytes(digits).finish())
    }
}

/// A positional field named by index is the field the world names by its decimal text (the
/// Reference, "Struct expressions": a tuple struct's fields are named by decimal integers,
/// `S { 0: x }`, the same field `s.0` reads under "Tuple indexing expressions"); the bug
/// this guards was `p.0` looked up as `Name(0)`.
#[test]
fn a_positional_name_is_its_decimal_text() {
    for i in [0u32, 1, 9, 10, 99, 100, 4_294_967_295] {
        assert_eq!(Name::of_index(i), Name::of(&std::format!("{i}")), "{i}");
    }
}

impl ContentHash for Name {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u64(self.0);
    }
}

/// Content hash of a structural record's sorted canonical field names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordSchema(pub u64);

impl RecordSchema {
    /// The empty structural record `{}`.
    pub const EMPTY: RecordSchema = RecordSchema(
        StableHasher::new("record-schema").bytes(&0u64.to_le_bytes()).finish(),
    );

    /// Computes the schema hash from canonical sorted field names.
    pub fn of_sorted_names(names: &[Name]) -> RecordSchema {
        let mut h = StableHasher::new("record-schema");
        h.u64(u64::try_from(names.len()).unwrap_or(u64::MAX));
        for &name in names {
            h.u64(name.0);
        }
        RecordSchema(h.finish())
    }

    /// Deterministic synthetic key under which anonymous record type-level field defaults are
    /// indexed in `World::field_defaults`.
    pub fn defaults_key(self) -> ItemKey {
        let mut lo = StableHasher::new("blaze-typeck record defaults lo");
        lo.u64(self.0);
        let mut hi = StableHasher::new("blaze-typeck record defaults hi");
        hi.u64(self.0);
        ItemKey(u128::from(lo.finish()) | (u128::from(hi.finish()) << 64))
    }
}

impl ContentHash for RecordSchema {
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
    // What the trait solver asks (blaze-solve's `Lang`).
    MetaSized,
    PointeeSized,
    Clone,
    Send,
    Sync,
    Unpin,
    Drop,
    Destruct,
    Tuple,
    Unsize,
    CoerceUnsized,
    DispatchFromDyn,
    FnPtrTrait,
    /// `FnOnce::Output`.
    FnOnceOutput,
    Pointee,
    /// `Pointee::Metadata`.
    Metadata,
    DynMetadata,
    DiscriminantKind,
    /// `DiscriminantKind::Discriminant`.
    Discriminant,
    PhantomData,
    OwnedBox,
    /// `core::ffi::CStr` (lang "CStr"): a C string literal's referent.
    CStr,
    ManuallyDrop,
    // What `.await` and `format_args!` ask (`Expr::Await`, `Expr::FormatArgs`).
    /// The trait `Future`.
    Future,
    /// The trait `IntoFuture`; `.await` is typed `<T as IntoFuture>::Output`.
    IntoFuture,
    /// The struct `core::fmt::Arguments` (`format_arguments`), `format_args!`'s type.
    FormatArguments,
    /// The trait `fmt::Display` (`{}`).
    FmtDisplay,
    /// The trait `fmt::Debug` (`{:?}`).
    FmtDebug,
    /// The trait `fmt::LowerHex` (`{:x}`).
    FmtLowerHex,
    /// The trait `fmt::UpperHex` (`{:X}`).
    FmtUpperHex,
    /// The trait `fmt::Octal` (`{:o}`).
    FmtOctal,
    /// The trait `fmt::Binary` (`{:b}`).
    FmtBinary,
    /// The trait `fmt::LowerExp` (`{:e}`).
    FmtLowerExp,
    /// The trait `fmt::UpperExp` (`{:E}`).
    FmtUpperExp,
    /// The trait `fmt::Pointer` (`{:p}`).
    FmtPointer,
    // Fns the lowering calls.
    PanicBoundsCheck,
    // What a desugaring's path names (blaze-lower's `LangItem`, `BodyPath::lang`), answered by
    // `World::lang_path` as a definition: a fn, a variant's ctor or a struct.
    /// `IntoIterator::into_iter`, by `for`.
    IntoIterIntoIter,
    /// `Iterator::next`, by `for`.
    IteratorNext,
    /// `Option::Some`, by `for`.
    OptionSome,
    /// `Option::None`, by `for`.
    OptionNone,
    /// `Try::branch`, by `?`.
    TryTraitBranch,
    /// `FromResidual::from_residual`, by `?`.
    TryTraitFromResidual,
    /// `ControlFlow::Continue`, by `?`.
    ControlFlowContinue,
    /// `ControlFlow::Break`, by `?`.
    ControlFlowBreak,
    /// The struct `Range`, by `a..b`.
    Range,
    /// The struct `RangeFrom`, by `a..`.
    RangeFrom,
    /// The struct `RangeTo`, by `..b`.
    RangeTo,
    /// The struct `RangeFull`, by `..`.
    RangeFull,
    /// `RangeInclusive::new`, by `a..=b`.
    RangeInclusiveNew,
    /// The struct `RangeToInclusive`, by `..=b`.
    RangeToInclusive,
    /// `IntoFuture::into_future`, by `.await`.
    IntoFutureIntoFuture,
    /// `Future::poll`, by `.await`.
    FuturePoll,
    /// `Pin::new_unchecked`, by `.await`.
    PinNewUnchecked,
    /// `core::future::get_context`, by `.await`.
    GetContext,
    /// `Poll::Ready`, by `.await`.
    PollReady,
    /// `Poll::Pending`, by `.await`.
    PollPending,
    /// `Future::Output` (`future_output`): the solver's builtin projection of an async
    /// block's future. Last, so the earlier items keep their content hashes.
    FutureOutput,
    /// `Try::from_output`, by a `try` block's tail. After the rest, for their hashes.
    TryTraitFromOutput,
    /// The auto trait `Freeze` (`freeze`): no interior mutability (core/src/marker.rs).
    /// Constant promotion asks it (rules/promote.rs). Last, for the earlier items' hashes.
    Freeze,
    // What an async closure's bounds ask (blaze-solve async_fn.rs; the library's
    // core/src/ops/async_function.rs). After the rest, for their hashes.
    /// The trait `AsyncFn` (`async_fn`).
    AsyncFn,
    /// The trait `AsyncFnMut` (`async_fn_mut`).
    AsyncFnMut,
    /// The trait `AsyncFnOnce` (`async_fn_once`).
    AsyncFnOnce,
    /// `AsyncFnOnce::Output` (`async_fn_once_output`).
    AsyncFnOnceOutput,
    /// `AsyncFnOnce::CallOnceFuture` (`call_once_future`).
    CallOnceFuture,
    /// `AsyncFnMut::CallRefFuture` (`call_ref_future`).
    CallRefFuture,
    /// The trait `From` (`from` / `core::convert::From`), for error-union `?` propagation.
    /// Last, so the earlier items keep their content hashes.
    From,
}

impl LangItem {
    /// For a lang item that is an associated type, its trait's lang item and its name, as the
    /// pinned library declares them (`#[lang = "fn_once_output"] type Output;` in `FnOnce`,
    /// `#[lang = "future_output"] type Output;` in `Future`, `#[lang = "discriminant_type"] type
    /// Discriminant` in `DiscriminantKind`, `#[lang = "metadata_type"] type Metadata` in
    /// `Pointee`). The solver names an associated type by `assoc_def(trait, name)`, so these are
    /// answered from the trait, never by the item's own key (typeck.md, "Closures against `Fn`
    /// bounds").
    pub fn assoc_type(self) -> Option<(LangItem, &'static str)> {
        match self {
            LangItem::FnOnceOutput => Some((LangItem::FnOnce, "Output")),
            LangItem::FutureOutput => Some((LangItem::Future, "Output")),
            LangItem::AsyncFnOnceOutput => Some((LangItem::AsyncFnOnce, "Output")),
            LangItem::CallOnceFuture => Some((LangItem::AsyncFnOnce, "CallOnceFuture")),
            LangItem::CallRefFuture => Some((LangItem::AsyncFnMut, "CallRefFuture")),
            LangItem::Discriminant => Some((LangItem::DiscriminantKind, "Discriminant")),
            LangItem::Metadata => Some((LangItem::Pointee, "Metadata")),
            _ => None,
        }
    }
}

impl ContentHash for LangItem {
    fn hash_into(&self, h: &mut StableHasher) {
        h.tag(*self as u8);
    }
}

impl LangItem {
    /// Every lang item in declaration order: `ALL[i] as u8 == i`, the tag its content hash
    /// and the checked-body record ([`crate::checked_record`]) write. The one home of the list
    /// (blaze-ladder's front2/langs.rs `ALL` names this one; tests/front2_bounds.rs checks its
    /// length against `variant_count` and its order, so a new variant cannot be left out).
    pub const ALL: [LangItem; 99] = {
        use LangItem::*;
        [
            Sized, Copy, Deref, DerefMut, Add, Sub, Mul, Div, Rem, BitAnd, BitOr, BitXor, Shl, Shr, AddAssign, SubAssign, MulAssign, DivAssign, RemAssign, BitAndAssign, BitOrAssign,
            BitXorAssign, ShlAssign, ShrAssign, Neg, Not, PartialEq, PartialOrd, Index, IndexMut, FnOnce, FnMut, Fn, MetaSized, PointeeSized, Clone, Send, Sync, Unpin, Drop, Destruct, Tuple,
            Unsize, CoerceUnsized, DispatchFromDyn, FnPtrTrait, FnOnceOutput, Pointee, Metadata, DynMetadata, DiscriminantKind, Discriminant, PhantomData, OwnedBox, CStr, ManuallyDrop, Future,
            IntoFuture, FormatArguments, FmtDisplay, FmtDebug, FmtLowerHex, FmtUpperHex, FmtOctal, FmtBinary, FmtLowerExp, FmtUpperExp, FmtPointer, PanicBoundsCheck, IntoIterIntoIter,
            IteratorNext, OptionSome, OptionNone, TryTraitBranch, TryTraitFromResidual, ControlFlowContinue, ControlFlowBreak, Range, RangeFrom, RangeTo, RangeFull, RangeInclusiveNew,
            RangeToInclusive, IntoFutureIntoFuture, FuturePoll, PinNewUnchecked, GetContext, PollReady, PollPending, FutureOutput, TryTraitFromOutput,
            Freeze,
            AsyncFn, AsyncFnMut, AsyncFnOnce, AsyncFnOnceOutput, CallOnceFuture, CallRefFuture,
            From,
        ]
    };

    /// The lang item whose tag (`as u8`) is `tag`, or `None` for a byte no item has.
    pub fn of_tag(tag: u8) -> Option<LangItem> {
        let item = *LangItem::ALL.get(usize::from(tag))?;
        (item as u8 == tag).then_some(item)
    }
}
