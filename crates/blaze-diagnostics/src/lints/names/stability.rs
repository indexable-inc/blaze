//! The stability checks: `deprecated` and `deprecated_in_future` of a use of a deprecated
//! definition (the crate's own or a dependency's: an item, a field, an associated item, a
//! variant, a macro), E0658 of a use of an unstable one of a crate with
//! `#![feature(staged_api)]`, `useless_deprecated` and the `unused_attributes` of a
//! `#[deprecated]` that does nothing, `ineffective_unstable_trait_impl` of an `#[unstable]`
//! trait impl of stable types and traits, and the warning of `#[macro_escape]`.
//!
//! Each is reported where rustc reports it: a path name resolution resolves by the
//! `Checker` of `check_mod_unstable_api_usage` after `check_mod_attrs`, module by module
//! (`Phase::Items`); a method call, a field and a type-relative path in a body by typeck
//! (`Phase::Bodies`); a macro by resolution (`Phase::Resolve`); a `#[deprecated]` on a target
//! it can't be on by the attribute parser, whose lints are emitted after the checks of each
//! module (`emit_ast_lowering_delayed_lints`).
//!
//! Cost: one pass over the crate walk every syntax pass shares (`cx.walk()`), reading only
//! the nodes of the kinds it looks at, checks every use as it comes to it (`walk.rs`); a
//! use is a probe of the per-check index (`lookup.rs`) and stores nothing. Text (paths,
//! messages) is made here, only for what is reported, and only what is reported is sorted
//! into rustc's order.
//!
//! Follows `compiler/rustc_middle/src/middle/stability.rs` (`deprecation_message`,
//! `deprecation_suggestion`, `early_report_macro_deprecation`, `eval_stability_allow_unstable`,
//! `check_stability_allow_unstable`, `report_unstable`, `skip_stability_check_due_to_privacy`),
//! `compiler/rustc_passes/src/stability.rs` (`check_mod_unstable_api_usage`, `Checker`),
//! `compiler/rustc_resolve/src/macros.rs` (`check_stability_and_deprecation`),
//! `compiler/rustc_resolve/src/build_reduced_graph.rs` (`contains_macro_use`) and
//! `compiler/rustc_session/src/parse.rs` (`feature_err_issue`,
//! `add_feature_diagnostics_for_issue`) (MIT or Apache-2.0).

mod attr;
mod def;
mod lookup;
mod target;
mod walk;

use blaze_items::Fact;
use hir::{InFile, ModuleDef};
use hir_def::attrs::stability::{
    DeprecatedSince, Deprecation, RustcVersion, Stability, StabilityLevel, UnstableReason,
};
use intern::Symbol;
use syntax::{SyntaxNodePtr, TextRange};

use self::def::Def;
use crate::{
    cx::Cx,
    lints::{registry::LintId, syntax::source_fact},
    lower::{Found, Phase, mutable_ref_binding::BUILT_ON},
    model::{Applicability, Diag, Level, MultiSpan},
};

pub(super) fn lints(cx: &Cx<'_, '_>) -> Vec<Found> {
    walk::walk(cx)
}

/// Where a use is, as reported.
#[derive(Clone, Copy)]
struct Site {
    /// Where the lint's level is decided.
    at: InFile<SyntaxNodePtr>,
    span: InFile<TextRange>,
    phase: Phase,
}

/// `deprecated`/`deprecated_in_future` of a use of `def` (its kind and path printed by rustc's
/// `def_descr` and `def_path_str`), with the suggestion at `suggestion` when there is one.
fn deprecated(
    cx: &Cx<'_, '_>,
    def: Def,
    depr: &Deprecation,
    current: Option<RustcVersion>,
    site: Site,
    suggestion: Option<InFile<TextRange>>,
) -> Option<Found> {
    let Some(in_effect) = in_effect(depr, current) else {
        return Some(Found::new(unknown_current(cx, site.span), site.phase));
    };
    let lint = match in_effect {
        true => LintId::named("deprecated"),
        false => LintId::named("deprecated_in_future"),
    };
    if !cx.lint_may_fire(lint) {
        return None;
    }
    let kind = def.descr(cx.db);
    let path = def::path(cx, def)?;
    let mut diag = deprecation_diag(cx, depr, in_effect, kind, &path, site.span, suggestion);
    // alloc: the fact of a deprecation that fires
    diag.fact_opt(definition_fact(cx, def), format!("the attributes of `{path}`"));
    source_fact(cx, &mut diag, site.at);
    Some(Found::lint(diag, site.phase, site.at))
}

/// `Deprecation::is_in_effect` with this crate's `RustcVersion::CURRENT`: none when `since`
/// is a version and there is no current one to compare it with (any other `since` doesn't
/// read it).
pub(super) fn in_effect(depr: &Deprecation, current: Option<RustcVersion>) -> Option<bool> {
    match (&depr.since, current) {
        (DeprecatedSince::RustcVersion(_), None) => None,
        (_, current) => Some(depr.is_in_effect(current.unwrap_or(RustcVersion { major: 0, minor: 0, patch: 0 }))),
    }
}

/// The error of a deprecation whose `since` is a version, at `span`, in a crate graph that
/// names no toolchain: whether it is `deprecated` or `deprecated_in_future` depends on the
/// version of rustc, which there is none of.
pub(super) fn unknown_current(cx: &Cx<'_, '_>, span: InFile<TextRange>) -> Diag {
    Diag::error(
        None,
        "blaze can't tell whether this deprecation is in effect: its `since` is a version and the crate graph names no toolchain to compare it with",
        cx.span(span),
    )
}

/// `deprecated` of the parent segment `def` of a path to an item allowed through unstable
/// modules, with the item's message (`Checker::visit_path`).
fn deprecated_through(
    cx: &Cx<'_, '_>,
    def: Def,
    message: &Symbol,
    current: Option<RustcVersion>,
    site: Site,
) -> Option<Found> {
    if !cx.lint_may_fire(LintId::named("deprecated")) {
        return None;
    }
    // clone: a refcount, on a deprecation that may fire, past the level test
    let depr = Deprecation { since: DeprecatedSince::Unspecified, note: Some(message.clone()), suggestion: None };
    deprecated(cx, def, &depr, current, site, None)
}

/// The macro deprecation of `early_report_macro_deprecation`: kind `macro`, the path as written.
fn deprecated_macro(
    cx: &Cx<'_, '_>,
    mac: hir::Macro,
    depr: &Deprecation,
    in_effect: bool,
    path: &str,
    site: Site,
) -> Found {
    let mut diag = deprecation_diag(cx, depr, in_effect, "macro", path, site.span, Some(site.span));
    // alloc: the fact of a macro deprecation that fires
    diag.fact_opt(definition_fact(cx, Def::Item(ModuleDef::Macro(mac))), format!("the attributes of `{path}`"));
    source_fact(cx, &mut diag, site.at);
    Found::lint(diag, site.phase, site.at)
}

/// `contains_macro_use`'s warning of a `#[macro_escape]` at `range`, on the `mod` at `at`.
fn macro_escape(cx: &Cx<'_, '_>, range: InFile<TextRange>, inner: bool, at: InFile<SyntaxNodePtr>) -> Found {
    let mut diag = Diag::new(Level::Warning, None, "`#[macro_escape]` is a deprecated synonym for `#[macro_use]`");
    diag.span = MultiSpan::of(cx.span(range));
    if inner {
        diag.help("try an outer attribute: `#[macro_use]`");
    }
    source_fact(cx, &mut diag, at);
    Found::new(diag, Phase::Resolve)
}

/// `IneffectiveUnstableImpl`: the `#[unstable]` at `span` of the trait impl at `at` has no
/// effect, every definition its self type and trait name (`defs`) being stable.
fn ineffective_unstable_impl(cx: &Cx<'_, '_>, span: InFile<TextRange>, at: InFile<SyntaxNodePtr>, defs: &[Def]) -> Found {
    let mut diag =
        Diag::lint("ineffective_unstable_trait_impl", "an `#[unstable]` annotation here has no effect", cx.span(span));
    diag.note("see issue #55436 <https://github.com/rust-lang/rust/issues/55436> for more information");
    for &def in defs {
        if let Some(path) = def::path(cx, def) {
            // alloc: the facts of an `ineffective_unstable_trait_impl` that fires
            diag.fact_opt(definition_fact(cx, def), format!("the stability of `{path}`"));
        }
    }
    source_fact(cx, &mut diag, at);
    Found::lint(diag, Phase::Items, at)
}

/// `report_unstable` of a use of `def` (whose path, for the fact, is `path`).
fn unstable(cx: &Cx<'_, '_>, def: Def, path: Option<&str>, stability: &Stability, site: Site) -> Option<Found> {
    let mut diag = unstable_diag(cx, stability, site.span)?;
    // alloc: the path of an E0658 that fires, for its fact
    let path = path.map(str::to_owned).or_else(|| def::path(cx, def));
    if let Some(path) = path {
        // alloc: the fact of an E0658 that fires
        diag.fact_opt(definition_fact(cx, def), format!("the stability of `{path}`"));
    }
    source_fact(cx, &mut diag, site.at);
    Some(Found::new(diag, site.phase))
}

fn definition_fact(cx: &Cx<'_, '_>, def: Def) -> Option<Fact> {
    match def.item(cx.db) {
        ModuleDef::Module(module) => Some(Fact::Signature(cx.module_key(module))),
        _ => def.item_id(cx.db).map(|it| Fact::Signature(cx.key(it))),
    }
}

/// `stability::Deprecated` of a use of the `kind` `path` deprecated by `depr`, with its
/// suggestion at `suggestion` when there is one.
fn deprecation_diag(
    cx: &Cx<'_, '_>,
    depr: &Deprecation,
    in_effect: bool,
    kind: &str,
    path: &str,
    span: InFile<TextRange>,
    suggestion: Option<InFile<TextRange>>,
) -> Diag {
    // alloc: the message of a deprecation that fires
    let note = depr.note.as_ref().map(|it| format!(": {}", it.as_str())).unwrap_or_default();
    let (lint, message) = match (&depr.since, in_effect) {
        // alloc: the message of a deprecation that fires
        (_, true) => ("deprecated", format!("use of deprecated {kind} `{path}`{note}")),
        (DeprecatedSince::RustcVersion(v), false) => (
            "deprecated_in_future",
            // alloc: the message of a deprecation that fires
            format!(
                "use of {kind} `{path}` that will be deprecated in future version {}.{}.{}{note}",
                v.major, v.minor, v.patch
            ),
        ),
        (_, false) => (
            "deprecated_in_future",
            // alloc: the message of a deprecation that fires
            format!("use of {kind} `{path}` that will be deprecated in a future Rust version{note}"),
        ),
    };
    let mut diag = Diag::lint(lint, message, cx.span(span));
    if let Some(replacement) = &depr.suggestion
        && let Some(at) = suggestion
    {
        diag.suggestion_verbose(
            cx.span(at),
            // alloc: the suggestion of a deprecation that fires
            format!("replace the use of the deprecated {kind}"),
            // alloc: the replacement text the firing diagnostic's suggestion owns
            replacement.as_str().to_owned(),
            Applicability::MachineApplicable,
        );
    }
    diag
}

/// `report_unstable` through `feature_err_issue`: E0658 of a use of an unstable library
/// feature, on a nightly compiler.
fn unstable_diag(cx: &Cx<'_, '_>, stability: &Stability, span: InFile<TextRange>) -> Option<Diag> {
    let StabilityLevel::Unstable { reason, issue, .. } = &stability.level else { return None };
    let feature = stability.feature.as_str();
    let message = match reason {
        // alloc: the message of an E0658 that fires
        UnstableReason::Some(reason) => format!("use of unstable library feature `{feature}`: {}", reason.as_str()),
        UnstableReason::Default => {
            // alloc: the message of an E0658 that fires
            format!("use of unstable library feature `{feature}`: {UNSTABLE_LOCATION_REASON_DEFAULT}")
        }
        // alloc: the message of an E0658 that fires
        UnstableReason::None => format!("use of unstable library feature `{feature}`"),
    };
    let mut diag = Diag::error(Some("E0658"), message, cx.span(span));
    if let Some(issue) = issue {
        // alloc: the note of an E0658 that fires
        diag.note(format!("see issue #{issue} <https://github.com/rust-lang/rust/issues/{issue}> for more information"));
    }
    // alloc: the help of an E0658 that fires
    diag.help(format!("add `#![feature({feature})]` to the crate attributes to enable"));
    // alloc: the note of an E0658 that fires
    diag.note(format!("this compiler was built on {BUILT_ON}; consider upgrading it if it is out of date"));
    Some(diag)
}

/// rustc's `sym::unstable_location_reason_default`: the reason of `UnstableReason::Default`, the
/// stability `-Zforce-unstable-if-unmarked` gives.
const UNSTABLE_LOCATION_REASON_DEFAULT: &str = "this crate is being loaded from the sysroot, an unstable location; did you mean to load this crate from crates.io via `Cargo.toml` instead?";
