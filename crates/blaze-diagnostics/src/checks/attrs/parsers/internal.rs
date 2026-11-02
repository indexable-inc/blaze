//! The errors of the parsers of the compiler's internal attributes: `#[lang]` (an unknown lang
//! item, E0522; one on a foreign item that is not a weak lang item, E0264; and its target,
//! which the lang item decides), `#[panic_handler]`, `#[rustc_legacy_const_generics]`,
//! `#[rustc_must_implement_one_of]`, `#[rustc_never_type_options]`, `#[rustc_scalable_vector]`,
//! the incremental tests' `#[rustc_clean]`, `#[rustc_if_this_changed]`,
//! `#[rustc_then_this_would_need]` and `#[rustc_partition_*]`, `#[rustc_mir]` and those of no
//! arguments.
//!
//! An unknown lang item is the parser's error in this checkout (`LangParser`), not
//! `check_attr`'s; so is a lang item on the wrong target (once E0718, now `InvalidTarget`).
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/rustc_internal.rs`, the lang
//! items of `compiler/rustc_hir/src/lang_items.rs` (`language_item_table!`: each name and its
//! target) and `compiler/rustc_hir/src/weak_lang_items.rs`, with the texts of
//! `compiler/rustc_attr_parsing/src/session_diagnostics.rs` and
//! `compiler/rustc_attr_parsing/src/diagnostics.rs` (MIT or Apache-2.0).

use syntax::TextRange;

use super::{Accepted, Finalize, GroupId, limit, no_args, single_integer};
use crate::{
    checks::attrs::{
        accept::Accept,
        args::{ArgParser, Lit, MetaItem, MetaItemOrLit},
        parsed::{Parsed, ParsedAttr},
    },
    lints::syntax::attrs::{
        Target,
        table::{Policy, Targets},
        target::MethodKind,
    },
};

pub(crate) const PATHS: &[&[&str]] = &[
    &["lang"],
    &["panic_handler"],
    &["prelude_import"],
    &["rustc_canonical_symbol"],
    &["rustc_capture_analysis"],
    &["rustc_clean"],
    &["rustc_conversion_suggestion"],
    &["rustc_deprecated_safe_2024"],
    &["rustc_diagnostic_item"],
    &["rustc_do_not_const_check"],
    &["rustc_doc_primitive"],
    &["rustc_effective_visibility"],
    &["rustc_expected_cgu_reuse"],
    &["rustc_has_incoherent_inherent_impls"],
    &["rustc_if_this_changed"],
    &["rustc_inherit_overflow_checks"],
    &["rustc_insignificant_dtor"],
    &["rustc_intrinsic"],
    &["rustc_intrinsic_const_stable_indirect"],
    &["rustc_legacy_const_generics"],
    &["rustc_lint_opt_deny_field_access"],
    &["rustc_lint_opt_ty"],
    &["rustc_lint_query_instability"],
    &["rustc_lint_untracked_query_information"],
    &["rustc_main"],
    &["rustc_mir"],
    &["rustc_must_implement_one_of"],
    &["rustc_must_match_exhaustively"],
    &["rustc_never_returns_null_ptr"],
    &["rustc_never_type_options"],
    &["rustc_no_implicit_autorefs"],
    &["rustc_no_mir_inline"],
    &["rustc_no_writable"],
    &["rustc_non_const_trait_method"],
    &["rustc_nonnull_optimization_guaranteed"],
    &["rustc_nounwind"],
    &["rustc_offload_kernel"],
    &["rustc_panics_when_zero"],
    &["rustc_partition_codegened"],
    &["rustc_partition_reused"],
    &["rustc_regions"],
    &["rustc_reservation_impl"],
    &["rustc_scalable_vector"],
    &["rustc_simd_monomorphize_lane_limit"],
    &["rustc_strict_coherence"],
    &["rustc_then_this_would_need"],
    &["rustc_trivial_field_reads"],
];

/// The state of the parsers of several attributes, of one node: rustc's `Combine` parsers
/// (`rustc_clean`, `rustc_mir`, `rustc_then_this_would_need`) and `RustcCguTestAttributeParser`
/// make their attribute once one is seen, whatever its items, so nothing is kept.
#[derive(Default)]
pub(crate) struct State {}

pub(crate) fn accept(_state: &mut State, cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Accepted {
    let other = |name| Parsed::Other(name);
    match cx.builtin.path {
        ["lang"] => Accepted::Single(lang(cx, args)),
        ["panic_handler"] => no_args(cx, args, Parsed::Lang("panic_impl")),
        ["prelude_import"] => no_args(cx, args, other("PreludeImport")),
        ["rustc_canonical_symbol"] => no_args(cx, args, other("RustcCanonicalSymbol")),
        ["rustc_capture_analysis"] => no_args(cx, args, other("RustcCaptureAnalysis")),
        ["rustc_clean"] => {
            clean(cx, args);
            Accepted::Group("rustc_clean")
        }
        ["rustc_conversion_suggestion"] => no_args(cx, args, other("RustcConversionSuggestion")),
        ["rustc_deprecated_safe_2024"] => Accepted::Single(deprecated_safe_2024(cx, args)),
        ["rustc_diagnostic_item"] => Accepted::Single(string(cx, args, "RustcDiagnosticItem")),
        ["rustc_do_not_const_check"] => no_args(cx, args, other("RustcDoNotConstCheck")),
        ["rustc_doc_primitive"] => Accepted::Single(string(cx, args, "RustcDocPrimitive")),
        ["rustc_effective_visibility"] => no_args(cx, args, other("RustcEffectiveVisibility")),
        ["rustc_expected_cgu_reuse"] => {
            cgu_fields(cx, args, true);
            Accepted::Group("rustc_cgu_test")
        }
        ["rustc_partition_codegened" | "rustc_partition_reused"] => {
            cgu_fields(cx, args, false);
            Accepted::Group("rustc_cgu_test")
        }
        ["rustc_has_incoherent_inherent_impls"] => no_args(cx, args, other("RustcHasIncoherentInherentImpls")),
        ["rustc_if_this_changed"] => Accepted::Single(if_this_changed(cx, args)),
        ["rustc_inherit_overflow_checks"] => no_args(cx, args, other("RustcInheritOverflowChecks")),
        ["rustc_insignificant_dtor"] => no_args(cx, args, other("RustcInsignificantDtor")),
        ["rustc_intrinsic"] => no_args(cx, args, other("RustcIntrinsic")),
        ["rustc_intrinsic_const_stable_indirect"] => no_args(cx, args, other("RustcIntrinsicConstStableIndirect")),
        ["rustc_legacy_const_generics"] => Accepted::Single(legacy_const_generics(cx, args)),
        ["rustc_lint_opt_deny_field_access"] => Accepted::Single(lint_opt_deny_field_access(cx, args)),
        ["rustc_lint_opt_ty"] => no_args(cx, args, other("RustcLintOptTy")),
        ["rustc_lint_query_instability"] => no_args(cx, args, other("RustcLintQueryInstability")),
        ["rustc_lint_untracked_query_information"] => {
            no_args(cx, args, other("RustcLintUntrackedQueryInformation"))
        }
        ["rustc_main"] => no_args(cx, args, other("RustcMain")),
        ["rustc_mir"] => {
            mir(cx, args);
            Accepted::Group("rustc_mir")
        }
        ["rustc_must_implement_one_of"] => Accepted::Single(must_implement_one_of(cx, args)),
        ["rustc_must_match_exhaustively"] => no_args(cx, args, other("RustcMustMatchExhaustively")),
        ["rustc_never_returns_null_ptr"] => no_args(cx, args, other("RustcNeverReturnsNullPtr")),
        ["rustc_never_type_options"] => Accepted::Single(never_type_options(cx, args)),
        ["rustc_no_implicit_autorefs"] => no_args(cx, args, other("RustcNoImplicitAutorefs")),
        ["rustc_no_mir_inline"] => no_args(cx, args, other("RustcNoMirInline")),
        ["rustc_no_writable"] => no_args(cx, args, other("RustcNoWritable")),
        ["rustc_non_const_trait_method"] => no_args(cx, args, other("RustcNonConstTraitMethod")),
        ["rustc_nonnull_optimization_guaranteed"] => {
            no_args(cx, args, other("RustcNonnullOptimizationGuaranteed"))
        }
        ["rustc_nounwind"] => no_args(cx, args, other("RustcNounwind")),
        ["rustc_offload_kernel"] => no_args(cx, args, other("RustcOffloadKernel")),
        ["rustc_panics_when_zero"] => no_args(cx, args, other("RustcPanicsWhenZero")),
        ["rustc_regions"] => no_args(cx, args, other("RustcRegions")),
        ["rustc_reservation_impl"] => Accepted::Single(string(cx, args, "RustcReservationImpl")),
        ["rustc_scalable_vector"] => Accepted::Single(scalable_vector(cx, args)),
        ["rustc_simd_monomorphize_lane_limit"] => Accepted::Single(simd_lane_limit(cx, args)),
        ["rustc_strict_coherence"] => no_args(cx, args, other("RustcStrictCoherence")),
        ["rustc_then_this_would_need"] => {
            then_this_would_need(cx, args);
            Accepted::Group("rustc_then_this_would_need")
        }
        ["rustc_trivial_field_reads"] => no_args(cx, args, other("RustcTrivialFieldReads")),
        path => unreachable!("{path:?} is not an internal attribute"),
    }
}

pub(crate) fn finalize(_state: &mut State, group: &'static str, _fx: &mut Finalize<'_, '_, '_>) -> Option<Parsed> {
    match group {
        "rustc_clean" => Some(Parsed::Other("RustcClean")),
        "rustc_mir" => Some(Parsed::Other("RustcMir")),
        "rustc_then_this_would_need" => Some(Parsed::Other("RustcThenThisWouldNeed")),
        "rustc_cgu_test" => Some(Parsed::Other("RustcCguTestAttr")),
        group => unreachable!("{group} is not an internal parser group"),
    }
}

/// None of `rustc_internal.rs`' parsers has a `finalize_check`.
pub(crate) fn check(_group: GroupId, _first: usize, _fx: &mut Finalize<'_, '_, '_>, _parsed: &[ParsedAttr]) {}

/// `LangParser::convert`.
fn lang(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let nv = cx.expect_name_value(args, cx.attr_range(), None)?;
    let name = cx.expect_str(nv)?;
    let Some((name, targets)) = lang_item(name) else {
        // alloc: the error's message
        let mut diag = cx.err(Some("E0522"), format!("definition of an unknown lang item: `{name}`"), cx.attr_range());
        // alloc: the error's label
        diag.span_label(cx.attr_span(), format!("definition of unknown lang item `{name}`"));
        cx.emit_err(diag);
        return None;
    };
    let foreign = matches!(cx.target(), Target::ForeignFn | Target::ForeignStatic | Target::ForeignTy | Target::ForeignMod);
    if foreign && !is_weak(name) {
        // alloc: the error's message
        let diag = cx.err(Some("E0264"), format!("unknown external lang item: `{name}`"), cx.attr_range());
        cx.emit_err(diag);
        return None;
    }
    cx.check_target_value(name, Targets::AllowList(targets));
    Some(Parsed::Lang(name))
}

/// rustc's `LangItem::is_weak` (`weak_lang_items!`): those with a `link_name`.
fn is_weak(name: &str) -> bool {
    matches!(name, "panic_impl" | "eh_personality")
}

/// The one target the lang item `name` may be applied to, if rustc has a lang item of that
/// name: rustc's `LangItem::from_name` and `LangItem::target`.
fn lang_item(name: &str) -> Option<(&'static str, &'static [Policy])> {
    LANG_ITEMS.binary_search_by(|(it, _)| (*it).cmp(name)).ok().map(|at| LANG_ITEMS[at])
}

const TRAIT: &[Policy] = &[Policy::Allow(Target::Trait)];
const STRUCT: &[Policy] = &[Policy::Allow(Target::Struct)];
const ENUM: &[Policy] = &[Policy::Allow(Target::Enum)];
const UNION: &[Policy] = &[Policy::Allow(Target::Union)];
const VARIANT: &[Policy] = &[Policy::Allow(Target::Variant)];
const FN: &[Policy] = &[Policy::Allow(Target::Fn)];
const FOREIGN_FN: &[Policy] = &[Policy::Allow(Target::ForeignFn)];
const ASSOC_CONST: &[Policy] = &[Policy::Allow(Target::AssocConst)];
const ASSOC_TY: &[Policy] = &[Policy::Allow(Target::AssocTy)];
const INHERENT_METHOD: &[Policy] = &[Policy::Allow(Target::Method(MethodKind::Inherent))];
const REQUIRED_METHOD: &[Policy] = &[Policy::Allow(Target::Method(MethodKind::Trait { body: false }))];

/// rustc's lang items by name, sorted by its bytes, with the target each must be applied to.
const LANG_ITEMS: &[(&str, &[Policy])] = &[
    ("AsyncGenFinished", ASSOC_CONST),
    ("AsyncGenPending", ASSOC_CONST),
    ("AsyncGenReady", INHERENT_METHOD),
    ("Break", VARIANT),
    ("CStr", STRUCT),
    ("Context", STRUCT),
    ("Continue", VARIANT),
    ("Err", VARIANT),
    ("From", TRAIT),
    ("None", VARIANT),
    ("Ok", VARIANT),
    ("Option", ENUM),
    ("Ordering", ENUM),
    ("Pending", VARIANT),
    ("Poll", ENUM),
    ("Range", STRUCT),
    ("RangeCopy", STRUCT),
    ("RangeFrom", STRUCT),
    ("RangeFromCopy", STRUCT),
    ("RangeFull", STRUCT),
    ("RangeInclusive", STRUCT),
    ("RangeInclusiveCopy", STRUCT),
    ("RangeMax", ASSOC_CONST),
    ("RangeMin", ASSOC_CONST),
    ("RangeSub", REQUIRED_METHOD),
    ("RangeTo", STRUCT),
    ("RangeToInclusive", STRUCT),
    ("RangeToInclusiveCopy", STRUCT),
    ("Ready", VARIANT),
    ("ResumeTy", STRUCT),
    ("Some", VARIANT),
    ("String", STRUCT),
    ("Try", TRAIT),
    ("add", TRAIT),
    ("add_assign", TRAIT),
    ("alloc_layout", STRUCT),
    ("async_drop", TRAIT),
    ("async_drop_in_place", FN),
    ("async_fn", TRAIT),
    ("async_fn_kind_helper", TRAIT),
    ("async_fn_kind_upvars", ASSOC_TY),
    ("async_fn_mut", TRAIT),
    ("async_fn_once", TRAIT),
    ("async_fn_once_output", ASSOC_TY),
    ("async_iterator", TRAIT),
    ("async_iterator_poll_next", REQUIRED_METHOD),
    ("begin_panic", FN),
    ("bikeshed_guaranteed_no_drop", TRAIT),
    ("bitand", TRAIT),
    ("bitand_assign", TRAIT),
    ("bitor", TRAIT),
    ("bitor_assign", TRAIT),
    ("bitxor", TRAIT),
    ("bitxor_assign", TRAIT),
    ("branch", REQUIRED_METHOD),
    ("c_void", ENUM),
    ("call_once_future", ASSOC_TY),
    ("call_ref_future", ASSOC_TY),
    ("clone", TRAIT),
    ("clone_fn", REQUIRED_METHOD),
    ("coerce_pointee_validated", TRAIT),
    ("coerce_shared", TRAIT),
    ("coerce_unsized", TRAIT),
    ("compiler_copy", FN),
    ("compiler_move", FN),
    ("const_panic_fmt", FN),
    ("const_param_ty", TRAIT),
    ("contract_build_check_ensures", FN),
    ("contract_check_ensures", FN),
    ("contract_check_requires", FN),
    ("copy", TRAIT),
    ("coroutine", TRAIT),
    ("coroutine_resume", REQUIRED_METHOD),
    ("coroutine_return", ASSOC_TY),
    ("coroutine_state", ENUM),
    ("coroutine_yield", ASSOC_TY),
    ("covariant_unsafe_cell", STRUCT),
    ("default_trait1", TRAIT),
    ("default_trait2", TRAIT),
    ("default_trait3", TRAIT),
    ("default_trait4", TRAIT),
    ("deref", TRAIT),
    ("deref_mut", TRAIT),
    ("deref_pure", TRAIT),
    ("deref_target", ASSOC_TY),
    ("destruct", TRAIT),
    ("discriminant_kind", TRAIT),
    ("discriminant_type", ASSOC_TY),
    ("dispatch_from_dyn", TRAIT),
    ("div", TRAIT),
    ("div_assign", TRAIT),
    ("drop", TRAIT),
    ("drop_glue", FN),
    ("dyn_metadata", STRUCT),
    ("eh_personality", FN),
    ("eq", TRAIT),
    ("field", TRAIT),
    ("field_base", ASSOC_TY),
    ("field_offset", ASSOC_CONST),
    ("field_representing_type", STRUCT),
    ("field_type", ASSOC_TY),
    ("fn", TRAIT),
    ("fn_mut", TRAIT),
    ("fn_once", TRAIT),
    ("fn_once_output", ASSOC_TY),
    ("fn_ptr_addr", REQUIRED_METHOD),
    ("fn_ptr_trait", TRAIT),
    ("format_argument", STRUCT),
    ("format_arguments", STRUCT),
    ("freeze", TRAIT),
    ("from_output", REQUIRED_METHOD),
    ("from_residual", REQUIRED_METHOD),
    ("from_yeet", FN),
    ("fused_iterator", TRAIT),
    ("future_output", ASSOC_TY),
    ("future_trait", TRAIT),
    ("get_context", FN),
    ("global_alloc_ty", STRUCT),
    ("index", TRAIT),
    ("index_mut", TRAIT),
    ("into_async_iter_into_iter", REQUIRED_METHOD),
    ("into_future", REQUIRED_METHOD),
    ("into_iter", REQUIRED_METHOD),
    ("into_try_type", FN),
    ("iterator", TRAIT),
    ("legacy_receiver", TRAIT),
    ("manually_drop", STRUCT),
    ("maybe_dangling", STRUCT),
    ("maybe_uninit", UNION),
    ("mem_align_const", ASSOC_CONST),
    ("mem_size_const", ASSOC_CONST),
    ("meta_sized", TRAIT),
    ("metadata_type", ASSOC_TY),
    ("mul", TRAIT),
    ("mul_assign", TRAIT),
    ("neg", TRAIT),
    ("new_unchecked", INHERENT_METHOD),
    ("next", REQUIRED_METHOD),
    ("not", TRAIT),
    ("offset_of", FN),
    ("owned_box", STRUCT),
    ("panic", FN),
    ("panic_bounds_check", FN),
    ("panic_cannot_unwind", FN),
    ("panic_const_add_overflow", FN),
    ("panic_const_async_fn_resumed", FN),
    ("panic_const_async_fn_resumed_drop", FN),
    ("panic_const_async_fn_resumed_panic", FN),
    ("panic_const_async_gen_fn_resumed", FN),
    ("panic_const_async_gen_fn_resumed_drop", FN),
    ("panic_const_async_gen_fn_resumed_panic", FN),
    ("panic_const_coroutine_resumed", FN),
    ("panic_const_coroutine_resumed_drop", FN),
    ("panic_const_coroutine_resumed_panic", FN),
    ("panic_const_div_by_zero", FN),
    ("panic_const_div_overflow", FN),
    ("panic_const_gen_fn_none", FN),
    ("panic_const_gen_fn_none_drop", FN),
    ("panic_const_gen_fn_none_panic", FN),
    ("panic_const_mul_overflow", FN),
    ("panic_const_neg_overflow", FN),
    ("panic_const_rem_by_zero", FN),
    ("panic_const_rem_overflow", FN),
    ("panic_const_shl_overflow", FN),
    ("panic_const_shr_overflow", FN),
    ("panic_const_sub_overflow", FN),
    ("panic_display", FN),
    ("panic_fmt", FN),
    ("panic_impl", FOREIGN_FN),
    ("panic_in_cleanup", FN),
    ("panic_info", STRUCT),
    ("panic_invalid_enum_construction", FN),
    ("panic_location", STRUCT),
    ("panic_misaligned_pointer_dereference", FN),
    ("panic_nounwind", FN),
    ("panic_null_pointer_dereference", FN),
    ("panic_null_reference_constructed", FN),
    ("partial_ord", TRAIT),
    ("phantom_data", STRUCT),
    ("pin", STRUCT),
    ("pointee_sized", TRAIT),
    ("pointee_trait", TRAIT),
    ("poll", REQUIRED_METHOD),
    ("range_inclusive_new", INHERENT_METHOD),
    ("reborrow", TRAIT),
    ("receiver", TRAIT),
    ("receiver_target", ASSOC_TY),
    ("rem", TRAIT),
    ("rem_assign", TRAIT),
    ("shl", TRAIT),
    ("shl_assign", TRAIT),
    ("shr", TRAIT),
    ("shr_assign", TRAIT),
    ("sized", TRAIT),
    ("slice_len_fn", INHERENT_METHOD),
    ("start", FN),
    ("structural_peq", TRAIT),
    ("sub", TRAIT),
    ("sub_assign", TRAIT),
    ("sync", TRAIT),
    ("termination", TRAIT),
    ("transmute_opts", STRUCT),
    ("transmute_trait", TRAIT),
    ("trivial_clone", TRAIT),
    ("try_as_dyn", TRAIT),
    ("tuple_trait", TRAIT),
    ("type_id", STRUCT),
    ("type_info", STRUCT),
    ("unpin", TRAIT),
    ("unsafe_cell", STRUCT),
    ("unsafe_pinned", STRUCT),
    ("unsafe_unpin", TRAIT),
    ("unsize", TRAIT),
    ("use_cloned", TRAIT),
    ("va_arg_safe", TRAIT),
    ("va_list", STRUCT),
];

/// `#[path = ".."]` of a string, made `Parsed::Other(name)`.
fn string(cx: &mut Accept<'_, '_, '_>, args: &ArgParser, name: &'static str) -> Option<Parsed> {
    let nv = cx.expect_name_value(args, cx.attr_range(), None)?;
    cx.expect_string_literal(nv)?;
    Some(Parsed::Other(name))
}

/// `AttributeRequiresOpt`: the incremental tests' attributes need `-Z query-dep-graph`, which
/// blaze never builds with.
fn requires_query_dep_graph(cx: &mut Accept<'_, '_, '_>) {
    let diag = cx.err(None, "attribute requires -Z query-dep-graph to be enabled", cx.attr_range());
    cx.emit_err(diag);
}

/// `RustcCleanParser::extend`.
fn clean(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) {
    requires_query_dep_graph(cx);
    let Some(list) = cx.expect_list(args, cx.attr_range()) else { return };
    let (mut cfg, mut except, mut loaded_from_disk) = (false, false, false);
    for item in list.mixed() {
        let Some((ident, value)) = cx.expect_name_value_item(item, None) else { continue };
        if cx.expect_string_literal(value).is_none() {
            continue;
        }
        let seen = match ident.name.as_str() {
            "cfg" => &mut cfg,
            "except" => &mut except,
            "loaded_from_disk" => &mut loaded_from_disk,
            _ => {
                cx.expected_specific_argument(ident.range, &["cfg", "except", "loaded_from_disk"]);
                continue;
            }
        };
        if *seen {
            cx.duplicate_key(item.range(), &ident.name);
        } else {
            *seen = true;
        }
    }
    if !cfg {
        cx.expected_specific_argument(list.range, &["cfg"]);
    }
}

/// `RustcIfThisChangedParser::convert`.
fn if_this_changed(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    requires_query_dep_graph(cx);
    match args {
        ArgParser::NoArgs => Some(Parsed::Other("RustcIfThisChanged")),
        ArgParser::List(list) => {
            let item = cx.expect_single(list)?;
            if item.meta_item_no_args().and_then(MetaItem::ident).is_none() {
                cx.expected_identifier(item.range());
                return None;
            }
            Some(Parsed::Other("RustcIfThisChanged"))
        }
        ArgParser::NameValue(_) => {
            cx.expected_list_or_no_args(cx.inner_range());
            None
        }
    }
}

/// `RustcThenThisWouldNeedParser::extend`.
fn then_this_would_need(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) {
    requires_query_dep_graph(cx);
    let Some(item) = cx.expect_single_element_list(args, cx.attr_range()) else { return };
    if item.meta_item_no_args().and_then(MetaItem::ident).is_none() {
        cx.expected_identifier(item.range());
    }
}

/// `parse_cgu_fields`: `cfg = ".."` and `module = ".."`, and `kind = ".."` where it is taken.
fn cgu_fields(cx: &mut Accept<'_, '_, '_>, args: &ArgParser, accepts_kind: bool) {
    let Some(list) = cx.expect_list(args, cx.attr_range()) else { return };
    let mut cfg: Option<(String, TextRange)> = None;
    let mut module: Option<(String, TextRange)> = None;
    let mut kind: Option<(String, TextRange)> = None;
    for arg in list.mixed() {
        let Some((ident, nv)) = cx.expect_name_value_item(arg, None) else { continue };
        let slot = match ident.name.as_str() {
            "cfg" => &mut cfg,
            "module" => &mut module,
            "kind" if accepts_kind => &mut kind,
            _ => {
                let possibilities: &[&str] = if accepts_kind { &["cfg", "module", "kind"] } else { &["cfg", "module"] };
                cx.expected_specific_argument(ident.range, possibilities);
                continue;
            }
        };
        let Some(value) = cx.expect_string_literal(nv) else { return };
        if slot.is_some() {
            cx.duplicate_key(TextRange::new(ident.range.start(), nv.args_range().end()), &ident.name);
            continue;
        }
        *slot = Some((value, nv.value_range()));
    }
    if cfg.is_none() {
        cgu_field_missing(cx, list.range, "cfg");
        return;
    }
    if module.is_none() {
        cgu_field_missing(cx, list.range, "module");
        return;
    }
    match kind {
        Some((kind, range)) => {
            if !matches!(kind.as_str(), "no" | "pre-lto" | "post-lto" | "any") {
                cx.expected_specific_argument_strings(range, &["no", "pre-lto", "post-lto", "any"]);
            }
        }
        None if accepts_kind => cgu_field_missing(cx, list.range, "kind"),
        None => {}
    }
}

/// `CguFieldsMissing`, at the list; the `)` in it is rustc's.
fn cgu_field_missing(cx: &mut Accept<'_, '_, '_>, list: TextRange, field: &str) {
    // alloc: the error's message
    let diag = cx.err(None, format!("`#[{})]` is missing a `{field}` argument", cx.name()), list);
    cx.emit_err(diag);
}

/// `RustcDeprecatedSafe2024Parser::convert`.
fn deprecated_safe_2024(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let single = cx.expect_single_element_list(args, cx.attr_range())?;
    let (path, arg) = cx.expect_name_value_item(single, None)?;
    if path.name != "audit_that" {
        cx.expected_specific_argument(path.range, &["audit_that"]);
        return None;
    }
    cx.expect_string_literal(arg)?;
    Some(Parsed::Other("RustcDeprecatedSafe2024"))
}

/// `RustcLegacyConstGenericsParser::convert`: each index an unsuffixed integer.
fn legacy_const_generics(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let list = cx.expect_list(args, cx.attr_range())?;
    let mut indices = Vec::with_capacity(list.len());
    let mut errored = false;
    for item in list.mixed() {
        match item.as_lit().filter(|it| it.suffix.is_none()).and_then(Lit::int) {
            Some(index) => indices.push((index as usize, item.range())),
            None => {
                cx.expected_integer_literal(item.range());
                errored = true;
            }
        }
    }
    if errored {
        return None;
    }
    if indices.is_empty() {
        cx.expected_at_least_one_argument(list.range);
        return None;
    }
    Some(Parsed::RustcLegacyConstGenerics(indices))
}

/// `RustcLintOptDenyFieldAccessParser::convert`: `(".. message ..")`.
fn lint_opt_deny_field_access(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let arg = cx.expect_single_element_list(args, cx.attr_range())?;
    cx.expect_string_literal_item(arg)?;
    Some(Parsed::Other("RustcLintOptDenyFieldAccess"))
}

/// `RustcMirParser::extend`: what rustc says of the arguments it knows; it skips the rest.
fn mir(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) {
    let Some(list) = cx.expect_list(args, cx.attr_range()) else { return };
    for item in list.mixed().filter_map(MetaItemOrLit::meta_item) {
        let Some(ident) = item.ident() else { continue };
        match ident.name.as_str() {
            "borrowck_graphviz_postflow" => {
                let Some(nv) = cx.expect_name_value(&item.args, item.range(), Some("borrowck_graphviz_postflow")) else {
                    continue;
                };
                let Some(path) = cx.expect_string_literal(nv) else { continue };
                if std::path::Path::new(&path).file_name().is_none() {
                    cx.expected_filename_literal(nv.value_range());
                }
            }
            "borrowck_graphviz_format" => {
                let Some(nv) = cx.expect_name_value(&item.args, item.range(), Some("borrowck_graphviz_format")) else {
                    continue;
                };
                match nv.value_as_str() {
                    None => cx.expected_identifier(nv.value_range()),
                    Some("two_phase") => {}
                    Some(_) => cx.expected_specific_argument(nv.value_range(), &["two_phase"]),
                }
            }
            _ => {}
        }
    }
}

/// `RustcMustImplementOneOfParser::convert`: two or more names of one segment.
fn must_implement_one_of(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let list = cx.expect_list(args, cx.attr_range())?;
    if list.len() < 2 {
        cx.expected_list_with_num_args_or_more(2, list.range);
        return None;
    }
    let mut names = Vec::with_capacity(list.len());
    let mut errored = false;
    for argument in list.mixed() {
        let Some(meta) = argument.meta_item_no_args() else {
            cx.expected_identifier(argument.range());
            return None;
        };
        let Some(ident) = meta.ident() else {
            let diag = cx.err(None, "must be a name of an associated function", meta.range());
            cx.emit_err(diag);
            errored = true;
            continue;
        };
        // alloc: the names the parsed `#[rustc_must_implement_one_of]` keeps
        names.push((ident.name.to_string(), ident.range));
    }
    if errored {
        return None;
    }
    Some(Parsed::RustcMustImplementOneOf(names))
}

/// `RustcNeverTypeOptionsParser::convert`.
fn never_type_options(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let list = cx.expect_list(args, cx.attr_range())?;
    let mut fallback: Option<(String, TextRange)> = None;
    let mut diverging_block_default: Option<(String, TextRange)> = None;
    for arg in list.mixed() {
        let Some((ident, nv)) = cx.expect_name_value_item(arg, None) else { continue };
        let slot = match ident.name.as_str() {
            "fallback" => &mut fallback,
            "diverging_block_default" => &mut diverging_block_default,
            _ => {
                cx.expected_specific_argument(ident.range, &["fallback", "diverging_block_default"]);
                continue;
            }
        };
        let field = cx.expect_string_literal(nv)?;
        if slot.is_some() {
            cx.duplicate_key(ident.range, &ident.name);
            continue;
        }
        *slot = Some((field, nv.value_range()));
    }
    if let Some((fallback, range)) = &fallback
        && !matches!(fallback.as_str(), "unit" | "never" | "no")
    {
        cx.expected_specific_argument_strings(*range, &["unit", "never", "no"]);
        return None;
    }
    if let Some((default, range)) = &diverging_block_default
        && !matches!(default.as_str(), "unit" | "never")
    {
        // rustc names "no", which it does not take, and not "never", which it does.
        cx.expected_specific_argument_strings(*range, &["unit", "no"]);
        return None;
    }
    Some(Parsed::Other("RustcNeverTypeOptions"))
}

/// `RustcScalableVectorParser::convert`: no arguments, or an element count that fits a `u16`.
fn scalable_vector(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    if args.as_no_args().is_ok() {
        return Some(Parsed::Other("RustcScalableVector"));
    }
    let count = single_integer(cx, args)?;
    if u16::try_from(count).is_err() {
        let mut diag =
            // alloc: the error's message
            cx.err(None, format!("element count in `rustc_scalable_vector` is too large: `{count}`"), cx.attr_range());
        diag.note("the value may not exceed `u16::MAX`");
        cx.emit_err(diag);
        return None;
    }
    Some(Parsed::Other("RustcScalableVector"))
}

/// `RustcSimdMonomorphizeLaneLimitParser::convert`: `= "N"`, a limit.
fn simd_lane_limit(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let nv = cx.expect_name_value(args, cx.attr_range(), None)?;
    limit(cx, nv)?;
    Some(Parsed::Other("RustcSimdMonomorphizeLaneLimit"))
}

#[cfg(test)]
mod tests {
    use super::{LANG_ITEMS, PATHS, Policy, Target, is_weak, lang_item};

    #[test]
    fn paths_are_sorted_and_distinct() {
        assert!(PATHS.windows(2).all(|it| it[0] < it[1]));
    }

    #[test]
    fn lang_items_are_sorted_by_their_bytes() {
        assert!(LANG_ITEMS.windows(2).all(|it| it[0].0.as_bytes() < it[1].0.as_bytes()));
        assert_eq!(LANG_ITEMS.len(), 216);
    }

    #[test]
    fn a_lang_item_names_its_target() {
        assert_eq!(lang_item("owned_box").map(|it| it.1), Some(&[Policy::Allow(Target::Struct)][..]));
        assert_eq!(lang_item("fn").map(|it| it.1), Some(&[Policy::Allow(Target::Trait)][..]));
        assert_eq!(lang_item("Some").map(|it| it.1), Some(&[Policy::Allow(Target::Variant)][..]));
        assert_eq!(lang_item("panic_impl").map(|it| it.1), Some(&[Policy::Allow(Target::ForeignFn)][..]));
        assert_eq!(lang_item("cookie").map(|it| it.1), None);
        assert_eq!(lang_item("Fn").map(|it| it.1), None);
    }

    #[test]
    fn only_the_linked_lang_items_are_weak() {
        assert!(is_weak("panic_impl"));
        assert!(is_weak("eh_personality"));
        assert!(!is_weak("copy"));
    }
}
