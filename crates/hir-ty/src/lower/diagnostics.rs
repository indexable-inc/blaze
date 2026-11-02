//! This files contains the declaration of diagnostics kinds for ty and path lowering.

use hir_def::{
    AssocItemId, EnumVariantId, GenericDefId, GenericParamId, ImplId, TraitId, TypeAliasId,
    TypeParamId,
    type_ref::{LifetimeRefId, TypeRefId},
};

use crate::{Span, consteval::CreateConstError};

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum TyLoweringDiagnostic {
    PathDiagnostic { source: TypeRefId, diag: PathLoweringDiagnostic },
    /// `_` placeholders where no inference is: one each, but those of a type its body infers (a
    /// return type, a const's or a static's), which rustc reports in one error.
    InferVarsNotAllowed { sources: Box<[Span]> },
    /// Lifetimes left out (`&T`, `'_`, a path without its lifetime arguments) where there is
    /// none to elide to. One site but for [`MissingLifetimeKind::ElisionFailure`], which has
    /// every site of one return type, as rustc reports them in one error.
    MissingLifetimes { sites: Box<[ElidedLifetimeSite]>, kind: MissingLifetimeKind },
    /// A const argument that is not a constant, at `site`: its expression, or the type ref of
    /// a path written where a type could stand.
    InvalidConstArg { site: Span, reason: CreateConstError },
    /// `impl Trait` where it is not allowed: E0562 (rustc's `ImplTraitPosition`, which the
    /// emitter reads from where `source` is written).
    ImplTraitNotAllowed { source: TypeRefId },
    /// A trait object with more than one non-auto trait: E0225, at the bound of the `first`
    /// and those of the `additional` ones (rustc's `lower_trait_object_ty`).
    AdditionalNonAutoTraits { first: TypeRefId, additional: Box<[TypeRefId]> },
    /// Two bounds of a trait object that give the associated type or const `assoc` different
    /// values, the second at `source`: E0719 (rustc's `lower_trait_object_ty`).
    ConflictingAssocTypeBindings { source: TypeRefId, assoc: AssocItemId },
    /// A trait object whose principal trait `trait_` leaves out the type parameters `params`
    /// whose defaults name `Self`, at the bound `source`: E0393 (rustc's
    /// `report_missing_type_params`).
    MissingSelfDefaultParams { source: TypeRefId, trait_: TraitId, params: Box<[TypeParamId]> },
    /// A trait object whose bounds are only relaxed ones (`dyn ?Sized`), at the first:
    /// E0224. A `dyn` with no trait written at all is the parser's.
    ObjectWithoutTrait { source: TypeRefId },
    /// The pattern of the pattern type `source` (`#![feature(pattern_types)]`), or one of its
    /// `|` alternatives, which is not lowered for `reason`.
    PatternTypeNotLowered { source: TypeRefId, reason: PatternTypeUnsupported },
}

/// Why a pattern type's pattern is not lowered.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum PatternTypeUnsupported {
    /// Neither a range, `!null` nor `|`: rustc's "pattern not supported in pattern types"
    /// (`pat_to_ty_pat` in `rustc_builtin_macros/src/pattern_type.rs`).
    /// A range's ends are always lowered: hir-def makes `a..b` and `a..` inclusive as rustc's
    /// `lower_excluded_range_end` and `lower_ty_pat_range_end` do.
    Pattern,
}

/// Where a lifetime is left out.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum ElidedLifetimeSite {
    /// `&T`: the reference type.
    Reference(TypeRefId),
    /// `'_`.
    Placeholder(LifetimeRefId),
    /// A path to `def` without its lifetime arguments; `source` is the path type.
    Path {
        source: TypeRefId,
        generics_source: PathGenericsSource,
        def: GenericDefId,
        expected_count: u32,
    },
}

/// Why a lifetime left out has nothing to elide to, as rustc's lifetime ribs tell.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum MissingLifetimeKind {
    /// In an item with no lifetime to elide to, a field or a free type alias: E0106.
    Missing,
    /// In a return type whose parameters give no single lifetime: E0106 with what the
    /// parameters have.
    ElisionFailure,
    /// Where `&` and `'_` are not allowed: a bound, a where clause, a const parameter's type,
    /// a parameter default, an associated type: E0637. A path there is E0106, `Missing`.
    NotAllowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenericArgsProhibitedReason {
    Module,
    TyParam,
    SelfTy,
    PrimitiveTy,
    Const,
    Static,
    LocalVariable,
    /// When there is a generic enum, within the expression `Enum::Variant`,
    /// either `Enum` or `Variant` are allowed to have generic arguments, but not both.
    EnumVariant,
    /// A variant a struct path reaches through a type (`<E>::V`, `Alias::V`, `Self::V`), which
    /// rustc's `lower_assoc_path` gives no generic arguments.
    TypeRelativeVariant,
}

/// A path can have many generic arguments: each segment may have one associated with the
/// segment, and in addition, each associated type binding may have generic arguments. This
/// enum abstracts over both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathGenericsSource {
    /// Generic arguments directly on the segment.
    Segment(u32),
    /// Generic arguments on an associated type, e.g. `Foo<Assoc<A, B> = C>` or `Foo<Assoc<A, B>: Bound>`.
    AssocType { segment: u32, assoc_type: u32 },
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum PathLoweringDiagnostic {
    /// Generic arguments on the `segments` (in order, at least one) that rustc's one
    /// `prohibit_generic_args` checks: a path's leading segments, with the last when it is a
    /// type parameter, `Self`, a primitive type or a module; `reason` is the last one's.
    GenericArgsProhibited {
        segments: Box<[u32]>,
        reason: GenericArgsProhibitedReason,
    },
    ParenthesizedGenericArgsWithoutFnTrait {
        segment: u32,
    },
    /// The expected lifetimes & types and consts counts can be found by inspecting the `GenericDefId`.
    IncorrectGenericsLen {
        generics_source: PathGenericsSource,
        provided_count: u32,
        expected_count: u32,
        kind: IncorrectGenericsLenKind,
        def: GenericDefId,
    },
    IncorrectGenericsOrder {
        generics_source: PathGenericsSource,
        param_id: GenericParamId,
        arg_idx: u32,
        /// Whether the `GenericArgs` contains a `Self` arg.
        has_self_arg: bool,
    },
    ElidedLifetimesInPath {
        generics_source: PathGenericsSource,
        def: GenericDefId,
        expected_count: u32,
        hard_error: bool,
    },
    /// An elided lifetimes was used (either implicitly, by not specifying lifetimes, or explicitly, by using `'_`),
    /// but lifetime elision could not find a lifetime to replace it with.
    ElisionFailure {
        generics_source: PathGenericsSource,
        def: GenericDefId,
        expected_count: u32,
    },
    MissingLifetime {
        generics_source: PathGenericsSource,
        def: GenericDefId,
        expected_count: u32,
    },
    /// Generic defaults are not allowed to refer to `Self`.
    GenericDefaultRefersToSelf {
        /// Index of the `Self` segment.
        segment: u32,
    },
    /// Lifetime arguments given to a function with late-bound lifetime parameters: rustc's
    /// `prohibit_explicit_late_bound_lifetimes`. E0794 when `hard_error` (a path to the function
    /// with other than one argument per early-bound lifetime), else the
    /// `late_bound_lifetime_arguments` lint; the lifetime arguments are ignored either way.
    LateBoundLifetimeArgs {
        generics_source: PathGenericsSource,
        def: GenericDefId,
        hard_error: bool,
    },
    /// `Self::Name` in a trait impl for an enum with a variant `Name`, resolved to the trait's
    /// associated type: the `ambiguous_associated_items` lint.
    AmbiguousAssociatedItem {
        segment: u32,
        variant: EnumVariantId,
        item: TypeAliasId,
        impl_: ImplId,
    },
    /// The associated type `segment` names, looked up in `qself`'s bounds (`T::Name`,
    /// `Self::Name`) or in the trait (`Trait::Name`), that `failure` gives: E0220 not found,
    /// E0221 ambiguous, E0391 a cycle (rustc's `probe_single_bound_for_assoc_item`).
    AssocTypeNotResolved { segment: u32, qself: AssocTypeQself, failure: AssocTypeFailure },
    /// An associated type `segment` of a type no bound can give it of (`T::A::B`,
    /// `<u8>::Name`, an inherent one not found): E0223 (rustc's
    /// `report_ambiguous_assoc_item`).
    AmbiguousAssocType { segment: u32 },
}

/// Where an associated type shorthand is looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssocTypeQself {
    /// The bounds of a type parameter, `Self` included.
    Param(TypeParamId),
    /// The trait itself.
    Trait(TraitId),
}

/// Why an associated type shorthand did not resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssocTypeFailure {
    NotFound,
    Ambiguous,
    Cycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IncorrectGenericsLenKind {
    Lifetimes,
    TypesAndConsts,
}
