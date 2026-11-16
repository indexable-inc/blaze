//! The solver's vocabulary: terms (types, regions and consts in one arena of 4-byte handles),
//! lists of them, definitions by key, predicates, certainty, and the simplified self shape impls
//! are bucketed by (solve.md, "The seams" and "Goals").
//!
//! `Ty`, `List`, `TyKind`, `Flags` and `Markers` are blaze-ty's; they are defined here until
//! that crate lands, then move there (one concept, one home: rule 30). The same `TyKind` is the
//! node of a portable answer (`facts::Portable`), where its handles index the answer's own
//! tables instead of the arena.

/// A term: a type, a region or a const, as a handle into the `Tys` arena (or, in a portable
/// answer, an index into its nodes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ty(pub u32);

impl Ty {
    /// No term: the sentinel of an optional slot in a column.
    pub const NONE: Ty = Ty(u32::MAX);
}

/// A list of terms: a range of the arena's list pool (or of a portable answer's `lists`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct List {
    pub start: u32,
    pub len: u32,
}

impl List {
    pub const EMPTY: List = List { start: 0, len: 0 };
}

/// A definition: the first 16 bytes of its `blaze_id::ItemKey` (blaze-typeck's `ItemKey` form):
/// portable, so canonical keys built from it are the same on every machine (rule 25).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Def(pub u128);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UintTy {
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FloatTy {
    F16,
    F32,
    F64,
    F128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mutbl {
    Not,
    Mut,
}

/// What an inference variable may become.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VarKind {
    /// Any type.
    Ty,
    /// An integer type (an unsuffixed integer literal).
    Int,
    /// A float type.
    Float,
    /// A region.
    Region,
    /// A const.
    Const,
}

/// An inference variable. `index` is unique across kinds in one arena, so one wake list per
/// index serves every kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Var {
    pub kind: VarKind,
    pub index: u32,
}

/// What an alias names (`rustc_type_ir::AliasTyKind`, and unevaluated consts).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AliasKind {
    /// `<args[0] as Trait<args[1..n]>>::A<args[n..]>`; the def is the trait's associated item.
    Projection,
    /// An inherent associated type; the def is the impl's item.
    Inherent,
    /// `impl Trait`; the def is the opaque type.
    Opaque,
    /// A free (lazy) type alias.
    Free,
    /// An unevaluated const: an associated or anonymous const; the def is the const.
    Const,
}

/// Whether an alias is rigid (`rustc_type_ir::IsRigid`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rigid {
    No,
    Yes,
}

/// A closure's kind as the term in its args (`ConstValue(0)` is `Fn`, 1 `FnMut`, 2 `FnOnce`),
/// or an inference variable while it is not known.
pub const CLOSURE_FN: u64 = 0;
pub const CLOSURE_FN_MUT: u64 = 1;
pub const CLOSURE_FN_ONCE: u64 = 2;

/// One level of a term. Children are handles; a closure's args end with its kind (a const),
/// its signature (a `FnPtr`) and its upvars (a `Tuple`), after the parent's generics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TyKind {
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
    Never,
    Tuple(List),
    /// Region, pointee.
    Ref(Ty, Ty, Mutbl),
    RawPtr(Ty, Mutbl),
    /// Element, length (a const).
    Array(Ty, Ty),
    Slice(Ty),
    Adt(Def, List),
    Foreign(Def),
    FnDef(Def, List),
    /// Inputs, then the output last.
    FnPtr(List),
    Closure(Def, List),
    /// Existential predicates (`ExTrait` principal first, then `ExProj`s, then `ExAuto`s,
    /// each sorted), then the region bound.
    Dynamic(List, Ty),
    /// `Alias(IsRigid, AliasTy)` of the reference: a rigid alias (one the solver failed to
    /// normalize further, marked by normalization) relates by structure like a type
    /// constructor; relating a non-rigid one emits a `Projection` goal.
    Alias(Rigid, AliasKind, Def, List),
    /// A generic parameter of the item in scope, by index (`Self` is 0 in a trait).
    Param(u32),
    /// A universally bound term: universe, index.
    Placeholder(u32, u32),
    /// A term bound by a binder: De Bruijn index, var.
    Bound(u32, u32),
    /// A variable of a canonical form: its index among the form's variables.
    Canonical(u32),
    Infer(Var),
    Error,
    // Regions.
    ReStatic,
    ReParam(u32),
    ReErased,
    // Consts.
    ConstValue(u64),
    ConstParam(u32),
    // Existential predicates, the elements of `Dynamic`'s list.
    /// `dyn Trait<args>`: the args after `Self`.
    ExTrait(Def, List),
    /// `Trait<.., A = term>`: the associated item, the args after `Self`, the term.
    ExProj(Def, List, Ty),
    /// `+ Send`.
    ExAuto(Def),
}

/// What a term contains, one load per term (blaze-ty's `flags` column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Flags(pub u16);

impl Flags {
    /// A type, int, float or const inference variable.
    pub const HAS_INFER: u16 = 1 << 0;
    /// A region inference variable.
    pub const HAS_RE_INFER: u16 = 1 << 1;
    /// A generic parameter (type, const or region).
    pub const HAS_PARAM: u16 = 1 << 2;
    pub const HAS_PLACEHOLDER: u16 = 1 << 3;
    /// A projection, inherent, free or const alias.
    pub const HAS_ALIAS: u16 = 1 << 4;
    pub const HAS_OPAQUE: u16 = 1 << 5;
    pub const HAS_BOUND: u16 = 1 << 6;
    pub const HAS_ERROR: u16 = 1 << 7;
    /// A region other than `'static` (a parameter, an erased one, a variable).
    pub const HAS_FREE_REGION: u16 = 1 << 8;
    pub const HAS_CANONICAL: u16 = 1 << 9;
    /// An alias with `Rigid::No` (`HAS_TY_NON_RIGID_ALIAS` and friends in rustc_type_ir
    /// flags.rs): relating the term may emit a `Projection` goal.
    pub const HAS_NON_RIGID_ALIAS: u16 = 1 << 10;

    pub const fn has(self, bits: u16) -> bool {
        self.0 & bits != 0
    }

    pub const fn union(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }

    /// No inference variable of any kind: the ground fast path applies.
    pub const fn no_infer(self) -> bool {
        self.0 & (Flags::HAS_INFER | Flags::HAS_RE_INFER) == 0
    }

    /// Nothing that depends on the environment or on inference: no variable, parameter,
    /// placeholder, alias, opaque or bound term. A marker answer for such a term is a fact
    /// of the term (solve.md (c)).
    pub const fn is_global_ground(self) -> bool {
        self.0
            & (Flags::HAS_INFER
                | Flags::HAS_RE_INFER
                | Flags::HAS_PARAM
                | Flags::HAS_PLACEHOLDER
                | Flags::HAS_ALIAS
                | Flags::HAS_OPAQUE
                | Flags::HAS_BOUND
                | Flags::HAS_CANONICAL)
            == 0
    }
}

/// The marker traits whose answers a term keeps in its `markers` byte (solve.md (c)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkerTrait {
    Sized = 0,
    Copy = 1,
    Send = 2,
    Sync = 3,
}

impl MarkerTrait {
    pub const ALL: [MarkerTrait; 4] = [MarkerTrait::Sized, MarkerTrait::Copy, MarkerTrait::Send, MarkerTrait::Sync];
}

/// Two bits per marker trait: whether the answer is known, and whether it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Markers(pub u8);

impl Markers {
    /// `Some(holds)` when the answer for `m` is known.
    pub const fn get(self, m: MarkerTrait) -> Option<bool> {
        let shift = (m as u8) * 2;
        if self.0 & (1 << shift) == 0 { None } else { Some(self.0 & (2 << shift) != 0) }
    }

    /// The byte with `m`'s answer set to `holds`.
    pub const fn with(self, m: MarkerTrait, holds: bool) -> Markers {
        let shift = (m as u8) * 2;
        let cleared = self.0 & !(3 << shift);
        Markers(cleared | (1 << shift) | if holds { 2 << shift } else { 0 })
    }
}

/// A trait goal's polarity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Polarity {
    Positive,
    Negative,
}

/// A host-effect goal's constness (`~const` is `Maybe`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Constness {
    Const,
    Maybe,
}

/// A predicate: a goal, an obligation's goal or an assumption of the param env. A fixed-size
/// `Copy` row (32 bytes). Its terms are arena handles, or portable indices in an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Pred {
    /// `args[0]: trait_<args[1..]>`.
    Trait { trait_: Def, args: List, polarity: Polarity },
    /// `alias == term`, `alias` an `Alias` term (`ClauseKind::Projection`, solved by
    /// project_goals/mod.rs): also the goal relating a non-rigid alias emits, with a fresh
    /// variable as `term` (rustc_type_ir relate/solver_relating.rs, the `IsRigid::No` arms).
    Projection { alias: Ty, term: Ty },
    /// The alias's value is `term`, which is unconstrained when asked
    /// (`PredicateKind::NormalizesTo`, normalizes_to.rs).
    NormalizesTo { alias: Ty, term: Ty },
    /// `ty: 'region`: recorded for the borrow checker.
    TypeOutlives { ty: Ty, region: Ty },
    /// `'a: 'b`: recorded for the borrow checker.
    RegionOutlives { a: Ty, b: Ty },
    WellFormed(Ty),
    ConstEvaluatable(Ty),
    /// `a <: b`; stalls while either side is an unresolved type variable.
    Subtype { a: Ty, b: Ty },
    /// `a` coerces to `b`; stalls as `Subtype`.
    Coerce { a: Ty, b: Ty },
    DynCompatible(Def),
    /// `args[0]: [const] trait_<args[1..]>`.
    HostEffect { trait_: Def, args: List, constness: Constness },
    /// The const `ct` has type `ty`.
    ConstArgHasType { ct: Ty, ty: Ty },
    /// Internal to a probe: unify `a` and `b` now (never canonicalized, never a query).
    Eq { a: Ty, b: Ty },
}

/// Why a nested goal was added (`rustc_type_ir::solve::GoalSource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GoalSource {
    Misc,
    ImplWhereBound,
    AliasBoundConstCondition,
    InstantiateHigherRanked,
    AliasWellFormed,
    NormalizeGoal,
    TypeRelating,
}

/// How sure an answer is (`rustc_type_ir::solve::Certainty` with `MaybeCause`), as one byte
/// ordered so that `and` is the maximum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Certainty {
    Yes = 0,
    /// `Maybe(Ambiguity)`.
    Ambiguous = 1,
    /// `Maybe(Overflow { suggest_increasing_limit: false })`: a fixpoint that did not settle.
    Overflow = 2,
    /// `Maybe(Overflow { suggest_increasing_limit: true })`: the depth limit.
    OverflowLimit = 3,
}

impl Certainty {
    /// Both hold (`Certainty::and`): overflow dominates ambiguity (`MaybeCause::and`).
    pub fn and(self, other: Certainty) -> Certainty {
        if self >= other { self } else { other }
    }

    /// Either holds (`MaybeInfo::or`, as `bail_with_ambiguity` folds it): ambiguity dominates
    /// overflow; yes is the identity of the fold's caller.
    pub fn or(self, other: Certainty) -> Certainty {
        match (self, other) {
            (Certainty::Yes, x) | (x, Certainty::Yes) => x,
            (Certainty::Ambiguous, _) | (_, Certainty::Ambiguous) => Certainty::Ambiguous,
            (a, b) => {
                if a <= b {
                    a
                } else {
                    b
                }
            }
        }
    }

    pub fn is_overflow(self) -> bool {
        matches!(self, Certainty::Overflow | Certainty::OverflowLimit)
    }

    pub fn from_byte(b: u8) -> Certainty {
        match b {
            0 => Certainty::Yes,
            1 => Certainty::Ambiguous,
            2 => Certainty::Overflow,
            _ => Certainty::OverflowLimit,
        }
    }
}

/// The typing mode (`rustc_type_ir::TypingMode`, the two the driver uses now).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TypingMode {
    /// Type checking and analysis of a body.
    Analysis,
    /// Coherence: the overlap check between impls, with unknowable candidates.
    Coherence,
}

/// The lang items the solver recognizes (`SolverTraitLangItem`, and the items next to them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lang {
    Sized,
    MetaSized,
    PointeeSized,
    Copy,
    Clone,
    Send,
    Sync,
    Unpin,
    Fn,
    FnMut,
    FnOnce,
    /// `FnOnce::Output`.
    FnOnceOutput,
    FnPtrTrait,
    Tuple,
    Pointee,
    /// `Pointee::Metadata`.
    Metadata,
    DynMetadata,
    DiscriminantKind,
    /// `DiscriminantKind::Discriminant`.
    Discriminant,
    Destruct,
    Drop,
    Unsize,
    CoerceUnsized,
    DispatchFromDyn,
    PhantomData,
    OwnedBox,
    ManuallyDrop,
}

/// The shape of a self type impls are bucketed by (rustc's `SimplifiedType`, blaze-impls'
/// `SelfShape`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Shape {
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
    Never,
    Tuple(u32),
    Ref(Mutbl),
    Ptr(Mutbl),
    Array,
    Slice,
    Adt(Def),
    Foreign(Def),
    FnDef(Def),
    FnPtr(u32),
    Closure(Def),
    /// `dyn Trait`: the principal, or 0 for `dyn Send` and the like.
    Dynamic(Def),
    /// An integer variable: the impls for every integer shape.
    AnyInt,
    /// A float variable: the impls for every float shape.
    AnyFloat,
    /// A parameter, placeholder, alias or opaque: only blanket impls can apply by shape.
    Blanket,
}

/// The shape of a resolved term's kind; `None` for an unresolved type variable (whose goal is
/// ambiguous before any impl is read).
pub fn shape_of_kind(k: TyKind) -> Option<Shape> {
    Some(match k {
        TyKind::Bool => Shape::Bool,
        TyKind::Char => Shape::Char,
        TyKind::Int(i) => Shape::Int(i),
        TyKind::Uint(u) => Shape::Uint(u),
        TyKind::Float(f) => Shape::Float(f),
        TyKind::Str => Shape::Str,
        TyKind::Never => Shape::Never,
        TyKind::Tuple(l) => Shape::Tuple(l.len),
        TyKind::Ref(_, _, m) => Shape::Ref(m),
        TyKind::RawPtr(_, m) => Shape::Ptr(m),
        TyKind::Array(..) => Shape::Array,
        TyKind::Slice(_) => Shape::Slice,
        TyKind::Adt(d, _) => Shape::Adt(d),
        TyKind::Foreign(d) => Shape::Foreign(d),
        TyKind::FnDef(d, _) => Shape::FnDef(d),
        TyKind::FnPtr(l) => Shape::FnPtr(l.len.saturating_sub(1)),
        TyKind::Closure(d, _) => Shape::Closure(d),
        TyKind::Dynamic(..) => Shape::Dynamic(Def(0)),
        TyKind::Infer(Var { kind: VarKind::Int, .. }) => Shape::AnyInt,
        TyKind::Infer(Var { kind: VarKind::Float, .. }) => Shape::AnyFloat,
        TyKind::Infer(_) => return None,
        TyKind::Alias(..)
        | TyKind::Param(_)
        | TyKind::Placeholder(..)
        | TyKind::Bound(..)
        | TyKind::Canonical(_)
        | TyKind::Error
        | TyKind::ReStatic
        | TyKind::ReParam(_)
        | TyKind::ReErased
        | TyKind::ConstValue(_)
        | TyKind::ConstParam(_)
        | TyKind::ExTrait(..)
        | TyKind::ExProj(..)
        | TyKind::ExAuto(_) => Shape::Blanket,
    })
}

/// Whether a kind is a region.
pub fn is_region(k: TyKind) -> bool {
    matches!(
        k,
        TyKind::ReStatic | TyKind::ReParam(_) | TyKind::ReErased | TyKind::Infer(Var { kind: VarKind::Region, .. })
    )
}

/// The children of one level, in the canonical order every walk uses (the canonicalizer, the
/// importer, the fingerprint and the stall-variable walk): lists in order, then the trailing
/// handles. Written into `out` (at most `lists` entries plus 2), returned as how many.
///
/// A list child is reported as a `ChildRef::List`; the caller reads its elements from the arena
/// or the portable table it walks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Children {
    None,
    One(Ty),
    Two(Ty, Ty),
    List(List),
    /// A list, then one more term.
    ListThen(List, Ty),
}

/// The children of `k` (see [`Children`]).
pub fn children(k: TyKind) -> Children {
    match k {
        TyKind::Tuple(l)
        | TyKind::Adt(_, l)
        | TyKind::FnDef(_, l)
        | TyKind::FnPtr(l)
        | TyKind::Closure(_, l)
        | TyKind::Alias(_, _, _, l)
        | TyKind::ExTrait(_, l) => Children::List(l),
        TyKind::Ref(r, t, _) => Children::Two(r, t),
        TyKind::Array(t, n) => Children::Two(t, n),
        TyKind::RawPtr(t, _) | TyKind::Slice(t) => Children::One(t),
        TyKind::Dynamic(l, r) => Children::ListThen(l, r),
        TyKind::ExProj(_, l, t) => Children::ListThen(l, t),
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Str
        | TyKind::Never
        | TyKind::Foreign(_)
        | TyKind::Param(_)
        | TyKind::Placeholder(..)
        | TyKind::Bound(..)
        | TyKind::Canonical(_)
        | TyKind::Infer(_)
        | TyKind::Error
        | TyKind::ReStatic
        | TyKind::ReParam(_)
        | TyKind::ReErased
        | TyKind::ConstValue(_)
        | TyKind::ConstParam(_)
        | TyKind::ExAuto(_) => Children::None,
    }
}

impl Pred {
    /// The self type of a trait-shaped predicate, read from `args[0]` by the caller; the
    /// predicate's trait for `Trait` and `HostEffect`.
    pub fn trait_of(&self) -> Option<Def> {
        match *self {
            Pred::Trait { trait_, .. } | Pred::HostEffect { trait_, .. } => Some(trait_),
            _ => None,
        }
    }
}
