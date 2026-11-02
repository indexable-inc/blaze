//! Each of the front end's diagnostics (`hir::AnyDiagnostic`) as rustc gives it: its code,
//! message, spans, labels, notes, help and suggestions in rustc's words, and the facts it read.
//! One module per kind, each `pub(crate) fn lower(cx, d) -> Vec<Found>`, following the rustc
//! code that gives the same error (named in each module).

use hir::{AnyDiagnostic, InFile};
use syntax::SyntaxNodePtr;

use crate::{cx::Cx, model::Diag};

pub(crate) mod parse;

pub(crate) mod additional_non_auto_traits;
pub(crate) mod ambiguous_assoc_type;
pub(crate) mod ambiguous_associated_item;
pub(crate) mod assoc_type_not_resolved;
pub(crate) mod autoderef_recursion_limit;
pub(crate) mod bad_rtn;
pub(crate) mod conflicting_assoc_type_bindings;
pub(crate) mod fn_trait_args_not_tuple;
pub(crate) mod impl_trait_not_allowed;
pub(crate) mod inherited_ref_mutability_mismatch;
pub(crate) mod method_on_unknown_raw_pointee;
pub(crate) mod missing_self_default_params;
pub(crate) mod not_a_struct_path;
pub(crate) mod object_without_trait;
pub(crate) mod pattern_type_not_lowered;
pub(crate) mod self_ctor_not_tuple_or_unit;
pub(crate) mod tyvar_behind_raw_pointer;
pub(crate) mod const_param_ty_not_allowed;
pub(crate) mod elided_lifetimes_in_path;
pub(crate) mod generic_args_prohibited;
pub(crate) mod generic_default_refers_to_self;
pub(crate) mod incoherent_impl;
pub(crate) mod incorrect_case;
pub(crate) mod incorrect_generics_len;
pub(crate) mod invalid_const_arg;
pub(crate) mod incorrect_generics_order;
pub(crate) mod infer_vars_not_allowed;
pub(crate) mod invalid_derive_target;
pub(crate) mod invalid_visibility;
pub(crate) mod late_bound_lifetime_args;
pub(crate) mod macro_def_error;
pub(crate) mod macro_error;
pub(crate) mod macro_expansion_parse_error;
pub(crate) mod malformed_derive;
pub(crate) mod missing_body;
pub(crate) mod missing_prelude;
pub(crate) mod missing_lifetime;
pub(crate) mod missing_lifetimes;
pub(crate) mod parenthesized_generic_args_without_fn_trait;
pub(crate) mod pattern_arg_in_extern_fn;
pub(crate) mod trait_impl_incorrect_safety;
pub(crate) mod trait_impl_missing_assoc_items;
pub(crate) mod trait_impl_orphan;
pub(crate) mod trait_impl_redundant_assoc_items;
pub(crate) mod trait_impl_unstable_default_body;
pub(crate) mod unimplemented_builtin_macro;
pub(crate) mod unresolved_extern_crate;
pub(crate) mod unresolved_import;
pub(crate) mod unresolved_macro_call;
pub(crate) mod unresolved_module;

pub(crate) mod array_pattern_without_fixed_length;
pub(crate) mod binary_operator_cannot_be_applied;
pub(crate) mod cannot_be_dereferenced;
pub(crate) mod cannot_implicitly_deref_trait_object;
pub(crate) mod cannot_index_into;
pub(crate) mod cast_to_unsized;
pub(crate) mod closure_kind_mismatch;
pub(crate) mod duplicate_field;
pub(crate) mod edition_dependent_method_call;
pub(crate) mod expected_array_or_slice_pat;
pub(crate) mod expected_function;
pub(crate) mod explicit_drop_method_use;
pub(crate) mod fru_in_destructuring_assignment;
pub(crate) mod fully_qualified_call_from_2018;
pub(crate) mod functional_record_update_on_non_struct;
pub(crate) mod incorrect_arguments;
pub(crate) mod invalid_cast;
pub(crate) mod invalid_lhs_of_assignment;
pub(crate) mod invalid_range_pat_type;
pub(crate) mod method_call_illegal_sized_bound;
pub(crate) mod mismatched_arg_count;
pub(crate) mod mismatched_array_pat_len;
pub(crate) mod mismatched_tuple_struct_pat_arg_count;
pub(crate) mod missing_fields;
pub(crate) mod mut_ref_in_imm_ref_pat;
pub(crate) mod mutable_ref_binding;
pub(crate) mod no_such_field;
pub(crate) mod non_exhaustive_record_expr;
pub(crate) mod non_exhaustive_record_pat;
pub(crate) mod private_assoc_item;
pub(crate) mod private_field;
pub(crate) mod obligation_cause;
pub(crate) mod trait_item_import;
pub(crate) mod trait_solver_error;
pub(crate) mod type_mismatch;
pub(crate) mod type_must_be_known;
pub(crate) mod typed_hole;
pub(crate) mod unary_operator_cannot_be_applied;
pub(crate) mod unimplemented_trait;
pub(crate) mod union_expr_must_have_exactly_one_field;
pub(crate) mod union_pat_has_rest;
pub(crate) mod union_pat_must_have_exactly_one_field;
pub(crate) mod unresolved_assoc_item;
pub(crate) mod unresolved_field;
pub(crate) mod unresolved_ident;
pub(crate) mod unresolved_method_call;
pub(crate) mod unresolved_path;
pub(crate) mod unsafe_binder_cast_of_non_binder;

pub(crate) mod async_coroutines_not_supported;
pub(crate) mod await_outside_of_async;
pub(crate) mod int_literal_too_large;
pub(crate) mod borrowck_error;
pub(crate) mod borrowck_skipped;
pub(crate) mod break_outside_of_loop;
pub(crate) mod const_check_error;
pub(crate) mod const_eval_failed;
pub(crate) mod const_impure;
pub(crate) mod const_pattern_refused;
pub(crate) mod empty_range_pattern;
pub(crate) mod missing_match_arms;
pub(crate) mod missing_unsafe;
pub(crate) mod mixed_deref_patterns;
pub(crate) mod non_exhaustive_let;
pub(crate) mod pattern_complexity_limit;
pub(crate) mod return_outside_function;
pub(crate) mod undeclared_label;
pub(crate) mod unlabeled_jump_in_labeled_block;
pub(crate) mod unplaced_diagnostic;
pub(crate) mod unreachable_label;
pub(crate) mod unused_must_use;
pub(crate) mod val_tree_error;
pub(crate) mod yield_outside_coroutine;

/// When rustc gives a diagnostic, by the pass that gives it: the driver writes them in this
/// order, and gives none of [`Phase::Late`] after an error that is not a lint's
/// (`rustc_interface::passes::analysis`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// The parser.
    Parse,
    /// The crate root's attributes rustc reads before expansion (`passes.rs`:
    /// `#![crate_type]`, `#![recursion_limit]`), whose errors are fatal.
    CrateAttrs,
    /// Expansion (`rustc_expand`: macro errors, E0583, macros not found).
    Expand,
    /// AST validation (`rustc_ast_passes::ast_validation`), after expansion and before the
    /// resolver's errors (`configure_and_expand`).
    Validate,
    /// Name resolution's errors at the end of `resolve_crate`, the feature gates and the
    /// early lints.
    Resolve,
    /// Lowering the AST to HIR (`rustc_ast_lowering`: E0728 `await` outside `async`, `yield`
    /// outside a `#[coroutine]` closure).
    Lower,
    /// The checks of items (`rustc_hir_analysis`: collect, coherence, well-formedness).
    Items,
    /// Type checking of bodies (`typeck`), with what it checks of patterns and calls.
    Bodies,
    /// What runs on MIR (`mir_borrowck`, `check_unsafety`, const checking, match checking).
    Mir,
    /// After the abort: late lints, dead code, privacy (`misc_checking_3`).
    Late,
    /// rustc's stashed warnings (the soft feature gates'), written last and only when the
    /// crate had no error at all, lints' included.
    Stashed,
}

/// A diagnostic found, before its lint level (when it is a lint's) is decided.
#[derive(Clone, Debug)]
pub struct Found {
    pub diag: Diag,
    pub phase: Phase,
    /// For a lint: the node whose attributes, and those of what is around it, decide its
    /// level (rustc's `HirId` it is emitted at).
    pub lint_at: Option<InFile<SyntaxNodePtr>>,
    /// For a lint of rustc's pre-expansion lint pass: its level is of `lint_at`'s file alone
    /// ([`hir_lint_levels::Levels::set_of_in_file`]).
    pub pre_expansion: bool,
}

impl Found {
    pub fn new(diag: Diag, phase: Phase) -> Found {
        Found { diag, phase, lint_at: None, pre_expansion: false }
    }

    /// A lint's diagnostic, its level decided at `at`.
    pub fn lint(diag: Diag, phase: Phase, at: InFile<SyntaxNodePtr>) -> Found {
        Found { diag, phase, lint_at: Some(at), pre_expansion: false }
    }
}

/// `d` as rustc gives it: none, one, or several diagnostics.
pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: AnyDiagnostic<'db>) -> Vec<Found> {
    match d {
        AnyDiagnostic::AmbiguousAssociatedItem(d) => ambiguous_associated_item::lower(cx, &d),
        AnyDiagnostic::BadRtn(d) => bad_rtn::lower(cx, &d),
        AnyDiagnostic::ConstParamTyNotAllowed(d) => const_param_ty_not_allowed::lower(cx, &d),
        AnyDiagnostic::ElidedLifetimesInPath(d) => elided_lifetimes_in_path::lower(cx, &d),
        AnyDiagnostic::GenericArgsProhibited(d) => generic_args_prohibited::lower(cx, &d),
        AnyDiagnostic::GenericDefaultRefersToSelf(d) => generic_default_refers_to_self::lower(cx, &d),
        AnyDiagnostic::IncoherentImpl(d) => incoherent_impl::lower(cx, &d),
        AnyDiagnostic::IncorrectCase(d) => incorrect_case::lower(cx, &d),
        AnyDiagnostic::IncorrectGenericsLen(d) => incorrect_generics_len::lower(cx, &d),
        AnyDiagnostic::IncorrectGenericsOrder(d) => incorrect_generics_order::lower(cx, &d),
        AnyDiagnostic::InferVarsNotAllowed(d) => infer_vars_not_allowed::lower(cx, &d),
        AnyDiagnostic::InvalidDeriveTarget(d) => invalid_derive_target::lower(cx, &d),
        AnyDiagnostic::InvalidVisibility(d) => invalid_visibility::lower(cx, &d),
        AnyDiagnostic::InvalidConstArg(d) => invalid_const_arg::lower(cx, &d),
        AnyDiagnostic::MissingPrelude(d) => missing_prelude::lower(cx, &d),
        AnyDiagnostic::AdditionalNonAutoTraits(d) => additional_non_auto_traits::lower(cx, &d),
        AnyDiagnostic::AmbiguousAssocType(d) => ambiguous_assoc_type::lower(cx, &d),
        AnyDiagnostic::AssocTypeNotResolved(d) => assoc_type_not_resolved::lower(cx, &d),
        AnyDiagnostic::ConflictingAssocTypeBindings(d) => conflicting_assoc_type_bindings::lower(cx, &d),
        AnyDiagnostic::ImplTraitNotAllowed(d) => impl_trait_not_allowed::lower(cx, &d),
        AnyDiagnostic::MissingSelfDefaultParams(d) => missing_self_default_params::lower(cx, &d),
        AnyDiagnostic::ObjectWithoutTrait(d) => object_without_trait::lower(cx, &d),
        AnyDiagnostic::PatternTypeNotLowered(d) => pattern_type_not_lowered::lower(cx, &d),
        AnyDiagnostic::NotAStructPath(d) => not_a_struct_path::lower(cx, &d),
        AnyDiagnostic::AutoderefRecursionLimit(d) => autoderef_recursion_limit::lower(cx, &d),
        AnyDiagnostic::FnTraitArgsNotTuple(d) => fn_trait_args_not_tuple::lower(cx, &d),
        AnyDiagnostic::InheritedRefMutabilityMismatch(d) => inherited_ref_mutability_mismatch::lower(cx, &d),
        AnyDiagnostic::MethodOnUnknownRawPointee(d) => method_on_unknown_raw_pointee::lower(cx, &d),
        AnyDiagnostic::SelfCtorNotTupleOrUnit(d) => self_ctor_not_tuple_or_unit::lower(cx, &d),
        AnyDiagnostic::TyvarBehindRawPointer(d) => tyvar_behind_raw_pointer::lower(cx, &d),
        AnyDiagnostic::LateBoundLifetimeArgs(d) => late_bound_lifetime_args::lower(cx, &d),
        AnyDiagnostic::MacroDefError(d) => macro_def_error::lower(cx, &d),
        AnyDiagnostic::MacroError(d) => macro_error::lower(cx, &d),
        AnyDiagnostic::MacroExpansionParseError(d) => macro_expansion_parse_error::lower(cx, &d),
        AnyDiagnostic::MalformedDerive(d) => malformed_derive::lower(cx, &d),
        AnyDiagnostic::MissingBody(d) => missing_body::lower(cx, &d),
        AnyDiagnostic::MissingLifetime(d) => missing_lifetime::lower(cx, &d),
        AnyDiagnostic::MissingLifetimes(d) => missing_lifetimes::lower(cx, &d),
        AnyDiagnostic::ParenthesizedGenericArgsWithoutFnTrait(d) => {
            parenthesized_generic_args_without_fn_trait::lower(cx, &d)
        }
        AnyDiagnostic::PatternArgInExternFn(d) => pattern_arg_in_extern_fn::lower(cx, &d),
        AnyDiagnostic::TraitImplIncorrectSafety(d) => trait_impl_incorrect_safety::lower(cx, &d),
        AnyDiagnostic::TraitImplMissingAssocItems(d) => trait_impl_missing_assoc_items::lower(cx, &d),
        AnyDiagnostic::TraitImplOrphan(d) => trait_impl_orphan::lower(cx, &d),
        AnyDiagnostic::TraitImplRedundantAssocItems(d) => {
            trait_impl_redundant_assoc_items::lower(cx, &d)
        }
        AnyDiagnostic::TraitImplUnstableDefaultBody(d) => trait_impl_unstable_default_body::lower(cx, &d),
        AnyDiagnostic::UnimplementedBuiltinMacro(d) => unimplemented_builtin_macro::lower(cx, &d),
        AnyDiagnostic::UnresolvedExternCrate(d) => unresolved_extern_crate::lower(cx, &d),
        AnyDiagnostic::UnresolvedImport(d) => unresolved_import::lower(cx, &d),
        AnyDiagnostic::TraitItemImport(d) => trait_item_import::lower(cx, &d),
        AnyDiagnostic::UnresolvedMacroCall(d) => unresolved_macro_call::lower(cx, &d),
        AnyDiagnostic::UnresolvedModule(d) => unresolved_module::lower(cx, &d),

        AnyDiagnostic::ArrayPatternWithoutFixedLength(d) => {
            array_pattern_without_fixed_length::lower(cx, &d)
        }
        AnyDiagnostic::BinaryOperatorCannotBeApplied(d) => {
            binary_operator_cannot_be_applied::lower(cx, &d)
        }
        AnyDiagnostic::CannotBeDereferenced(d) => cannot_be_dereferenced::lower(cx, &d),
        AnyDiagnostic::CannotImplicitlyDerefTraitObject(d) => {
            cannot_implicitly_deref_trait_object::lower(cx, &d)
        }
        AnyDiagnostic::CannotIndexInto(d) => cannot_index_into::lower(cx, &d),
        AnyDiagnostic::CastToUnsized(d) => cast_to_unsized::lower(cx, &d),
        AnyDiagnostic::ClosureKindMismatch(d) => closure_kind_mismatch::lower(cx, &d),
        AnyDiagnostic::DuplicateField(d) => duplicate_field::lower(cx, &d),
        AnyDiagnostic::EditionDependentMethodCall(d) => edition_dependent_method_call::lower(cx, &d),
        AnyDiagnostic::ExpectedArrayOrSlicePat(d) => expected_array_or_slice_pat::lower(cx, &d),
        AnyDiagnostic::ExpectedFunction(d) => expected_function::lower(cx, &d),
        AnyDiagnostic::ExplicitDropMethodUse(d) => explicit_drop_method_use::lower(cx, &d),
        AnyDiagnostic::FruInDestructuringAssignment(d) => {
            fru_in_destructuring_assignment::lower(cx, &d)
        }
        AnyDiagnostic::FullyQualifiedCallFrom2018(d) => fully_qualified_call_from_2018::lower(cx, &d),
        AnyDiagnostic::FunctionalRecordUpdateOnNonStruct(d) => {
            functional_record_update_on_non_struct::lower(cx, &d)
        }
        AnyDiagnostic::IncorrectArguments(d) => incorrect_arguments::lower(cx, &d),
        AnyDiagnostic::InvalidCast(d) => invalid_cast::lower(cx, &d),
        AnyDiagnostic::InvalidLhsOfAssignment(d) => invalid_lhs_of_assignment::lower(cx, &d),
        AnyDiagnostic::InvalidRangePatType(d) => invalid_range_pat_type::lower(cx, &d),
        AnyDiagnostic::MethodCallIllegalSizedBound(d) => {
            method_call_illegal_sized_bound::lower(cx, &d)
        }
        AnyDiagnostic::MismatchedArgCount(d) => mismatched_arg_count::lower(cx, &d),
        AnyDiagnostic::MismatchedArrayPatLen(d) => mismatched_array_pat_len::lower(cx, &d),
        AnyDiagnostic::MismatchedTupleStructPatArgCount(d) => {
            mismatched_tuple_struct_pat_arg_count::lower(cx, &d)
        }
        AnyDiagnostic::MissingFields(d) => missing_fields::lower(cx, &d),
        AnyDiagnostic::MutRefInImmRefPat(d) => mut_ref_in_imm_ref_pat::lower(cx, &d),
        AnyDiagnostic::MutableRefBinding(d) => mutable_ref_binding::lower(cx, &d),
        AnyDiagnostic::NoSuchField(d) => no_such_field::lower(cx, &d),
        AnyDiagnostic::NonExhaustiveRecordExpr(d) => non_exhaustive_record_expr::lower(cx, &d),
        AnyDiagnostic::NonExhaustiveRecordPat(d) => non_exhaustive_record_pat::lower(cx, &d),
        AnyDiagnostic::PrivateAssocItem(d) => private_assoc_item::lower(cx, &d),
        AnyDiagnostic::PrivateField(d) => private_field::lower(cx, &d),
        AnyDiagnostic::TypeMismatch(d) => type_mismatch::lower(cx, &d),
        AnyDiagnostic::TypeMustBeKnown(d) => type_must_be_known::lower(cx, &d),
        AnyDiagnostic::TraitSolverError(d) => trait_solver_error::lower(cx, &d),
        AnyDiagnostic::TypedHole(d) => typed_hole::lower(cx, &d),
        AnyDiagnostic::UnaryOperatorCannotBeApplied(d) => {
            unary_operator_cannot_be_applied::lower(cx, &d)
        }
        AnyDiagnostic::UnimplementedTrait(d) => unimplemented_trait::lower(cx, &d),
        AnyDiagnostic::UnionExprMustHaveExactlyOneField(d) => {
            union_expr_must_have_exactly_one_field::lower(cx, &d)
        }
        AnyDiagnostic::UnionPatHasRest(d) => union_pat_has_rest::lower(cx, &d),
        AnyDiagnostic::UnionPatMustHaveExactlyOneField(d) => {
            union_pat_must_have_exactly_one_field::lower(cx, &d)
        }
        AnyDiagnostic::UnresolvedAssocItem(d) => unresolved_assoc_item::lower(cx, &d),
        AnyDiagnostic::UnresolvedField(d) => unresolved_field::lower(cx, &d),
        AnyDiagnostic::UnresolvedIdent(d) => unresolved_ident::lower(cx, &d),
        AnyDiagnostic::UnresolvedMethodCall(d) => unresolved_method_call::lower(cx, &d),
        AnyDiagnostic::UnresolvedPath(d) => unresolved_path::lower(cx, &d),

        AnyDiagnostic::AsyncCoroutinesNotSupported(d) => async_coroutines_not_supported::lower(cx, &d),
        AnyDiagnostic::AwaitOutsideOfAsync(d) => await_outside_of_async::lower(cx, &d),
        AnyDiagnostic::IntLiteralTooLarge(d) => int_literal_too_large::lower(cx, &d),
        AnyDiagnostic::BorrowckError(d) => borrowck_error::lower(cx, &d),
        AnyDiagnostic::BorrowckSkipped(d) => borrowck_skipped::lower(cx, &d),
        AnyDiagnostic::BreakOutsideOfLoop(d) => break_outside_of_loop::lower(cx, &d),
        AnyDiagnostic::ConstCheckError(d) => const_check_error::lower(cx, &d),
        AnyDiagnostic::ConstEvalFailed(d) => const_eval_failed::lower(cx, &d),
        AnyDiagnostic::ConstImpure(d) => const_impure::lower(cx, &d),
        AnyDiagnostic::ConstPatternRefused(d) => const_pattern_refused::lower(cx, &d),
        AnyDiagnostic::EmptyRangePattern(d) => empty_range_pattern::lower(cx, &d),
        AnyDiagnostic::MissingMatchArms(d) => missing_match_arms::lower(cx, &d),
        AnyDiagnostic::MissingUnsafe(d) => missing_unsafe::lower(cx, &d),
        AnyDiagnostic::MixedDerefPatterns(d) => mixed_deref_patterns::lower(cx, &d),
        AnyDiagnostic::NonExhaustiveLet(d) => non_exhaustive_let::lower(cx, &d),
        AnyDiagnostic::PatternComplexityLimit(d) => pattern_complexity_limit::lower(cx, &d),
        AnyDiagnostic::ReturnOutsideFunction(d) => return_outside_function::lower(cx, &d),
        AnyDiagnostic::UnsafeBinderCastOfNonBinder(d) => unsafe_binder_cast_of_non_binder::lower(cx, &d),
        AnyDiagnostic::UndeclaredLabel(d) => undeclared_label::lower(cx, &d),
        AnyDiagnostic::UnlabeledJumpInLabeledBlock(d) => unlabeled_jump_in_labeled_block::lower(cx, &d),
        AnyDiagnostic::UnplacedDiagnostic(d) => unplaced_diagnostic::lower(cx, &d),
        AnyDiagnostic::UnreachableLabel(d) => unreachable_label::lower(cx, &d),
        AnyDiagnostic::UnusedMustUse(d) => unused_must_use::lower(cx, &d),
        AnyDiagnostic::ValTreeError(d) => val_tree_error::lower(cx, &d),
        AnyDiagnostic::YieldOutsideCoroutine(d) => yield_outside_coroutine::lower(cx, &d),

        // rust-analyzer's own: code rustc says nothing of (a `cfg` that is off) and style
        // hints that are clippy's (blaze-lints), not rustc's.
        AnyDiagnostic::InactiveCode(_)
        | AnyDiagnostic::RemoveTrailingReturn(_)
        | AnyDiagnostic::RemoveUnnecessaryElse(_)
        // alloc: empty, allocates nothing
        | AnyDiagnostic::ReplaceFilterMapNextWithFindMap(_) => Vec::new(),
    }
}
