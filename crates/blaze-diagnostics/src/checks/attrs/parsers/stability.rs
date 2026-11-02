//! The errors of the parsers of stability attributes (`#[stable]`, `#[unstable]`,
//! `#[rustc_const_stable]`, `#[rustc_const_unstable]`, `#[rustc_promotable]`,
//! `#[rustc_default_body_unstable]`, `#[rustc_allowed_through_unstable_modules]`,
//! `#![unstable_removed]`), of `#[deprecated]`, the feature lists of
//! `#[allow_internal_unstable]`, `#[unstable_feature_bound]` and
//! `#[rustc_allow_const_fn_unstable]`, `#[rustc_confusables]`, `#[rustc_macro_transparency]`,
//! `#[rustc_skip_during_method_dispatch]`, `#[must_not_suspend]`, the `rustc_dump_*` attributes
//! and the marker attributes of no arguments; and ([`crate_checks`]) the checks rustc's
//! stability pass makes of what they parse to: a `#[deprecated]` without a stability, a
//! stability where one is useless, a stability after a deprecation, const stability on what is
//! not a `const fn`.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/` `stability.rs`,
//! `deprecation.rs`, `allow_unstable.rs`, `confusables.rs`, `lint_helpers.rs`, `traits.rs`,
//! `transparency.rs`, `semantics.rs`, `pin_v2.rs`, `dummy.rs`, `must_not_suspend.rs`,
//! `no_implicit_prelude.rs`, `rustc_dump.rs` and `macro_attrs.rs` (`check_macro_only`), with
//! the texts of `compiler/rustc_attr_parsing/src/session_diagnostics.rs`;
//! `compiler/rustc_ast_lowering/src/item.rs` (`lower_constness`) with the text of
//! `compiler/rustc_ast_lowering/src/diagnostics.rs` (`ConstComptimeFn`);
//! `compiler/rustc_passes/src/stability.rs` (`lookup_stability`, `lookup_deprecation_entry`,
//! `annotation_kind`, `check_compatible_stability`, `MissingStabilityAnnotations`) with the
//! texts of
//! `compiler/rustc_passes/src/diagnostics.rs`; and `compiler/rustc_ast/src/util/literal.rs`
//! (`escape_string_symbol`, `LitKind`'s `Display`) (MIT or Apache-2.0).

use std::{
    cell::{OnceCell, RefCell},
    num::{IntErrorKind, NonZeroU32},
};

use blaze_hash::{HashMap, HashSet};
use blaze_ids::radix;
use cfg::CfgAtom;
use either::Either;
use hir::{HirFileId, InFile, ModuleDef};
use hir_def::{Lookup as _, UseId};
use hir_expand::MacroCallKind;
use intern::sym;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use super::{Accepted, Finalize, GroupId, no_args, parse_version};
use crate::{
    checks::attrs::{
        Lowered,
        accept::{Accept, Session},
        args::{ArgParser, Ident, Lit, MetaItem},
        is,
        parsed::{
            DeprecatedSince, Parsed, ParsedAttr, Repr, RustcVersion, Stability, StabilityLevel,
            StableSince,
        },
    },
    cx::{Cx, def_range_view as def_range, signature_range_view as signature_range},
    lints::{
        Effective, EffectiveDef, def_span,
        syntax::attrs::{
            Owner, Target,
            features::table as features,
            target::{GenericParamKind, MethodKind},
        },
    },
    model::{Applicability, Diag},
};

pub(crate) const PATHS: &[&[&str]] = &[
    &["allow_internal_unstable"],
    &["automatically_derived"],
    &["deprecated"],
    &["fundamental"],
    &["marker"],
    &["may_dangle"],
    &["must_not_suspend"],
    &["no_implicit_prelude"],
    &["pin_v2"],
    &["rustc_allow_const_fn_unstable"],
    &["rustc_allow_incoherent_impl"],
    &["rustc_allowed_through_unstable_modules"],
    &["rustc_as_ptr"],
    &["rustc_coinductive"],
    &["rustc_comptime"],
    &["rustc_confusables"],
    &["rustc_const_stable"],
    &["rustc_const_stable_indirect"],
    &["rustc_const_unstable"],
    &["rustc_default_body_unstable"],
    &["rustc_deny_explicit_impl"],
    &["rustc_dummy"],
    &["rustc_dump_def_parents"],
    &["rustc_dump_def_path"],
    &["rustc_dump_generics"],
    &["rustc_dump_hidden_type_of_opaques"],
    &["rustc_dump_inferred_outlives"],
    &["rustc_dump_item_bounds"],
    &["rustc_dump_layout"],
    &["rustc_dump_object_lifetime_defaults"],
    &["rustc_dump_predicates"],
    &["rustc_dump_symbol_name"],
    &["rustc_dump_user_args"],
    &["rustc_dump_variances"],
    &["rustc_dump_variances_of_opaques"],
    &["rustc_dump_vtable"],
    &["rustc_dyn_incompatible_trait"],
    &["rustc_macro_transparency"],
    &["rustc_paren_sugar"],
    &["rustc_pass_by_value"],
    &["rustc_promotable"],
    &["rustc_pub_transparent"],
    &["rustc_should_not_be_called_on_const_items"],
    &["rustc_skip_during_method_dispatch"],
    &["rustc_specialization_trait"],
    &["rustc_unsafe_specialization_marker"],
    &["stable"],
    &["unstable"],
    &["unstable_feature_bound"],
    &["unstable_removed"],
];

/// rustc's `VERSION_PLACEHOLDER`: the version a feature is stabilized in before it is known.
const VERSION_PLACEHOLDER: &str = "CURRENT_RUSTC_VERSION";

/// `RustcVersion::CURRENT` of the pinned nightly (`harness/toolchain`, `nightly-2026-10-27`):
/// what `VERSION_PLACEHOLDER` stands for.
const CURRENT: RustcVersion = (1, 101, 0);

/// The state of the parsers of several attributes, of one node.
#[derive(Default)]
pub(crate) struct State {
    /// `StabilityParser`: the first `#[stable]` or `#[unstable]` it took.
    stability: Option<Stability>,
    allowed_through_unstable_modules: bool,
    /// `BodyStabilityParser`.
    body_stability: Option<Stability>,
    /// `ConstStabilityParser`.
    const_stability: Option<Stability>,
    promotable: bool,
    /// `ConfusablesParser`: whether it has a name.
    confusables: bool,
}

pub(crate) fn accept(state: &mut State, cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Accepted {
    let other = |name| Parsed::Other(name);
    let path: &'static [&'static str] = cx.builtin.path;
    match path {
        [name @ ("stable" | "unstable")] => {
            if state.stability.is_some() {
                multiple_levels(cx);
            } else if let Some((feature, level)) = match *name {
                "stable" => parse_stability(cx, args),
                _ => parse_unstability(cx, args),
            } {
                state.stability = Some(Stability { feature, level, attr: cx.index });
            }
            Accepted::Group("stability")
        }
        ["rustc_allowed_through_unstable_modules"] => {
            if let Some(nv) = cx.expect_name_value(args, cx.attr_range(), None)
                && cx.expect_string_literal(nv).is_some()
            {
                state.allowed_through_unstable_modules = true;
            }
            Accepted::Group("stability")
        }
        ["rustc_default_body_unstable"] => {
            if state.body_stability.is_some() {
                multiple_levels(cx);
            } else if let Some((feature, level)) = parse_unstability(cx, args) {
                state.body_stability = Some(Stability { feature, level, attr: cx.index });
            }
            Accepted::Group("body_stability")
        }
        [name @ ("rustc_const_stable" | "rustc_const_unstable")] => {
            if state.const_stability.is_some() {
                multiple_levels(cx);
            } else if let Some((feature, level)) = match *name {
                "rustc_const_stable" => parse_stability(cx, args),
                _ => parse_unstability(cx, args),
            } {
                state.const_stability = Some(Stability { feature, level, attr: cx.index });
            }
            Accepted::Group("const_stability")
        }
        ["rustc_promotable"] => {
            state.promotable = true;
            Accepted::Group("const_stability")
        }
        ["rustc_const_stable_indirect"] => no_args(cx, args, Parsed::RustcConstStableIndirect),
        ["unstable_removed"] => {
            unstable_removed(cx, args);
            Accepted::Group("unstable_removed")
        }
        ["deprecated"] => Accepted::Single(deprecated(cx, args)),
        [name @ ("allow_internal_unstable" | "unstable_feature_bound" | "rustc_allow_const_fn_unstable")] => {
            feature_list(cx, args, name);
            Accepted::Group(match *name {
                "allow_internal_unstable" => "allow_internal_unstable",
                "unstable_feature_bound" => "unstable_feature_bound",
                _ => "rustc_allow_const_fn_unstable",
            })
        }
        ["rustc_confusables"] => {
            state.confusables |= confusables(cx, args);
            Accepted::Group("confusables")
        }
        ["rustc_as_ptr"] => no_args(cx, args, other("RustcAsPtr")),
        ["rustc_pub_transparent"] => no_args(cx, args, other("RustcPubTransparent")),
        ["rustc_pass_by_value"] => no_args(cx, args, other("RustcPassByValue")),
        ["rustc_should_not_be_called_on_const_items"] => {
            no_args(cx, args, other("RustcShouldNotBeCalledOnConstItems"))
        }
        ["automatically_derived"] => no_args(cx, args, other("AutomaticallyDerived")),
        ["rustc_skip_during_method_dispatch"] => Accepted::Single(skip_during_method_dispatch(cx, args)),
        ["rustc_paren_sugar"] => no_args(cx, args, other("RustcParenSugar")),
        ["marker"] => no_args(cx, args, other("Marker")),
        ["rustc_deny_explicit_impl"] => no_args(cx, args, other("RustcDenyExplicitImpl")),
        ["rustc_dyn_incompatible_trait"] => no_args(cx, args, other("RustcDynIncompatibleTrait")),
        ["rustc_specialization_trait"] => no_args(cx, args, other("RustcSpecializationTrait")),
        ["rustc_unsafe_specialization_marker"] => no_args(cx, args, other("RustcUnsafeSpecializationMarker")),
        ["rustc_coinductive"] => no_args(cx, args, other("RustcCoinductive")),
        ["rustc_allow_incoherent_impl"] => no_args(cx, args, other("RustcAllowIncoherentImpl")),
        ["fundamental"] => no_args(cx, args, other("Fundamental")),
        ["rustc_macro_transparency"] => Accepted::Single(macro_transparency(cx, args)),
        ["may_dangle"] => no_args(cx, args, Parsed::MayDangle),
        ["rustc_comptime"] => no_args(cx, args, other("RustcComptime")),
        ["pin_v2"] => no_args(cx, args, other("PinV2")),
        ["rustc_dummy"] => Accepted::Single(Some(other("RustcDummy"))),
        ["must_not_suspend"] => Accepted::Single(must_not_suspend(cx, args)),
        ["no_implicit_prelude"] => no_args(cx, args, other("NoImplicitPrelude")),
        ["rustc_dump_user_args"] => no_args(cx, args, other("RustcDumpUserArgs")),
        ["rustc_dump_def_parents"] => no_args(cx, args, other("RustcDumpDefParents")),
        ["rustc_dump_def_path"] => Accepted::Single(cx.expect_no_args(args).map(|()| other("RustcDumpDefPath"))),
        ["rustc_dump_generics"] => no_args(cx, args, other("RustcDumpGenerics")),
        ["rustc_dump_hidden_type_of_opaques"] => no_args(cx, args, other("RustcDumpHiddenTypeOfOpaques")),
        ["rustc_dump_inferred_outlives"] => no_args(cx, args, other("RustcDumpInferredOutlives")),
        ["rustc_dump_item_bounds"] => no_args(cx, args, other("RustcDumpItemBounds")),
        ["rustc_dump_layout"] => {
            dump_layout(cx, args);
            Accepted::Group("rustc_dump_layout")
        }
        ["rustc_dump_object_lifetime_defaults"] => no_args(cx, args, Parsed::RustcDumpObjectLifetimeDefaults),
        ["rustc_dump_predicates"] => no_args(cx, args, other("RustcDumpPredicates")),
        ["rustc_dump_symbol_name"] => {
            Accepted::Single(cx.expect_no_args(args).map(|()| other("RustcDumpSymbolName")))
        }
        ["rustc_dump_variances"] => no_args(cx, args, other("RustcDumpVariances")),
        ["rustc_dump_variances_of_opaques"] => no_args(cx, args, other("RustcDumpVariancesOfOpaques")),
        ["rustc_dump_vtable"] => no_args(cx, args, other("RustcDumpVtable")),
        path => unreachable!("{path:?} is not a stability attribute"),
    }
}

pub(crate) fn finalize(state: &mut State, group: &'static str, fx: &mut Finalize<'_, '_, '_>) -> Option<Parsed> {
    match group {
        "stability" => {
            let stable = matches!(&state.stability, Some(Stability { level: StabilityLevel::Stable(_), .. }));
            if state.allowed_through_unstable_modules && !stable {
                let diag = Diag::error(
                    Some("E0789"),
                    "`rustc_allowed_through_unstable_modules` attribute must be paired with a `stable` attribute",
                    fx.target_span(),
                );
                fx.emit_err(diag);
            }
            if stable {
                let bounds = fx.owner.attrs.iter().filter(|it| is(it, "unstable_feature_bound")).count();
                for _ in 0..bounds {
                    let mut diag =
                        Diag::error(None, "item annotated with `#[unstable_feature_bound]` should not be stable", fx.target_span());
                    diag.help("if this item is meant to be stable, do not use any functions annotated with `#[unstable_feature_bound]`. Otherwise, mark this item as unstable with `#[unstable]`");
                    fx.emit_err(diag);
                }
            }
            state.stability.take().map(Parsed::Stability)
        }
        "body_stability" => state.body_stability.take().map(Parsed::BodyStability),
        "const_stability" => {
            if state.promotable && state.const_stability.is_none() {
                let diag = Diag::error(
                    Some("E0717"),
                    "`rustc_promotable` attribute must be paired with either a `rustc_const_unstable` or a `rustc_const_stable` attribute",
                    fx.target_span(),
                );
                fx.emit_err(diag);
            }
            state.const_stability.take().map(Parsed::ConstStability)
        }
        "confusables" => state.confusables.then_some(Parsed::Other("RustcConfusables")),
        "allow_internal_unstable" => Some(Parsed::Other("AllowInternalUnstable")),
        "unstable_feature_bound" => Some(Parsed::Other("UnstableFeatureBound")),
        "rustc_allow_const_fn_unstable" => Some(Parsed::RustcAllowConstFnUnstable),
        "unstable_removed" => Some(Parsed::Other("UnstableRemoved")),
        "rustc_dump_layout" => Some(Parsed::Other("RustcDumpLayout")),
        group => unreachable!("{group} is not a stability parser group"),
    }
}

pub(crate) fn check(group: GroupId, first: usize, fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    match group {
        GroupId::Named("allow_internal_unstable") => {
            // `check_macro_only`: on a function only when it is a proc macro.
            if fx.target() == Target::Fn && !parsed.iter().any(|it| matches!(it.parsed, Parsed::ProcMacro(_))) {
                let mut diag = Diag::error(None, "attribute should be applied to a macro", fx.attr_span(first));
                diag.span_label(fx.target_span(), "not a macro");
                fx.emit_err(diag);
            }
        }
        GroupId::Single(["rustc_pub_transparent"]) => {
            let transparent = parsed.iter().any(|it| match &it.parsed {
                Parsed::Repr(reprs) => reprs.iter().any(|(repr, _)| *repr == Repr::Transparent),
                _ => false,
            });
            if !transparent {
                let mut diag =
                    Diag::error(None, "attribute should be applied to `#[repr(transparent)]` types", fx.attr_span(first));
                diag.span_label(fx.target_span(), "not a `#[repr(transparent)]` type");
                fx.emit_err(diag);
            }
        }
        GroupId::Single(["rustc_comptime"]) => const_comptime(fx, first),
        _ => {}
    }
}

/// `MultipleStabilityLevels`.
fn multiple_levels(cx: &mut Accept<'_, '_, '_>) {
    let diag = cx.err(Some("E0544"), "multiple stability levels", cx.attr_range());
    cx.emit_err(diag);
}

/// `insert_value_into_option_or_error`: `name = "value"` into `slot`, once.
fn insert(cx: &mut Accept<'_, '_, '_>, param: &MetaItem, slot: &mut Option<String>, name: &Ident) -> Option<()> {
    if slot.is_some() {
        cx.duplicate_key(name.range, &name.name);
        return None;
    }
    let (_, nv) = cx.expect_name_value_meta(param, Some(name.name.as_str()))?;
    let value = cx.expect_string_literal(nv)?;
    *slot = Some(value);
    Some(())
}

/// rustc's `rustc_lexer::is_ident`.
fn is_ident(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else { return false };
    (first == '_' || unicode_ident::is_xid_start(first)) && chars.all(unicode_ident::is_xid_continue)
}

/// `MissingFeature` or `NonIdentFeature` where the feature is not one.
fn checked_feature(cx: &mut Accept<'_, '_, '_>, feature: Option<String>) -> Option<String> {
    let message = match feature.as_deref() {
        Some(name) if is_ident(name) => None,
        Some(_) => Some("'feature' is not an identifier"),
        None => Some("missing 'feature'"),
    };
    let Some(message) = message else { return feature };
    let diag = cx.err(Some("E0546"), message, cx.attr_range());
    cx.emit_err(diag);
    None
}

/// `InvalidSince`.
fn invalid_since(cx: &mut Accept<'_, '_, '_>) {
    let diag = cx.err(None, "'since' must be a Rust version number, such as \"1.31.0\"", cx.attr_range());
    cx.emit_err(diag);
}

/// `parse_stability`: `#[stable]` and `#[rustc_const_stable]`.
fn parse_stability(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<(String, StabilityLevel)> {
    let mut feature = None;
    let mut since = None;
    let list = cx.expect_list(args, cx.attr_range())?;
    for param in list.mixed() {
        let Some(meta) = param.meta_item() else {
            cx.expected_not_literal(param.range());
            return None;
        };
        match meta.path.word() {
            Some(word) if word.name == "feature" => insert(cx, meta, &mut feature, word)?,
            Some(word) if word.name == "since" => insert(cx, meta, &mut since, word)?,
            _ => {
                cx.expected_specific_argument(param.range(), &["feature", "since"]);
                return None;
            }
        }
    }
    let feature = checked_feature(cx, feature);
    let since = match since {
        Some(since) if since == VERSION_PLACEHOLDER => StableSince::Current,
        Some(since) => match parse_version(&since) {
            Some(version) => StableSince::Version(version),
            None => {
                invalid_since(cx);
                StableSince::Err
            }
        },
        None => {
            let diag = cx.err(Some("E0542"), "missing 'since'", cx.attr_range());
            cx.emit_err(diag);
            StableSince::Err
        }
    };
    Some((feature?, StabilityLevel::Stable(since)))
}

/// `parse_unstability`: `#[unstable]`, `#[rustc_const_unstable]` and
/// `#[rustc_default_body_unstable]`.
fn parse_unstability(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<(String, StabilityLevel)> {
    let mut feature = None;
    let mut reason = None;
    let mut issue = None;
    let mut implied_by = None;
    let mut old_name = None;
    let list = cx.expect_list(args, cx.attr_range())?;
    for param in list.mixed() {
        let Some(meta) = param.meta_item() else {
            cx.expected_not_literal(param.range());
            return None;
        };
        let Some(word) = meta.path.word() else {
            cx.expected_specific_argument(meta.range(), &["feature", "reason", "issue", "implied_by", "old_name"]);
            return None;
        };
        match word.name.as_str() {
            "feature" => insert(cx, meta, &mut feature, word)?,
            "reason" => insert(cx, meta, &mut reason, word)?,
            "issue" => {
                insert(cx, meta, &mut issue, word)?;
                let value = issue.as_deref().unwrap_or_default();
                if value != "none"
                    && let Err(error) = value.parse::<NonZeroU32>()
                {
                    let mut diag = cx.err(Some("E0545"), "`issue` must be a non-zero numeric string or \"none\"", meta.range());
                    if let Some(nv) = meta.args.as_name_value()
                        && let Some(label) = issue_cause(error.kind())
                    {
                        diag.span_label(cx.span(nv.value_range()), label);
                    }
                    cx.emit_err(diag);
                    return None;
                }
            }
            "implied_by" => insert(cx, meta, &mut implied_by, word)?,
            "old_name" => insert(cx, meta, &mut old_name, word)?,
            _ => {
                cx.expected_specific_argument(meta.range(), &["feature", "reason", "issue", "implied_by", "old_name"]);
                return None;
            }
        }
    }
    let feature = checked_feature(cx, feature);
    if issue.is_none() {
        let diag = cx.err(Some("E0547"), "missing 'issue'", cx.attr_range());
        cx.emit_err(diag);
        return None;
    }
    let feature = feature?;
    if features::lang(&feature).is_some_and(|it| it.kind == features::Kind::Accepted) {
        let attr = cx.attr_span();
        // clone: the error's span
        let mut diag = Diag::error(None, "can't mark as unstable using an already stable feature", attr.clone());
        // clone: the error's span
        diag.span_label(attr.clone(), "this feature is already stable");
        diag.span_help(attr, "consider removing the attribute");
        diag.span_label(cx.target_span(), "the stability attribute annotates this item");
        cx.emit_err(diag);
        return None;
    }
    Some((feature, StabilityLevel::Unstable))
}

/// `InvalidIssueStringCause::from_int_error_kind`.
fn issue_cause(kind: &IntErrorKind) -> Option<&'static str> {
    Some(match kind {
        IntErrorKind::Empty => "cannot parse integer from empty string",
        IntErrorKind::InvalidDigit => "invalid digit found in string",
        IntErrorKind::PosOverflow => "number too large to fit in target type",
        IntErrorKind::NegOverflow => "number too small to fit in target type",
        IntErrorKind::Zero => "`issue` must not be \"0\", use \"none\" instead",
        _ => return None,
    })
}

/// `UnstableRemovedParser::extend`.
fn unstable_removed(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<()> {
    const KEYS: &[&str] = &["feature", "reason", "link", "since"];
    let mut feature = None;
    let mut reason = None;
    let mut link = None;
    let mut since = None;
    let list = cx.expect_list(args, cx.attr_range())?;
    for param in list.mixed() {
        let Some(meta) = param.meta_item() else {
            cx.expected_not_literal(param.range());
            return None;
        };
        let Some(word) = meta.path.word() else {
            cx.expected_specific_argument(meta.range(), KEYS);
            return None;
        };
        let slot = match word.name.as_str() {
            "feature" => &mut feature,
            "since" => &mut since,
            "reason" => &mut reason,
            "link" => &mut link,
            _ => {
                cx.expected_specific_argument(meta.range(), KEYS);
                return None;
            }
        };
        insert(cx, meta, slot, word)?;
    }
    for (value, key) in [(&feature, "feature"), (&reason, "reason"), (&link, "link")] {
        if value.is_none() {
            cx.missing_name_value(list.range, key);
            return None;
        }
    }
    let Some(since) = since else {
        cx.missing_name_value(list.range, "since");
        return None;
    };
    if parse_version(&since).is_none() {
        invalid_since(cx);
        return None;
    }
    Some(())
}

/// `parse_since` of `#[deprecated]`.
fn parse_since(since: &str, is_rustc: bool) -> DeprecatedSince {
    if since == "TBD" {
        DeprecatedSince::Future
    } else if !is_rustc {
        // alloc: the value the parsed `#[deprecated]` keeps
        DeprecatedSince::NonStandard(since.to_owned())
    } else if since == VERSION_PLACEHOLDER {
        DeprecatedSince::RustcVersion(CURRENT)
    } else if let Some(version) = parse_version(since) {
        DeprecatedSince::RustcVersion(version)
    } else {
        DeprecatedSince::Err
    }
}

/// `deprecation.rs`' `get`: the string of `name = ".."`, once.
fn get(cx: &mut Accept<'_, '_, '_>, name: &str, param: TextRange, args: &ArgParser, present: bool) -> Option<String> {
    if present {
        cx.duplicate_key(param, name);
        return None;
    }
    let nv = cx.expect_name_value(args, param, Some(name))?;
    cx.expect_string_literal(nv)
}

/// `DeprecatedParser::convert`.
fn deprecated(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let is_rustc = cx.feature("staged_api");
    let suggestions = cx.feature("deprecated_suggestion");
    let mut since: Option<String> = None;
    let mut note: Option<String> = None;
    let mut suggestion = false;
    match args {
        ArgParser::NoArgs => {}
        ArgParser::List(list) => {
            if let Some(elem) = list.as_single()
                && let Some(lit) = elem.as_lit()
                && let Some(text) = lit.str()
            {
                let shown = shown_str(cx, lit, text);
                match parse_since(text, true) {
                    DeprecatedSince::Future | DeprecatedSince::RustcVersion(_) => {
                        // alloc: the error's suggestion
                        cx.push_suggestion("try specifying a deprecated since version", elem.range(), format!("since = {shown}"));
                    }
                    _ => {
                        // alloc: the error's suggestion
                        cx.push_suggestion("try using `=` instead", list.range, format!(" = {shown}"));
                    }
                }
                cx.expected_not_literal(elem.range());
                return None;
            }
            for param in list.mixed() {
                let Some(meta) = param.meta_item() else {
                    cx.expected_not_literal(param.range());
                    return None;
                };
                match meta.path.word_sym() {
                    Some("since") => since = Some(get(cx, "since", meta.range(), &meta.args, since.is_some())?),
                    Some("note") => note = Some(get(cx, "note", meta.range(), &meta.args, note.is_some())?),
                    Some("suggestion") => {
                        if !suggestions {
                            let mut diag = cx.err(None, "suggestions on deprecated items are unstable", meta.range());
                            if cx.session.features.nightly {
                                diag.help("add `#![feature(deprecated_suggestion)]` to the crate root");
                            }
                            diag.note("see #94785 for more details");
                            cx.emit_err(diag);
                        }
                        get(cx, "suggestion", meta.range(), &meta.args, suggestion)?;
                        suggestion = true;
                    }
                    _ => {
                        let expected: &[&str] =
                            if suggestions { &["since", "note", "suggestion"] } else { &["since", "note"] };
                        cx.expected_specific_argument(param.range(), expected);
                        return None;
                    }
                }
            }
        }
        ArgParser::NameValue(nv) => {
            let Some(value) = nv.value_as_str() else {
                cx.expected_string_literal(nv.value_range(), Some(nv.value_as_lit()));
                return None;
            };
            // alloc: the value the parsed `#[deprecated]` keeps
            note = Some(value.to_owned());
        }
    }
    let since = match since {
        Some(since) => {
            let since = parse_since(&since, is_rustc);
            if since == DeprecatedSince::Err {
                invalid_since(cx);
            }
            since
        }
        None if is_rustc => {
            let diag = cx.err(Some("E0542"), "missing 'since'", cx.attr_range());
            cx.emit_err(diag);
            DeprecatedSince::Err
        }
        None => DeprecatedSince::Unspecified,
    };
    if is_rustc && note.is_none() {
        let diag = cx.err(Some("E0543"), "missing 'note'", cx.attr_range());
        cx.emit_err(diag);
        return None;
    }
    Some(Parsed::Deprecated(since))
}

/// A string literal as rustc prints its `LitKind`: raw as it is written, else quoted with
/// `escape_string_symbol`'s escapes.
fn shown_str(cx: &Accept<'_, '_, '_>, lit: &Lit, value: &str) -> String {
    let written = cx.cx.snippet(&cx.span(lit.range)).unwrap_or_default();
    match written.strip_prefix('r') {
        Some(raw) => {
            let delim = "#".repeat(raw.chars().take_while(|it| *it == '#').count());
            // alloc: the error's suggestion
            format!("r{delim}\"{value}\"{delim}")
        }
        // alloc: the error's suggestion
        None => format!("\"{}\"", escape_string(value)),
    }
}

/// rustc's `escape_string_symbol`.
fn escape_string(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            '\n' => escaped.push_str("\\n"),
            '\\' => escaped.push_str("\\\\"),
            '\'' => escaped.push_str("\\'"),
            '"' => escaped.push_str("\\\""),
            '\x20'..='\x7e' => escaped.push(c),
            // alloc: the error's suggestion
            c => escaped.push_str(&format!("\\u{{{:x}}}", c as u32)),
        }
    }
    escaped
}

/// `parse_unstable`: the feature names of `#[allow_internal_unstable(..)]`,
/// `#[unstable_feature_bound(..)]` and `#[rustc_allow_const_fn_unstable(..)]`.
fn feature_list(cx: &mut Accept<'_, '_, '_>, args: &ArgParser, name: &str) {
    let Some(list) = args.as_list() else {
        // alloc: the error's message
        let diag = cx.err(None, format!("`{name}` expects a list of feature names"), cx.attr_range());
        cx.emit_err(diag);
        return;
    };
    for param in list.mixed() {
        if param.meta_item_no_args().and_then(|it| it.path.word()).is_none() {
            // alloc: the error's message
            let diag = cx.err(None, format!("`{name}` expects feature names"), param.range());
            cx.emit_err(diag);
        }
    }
}

/// `ConfusablesParser`'s accept: whether it took a name.
fn confusables(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> bool {
    let Some(list) = cx.expect_list(args, cx.attr_range()) else { return false };
    if list.is_empty() {
        let diag = cx.err(None, "expected at least one confusable name", cx.attr_range());
        cx.emit_err(diag);
    }
    let mut any = false;
    for param in list.mixed() {
        any |= cx.expect_string_literal_item(param).is_some();
    }
    any
}

fn skip_during_method_dispatch(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let list = cx.expect_list(args, cx.attr_range())?;
    if list.is_empty() {
        cx.expected_at_least_one_argument(list.range);
        return None;
    }
    let (mut array, mut boxed_slice) = (false, false);
    for arg in list.mixed() {
        let Some(meta) = arg.meta_item() else {
            cx.expected_not_literal(arg.range());
            continue;
        };
        let _ = cx.expect_no_args(&meta.args);
        let (key, skip) = match meta.path.word_sym() {
            Some("array") => ("array", &mut array),
            Some("boxed_slice") => ("boxed_slice", &mut boxed_slice),
            _ => {
                cx.expected_specific_argument(meta.path.range, &["array", "boxed_slice"]);
                continue;
            }
        };
        if std::mem::replace(skip, true) {
            cx.duplicate_key(meta.range(), key);
        }
    }
    Some(Parsed::Other("RustcSkipDuringMethodDispatch"))
}

fn macro_transparency(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    let nv = cx.expect_name_value(args, cx.attr_range(), None)?;
    match nv.value_as_str()? {
        "transparent" | "semiopaque" | "opaque" => Some(Parsed::Other("RustcMacroTransparency")),
        _ => {
            cx.expected_specific_argument_strings(nv.value_range(), &["transparent", "semiopaque", "opaque"]);
            None
        }
    }
}

fn must_not_suspend(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<Parsed> {
    match args {
        ArgParser::NameValue(reason) => {
            let _ = cx.expect_string_literal(reason);
        }
        ArgParser::NoArgs => {}
        ArgParser::List(list) => {
            cx.expected_nv_or_no_args(list.range);
            return None;
        }
    }
    Some(Parsed::Other("MustNotSupend"))
}

/// `RustcDumpLayoutParser::extend`.
fn dump_layout(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) {
    let Some(list) = cx.expect_list(args, cx.attr_range()) else { return };
    for item in list.mixed() {
        let Some(arg) = item.meta_item_no_args() else {
            cx.expected_not_literal(item.range());
            continue;
        };
        let Some(ident) = arg.ident() else {
            cx.expected_identifier(arg.range());
            return;
        };
        match ident.name.as_str() {
            "align" | "backend_repr" | "debug" | "homogeneous_aggregate" | "largest_niche" | "size" => {}
            _ => cx.expected_specific_argument(ident.range, &["align", "backend_repr", "debug", "homogeneous_aggregate", "size"]),
        }
    }
}

/// `lower_constness`' `ConstComptimeFn`: `#[rustc_comptime]` on a `const fn`.
fn const_comptime(fx: &mut Finalize<'_, '_, '_>, attr: usize) {
    let node = fx.owner.node.value;
    let keyword = match ast::Fn::cast(node) {
        Some(function) => function.const_token(),
        None => ast::Impl::cast(node).and_then(|it| it.const_token()),
    };
    let Some(keyword) = keyword else { return };
    let span = fx.cx.span(InFile::new(fx.owner.node.file_id, keyword.text_range()));
    // clone: the error's span
    let mut diag = Diag::error(None, "a function cannot be both `comptime` and `const`", span.clone());
    // clone: the error's span
    diag.suggestion(span.clone(), "remove the `const`", "", Applicability::MachineApplicable);
    diag.span_note(span, "`const` implies the function can be called at runtime, too");
    diag.span_label(fx.attr_span(attr), "`comptime` because of this");
    fx.emit_err(diag);
}

/// The errors rustc's stability pass (`MissingStabilityAnnotations`) gives of the crate's
/// definitions, each with the node it is of, a module's after another: of each,
/// `check_compatible_stability` of its parsed stability attributes (E0549, `UselessStability`,
/// `CannotStabilizeDeprecated`, `MissingConstErr`, `ConstStableNotStable`,
/// `RustcConstStableIndirectPairing`), then `check_missing_stability` and
/// `check_missing_const_stability` (`MissingStabilityAttr`, `MissingConstStabAttr`), the crate
/// root's missing stability first. `parsed` is what each of `owners` is parsed to (nothing for
/// a macro call). Only a crate with `#![feature(staged_api)]` has them.
pub(crate) fn crate_checks<'db>(
    cx: &Cx<'_, 'db>,
    session: &Session,
    owners: &[Owner<'db>],
    parsed: &Lowered<'_>,
) -> Vec<(InFile<SyntaxNodePtr>, Diag)> {
    if !session.feature("staged_api") {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    // alloc: empty, allocates nothing
    let Some(root) = cx.krate.root_module(cx.db).as_source_file_id(cx.db) else { return Vec::new() };
    let crate_root = HirFileId::from(root);
    let mut defs = Defs::new(cx, owners, parsed, crate_root);
    let missing = Missing::new(cx);
    // alloc: only a `staged_api` crate (the standard library) gets here, once; it holds errors
    let mut keyed: Vec<((usize, u8), InFile<SyntaxNodePtr>, Diag)> = Vec::new();
    for tree in &cx.walk().trees {
        for node in tree.of_kind(is_def_kind) {
            let Some(target) = Target::of(cx, InFile::new(tree.file, node), crate_root) else { continue };
            // A module file's root is met as the definition of its `mod` declaration.
            if !is_def(target) || (target == Target::Mod && node.kind() == SyntaxKind::SOURCE_FILE) {
                continue;
            }
            let def = Def { node: InFile::new(tree.file, node), target };
            let key = defs.order_key(&def);
            let at = def.ptr();
            if visited(target)
                && let Some(owner) = defs.owner(&def)
            {
                for diag in defs.compatible(owner, &def) {
                    keyed.push((key, at, diag));
                }
            }
            for diag in missing.check(&mut defs, &def) {
                keyed.push((key, at, diag));
            }
        }
    }
    radix::sort_by_key(&mut keyed, |&((def, kind), _, _)| {
        radix::pack_64(def as u64, u64::from(kind))
    });
    keyed.into_iter().map(|(_, at, diag)| (at, diag)).collect() // alloc: the errors, in place
}

/// The kinds of the nodes that can be a definition of rustc's `DefId` tree ([`is_def`]): a scan
/// of the walk's kind column reads no other node.
fn is_def_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::SOURCE_FILE
            | SyntaxKind::MODULE
            | SyntaxKind::EXTERN_CRATE
            | SyntaxKind::USE
            | SyntaxKind::STATIC
            | SyntaxKind::CONST
            | SyntaxKind::FN
            | SyntaxKind::TYPE_ALIAS
            | SyntaxKind::EXTERN_BLOCK
            | SyntaxKind::ENUM
            | SyntaxKind::VARIANT
            | SyntaxKind::STRUCT
            | SyntaxKind::UNION
            | SyntaxKind::RECORD_FIELD
            | SyntaxKind::TUPLE_FIELD
            | SyntaxKind::TRAIT
            | SyntaxKind::IMPL
            | SyntaxKind::MACRO_RULES
            | SyntaxKind::MACRO_DEF
            | SyntaxKind::TYPE_PARAM
            | SyntaxKind::LIFETIME_PARAM
            | SyntaxKind::CONST_PARAM
    )
}

/// What `check_missing_stability` and `check_missing_const_stability` read besides the
/// stability attributes: whether the crate is a test crate, its effective visibilities, and
/// each of its modules' `use` items by where it starts.
struct Missing<'a> {
    test: bool,
    effective: &'a Effective,
    uses: HashMap<(HirFileId, TextSize), UseId>,
    /// The node of each of the crate's reachable definitions but its `use`s and its root, made
    /// the first time [`Missing::reachable`] asks: one probe per question, where mapping each
    /// node to its definition costs a `to_def`.
    nodes: OnceCell<HashSet<InFile<SyntaxNodePtr>>>,
}

impl<'a> Missing<'a> {
    fn new(cx: &'a Cx<'_, '_>) -> Missing<'a> {
        let db = cx.db;
        let mut uses = HashMap::default();
        for module in crate::checks::modules(cx) {
            let id = hir_def::ModuleId::from(module);
            for use_ in id.def_map(db).module(db, id).scope(db).use_decls() {
                let loc = use_.lookup(db);
                uses.insert((loc.id.file_id, loc.id.to_ptr(db).text_range().start()), use_);
            }
        }
        Missing {
            test: cx.krate.cfg(db).check_atom(&CfgAtom::Flag(sym::test)),
            effective: cx.effective(),
            uses,
            nodes: OnceCell::new(),
        }
    }

    /// What rustc's visitor checks of `def`, in its order: `check_missing_stability` of every
    /// item but an inherent impl and a foreign module, of trait items and inherent impl items,
    /// of variants (then of a tuple or unit variant's constructor), fields and foreign items, and
    /// of the crate root; `check_missing_const_stability` of items and inherent impl items.
    fn check<'db>(&self, defs: &mut Defs<'_, '_, 'db>, def: &Def<'db>) -> Vec<Diag> {
        let cx = defs.cx;
        let (stability, const_stability) = match def.target {
            Target::Impl { of_trait: false }
            | Target::ForeignMod
            | Target::GenericParam { .. }
            | Target::Method(MethodKind::TraitImpl) => (false, false),
            Target::Method(MethodKind::Trait { .. }) => (true, false),
            Target::Method(MethodKind::Inherent) => (true, true),
            Target::AssocTy | Target::AssocConst => match defs.parent(def).map(|it| it.target) {
                Some(Target::Trait) => (true, false),
                Some(Target::Impl { of_trait: false }) => (true, true),
                _ => (false, false),
            },
            Target::Crate
            | Target::Variant
            | Target::Field
            | Target::ForeignFn
            | Target::ForeignStatic
            | Target::ForeignTy => (true, false),
            _ => (true, true),
        };
        let mut errors = Vec::new(); // alloc: empty unless an error is found
        let file = def.node.file_id;
        if stability && !self.test && defs.lookup_stability(def).is_none() && self.reachable(cx, def) {
            match def.target {
                // Each name of a `use` is an import of its own, with the `use`'s attributes,
                // spanned by its path; the list that holds them is never reachable.
                Target::Use => {
                    let leaves = def.node.value.descendants().filter_map(ast::UseTree::cast);
                    for leaf in leaves.filter(|it| it.use_tree_list().is_none()) {
                        let range = leaf.path().map_or(leaf.syntax().text_range(), |it| it.syntax().text_range());
                        errors.push(missing_stability(cx, InFile::new(file, range), "import"));
                    }
                }
                Target::Crate => {
                    errors.push(missing_stability(cx, InFile::new(file, crate_range(def.node.value)), "module"));
                }
                _ => {
                    let span = InFile::new(file, def_span(def.node.value));
                    errors.push(missing_stability(cx, span, descr(def)));
                    // The constructor inherits the variant's stability and is reachable with it,
                    // unless `#[non_exhaustive]` keeps it in the crate; its span is the variant's.
                    if def.target == Target::Variant
                        && let Some(variant) = ast::Variant::cast(def.node.value)
                        && !defs.owner(def).is_some_and(|at| defs.owners[at].attrs.iter().any(|it| it.is("non_exhaustive")))
                    {
                        let ctor = match variant.field_list() {
                            Some(ast::FieldList::TupleFieldList(_)) => Some("tuple variant"),
                            Some(ast::FieldList::RecordFieldList(_)) => None,
                            None => Some("unit variant"),
                        };
                        if let Some(ctor) = ctor {
                            errors.push(missing_stability(cx, span, ctor));
                        }
                    }
                }
            }
        }
        if const_stability && is_const(defs, def) && self.reachable(cx, def) && !defs.has_const_stability(def) {
            let span = cx.span(InFile::new(file, def_span(def.node.value)));
            // alloc: the error's message
            errors.push(Diag::error(None, format!("{} has missing const stability attribute", descr(def)), span));
        }
        errors
    }

    /// rustc's `is_reachable` of `def`: another crate can name it or use it through what it can
    /// name. An `extern crate` and a global `asm!` have no effective visibility.
    ///
    /// A node of a definition's kind is the source of one definition, the one `to_def` gives,
    /// so it is reachable when it is among [`Missing::nodes`]; a procedural macro's `fn` is the
    /// function's node there, not the macro's, as `to_def` of an `ast::Fn` gives the function.
    fn reachable<'db>(&self, cx: &Cx<'_, 'db>, def: &Def<'db>) -> bool {
        let node = def.node;
        match def.target {
            Target::Crate => {
                self.effective.is_reachable(EffectiveDef::Item(ModuleDef::Module(cx.krate.root_module(cx.db))))
            }
            Target::Use => (self.uses.get(&(node.file_id, node.value.text_range().start())))
                .is_some_and(|&it| self.effective.is_reachable(EffectiveDef::Use(it))),
            Target::Mod
            | Target::Fn
            | Target::Method(_)
            | Target::ForeignFn
            | Target::Struct
            | Target::Enum
            | Target::Union
            | Target::Trait
            | Target::TraitAlias
            | Target::Const
            | Target::AssocConst
            | Target::Static
            | Target::ForeignStatic
            | Target::TyAlias
            | Target::AssocTy
            | Target::ForeignTy
            | Target::Variant
            | Target::Field
            | Target::Impl { .. }
            | Target::MacroDef => self.nodes.get_or_init(|| reachable_nodes(cx, self.effective)).contains(&def.ptr()),
            _ => false,
        }
    }
}

/// The node of each of the crate's reachable definitions, as `to_def` maps it back: a module's
/// `mod` item, a macro's `macro_rules!` or `macro` (a procedural macro's `fn` is its
/// function's), a field's declaration. A `use` and the crate's root are asked of [`Effective`]
/// directly; a derive's impl has no node.
fn reachable_nodes(cx: &Cx<'_, '_>, effective: &Effective) -> HashSet<InFile<SyntaxNodePtr>> {
    fn ptr<'db, N: ast::AstNode<'db>>(it: Option<InFile<N>>) -> Option<InFile<SyntaxNodePtr>> {
        it.map(|it| it.map(|it| ast::ptr(it.syntax())))
    }
    let db = cx.db;
    // alloc: once per `staged_api` crate (the standard library), the reachable definitions
    let mut nodes = HashSet::default();
    for def in effective.reachable_local() {
        let node = match def {
            EffectiveDef::Item(item) => match item {
                ModuleDef::Module(it) => ptr(it.declaration_source_view(db)),
                ModuleDef::Function(it) => ptr(cx.vsema.source(it)),
                ModuleDef::Adt(it) => ptr(cx.vsema.source(it)),
                ModuleDef::EnumVariant(it) => ptr(cx.vsema.source(it)),
                ModuleDef::Const(it) => ptr(cx.vsema.source(it)),
                ModuleDef::Static(it) => ptr(cx.vsema.source(it)),
                ModuleDef::Trait(it) => ptr(cx.vsema.source(it)),
                ModuleDef::TypeAlias(it) => ptr(cx.vsema.source(it)),
                ModuleDef::Macro(it) => match cx.vsema.source(it) {
                    Some(InFile { file_id, value: Either::Left(mac) }) => ptr(Some(InFile::new(file_id, mac))),
                    _ => None,
                },
                ModuleDef::BuiltinType(_) => None,
            },
            EffectiveDef::Field(it) => ptr(cx.vsema.source(it)),
            EffectiveDef::Impl(it) => ptr(cx.vsema.source(it)),
            EffectiveDef::Use(_) => None,
        };
        nodes.extend(node);
    }
    nodes
}

/// `MissingStabilityAttr`.
fn missing_stability(cx: &Cx<'_, '_>, range: InFile<TextRange>, descr: &str) -> Diag {
    Diag::error(None, format!("{descr} has missing stability attribute"), cx.span(range)) // alloc: the error's message
}

/// rustc's `def_descr` of a definition the stability pass checks.
fn descr(def: &Def<'_>) -> &'static str {
    match def.target {
        Target::Crate | Target::Mod => "module",
        Target::ExternCrate => "extern crate",
        Target::Use => "import",
        Target::Static | Target::ForeignStatic => "static",
        Target::Const => "constant",
        Target::Fn | Target::ForeignFn => "function",
        Target::GlobalAsm => "global assembly block",
        Target::TyAlias => "type alias",
        Target::Enum => "enum",
        Target::Variant => "variant",
        Target::Struct => "struct",
        Target::Field => "field",
        Target::Union => "union",
        Target::Trait => "trait",
        Target::TraitAlias => "trait alias",
        Target::Impl { .. } => "implementation",
        Target::AssocConst => "associated constant",
        Target::AssocTy => "associated type",
        Target::Method(_) => {
            let method = ast::Fn::cast(def.node.value).and_then(|it| it.param_list()?.self_param()).is_some();
            if method { "method" } else { "associated function" }
        }
        Target::ForeignTy => "foreign type",
        Target::MacroDef => "macro",
        Target::GenericParam { kind: GenericParamKind::Type, .. } => "type parameter",
        Target::GenericParam { kind: GenericParamKind::Const, .. } => "const parameter",
        Target::GenericParam { kind: GenericParamKind::Lifetime, .. } => "lifetime parameter",
        other => other.name(),
    }
}

/// rustc's `is_const_fn` of a function or an inherent impl's method (`const` there or on its
/// impl), or `is_const_trait`.
fn is_const<'db>(defs: &Defs<'_, '_, 'db>, def: &Def<'db>) -> bool {
    let node = def.node.value;
    let const_fn = || ast::Fn::cast(node).is_some_and(|it| it.const_token().is_some());
    match def.target {
        Target::Fn => const_fn(),
        Target::Method(MethodKind::Inherent) => {
            const_fn()
                || defs.parent(def).and_then(|it| ast::Impl::cast(it.node.value)).is_some_and(|it| it.const_token().is_some())
        }
        Target::Trait => node.children_with_tokens().any(|it| it.kind() == SyntaxKind::CONST_KW),
        _ => false,
    }
}

/// rustc's `def_span` of the crate root: from its first token to its last, its inner
/// attributes and doc comments included.
fn crate_range(root: ast::Node<'_>) -> TextRange {
    // Doc comments are tokens of their own kinds, not trivia.
    let is_token = |token: ast::Token<'_>| !token.kind().is_trivia();
    let mut first = root.first_token();
    while let Some(token) = first.filter(|&it| !is_token(it)) {
        first = token.next_token();
    }
    let mut last = root.last_token();
    while let Some(token) = last.filter(|&it| !is_token(it)) {
        last = token.prev_token();
    }
    match (first, last) {
        (Some(first), Some(last)) => TextRange::new(first.text_range().start(), last.text_range().end()),
        _ => TextRange::empty(root.text_range().start()),
    }
}

/// Whether `MissingStabilityAnnotations` checks a definition of `target`: an item, a trait's or
/// an impl's item, a variant, a field, a foreign item or a generic parameter.
fn visited(target: Target) -> bool {
    !matches!(
        target,
        Target::Crate
            | Target::Closure
            | Target::Expression
            | Target::Statement
            | Target::Arm
            | Target::Param
            | Target::PatField
            | Target::ExprField
            | Target::WherePredicate
            | Target::MacroCall
            | Target::ForLoop
            | Target::While
            | Target::Loop
            | Target::Break
    )
}

/// Whether a node of `target` is a definition of rustc's `DefId` tree that matters to what it
/// inherits.
fn is_def(target: Target) -> bool {
    target == Target::Crate || visited(target)
}

/// rustc's `inherit_deprecation`: all but generic parameters inherit from their parent.
fn inherits(target: Target) -> bool {
    !matches!(target, Target::GenericParam { .. })
}

/// rustc's `AnnotationKind`, of what `check_compatible_stability` reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Annotation {
    Required,
    Prohibited,
    Container,
}

/// A definition: its node and target.
#[derive(Clone, Copy)]
struct Def<'db> {
    node: InFile<ast::Node<'db>>,
    target: Target,
}

impl Def<'_> {
    fn ptr(&self) -> InFile<SyntaxNodePtr> {
        self.node.map(ast::ptr)
    }
}

/// The crate's definitions as rustc's stability pass looks them up, with what each inherits.
struct Defs<'a, 'c, 'db> {
    cx: &'a Cx<'c, 'db>,
    owners: &'a [Owner<'db>],
    parsed: &'a Lowered<'a>,
    crate_root: HirFileId,
    /// Each owner by its node.
    index: HashMap<InFile<SyntaxNodePtr>, usize>,
    /// The owner of a module file's root, when it has its own (its declaration has none).
    file_roots: HashMap<HirFileId, usize>,
    /// The declaration of each module file, in a tree `cx.vsema` handed out.
    declared: HashMap<HirFileId, InFile<ast::Node<'db>>>,
    /// The file each declaration of a module file declares.
    declares: HashMap<InFile<SyntaxNodePtr>, HirFileId>,
    /// `lookup_stability`: the owner whose `Stability` it is.
    stability: HashMap<InFile<SyntaxNodePtr>, Option<usize>>,
    /// `lookup_deprecation_entry`: the owner whose `Deprecated` it is.
    deprecation: HashMap<InFile<SyntaxNodePtr>, Option<usize>>,
    /// Each module, in the order its first definition comes.
    modules: HashMap<InFile<SyntaxNodePtr>, usize>,
    /// [`Defs::parent`] of each definition asked about: `order_key`, the lookups and the
    /// annotation kind each ask it, so each definition's ancestors are walked once.
    parents: RefCell<HashMap<InFile<SyntaxNodePtr>, Option<Def<'db>>>>,
}

impl<'a, 'c, 'db> Defs<'a, 'c, 'db> {
    fn new(cx: &'a Cx<'c, 'db>, owners: &'a [Owner<'db>], parsed: &'a Lowered<'a>, crate_root: HirFileId) -> Self {
        let mut index = HashMap::default();
        let mut file_roots = HashMap::default();
        for (at, owner) in owners.iter().enumerate() {
            index.insert(owner.at(), at);
            if owner.node.value.kind() == SyntaxKind::SOURCE_FILE {
                file_roots.insert(owner.node.file_id, at);
            }
        }
        let mut declared = HashMap::default();
        let mut declares = HashMap::default();
        for module in crate::checks::modules(cx) {
            if let Some(file) = module.as_source_file_id(cx.db)
                && let Some(declaration) = module.declaration_source_view(cx.db)
            {
                // The declaration's tree is given out by `vsema`, so `reachable` may ask it of
                // the `mod` item.
                cx.vsema.parse_or_expand(declaration.file_id);
                let node = declaration.map(|it| it.syntax());
                declares.insert(node.map(ast::ptr), HirFileId::from(file));
                declared.insert(HirFileId::from(file), node);
            }
        }
        let mut modules = HashMap::default();
        if let Some(krate) = owners.iter().find(|it| it.target == Target::Crate) {
            modules.insert(krate.at(), 0);
        }
        Defs {
            cx,
            owners,
            parsed,
            crate_root,
            index,
            file_roots,
            declared,
            declares,
            stability: HashMap::default(),
            deprecation: HashMap::default(),
            modules,
            parents: RefCell::new(HashMap::default()),
        }
    }

    /// The owner of `def`'s attributes, if it has any.
    fn owner(&self, def: &Def<'db>) -> Option<usize> {
        let ptr = def.ptr();
        if let Some(&at) = self.index.get(&ptr) {
            return Some(at);
        }
        self.declares.get(&ptr).and_then(|file| self.file_roots.get(file)).copied()
    }

    /// `opt_local_parent`: the definition `def` is in; through a module file to its
    /// declaration and through an expansion to where the macro is called. What a derive makes
    /// is beside the item it derives from, in that item's parent.
    fn parent(&self, def: &Def<'db>) -> Option<Def<'db>> {
        let ptr = def.ptr();
        if let Some(&parent) = self.parents.borrow().get(&ptr) {
            return parent;
        }
        let parent = self.walk_to_parent(def);
        self.parents.borrow_mut().insert(ptr, parent);
        parent
    }

    /// [`Defs::parent`], found by walking up from `def`.
    fn walk_to_parent(&self, def: &Def<'db>) -> Option<Def<'db>> {
        let mut file = def.node.file_id;
        if def.node.value.kind() == SyntaxKind::SOURCE_FILE
            && file != self.crate_root
            && let Some(&declaration) = self.declared.get(&file)
        {
            return self.parent(&Def { node: declaration, target: Target::Mod });
        }
        let mut at = def.node.value.parent();
        loop {
            let Some(node) = at else {
                let db = self.cx.db;
                let loc = file.macro_file()?.loc(db);
                let call = match loc.kind {
                    MacroCallKind::Derive { .. } => loc.to_node_item_view(db).map(|it| it.syntax()),
                    _ => loc.to_node_view(db),
                };
                // The call's tree is given out by `vsema`, so `reachable` may ask it of the call.
                self.cx.vsema.parse_or_expand(call.file_id);
                file = call.file_id;
                at = call.value.parent();
                continue;
            };
            if node.kind() == SyntaxKind::SOURCE_FILE
                && file != self.crate_root
                && let Some(&declaration) = self.declared.get(&file)
            {
                return Some(Def { node: declaration, target: Target::Mod });
            }
            if let Some(target) = Target::of(self.cx, InFile::new(file, node), self.crate_root).filter(|it| is_def(*it)) {
                return Some(Def { node: InFile::new(file, node), target });
            }
            at = node.parent();
        }
    }

    fn own_stability(&self, owner: usize) -> Option<&'a Stability> {
        self.parsed.of(owner).iter().find_map(|it| match &it.parsed {
            Parsed::Stability(stability) => Some(stability),
            _ => None,
        })
    }

    fn own_const_stability(&self, owner: usize) -> Option<&'a Stability> {
        self.parsed.of(owner).iter().find_map(|it| match &it.parsed {
            Parsed::ConstStability(stability) => Some(stability),
            _ => None,
        })
    }

    /// `lookup_const_stability(..).is_some()`: its own `#[rustc_const_*]`; of a `const fn`, an
    /// unstable stability it has or inherits; of an associated item of a trait or an impl, its
    /// parent's own const instability.
    fn has_const_stability(&mut self, def: &Def<'db>) -> bool {
        if self.owner(def).is_some_and(|at| self.own_const_stability(at).is_some()) {
            return true;
        }
        let const_fn = ast::Fn::cast(def.node.value).is_some_and(|it| it.const_token().is_some());
        if const_fn && self.lookup_stability(def).is_some_and(|it| it.level == StabilityLevel::Unstable) {
            return true;
        }
        if def.target.is_associated_item()
            && let Some(parent) = self.parent(def)
            && matches!(parent.target, Target::Trait | Target::Impl { .. })
            && let Some(at) = self.owner(&parent)
        {
            return self.own_const_stability(at).is_some_and(|it| it.level == StabilityLevel::Unstable);
        }
        false
    }

    /// The node's `#[deprecated]` and its attribute.
    fn own_deprecation(&self, owner: usize) -> Option<(&'a DeprecatedSince, usize)> {
        self.parsed.of(owner).iter().find_map(|it| match &it.parsed {
            Parsed::Deprecated(since) => Some((since, it.attr)),
            _ => None,
        })
    }

    /// `lookup_stability`: its own, or its parent's where that is unstable, or for a field or
    /// a variant, whatever its parent's is.
    fn lookup_stability(&mut self, def: &Def<'db>) -> Option<&'a Stability> {
        let ptr = def.ptr();
        let owner = match self.stability.get(&ptr) {
            Some(owner) => *owner,
            None => {
                let owner = self.stability_owner(def);
                self.stability.insert(ptr, owner);
                owner
            }
        };
        owner.and_then(|it| self.own_stability(it))
    }

    fn stability_owner(&mut self, def: &Def<'db>) -> Option<usize> {
        if let Some(owner) = self.owner(def)
            && self.own_stability(owner).is_some()
        {
            return Some(owner);
        }
        if !inherits(def.target) {
            return None;
        }
        let parent = self.parent(def)?;
        let stability = self.lookup_stability(&parent)?;
        let inherit = stability.level == StabilityLevel::Unstable || matches!(def.target, Target::Field | Target::Variant);
        if !inherit {
            return None;
        }
        let ptr = parent.ptr();
        self.stability.get(&ptr).copied().flatten()
    }

    /// `lookup_deprecation_entry`: its own, or its parent's.
    fn lookup_deprecation(&mut self, def: &Def<'db>) -> Option<&'a DeprecatedSince> {
        let ptr = def.ptr();
        let owner = match self.deprecation.get(&ptr) {
            Some(owner) => *owner,
            None => {
                let owner = self.deprecation_owner(def);
                self.deprecation.insert(ptr, owner);
                owner
            }
        };
        owner.and_then(|it| self.own_deprecation(it)).map(|(since, _)| since)
    }

    fn deprecation_owner(&mut self, def: &Def<'db>) -> Option<usize> {
        if let Some(owner) = self.owner(def)
            && self.own_deprecation(owner).is_some()
        {
            return Some(owner);
        }
        if !inherits(def.target) {
            return None;
        }
        let parent = self.parent(def)?;
        self.lookup_deprecation(&parent)?;
        self.deprecation.get(&parent.ptr()).copied().flatten()
    }

    /// `annotation_kind`.
    fn annotation(&self, def: &Def<'db>) -> Annotation {
        match def.target {
            Target::Impl { of_trait: false } | Target::ForeignMod => Annotation::Container,
            Target::GenericParam { kind: GenericParamKind::Type | GenericParamKind::Const, has_default } => {
                if has_default { Annotation::Container } else { Annotation::Prohibited }
            }
            Target::Method(MethodKind::TraitImpl) => Annotation::Prohibited,
            Target::AssocTy | Target::AssocConst => match self.parent(def) {
                Some(Def { target: Target::Impl { of_trait: true }, .. }) => Annotation::Prohibited,
                _ => Annotation::Required,
            },
            _ => Annotation::Required,
        }
    }

    /// The order rustc's visit of each module comes to `def` in: its module's place, and
    /// whether it is (or is in) an item, a trait's item, an impl's item or a foreign item
    /// (`hir_visit_item_likes_in_module`).
    fn order_key(&mut self, def: &Def<'db>) -> (usize, u8) {
        let mut kind = None;
        let mut at = *def;
        loop {
            if kind.is_none() {
                kind = match at.target {
                    Target::Field | Target::Variant | Target::GenericParam { .. } => None,
                    Target::Method(MethodKind::Trait { .. }) => Some(1),
                    Target::Method(MethodKind::TraitImpl | MethodKind::Inherent) => Some(2),
                    Target::AssocTy | Target::AssocConst => match self.parent(&at) {
                        Some(Def { target: Target::Trait, .. }) => Some(1),
                        _ => Some(2),
                    },
                    Target::ForeignFn | Target::ForeignStatic | Target::ForeignTy => Some(3),
                    _ => Some(0),
                };
            }
            let Some(parent) = self.parent(&at) else { break };
            if matches!(parent.target, Target::Mod | Target::Crate) {
                let ptr = parent.ptr();
                let next = self.modules.len();
                let module = *self.modules.entry(ptr).or_insert(next);
                return (module, kind.unwrap_or(0));
            }
            at = parent;
        }
        (0, kind.unwrap_or(0))
    }

    /// `check_compatible_stability` of the owner `at`, whose node is `def`.
    fn compatible(&mut self, at: usize, def: &Def<'db>) -> Vec<Diag> {
        let stability = self.lookup_stability(def);
        let deprecation = self.lookup_deprecation(def);
        let kind = self.annotation(def);
        let own_stability = self.own_stability(at);
        let own_deprecation = self.own_deprecation(at);
        let cx: &'a Cx<'c, 'db> = self.cx;
        let owners: &'a [Owner<'db>] = self.owners;
        let all: &'a Lowered<'a> = self.parsed;
        let owner = &owners[at];
        let parsed = all.of(at);
        let attr_span = |index: usize| {
            let attr = &owner.attrs[index];
            cx.span(attr.in_file(attr.range()))
        };
        let path_span = |index: usize| {
            let attr = &owner.attrs[index];
            cx.span(attr.in_file(attr.path_range()))
        };
        let item_span = || cx.span(InFile::new(def.node.file_id, def_range(def.node.value)));
        let const_stability = parsed.iter().find_map(|it| match &it.parsed {
            Parsed::ConstStability(stability) => Some(stability),
            _ => None,
        });
        let const_stable_indirect = parsed.iter().any(|it| matches!(it.parsed, Parsed::RustcConstStableIndirect));
        // alloc: empty, allocates only when an annotation is in error
        let mut errors = Vec::new();
        if stability.is_none()
            && let Some((DeprecatedSince::RustcVersion(_), attr)) = own_deprecation
        {
            let message = "deprecated attribute must be paired with either stable or unstable attribute";
            errors.push(Diag::error(Some("E0549"), message, attr_span(attr)));
        }
        if let Some(stability) = own_stability {
            let stable = matches!(stability.level, StabilityLevel::Stable(_));
            if kind == Annotation::Prohibited || kind == Annotation::Container && stable && deprecation.is_some() {
                let span = attr_span(stability.attr);
                // clone: the error's span
                let mut diag = Diag::error(None, "this stability annotation is useless", span.clone());
                diag.span_label(span, "useless stability annotation");
                diag.span_label(item_span(), "the stability attribute annotates this item");
                errors.push(diag);
            }
            if let Some(DeprecatedSince::RustcVersion(deprecated)) = deprecation
                && let StabilityLevel::Stable(since) = stability.level
            {
                let after = match since {
                    StableSince::Current => true,
                    StableSince::Version(since) => *deprecated < since,
                    StableSince::Err => false,
                };
                if after {
                    let span = attr_span(stability.attr);
                    // clone: the error's span
                    let mut diag = Diag::error(None, "an API can't be stabilized after it is deprecated", span.clone());
                    diag.span_label(span, "invalid version");
                    diag.span_label(item_span(), "the stability attribute annotates this item");
                    errors.push(diag);
                }
            }
        }
        let function = match def.target {
            Target::Fn | Target::Method(_) | Target::ForeignFn => ast::Fn::cast(def.node.value),
            _ => None,
        };
        if let Some(function) = function
            && let Some(const_stability) = const_stability
        {
            let signature = cx.span(InFile::new(def.node.file_id, signature_range(function)));
            let comptime = owner.attrs.iter().any(|it| it.is("rustc_comptime"));
            if function.const_token().is_none() && !comptime {
                let message = "attributes `#[rustc_const_unstable]`, `#[rustc_const_stable]` and `#[rustc_const_stable_indirect]` require the function or method to be `const`";
                // clone: the error's span
                let mut diag = Diag::error(None, message, signature.clone());
                // clone: the error's span
                diag.span_help(signature.clone(), "make the function or method const");
                errors.push(diag);
            }
            let const_stable = matches!(const_stability.level, StabilityLevel::Stable(_));
            let stable = stability.is_some_and(|it| matches!(it.level, StabilityLevel::Stable(_)));
            if const_stable && !stable {
                let message = "the `rustc_const_stable` attribute can only be applied to functions marked with the `stable` attribute";
                let mut diag = Diag::error(None, message, signature);
                diag.span_label(path_span(const_stability.attr), "attribute specified here");
                errors.push(diag);
            }
        }
        if let Some(const_stability) = const_stability
            && matches!(const_stability.level, StabilityLevel::Stable(_))
            && const_stable_indirect
        {
            let message = "`const_stable_indirect` attribute does not make sense on `rustc_const_stable` function, its behavior is already implied";
            errors.push(Diag::error(None, message, path_span(const_stability.attr)));
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_sorted_and_distinct() {
        assert!(PATHS.windows(2).all(|it| it[0] < it[1]));
    }

    #[test]
    fn a_feature_is_an_identifier() {
        assert!(is_ident("rust1"));
        assert!(is_ident("_a"));
        assert!(!is_ident("1a"));
        assert!(!is_ident("a-b"));
        assert!(!is_ident(""));
    }

    #[test]
    fn an_issue_names_why_it_is_not_a_number() {
        let kind = |text: &str| text.parse::<NonZeroU32>().err().and_then(|it| issue_cause(it.kind()));
        assert_eq!(kind("no"), Some("invalid digit found in string"));
        assert_eq!(kind("0"), Some("`issue` must not be \"0\", use \"none\" instead"));
        assert_eq!(kind(""), Some("cannot parse integer from empty string"));
        assert_eq!(kind("99999999999"), Some("number too large to fit in target type"));
        assert_eq!(kind("12"), None);
    }

    #[test]
    fn deprecated_since_reads_versions_only_in_the_standard_library() {
        assert_eq!(parse_since("TBD", false), DeprecatedSince::Future);
        assert_eq!(parse_since("a", false), DeprecatedSince::NonStandard("a".to_owned()));
        assert_eq!(parse_since("1.2.3", true), DeprecatedSince::RustcVersion((1, 2, 3)));
        assert_eq!(parse_since("1.2", true), DeprecatedSince::RustcVersion((1, 2, 0)));
        assert_eq!(parse_since("CURRENT_RUSTC_VERSION", true), DeprecatedSince::RustcVersion(CURRENT));
        assert_eq!(parse_since("1.2.3-beta", true), DeprecatedSince::Err);
    }

    /// The crate root is spanned from its first doc comment or attribute to its last item, the
    /// plain comments and white space around them left out.
    #[test]
    fn the_crate_root_is_spanned_by_its_doc_comments_attributes_and_items() {
        let text = "// a\n\n//! doc\n#![feature(staged_api)]\nfn f() {}\n/* b */\n";
        let parse = syntax::SourceFile::parse(text, span::Edition::CURRENT);
        assert_eq!(&text[crate_range(parse.root())], "//! doc\n#![feature(staged_api)]\nfn f() {}");
        let text = "fn f() {}\n/// trailing\n";
        let parse = syntax::SourceFile::parse(text, span::Edition::CURRENT);
        assert_eq!(&text[crate_range(parse.root())], "fn f() {}\n/// trailing");
        let parse = syntax::SourceFile::parse("// only\n", span::Edition::CURRENT);
        assert_eq!(crate_range(parse.root()), TextRange::empty(0.into()));
    }

    /// Every kind of definition the missing-stability check asks about, public and private,
    /// reachable through a signature only, in a block, and of the three kinds of macro.
    const DEFINITIONS: &str = r#"
#![feature(staged_api, decl_macro, extern_types)]
#![stable(feature = "s", since = "1.0.0")]
pub fn unmarked() {}
fn private() {}
pub struct S { pub a: u8, b: u8 }
pub struct T(pub u8, u8);
struct Hidden;
pub enum E { A, B(u8), C { x: u8 } }
pub trait Tr { fn m(&self); type A; const C: u8; }
impl Tr for S { fn m(&self) {} type A = u8; const C: u8 = 0; }
impl S { pub fn new() -> S { S { a: 0, b: 0 } } fn p(&self) {} }
impl Hidden { pub fn q() {} }
pub mod m { pub fn f() {} pub(crate) fn g() {} mod inner { pub fn h() {} } }
mod private_mod { pub struct Leak; pub fn unleaked() {} }
pub fn leak() -> private_mod::Leak { private_mod::Leak }
#[macro_export] macro_rules! mac { () => () }
macro_rules! local { () => () }
pub macro m2() {}
pub type Alias = u8;
pub static ST: u8 = 0;
pub const CO: u8 = 0;
extern "C" { pub fn ext(); pub static EXT: u8; pub type Opaque; }
pub fn with_block() { fn inner() {} struct InBlock; }
pub union U { pub a: u8 }
pub trait Alias2 = Tr;
"#;

    /// `Missing::reachable` as it was: each node mapped to its definition by `to_def`, then
    /// asked of `Effective`.
    fn plain_reachable<'db>(cx: &Cx<'_, 'db>, missing: &Missing<'_>, def: &Def<'db>) -> bool {
        let node = def.node;
        macro_rules! def_of {
            ($ast:ty) => {
                <$ast>::cast(node.value).and_then(|it| cx.vsema.to_def(it))
            };
        }
        let found = match def.target {
            Target::Crate => Some(EffectiveDef::Item(ModuleDef::Module(cx.krate.root_module(cx.db)))),
            Target::Mod => def_of!(ast::Module).map(|it| EffectiveDef::Item(it.into())),
            Target::Fn | Target::Method(_) | Target::ForeignFn => def_of!(ast::Fn).map(|it| EffectiveDef::Item(it.into())),
            Target::Struct | Target::Enum | Target::Union => def_of!(ast::Adt).map(|it| EffectiveDef::Item(it.into())),
            Target::Trait | Target::TraitAlias => def_of!(ast::Trait).map(|it| EffectiveDef::Item(it.into())),
            Target::Const | Target::AssocConst => def_of!(ast::Const).map(|it| EffectiveDef::Item(it.into())),
            Target::Static | Target::ForeignStatic => def_of!(ast::Static).map(|it| EffectiveDef::Item(it.into())),
            Target::TyAlias | Target::AssocTy | Target::ForeignTy => {
                def_of!(ast::TypeAlias).map(|it| EffectiveDef::Item(it.into()))
            }
            Target::Variant => def_of!(ast::Variant).map(|it| EffectiveDef::Item(ModuleDef::EnumVariant(it))),
            Target::Field => def_of!(ast::RecordField).or_else(|| def_of!(ast::TupleField)).map(EffectiveDef::Field),
            Target::Impl { .. } => def_of!(ast::Impl).map(EffectiveDef::Impl),
            Target::MacroDef => def_of!(ast::Macro).map(|it| EffectiveDef::Item(ModuleDef::Macro(it))),
            Target::Use => {
                missing.uses.get(&(node.file_id, node.value.text_range().start())).map(|&it| EffectiveDef::Use(it))
            }
            _ => None,
        };
        found.is_some_and(|it| missing.effective.is_reachable(it))
    }

    /// Asking a node's reachability of the set of reachable definitions' nodes gives what
    /// `to_def` and `Effective` give, at every definition the check visits.
    #[test]
    fn reachable_by_node_is_reachable_by_to_def() {
        crate::lower::break_outside_of_loop::tests::with_file(DEFINITIONS, |cx, _| {
            let root = cx.krate.root_module(cx.db).as_source_file_id(cx.db).expect("the crate's root is a file");
            let crate_root = HirFileId::from(root);
            let missing = Missing::new(cx);
            let (mut compared, mut reachable) = (0, 0);
            for tree in &cx.walk().trees {
                for node in tree.of_kind(is_def_kind) {
                    let node = InFile::new(tree.file, node);
                    let Some(target) = Target::of(cx, node, crate_root) else { continue };
                    if !is_def(target) {
                        continue;
                    }
                    let def = Def { node, target };
                    let by_node = missing.reachable(cx, &def);
                    assert_eq!(by_node, plain_reachable(cx, &missing, &def), "at {target:?} {:?}", node.value);
                    compared += 1;
                    reachable += usize::from(by_node);
                }
            }
            assert!(compared >= 40, "the fixture has about 55 definitions: {compared}");
            assert!(reachable >= 15, "and reachable ones among them: {reachable}");
        });
    }

    #[test]
    fn strings_are_escaped_as_rustc_prints_them() {
        assert_eq!(escape_string("a\"b\n\u{e9}"), "a\\\"b\\n\\u{e9}");
        assert_eq!(escape_string("1.2.3"), "1.2.3");
    }
}
