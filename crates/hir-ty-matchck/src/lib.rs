//! rustc's match checking (`rustc_mir_build::thir::pattern::check_match`) over the front end's
//! bodies and inference results, with `rustc_pattern_analysis` deciding usefulness.
//!
//! One pass over a body, in the order rustc's THIR visitor takes, lowers each pattern of a
//! `match`, a `let` (statement, expression or `else`), a parameter, a `for` binding and a
//! destructuring assignment straight to `rustc_pattern_analysis`'s `DeconstructedPat`s (rustc's
//! `PatCtxt::lower_pat` and `const_to_pat` folded into one step, constants read from their
//! evaluated bytes), runs the analysis, and records what rustc lints and refuses: E0004, E0005,
//! E0030/E0579, `unreachable_patterns`, `irrefutable_let_patterns` and
//! `bindings_with_variant_name`. Witnesses become text only when they go into an error.

#![forbid(unsafe_code)]

extern crate ra_ap_rustc_pattern_analysis as rustc_pattern_analysis;

mod cx;
mod lower;
mod print;
mod typo;
mod visit;

#[cfg(test)]
mod tests;

use base_db::Db;
use hir_def::{
    AdtId, ConstId, DefWithBodyId, EnumId, EnumVariantId, ImplId,
    hir::{ExprId, ExprOrPatId, PatId},
    item_scope::ImportId,
};
use hir_ty::{InferenceResult, next_solver::StoredTy};

/// What match checking finds in a body and the closures and inline constants in it.
#[derive(Debug, Default, PartialEq, Eq, Hash)]
pub struct MatchCheck {
    /// The lints, in the order rustc emits them.
    pub lints: Box<[MatchLint]>,
    /// The errors, in the order rustc emits them.
    pub errors: Box<[MatchError]>,
    /// `check_match` returned `Err`: an error above, or one found while lowering a pattern
    /// (a constant that does not evaluate, a pattern of an erroneous type). rustc then builds
    /// no MIR of the body but its error body.
    pub failed: bool,
    /// The body was not checked: type checking found an error in it (rustc's `thir_body`
    /// fails for a body whose typeck results are tainted).
    pub not_checked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MatchLint {
    UnreachablePattern(UnreachablePattern),
    IrrefutableLet(IrrefutableLet),
    BindingWithVariantName(BindingWithVariantName),
    OverlappingRangeEndpoints(OverlappingRangeEndpoints),
    NonContiguousRangeEndpoints(NonContiguousRangeEndpoints),
}

/// `overlapping_range_endpoints` (`lint_overlapping_range_endpoints`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OverlappingRangeEndpoints {
    pub pat: PatId,
    /// The values the ranges share, printed as `print_pat_range` prints them.
    pub overlap: Box<str>,
    pub overlaps_with: Box<[PatId]>,
}

/// `non_contiguous_range_endpoints` (`lint_non_contiguous_range_endpoints`): `pat` is an
/// exclusive range `lo..hi` that leaves out `hi`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonContiguousRangeEndpoints {
    pub pat: PatId,
    /// `hi`, printed as `print_pat_range` prints it.
    pub gap: Box<str>,
    /// `lo..=hi`, as rustc's `PatRange` prints it.
    pub suggestion: Box<str>,
    /// The ranges that start at `hi + 1`; empty when `hi` is the type's maximum
    /// (`ExclusiveRangeMissingMax`).
    pub gapped_with: Box<[PatId]>,
}

/// `unreachable_patterns` (`report_unreachable_pattern`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnreachablePattern {
    pub pat: PatId,
    /// `RedundancyExplanation::covered_by`, in its order; empty when `pat` matches no values.
    pub covered_by: Box<[PatId]>,
    /// `covered_by` is one pattern and `pat_is_catchall` of it.
    pub catchall: bool,
    /// `matches_no_values_ty`: `pat`'s type, or the uninhabited type `pat.walk` finds.
    pub ty: StoredTy,
    /// A whole arm of a `match` (`whole_arm_span`): the arm whose top pattern `pat` is.
    pub whole_arm: bool,
    /// `find_fallback_pattern_typo`, when `catchall`.
    pub typo: Option<PatternTypo>,
}

/// `find_fallback_pattern_typo`'s finding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PatternTypo {
    /// `wanted_constant`: the binding names the constant (`is_typo` false) or is a typo of it.
    WantedConstant { is_typo: bool, const_name: Box<str>, const_path: Box<str> },
    /// `accessible_constant`: the `use` that re-exports a constant of the binding's name.
    AccessibleConstant(ImportId),
    /// `inaccessible_constant`: a constant of the name that the pattern can't see.
    InaccessibleConstant(ConstId),
    /// `pattern_let_binding`: the pattern of a `let` that binds the name.
    PatternLetBinding(PatId),
}

/// rustc's `LetSource`, of the `let`s that can be linted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LetSource {
    IfLet,
    ElseIfLet,
    IfLetGuard,
    LetElse,
    WhileLet,
}

/// Where an irrefutable `let` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LetAt {
    /// The `let` expression, or the `&&` chain it is the one `let` of (`lint_single_let`).
    Expr(ExprId),
    /// The `let` statement whose pattern this is (`LetElse`).
    Stmt(PatId),
}

/// `irrefutable_let_patterns` (`report_irrefutable_let_patterns`, always `count` 1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IrrefutableLet {
    pub source: LetSource,
    pub at: LetAt,
    /// `check_irrefutable_option_some`: the pattern and the inner `Option` of the initializer.
    pub replacement: Option<(PatId, ExprId)>,
}

/// `bindings_with_variant_name` (E0170).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BindingWithVariantName {
    pub pat: PatId,
    pub enum_: EnumId,
    /// `rf == Refutable || variant_count == 1`.
    pub suggest: bool,
}

/// Why a type in a witness gets a note of its own (`collect_special_tys`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpecialTy {
    /// `usize` past `usize::MAX`: "`usize::MAX` is not treated as exhaustive".
    Usize,
    /// `isize` past its bounds: "`isize::MIN` and `isize::MAX` are not treated as exhaustive".
    Isize,
    /// `str`: "`&str` cannot be matched exhaustively".
    Str,
    /// An enum of another crate marked `#[non_exhaustive]`.
    ForeignNonExhaustive,
    /// An uninhabited type that has a witness: the place may hold invalid data.
    Uninhabited,
}

/// What both E0004 and E0005 say of the values not covered.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Uncovered {
    /// Every witness, printed as `print_witness_pat` prints it.
    pub witnesses: Box<[Box<str>]>,
    /// `maybe_point_at_variant` of the first five witnesses, for the peeled matched ADT
    /// (`report_adt_defined_here`: for E0005 only when that ADT is of this crate).
    pub variants: Box<[EnumVariantId]>,
    /// `collect_special_tys` of the first witness, in its order; E0004 of a match with arms
    /// only.
    pub special_tys: Box<[(StoredTy, SpecialTy)]>,
}

/// Where a binding that must be irrefutable is (`check_binding_is_irrefutable`'s `origin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingOrigin {
    /// "local binding"
    Let,
    /// "function argument"
    FnParam,
    /// "closure argument"
    ClosureParam,
    /// "`for` loop binding"
    ForLoop,
    /// A destructuring assignment. rustc lowers it to a `let` and checks that, so its origin
    /// text is "local binding".
    Assignment,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MatchError {
    /// E0004.
    MissingMatchArms {
        match_expr: ExprId,
        scrutinee: ExprId,
        scrutinee_ty: StoredTy,
        /// The match has no arms: rustc says "type `T` is non-empty" for a type that is not
        /// an enum with variants.
        no_arms: bool,
        uncovered: Uncovered,
        /// The match would be exhaustive if its guards were taken away.
        would_be_exhaustive_without_guards: bool,
        /// The scrutinee is a reference to a type uninhabited from here: "references are
        /// always considered inhabited".
        ref_to_uninhabited: bool,
        /// The arms whose pattern is a constant that looks like a binding
        /// (`is_const_pat_that_looks_like_binding`), in arm order.
        consts_like_bindings: Box<[(PatId, ConstId)]>,
    },
    /// E0005.
    NonExhaustiveLet {
        pat: PatId,
        origin: BindingOrigin,
        /// The binding has a value it binds (`let` with an initializer), for the
        /// "consider using `if let`/`let else`" suggestion.
        has_initializer: bool,
        pattern_ty: StoredTy,
        uncovered: Uncovered,
        /// The first witness's variant is inhabited from here but not ignoring visibility.
        privately_uninhabited: bool,
        /// `is_const_pat_that_looks_like_binding`.
        interpreted_as_const: Option<ConstId>,
    },
    /// E0030 (`..=`, `lo > hi`) or E0579 (`..`, `lo >= hi`).
    EmptyRange { pat: PatId, inclusive: bool },
    /// "reached pattern complexity limit", at the scrutinee of a `match` or the pattern of a
    /// binding.
    ComplexityLimit { at: ExprOrPatId },
    /// "mix of deref patterns and normal constructors".
    MixedDerefPatterns { deref_pat: PatId, normal_pat: PatId },
    /// `const_to_pat` refused the constant the path pattern `pat` names. A value it can't
    /// make a valtree of gets one error; else each part of the value `valtree_to_pat` refuses
    /// gets one, outermost first.
    ConstPattern { pat: PatId, error: ConstPatternError },
}

/// What `const_to_pat` refuses in a constant used as a pattern.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConstPatternError {
    /// `UnionPattern`: "cannot use unions in constant patterns".
    Union,
    /// `PointerPattern`: a function pointer, or a raw pointer into an allocation.
    Pointer,
    /// `InvalidPattern`: "{prefix} `{ty}` cannot be used in patterns", at the part of the
    /// value of this type.
    Invalid(StoredTy),
    /// `TypeNotStructural`: the value holds this ADT, which doesn't derive `PartialEq`.
    NotStructural {
        ty: StoredTy,
        /// `possibly_inapplicable_structural_partial_eq`: the ADT has a `StructuralPartialEq`
        /// impl whose bounds don't hold, so rustc says nothing of a manual `PartialEq` impl.
        inapplicable_structural: bool,
    },
    /// `TypeNotPartialEq`: `ty` doesn't implement `PartialEq`. It is the constant's type, or
    /// an ADT in its value whose derived `PartialEq` doesn't apply. The lists are what
    /// `extend_type_not_partial_eq` finds in `ty`, each in the order it meets them, without
    /// repeats; rustc prints the spans by position and the types sorted by their text. All
    /// are empty when `ty` holds a `dyn` or an unsafe binder.
    NotPartialEq {
        ty: StoredTy,
        /// A local ADT with a manual `PartialEq` impl: a span note at the impl, or at the ADT
        /// when no local impl is written for its type.
        local_manual: Box<[ManualPartialEq]>,
        /// A local ADT with no `PartialEq` impl: a label at it.
        local_without: Box<[AdtId]>,
        /// A foreign type with a manual `PartialEq` impl: a note.
        manual: Box<[StoredTy]>,
        /// A foreign type with no `PartialEq` impl: a note.
        without: Box<[StoredTy]>,
    },
    /// `NaNPattern`: "cannot use NaN in patterns".
    NaN,
}

/// Where `extend_type_not_partial_eq`'s note about a local manual `PartialEq` impl points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManualPartialEq {
    /// The impl written for the type.
    Impl(ImplId),
    /// The ADT, whose `PartialEq` comes from a blanket or foreign impl.
    Adt(AdtId),
}

/// Match checking of `def`, once per body: the closures and inline constants written in it are
/// checked with it, as they are lowered with it.
#[blaze_query::tracked(returns(ref))]
pub fn match_check(db: &Db, def: DefWithBodyId) -> MatchCheck {
    let infer = InferenceResult::of(db, def);
    // rustc's `thir_body` is `Err` for a body type checking found an error in.
    if infer.has_errors || infer.has_type_mismatches() || infer.is_erroneous() {
        return MatchCheck { not_checked: true, ..MatchCheck::default() };
    }
    visit::check_body(db, def, infer)
}
