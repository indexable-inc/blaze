//! Diagnostics as data (typeck.md, "Diagnostics are values"): a user's type error is a value in
//! the record with an item-relative range (rule 71), never a panic and never a stop. Codes are
//! rustc's, so a person's search finds the same page.

use crate::ids::{ExprId, ItemKey, LangItem, List, Name, PatId, Range};
use crate::portable::PTy;
use crate::ty::{Ty, TyList};

mod about;
pub(crate) use about::About;
mod node;
pub use node::{Node, expr_form, node_form, pat_form, range_shared, sketch};

/// The mark of a finding built by a constructor of src/diag/about.rs, each of which takes what
/// the finding is about (rule 76). Private to `diag`, so no struct literal of [`Diag`] or
/// [`PendingDiag`] compiles anywhere else: a finding with a code and nothing in it cannot be
/// written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Said(());

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
    /// E0308, a compiler intrinsic (`#[rustc_intrinsic]`, e.g. `transmute`) where a fn pointer
    /// is wanted: it has no address, so it is never reified (rules/reify.rs; core/src/intrinsics/
    /// mod.rs: "The actual implementations live in the compiler."). `found` is its fn item type.
    IntrinsicToFnPointer,
    /// E0308, a safe `#[target_feature]` fn where a SAFE fn pointer is wanted, in a body that
    /// enables none of its features (rules/target_feature.rs; the Reference, "The
    /// `target_feature` attribute"): only an `unsafe fn` pointer takes it. `found` is its fn item
    /// type, `expected` the pointer.
    TargetFeatureToSafeFnPointer,
    /// A safe `#[target_feature]` fn as a fn pointer in a body with `#[target_feature]` of its
    /// own: allowed when the body enables every feature of the fn, which the world does not
    /// answer yet (only the attribute): refused by name, never guessed. `found` is the fn item.
    TargetFeatureContextUnread,
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
    /// E0412: cannot find type. Filed by the front end for a written type whose path names
    /// nothing (the checker sees it as the error type): blaze-ladder front2 body.rs `TypeMiss`.
    UnresolvedType,
    /// E0405: cannot find trait. Filed by the front end for a bound of a fn's own signature
    /// whose trait path names nothing (the bound is dropped, the head made without it):
    /// blaze-ladder front2 type_miss.rs `HeadMiss`.
    UnresolvedTrait,
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
    /// E0624: the only methods of the name that apply are inherent ones the body may not name
    /// (the Reference, "Visibility and privacy"; rules/method.rs).
    PrivateMethod,
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
    /// E0617: a variable argument C would promote (`f32`, `i8`, `i16`, `bool`, `u8`, `u16`) or
    /// a fn item, passed to `...` uncast (rules/c_variadic.rs).
    VariadicArg,
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
    /// E0596: a mutable place through an overloaded index of a type that implements `Index`
    /// but not `IndexMut` at that index type (rules/mut_places.rs); `found` is the indexed
    /// type. rustc's borrow check says "cannot borrow data in an index of `T` as mutable".
    IndexNotMut,
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
    /// E0220: `T::Assoc` where no trait of `T`'s bounds declares an associated type `Assoc`
    /// (`name`); `found` is the prefix's type.
    NoAssocTy,
    /// E0221: `T::Assoc` where two traits of `T`'s bounds declare an associated type `Assoc`.
    AmbiguousAssocTyInBounds,
    /// E0223: `P::Assoc` where `P` is no parameter and no `Self` of a trait or of a trait's
    /// impl, so no bound says which trait's `Assoc` is meant (`<P as Trait>::Assoc` does).
    AmbiguousAssocTy,
    /// An `asm!` operand whose type its kind or class does not take (the Reference,
    /// inline-assembly, "Register operands", "Operand type"): `found` the type, `name` the
    /// class (`reg`, `vreg` ..), or `sym`/`const`; for a split `inout`, `expected` is the
    /// input's type and `found` the output's. rustc gives these no code.
    AsmOperandType,
    /// An `asm!` register that is no class or register of the target, a reserved one, or one
    /// explicit register twice among the inputs or the outputs; `name` the register.
    AsmRegister,
    /// E0133: an operation only `unsafe` code may do, outside any `unsafe` block or `unsafe fn`
    /// (the Reference, "Unsafety"); the operation is [`Because::Unsafe`] (rules/unsafety.rs).
    RequiresUnsafe,
    /// E0784: a union's struct pattern that names other than exactly one field (rules/pat.rs).
    UnionPatternFields,
    /// E0271: an associated type resolves to a different type than a required equality.
    ProjectionMismatch,
    /// E0004: non-exhaustive patterns in a structural union `match`; `expected` names the first
    /// uncovered member type.
    NonExhaustiveMatch,
    /// Redundant / unreachable arm in a structural union `match` whose variant was already
    /// covered by an earlier unguarded arm; `found` names the already-covered member type.
    RedundantPattern,
    /// E0391: recursive cycle while inferring a return-type `_` placeholder (e.g. `Result<T, _>`
    /// or `-> _`); add an explicit return/error type to break the cycle.
    InferredErrorSetCycle,
}

impl DiagCode {
    /// Whether a finding of this kind may be about the whole item, with no place in its body: a
    /// lang item the world does not have, and a `dyn` written in the item's signature. Every
    /// other finding points at the expression or pattern it is about; one that does not is
    /// [`crate::error::CheckError::DiagWithoutPlace`].
    pub const fn covers_the_item(self) -> bool {
        matches!(self, DiagCode::MissingLangItem | DiagCode::DynIncompatible | DiagCode::InferredErrorSetCycle)
    }

    /// rustc's error code, for printing.
    pub const fn code(self) -> &'static str {
        match self {
            DiagCode::Mismatch
            | DiagCode::IncompatibleArms
            | DiagCode::PatternNotBase
            | DiagCode::IntrinsicToFnPointer
            | DiagCode::TargetFeatureToSafeFnPointer => "E0308",
            DiagCode::TargetFeatureContextUnread => "E0000",
            DiagCode::IfWithoutElse => "E0317",
            DiagCode::AnnotationsNeeded => "E0282",
            DiagCode::CannotSatisfy => "E0283",
            DiagCode::ProjectionMismatch => "E0271",
            DiagCode::NonExhaustiveMatch => "E0004",
            DiagCode::RedundantPattern => "E0000",
            DiagCode::InferredErrorSetCycle => "E0391",
            DiagCode::Unsatisfied => "E0277",
            DiagCode::UnresolvedPath => "E0425",
            DiagCode::UnresolvedType => "E0412",
            DiagCode::UnresolvedTrait => "E0405",
            DiagCode::NotAValue => "E0423",
            DiagCode::NotAStruct => "E0574",
            DiagCode::NoMethod | DiagCode::NotAMethod => "E0599",
            DiagCode::AmbiguousMethod => "E0034",
            DiagCode::PrivateMethod => "E0624",
            DiagCode::NoField => "E0609",
            DiagCode::PrimitiveField => "E0610",
            DiagCode::NoSuchFieldInit => "E0560",
            DiagCode::MissingFields => "E0063",
            DiagCode::ArgCount => "E0061",
            DiagCode::VariadicArg => "E0617",
            DiagCode::GenericArgCount => "E0107",
            DiagCode::NotCallable => "E0618",
            DiagCode::CallArgsNotTuple => "E0059",
            DiagCode::CantDeref => "E0614",
            DiagCode::CantIndex => "E0608",
            DiagCode::IndexNotMut => "E0596",
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
            DiagCode::NoAssocTy => "E0220",
            DiagCode::AmbiguousAssocTyInBounds => "E0221",
            DiagCode::AmbiguousAssocTy => "E0223",
            DiagCode::AsmOperandType | DiagCode::AsmRegister => "E0000",
            DiagCode::RequiresUnsafe => "E0133",
            DiagCode::UnionPatternFields => "E0784",
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
    /// A rule of the Rust Reference refuses the construct outright, no trait asked: `rule` is
    /// its identifier (`expr.negate.results`) and `says` its words, quoted.
    Reference { rule: &'static str, says: &'static str },
    /// E0425: the written segment of the path that named nothing, why, and what the segments
    /// before it named (the world's [`crate::PathMiss`]); the diagnostic's `name` is that
    /// segment's, not the path's last one.
    Path(crate::path_miss::PathMiss),
    /// How many were wanted and how many written, where no item owns the count: a call's
    /// arguments (E0061, E0057), a tuple-struct pattern's subpatterns (E0023).
    Count { expected: u32, got: u32 },
    /// The finding is about the path written at its range, and the body keeps no segment of it
    /// to name (src/diag/about.rs, `About::Path`): the range is the thing.
    NoSegment,
    /// E0133: the operation that needs `unsafe` (rules/unsafety.rs).
    Unsafe(UnsafeOp),
}

/// An operation only `unsafe` code may do (the Reference, "Unsafety": "the following language
/// level features cannot be used in the safe subset of Rust"), as an E0133 finding names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnsafeOp {
    /// A call of an `unsafe` fn: the item where the callee is one (a fn, a method, an
    /// intrinsic, a foreign fn not written `safe`), `None` through an `unsafe fn` pointer.
    Call(Option<ItemKey>),
    /// A call of a safe `#[target_feature]` fn from a body that enables none of its features.
    TargetFeatureCall(ItemKey),
    /// `*p` of a raw pointer `p`, not directly under `&raw`.
    RawDeref,
    /// A read or write of a `static mut`, not directly under `&raw`.
    MutStatic(ItemKey),
    /// A use of a static of an `extern` block not written `safe`, not directly under `&raw`.
    ExternStatic(ItemKey),
    /// A read of a union's field (assigning to one is safe).
    UnionField,
    /// `asm!`.
    InlineAsm,
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

/// Where an inference variable was made (rule 19: an E0282 names WHAT is undecided, not only
/// where the walk needed it). Kept per variable when it is made (machine.rs `var_origins`,
/// [`crate::machine::Machine::fresh_from`]); a variable no rule named is [`VarOrigin::Other`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VarOrigin {
    /// Generic parameter `index` (the item's own and its parents', parents first) of `item`, at
    /// a use that wrote no argument for it (a path, a method call).
    GenericArg { item: ItemKey, index: u32 },
    /// Parameter `index` of closure `closure`: no type written, none expected.
    ClosureParam { closure: ExprId, index: u32 },
    /// The return type of closure `closure`: none written, none expected.
    ClosureReturn { closure: ExprId },
    /// What the projection `<_ as trait_<..>>::name` normalizes to, not decided yet: its
    /// arguments still hold a variable (machine.rs `projection_for_use`, an ambiguous answer).
    Projection { trait_: ItemKey, name: Name },
    /// A variable a rule made for a value of its own, not named yet.
    Other,
}

/// How many of the goals pending on an undecided variable a finding keeps ([`UndecidedVar`]).
pub const MOST_PENDING_SHOWN: usize = 2;

/// The variable an E0282 is about (the type the walk needed and found undecided): where it
/// was made, and the goals still pending that name it, the first [`MOST_PENDING_SHOWN`] of
/// them (trait and projection goals, in the order they were registered) and how many in all.
/// What a person reads to fix it: which argument to annotate, and which bound did not decide
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UndecidedVar {
    pub origin: VarOrigin,
    pub pending: [Option<UnmetGoal>; MOST_PENDING_SHOWN],
    /// Every pending goal naming the variable, those not kept included.
    pub pending_count: u32,
}

/// [`UndecidedVar`] before writeback: the goals in terms of the inference table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingUndecided {
    pub origin: VarOrigin,
    pub pending: [Option<PendingGoal>; MOST_PENDING_SHOWN],
    pub pending_count: u32,
}

/// Why an unmet trait goal has no solution, as the solver walked it (blaze-solve why_no.rs,
/// `Solver::why_no`): the steps from the finding's goal down to the first goal under it that
/// fails by itself, and why that one fails. The Reference is the spec of each step ("Auto
/// traits": an ADT, tuple, closure or async block's future is `Send` when every field,
/// element, capture or held value is; "Implementations": an impl holds where its where-clauses
/// hold). Typed data, no words: the front end prints it (front2 check.rs `diag_about`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Why {
    /// The steps' first row in the record's `why` column ([`crate::Checked::why_of`]), the
    /// finding's own goal first ([`WhyReached::Root`]), each next one a goal of the one before.
    pub start: u32,
    /// How many steps (at least one: the root).
    pub len: u32,
    /// Why the last step's goal fails by itself.
    pub leaf: WhyLeaf,
}

/// One step of a [`Why`]: the goal, and what it is of the step before.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WhyStep {
    /// The step's goal, `Self` first, in the record's type table: `None` where the step is no
    /// positive trait goal (a projection, a well-formedness goal), or its goal names a type the
    /// solver's walk left open (its variables die with the walk; never printed as a guess).
    pub goal: Option<UnmetGoal>,
    pub reached: WhyReached,
}

/// What a step's goal is of the step before it (blaze-solve why_no.rs `Reached`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WhyReached {
    /// The finding's own goal.
    Root,
    /// Where-clause `clause` (in the impl's order) of `impl_`, the impl that applied.
    ImplClause { impl_: ItemKey, clause: u32 },
    /// Field `field` of the ADT `adt` (an auto trait's constituent; every variant's fields in
    /// declaration order).
    Field { adt: ItemKey, field: u32 },
    /// Type argument `arg` of the ADT `adt` (its fields hold it).
    AdtArg { adt: ItemKey, arg: u32 },
    /// The target of a reference.
    RefTarget,
    /// The pointee of a raw pointer.
    PtrTarget,
    /// The element of an array, slice or pattern type, or `[u8]` of `str`.
    Element,
    /// Element `index` of a tuple.
    TupleElement { index: u32 },
    /// Capture `capture` of `closure`, in the closure's capture order.
    Capture { closure: WhyDef, capture: u32 },
    /// Slot `slot` of the interior of the async block's future `coroutine`: its captures, then
    /// what it holds across an `.await`, in the interior's order. The check's interior is a
    /// superset (rules/coroutine.rs `analyze_futures`) and keeps no value or `.await` per slot;
    /// the exact one's are the flow's (blaze-flow-lower `interior`).
    CoroutineSlot { coroutine: WhyDef, slot: u32 },
    /// The captures of `closure` as one tuple (the next step names the capture).
    Captures { closure: WhyDef },
    /// The interior of `coroutine` as one tuple (the next step names the slot).
    Interior { coroutine: WhyDef },
    /// `T` of `PhantomData<T>`.
    Phantom,
    /// The hidden type of the opaque type `opaque` (an auto trait "leaks" through it).
    Hidden { opaque: ItemKey },
    /// The `nth` goal a candidate added for its own reason (a where-clause of the environment,
    /// an alias's bound, a builtin's requirement); `u32::MAX` for one a nested goal handed back.
    Other { nth: u32 },
}

/// A closure or async block a [`WhyReached`] step names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WhyDef {
    /// This body's closure or async block, by its expression.
    Expr(ExprId),
    /// The `ordinal`-th closure or async block of another body, `owner`'s
    /// (src/coroutine_id.rs).
    Of { owner: ItemKey, ordinal: u32 },
}

/// Why the last step of a [`Why`] fails by itself (blaze-solve why_no.rs `Leaf`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WhyLeaf {
    /// No impl names the type, no where-clause or bound says it, no builtin rule applies.
    NoImpl,
    /// The negative impl `impl_` names the type (`impl !Send for Rc<T>`).
    NegativeImpl { impl_: ItemKey },
    /// Every candidate was refused by its rule (`count` of them): `first_impl` is the first
    /// one where it is an impl (an impl for other types), `None` where a builtin or a bound.
    Refused { first_impl: Option<ItemKey>, count: u32 },
    /// A builtin rule refused the goal and no other candidate came (a coroutine is never
    /// `Unpin`).
    BuiltinNo,
    /// The walk met a goal already on its path: a cycle the search answered No.
    Cycle,
    /// The solver kept no more rows, or the chain is deeper than it walks: the chain stops
    /// here, said (blaze-solve `MAX_WHY_ROWS`, `MAX_WHY_DEPTH`).
    NotKept,
    // An undecided goal's leaves (blaze-solve why_maybe.rs `Leaf`): why the last step's goal
    // cannot be decided yet by itself.
    /// Term `term` of the last step's goal (a trait goal's arguments, `Self` first; a
    /// projection's alias, then its term) is a type not known yet.
    Unresolved { term: u32 },
    /// `count` candidates may each hold and none can be chosen (rustc's E0283), the first two
    /// in the solver's order.
    Ambiguous { first: WhyCand, second: WhyCand, count: u32 },
    /// The evaluation met a limit (rustc's E0275).
    Limit { hit: WhyLimit },
    /// The goal looks into the hidden type of the opaque type `opaque`, not defined yet.
    HiddenUnknown { opaque: ItemKey },
    /// The goal looks into the interior of `coroutine`, not decided yet.
    InteriorUnknown { coroutine: WhyDef },
    /// The goal looks into the captures of `closure`, not decided yet.
    CapturesUnknown { closure: WhyDef },
    /// The projection of the associated item `alias` does not normalize yet.
    Unnormalized { alias: ItemKey },
    /// The rule of `cand` cannot decide the goal yet, with no unknown type in it (an
    /// unevaluated constant, a closure kind not known yet).
    RuleUndecided { cand: WhyCand },
}

/// A candidate a [`WhyLeaf`] names (blaze-solve `Choice`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WhyCand {
    /// The impl `impl_`.
    Impl { impl_: ItemKey },
    /// A where-clause in scope (the solver's index `nth` among the environment's).
    WhereClause { nth: u32 },
    /// A bound of the self type's alias or opaque type.
    AliasBound,
    /// A rule of the language (a builtin impl, a trait object's own bounds).
    Builtin,
    /// Coherence: an impl a downstream or future crate may write.
    Unknowable,
}

/// The limit a [`WhyLeaf::Limit`] met (blaze-solve why_maybe.rs `LimitHit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WhyLimit {
    /// The goal stood `depth` deep, at the recursion limit `limit`.
    Depth { depth: u32, limit: u32 },
    /// A cycle's answer did not settle in `reruns` reruns.
    Fixpoint { reruns: u32 },
    /// A candidate's nested goals kept changing for `rounds` rounds.
    Rounds { rounds: u32 },
}

/// [`WhyStep`] before writeback, as [`crate::TypeCtx::why_chain`] gives it: the goal
/// (`trait_` and every argument, `Self` first) in terms of the inference table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingWhyStep {
    pub goal: Option<(ItemKey, TyList)>,
    pub reached: WhyReached,
}

/// A [`Why`] before writeback: its steps a range of the machine's `why_steps`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingWhy {
    pub start: u32,
    pub len: u32,
    pub leaf: WhyLeaf,
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
    /// Written text that is no expression or pattern (a turbofish argument, a segment, an
    /// anchor's type, a method's name), by its range (rules/obligation_site.rs).
    Written(crate::ids::Range),
}

impl At {
    /// Where an obligation or coercion with `cause` points.
    pub(crate) const fn of(cause: crate::ty::Cause) -> At {
        match cause {
            crate::ty::Cause::Expr(e) => At::Expr(e),
            crate::ty::Cause::Pat(p) => At::Pat(p),
            crate::ty::Cause::Body => At::Body,
            // The whole use; `Machine::clause_at` narrows it when the clause is reported unmet.
            crate::ty::Cause::Clause { expr, .. } => At::Expr(expr),
            crate::ty::Cause::Written(r) => At::Written(r),
        }
    }

    /// The cause of an obligation made for the place `self`.
    pub(crate) const fn cause(self) -> crate::ty::Cause {
        match self {
            At::Expr(e) => crate::ty::Cause::Expr(e),
            At::Pat(p) => crate::ty::Cause::Pat(p),
            At::Body => crate::ty::Cause::Body,
            At::Written(r) => crate::ty::Cause::Written(r),
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
    /// Why the unmet trait goal has no solution, the solver's chain (set only with `unmet`).
    pub why: Option<PendingWhy>,
    /// For an E0282 about a variable: where it was made and the goals pending on it
    /// ([`PendingDiag::with_undecided`]).
    pub undecided: Option<PendingUndecided>,
    /// Built only by a constructor of src/diag/about.rs.
    said: Said,
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
    /// The name the error is about (a method, a field, the path segment that failed: see
    /// [`Because::Path`]).
    pub name: Option<Name>,
    /// For an unmet bound (E0277): the impl's `where const` condition that is the reason, with
    /// `found` the bound's self type.
    pub because: Option<Because>,
    /// The node of the body the finding was made at (src/diag/node.rs): what names it when its
    /// range does not (in an expansion every node has the call's range; rule 19). `None` for a
    /// finding about the whole body or about written text that is no node.
    pub node: Option<Node>,
    /// For an unmet trait goal (E0277, `because` is [`Because::Unmet`]): why it has no
    /// solution, the solver's chain from the goal to the goal that fails by itself, its steps
    /// in the record's `why` column. `None` where the check asked none (a stalled goal's
    /// finding: the chain on the superset interior is not the exact one's).
    pub why: Option<Why>,
    /// For an E0282 (`AnnotationsNeeded`) about an inference variable: where the variable was
    /// made and the goals pending on it (rule 19). `None` for every other finding, and for an
    /// E0282 whose type held no variable the walk could name.
    pub undecided: Option<UndecidedVar>,
    /// Built only by [`PendingDiag::export`] or [`Diag::dyn_incompatible`].
    said: Said,
}
