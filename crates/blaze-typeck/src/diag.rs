//! Diagnostics as data (typeck.md, "Diagnostics are values"): a user's type error is a value in
//! the record with an item-relative range (rule 71), never a panic and never a stop. Codes are
//! rustc's, so a person's search finds the same page.

use crate::ids::{ExprId, ItemKey, LangItem, List, Name, PatId, Range};
use crate::portable::PTy;
use crate::ty::{Ty, TyList};

/// What went wrong, by rustc's code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagCode {
    /// E0308: mismatched types.
    Mismatch,
    /// E0308, `if` and `else` (or match arms, or breaks) have incompatible types.
    IncompatibleArms,
    /// E0308, a pattern type is not its base type (typeck.md, "Pattern types"): `T is PAT`
    /// where `T` is expected or the reverse; `expected` and `found` name both. Only
    /// `transmute` goes between them.
    PatternNotBase,
    /// E0317: `if` without `else` whose type is not `()`.
    IfWithoutElse,
    /// E0282: type annotations needed.
    AnnotationsNeeded,
    /// E0283: type annotations needed, a trait goal other than `Sized` still ambiguous at the
    /// end ("cannot satisfy `A: Conv<_>`"; rustc's `maybe_report_ambiguity`).
    CannotSatisfy,
    /// E0277: a bound is not satisfied.
    Unsatisfied,
    /// E0425: cannot find value.
    UnresolvedPath,
    /// E0423 / E0533: expected value, found a type, trait or struct name.
    NotAValue,
    /// E0574: expected struct, found something else.
    NotAStruct,
    /// E0599: no method named so.
    NoMethod,
    /// E0599, the name is an associated function, not a method.
    NotAMethod,
    /// E0034: multiple applicable items in scope.
    AmbiguousMethod,
    /// E0609: no field on type.
    NoField,
    /// E0610: a primitive type has no fields.
    PrimitiveField,
    /// E0560: a struct has no field so named.
    NoSuchFieldInit,
    /// E0063: missing fields in a struct expression.
    MissingFields,
    /// E0061: wrong number of arguments.
    ArgCount,
    /// E0107: wrong number of generic arguments.
    GenericArgCount,
    /// E0618: expected function, found something else.
    NotCallable,
    /// E0059: a call through `Fn*` whose `Args` is no tuple ("cannot use call notation; the
    /// first type parameter for the function trait is neither a tuple nor unit").
    CallArgsNotTuple,
    /// E0614: type cannot be dereferenced.
    CantDeref,
    /// E0608: cannot index into a value of this type.
    CantIndex,
    /// E0369: binary operation cannot be applied.
    BinOp,
    /// E0600: unary operation cannot be applied.
    UnOp,
    /// E0606: casting is invalid.
    InvalidCast,
    /// E0605: non-primitive cast.
    NonPrimitiveCast,
    /// E0604: only `u8` can be cast as `char`.
    CharCast,
    /// E0055: the autoderef recursion limit.
    RecursionLimit,
    /// E0023: wrong number of fields in a tuple-struct pattern.
    PatternArity,
    /// E0730: a slice pattern with `..` against an array whose length is not a known value
    /// (a const parameter): the rest's length `N - k` is no term of the type system.
    ArrayLenUnknown,
    /// E0747: a generic argument of the wrong kind: a constant where a type parameter is
    /// declared, or a type where a const parameter is.
    GenericArgKind,
    /// E0026: a struct pattern names a field the struct lacks.
    NoSuchFieldPat,
    /// E0027: a struct pattern without `..` misses fields.
    MissingFieldPats,
    /// Edition 2024: a binding modifier (`mut`, `ref`) or `&` pattern where the default binding
    /// mode is not `move`.
    BindingModifier2024,
    /// E0070: invalid left-hand side of an assignment.
    InvalidLhs,
    /// E0067: invalid left-hand side of a compound assignment (`+=`): the Reference's
    /// "Compound assignment expressions": "the assigned place operand must be a place
    /// expression"; a tuple there does not destructure.
    InvalidCompoundLhs,
    /// E0728: `.await` outside an async fn or block.
    AwaitOutsideAsync,
    /// A form needs an item the crate graph does not declare (`IntoFuture` for `.await`,
    /// `fmt::Display` for `{}`, `ControlFlow::Continue` for `?`): rustc's "requires `X`
    /// lang_item", which has no code. `because` is `Because::NoLangItem` naming the item.
    MissingLangItem,
    /// `_` as an expression anywhere but the left side of an assignment (rustc's AST lowering
    /// error, which has no code).
    UnderscoreExpr,
    /// E0745: `&raw const` or `&raw mut` of a value that is not a place.
    RawBorrowOfTemporary,
    /// E0029: a range pattern's end is not a number or a `char`.
    RangePatType,
    /// E0576: `<T as Tr>::x` names no associated item `x` of `Tr`.
    NoAssocItem,
    /// E0404: the trait of `<T as Tr>::x` is not a trait.
    NotATrait,
    /// E0038: a trait that is not dyn compatible used in `dyn`; `name` is the member and
    /// `because` is `Because::Dyn` with the rule.
    DynIncompatible,
    /// E0057: a call through `Fn*` with the wrong number of arguments for its `Args` tuple.
    FnTraitArgCount,
}

impl DiagCode {
    /// Whether a finding of this kind may be about the whole item, with no place in its body: a
    /// lang item the world does not have, and a `dyn` written in the item's signature. Every
    /// other finding points at the expression or pattern it is about; one that does not is
    /// [`crate::error::CheckError::DiagWithoutPlace`].
    pub const fn covers_the_item(self) -> bool {
        matches!(self, DiagCode::MissingLangItem | DiagCode::DynIncompatible)
    }

    /// rustc's error code, for printing.
    pub const fn code(self) -> &'static str {
        match self {
            DiagCode::Mismatch | DiagCode::IncompatibleArms | DiagCode::PatternNotBase => "E0308",
            DiagCode::IfWithoutElse => "E0317",
            DiagCode::AnnotationsNeeded => "E0282",
            DiagCode::CannotSatisfy => "E0283",
            DiagCode::Unsatisfied => "E0277",
            DiagCode::UnresolvedPath => "E0425",
            DiagCode::NotAValue => "E0423",
            DiagCode::NotAStruct => "E0574",
            DiagCode::NoMethod | DiagCode::NotAMethod => "E0599",
            DiagCode::AmbiguousMethod => "E0034",
            DiagCode::NoField => "E0609",
            DiagCode::PrimitiveField => "E0610",
            DiagCode::NoSuchFieldInit => "E0560",
            DiagCode::MissingFields => "E0063",
            DiagCode::ArgCount => "E0061",
            DiagCode::GenericArgCount => "E0107",
            DiagCode::NotCallable => "E0618",
            DiagCode::CallArgsNotTuple => "E0059",
            DiagCode::CantDeref => "E0614",
            DiagCode::CantIndex => "E0608",
            DiagCode::BinOp => "E0369",
            DiagCode::UnOp => "E0600",
            DiagCode::InvalidCast => "E0606",
            DiagCode::NonPrimitiveCast => "E0605",
            DiagCode::CharCast => "E0604",
            DiagCode::RecursionLimit => "E0055",
            DiagCode::PatternArity => "E0023",
            DiagCode::ArrayLenUnknown => "E0730",
            DiagCode::GenericArgKind => "E0747",
            DiagCode::NoSuchFieldPat => "E0026",
            DiagCode::MissingFieldPats => "E0027",
            DiagCode::BindingModifier2024 => "E0000",
            DiagCode::InvalidLhs => "E0070",
            DiagCode::InvalidCompoundLhs => "E0067",
            DiagCode::AwaitOutsideAsync => "E0728",
            DiagCode::MissingLangItem | DiagCode::UnderscoreExpr => "E0000",
            DiagCode::RawBorrowOfTemporary => "E0745",
            DiagCode::RangePatType => "E0029",
            DiagCode::NoAssocItem => "E0576",
            DiagCode::NotATrait => "E0404",
            DiagCode::DynIncompatible => "E0038",
            DiagCode::FnTraitArgCount => "E0057",
        }
    }
}

/// Why a bound failed, when a `where const` condition of the impl that would meet it is the
/// reason: E0277 with "this impl requires `COND`, which is false for T = ..".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Because {
    /// The condition (its anonymous constant) evaluated to `false` at the bound's types.
    False(ItemKey),
    /// Evaluating the condition failed (a panic, an evaluator error, a fact it cannot read yet):
    /// never taken as `false` or `true`.
    Error(ItemKey),
    /// The condition is over a generic parameter here, so it is not decided.
    Generic(ItemKey),
    /// E0038: `trait_` in `dyn` is not dyn compatible: `by` (it or a supertrait) breaks `rule`
    /// at `member` (`None`: `Sized` is a supertrait).
    Dyn { trait_: ItemKey, by: ItemKey, member: Option<Name>, rule: crate::dyn_compat::DynRule },
    /// `DiagCode::MissingLangItem`: "the lang item `item` is not known to this world". The one
    /// finding for every lang item the checker asks and the world cannot answer (a desugaring's
    /// path, an operator's trait, `Fn*`, `Unsize`, `Deref`, ..): never an unnamed E0425, never
    /// a quiet skip that surfaces later as another error.
    NoLangItem(LangItem),
    /// E0277 with no `where const` reason: the goal that failed, entire, and the leaf
    /// where-clause that failed under it where the checker can name one. blaze's choice: the
    /// leaf is what a person can fix (`Result<u8, Error>: FromResidual<Result<Infallible,
    /// Other>>` fails because `Error: From<Other>` does: the finding says both, the leaf first
    /// to act on). `found` stays the outer goal's self type.
    Unmet { goal: UnmetGoal, leaf: Option<UnmetGoal> },
    /// E0107: `item` takes `expected` generic arguments where `got` were written. For a fn,
    /// `expected` counts its declared parameters only: an `impl Trait` parameter is anonymous
    /// and never given explicitly (the Reference, "Impl trait").
    ArgCount { item: ItemKey, expected: u32, got: u32 },
}

/// A goal as a finding keeps it: every argument, `Self` first, a list of the record's type
/// table (the record's `types`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnmetGoal {
    /// `args[0]: trait_<args[1..]>`.
    Trait { trait_: ItemKey, args: List },
    /// `<args[0] as trait_<args[1..]>>::name == term`.
    Projection { trait_: ItemKey, args: List, name: Name, term: PTy },
}

/// [`UnmetGoal`] before writeback: terms of the inference table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingGoal {
    Trait { trait_: ItemKey, args: TyList },
    Projection { trait_: ItemKey, args: TyList, name: Name, term: Ty },
}

/// Where a diagnostic points, before ranges are looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum At {
    Expr(ExprId),
    Pat(PatId),
    Body,
}

impl At {
    /// Where an obligation or coercion with `cause` points.
    pub(crate) const fn of(cause: crate::ty::Cause) -> At {
        match cause {
            crate::ty::Cause::Expr(e) => At::Expr(e),
            crate::ty::Cause::Pat(p) => At::Pat(p),
            crate::ty::Cause::Body => At::Body,
        }
    }

    /// The cause of an obligation made for the place `self`.
    pub(crate) const fn cause(self) -> crate::ty::Cause {
        match self {
            At::Expr(e) => crate::ty::Cause::Expr(e),
            At::Pat(p) => crate::ty::Cause::Pat(p),
            At::Body => crate::ty::Cause::Body,
        }
    }
}

/// A diagnostic while the walk runs: types as handles, exported at writeback.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingDiag {
    pub code: DiagCode,
    pub at: At,
    pub expected: Option<Ty>,
    pub found: Option<Ty>,
    pub name: Option<Name>,
    pub because: Option<Because>,
    /// An unmet goal and its leaf, exported at writeback as `Because::Unmet` (set only with
    /// `because: None`).
    pub unmet: Option<(PendingGoal, Option<PendingGoal>)>,
}

/// A diagnostic of the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Diag {
    pub code: DiagCode,
    /// Item-relative byte range.
    pub at: Range,
    /// The expected type, in the record's type table.
    pub expected: Option<PTy>,
    /// The type found.
    pub found: Option<PTy>,
    /// The name the error is about (a method, a field, a path's last segment).
    pub name: Option<Name>,
    /// For an unmet bound (E0277): the impl's `where const` condition that is the reason, with
    /// `found` the bound's self type.
    pub because: Option<Because>,
}
