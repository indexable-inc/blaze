//! rustc's feature gates of syntax, as `check_crate` gives them after its checks of the
//! features themselves: the pre-expansion gates of what its parser saw ([`pre`]: E0658, and
//! `associated_const_equality`, `type const` and `yield` with their own rules), the soft
//! gates (a warning, of syntax only the parser saw), then the post-expansion visitor
//! ([`post`]) on what is left once `cfg` and the macros have done their work.
//!
//! A soft gate's warning is stashed: an error of the visitor at the same span cancels it, and
//! what is left rustc writes at the very end, and only when nothing was an error. None are
//! given once a gate here errors; the rest at [`Phase::Stashed`], which the driver writes last
//! and drops when anything else was an error.
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/feature_gate.rs` (`check_crate`: `gate!`,
//! `gate_all!`, `soft_gate_all_legacy_dont_use!`), `compiler/rustc_session/src/diagnostics.rs`
//! (`feature_err_issue` stealing a stashed `feature_warn_issue`) and
//! `compiler/rustc_errors/src/lib.rs` (`stash_diagnostic`, `steal_non_err`,
//! `emit_stashed_diagnostics`) (MIT or Apache-2.0).

mod post;
mod pre;

use hir::InFile;
use syntax::{SyntaxNodePtr, TextRange};

use self::pre::Pre;
use super::{Features, Flags, allows_unstable, feature_err, feature_warn};
use crate::{
    cx::Cx,
    lints::syntax::{Walk, source_fact},
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// A span rustc's parser gates, and the node it is of.
#[derive(Clone, Copy, Debug)]
struct Gated {
    at: InFile<TextRange>,
    node: InFile<SyntaxNodePtr>,
}

/// The feature gates of the crate's syntax, in rustc's order.
pub(super) fn check(cx: &Cx<'_, '_>, walk: &Walk<'_>, features: &Features, _flags: &Flags) -> Vec<Found> {
    let spans = pre::spans(walk);
    let mut found = hard(cx, features, &spans);
    let mut stash = soft(cx, features, &spans);
    for posted in post::visit(cx, walk, features) {
        if let Some(primary) = posted.diag.span.primary_span() {
            stash.steal(primary);
        }
        let mut diag = posted.diag;
        if posted.fatal {
            diag.stop();
            found.push(Found::new(diag, Phase::Resolve));
            return found;
        }
        found.push(Found::new(diag, Phase::Resolve));
    }
    if found.is_empty() {
        found.extend(stash.warnings.into_iter().map(|(_, diag)| Found::new(diag, Phase::Stashed)));
    }
    found
}

/// Whether rustc's `gate!` errors at `at`: `feature` is not enabled and the macro `at` comes
/// from does not allow it.
fn is_gated(cx: &Cx<'_, '_>, features: &Features, feature: &str, at: InFile<TextRange>) -> bool {
    !features.enabled(feature) && !allows_unstable(cx, at.file_id, at.value, feature)
}

/// What `gate_all!` and the rules after it say of a feature: the feature `feature_err`
/// names, what it explains and the help it adds.
fn hard_gate(pre: Pre) -> Option<(&'static str, &'static str, Option<&'static str>)> {
    Some(match pre {
        Pre::BuiltinSyntax => ("builtin_syntax", "`builtin #` syntax is unstable", None),
        Pre::ConstClosures => ("const_closures", "const closures are experimental", None),
        Pre::ConstTraitImpl => ("const_trait_impl", "const trait impls are experimental", None),
        Pre::Coroutines => ("coroutines", "coroutine syntax is experimental", None),
        Pre::DefaultFieldValues => ("default_field_values", "default values on fields are experimental", None),
        Pre::ExplicitTailCalls => ("explicit_tail_calls", "`become` expression is experimental", None),
        Pre::Frontmatter => ("frontmatter", "frontmatters are experimental", None),
        Pre::GenBlocks => ("gen_blocks", "gen blocks are experimental", None),
        Pre::GenericConstItems => ("generic_const_items", "generic const items are experimental", None),
        Pre::ImplRestriction => ("impl_restriction", "`impl` restrictions are experimental", None),
        Pre::MinGenericConstArgs => {
            ("gca_min_const_items", "unbraced const blocks as const args are experimental", None)
        }
        Pre::MoreQualifiedPaths => {
            ("more_qualified_paths", "usage of qualified paths in this context is experimental", None)
        }
        Pre::MutRestriction => ("mut_restriction", "`mut` restrictions are experimental", None),
        Pre::ReturnTypeNotation => ("return_type_notation", "return type notation is experimental", None),
        Pre::SuperLet => ("super_let", "`super let` is experimental", None),
        Pre::TryBlocksHeterogeneous => {
            ("try_blocks_heterogeneous", "`try bikeshed` expression is experimental", None)
        }
        Pre::UnnamedEnumVariants => ("unnamed_enum_variants", "unnamed enum variants are experimental", None),
        Pre::UnsafeFields => ("unsafe_fields", "`unsafe` fields are experimental", None),
        Pre::YeetExpr => ("yeet_expr", "`do yeet` expression is experimental", None),
        Pre::AsyncTraitBounds => (
            "async_trait_bounds",
            "`async` trait bounds are unstable",
            Some("use the desugared name of the async trait, such as `AsyncFn`"),
        ),
        Pre::ClosureLifetimeBinder => (
            "closure_lifetime_binder",
            "`for<...>` binders for closures are experimental",
            Some("consider removing `for<...>`"),
        ),
        Pre::AssociatedConstEquality => ("gca_min_const_items", "associated const equality is incomplete", None),
        Pre::MgcaTypeConstSyntax => ("min_generic_const_args", "`type const` syntax is experimental", None),
        Pre::YieldExpr => ("yield_expr", "yield syntax is experimental", None),
        Pre::AutoTraits
        | Pre::DeclMacro
        | Pre::NegativeImpls
        | Pre::Specialization
        | Pre::TraitAlias
        | Pre::TryBlocks
        | Pre::MinSpecialization => return None,
    })
}

/// Whether rustc errors at `at` for the hard gate `pre`, by the rule `check_crate` reads it
/// with.
fn hard_gated(cx: &Cx<'_, '_>, features: &Features, pre: Pre, feature: &str, at: InFile<TextRange>) -> bool {
    match pre {
        // Part of `min_generic_const_args`: either enables it.
        Pre::MgcaTypeConstSyntax => {
            is_gated(cx, features, "min_generic_const_args", at) && is_gated(cx, features, "mgca_type_const_syntax", at)
        }
        // `coroutines` and `gen_blocks` imply `yield_expr`.
        Pre::YieldExpr => {
            is_gated(cx, features, "coroutines", at)
                && is_gated(cx, features, "gen_blocks", at)
                && is_gated(cx, features, "yield_expr", at)
        }
        _ => is_gated(cx, features, feature, at),
    }
}

/// The pre-expansion gates' errors, feature by feature in `check_crate`'s order.
fn hard(cx: &Cx<'_, '_>, features: &Features, spans: &pre::Spans) -> Vec<Found> {
    // alloc: empty, allocates only when a gate is hit
    let mut found = Vec::new();
    for (pre, gated_spans) in PRES.into_iter().zip(spans) {
        let Some((feature, explain, help)) = hard_gate(pre) else { continue };
        for gated in gated_spans {
            if !hard_gated(cx, features, pre, feature, gated.at) {
                continue;
            }
            let mut diag = feature_err(features, feature, cx.span(gated.at), explain);
            if let Some(help) = help {
                diag.help(help);
            }
            source_fact(cx, &mut diag, gated.node);
            found.push(Found::new(diag, Phase::Resolve));
        }
    }
    found
}

/// Every [`Pre`], at its index.
const PRES: [Pre; pre::COUNT] = [
    Pre::BuiltinSyntax,
    Pre::ConstClosures,
    Pre::ConstTraitImpl,
    Pre::Coroutines,
    Pre::DefaultFieldValues,
    Pre::ExplicitTailCalls,
    Pre::Frontmatter,
    Pre::GenBlocks,
    Pre::GenericConstItems,
    Pre::ImplRestriction,
    Pre::MinGenericConstArgs,
    Pre::MoreQualifiedPaths,
    Pre::MutRestriction,
    Pre::ReturnTypeNotation,
    Pre::SuperLet,
    Pre::TryBlocksHeterogeneous,
    Pre::UnnamedEnumVariants,
    Pre::UnsafeFields,
    Pre::YeetExpr,
    Pre::AsyncTraitBounds,
    Pre::ClosureLifetimeBinder,
    Pre::AssociatedConstEquality,
    Pre::MgcaTypeConstSyntax,
    Pre::YieldExpr,
    Pre::AutoTraits,
    Pre::DeclMacro,
    Pre::NegativeImpls,
    Pre::Specialization,
    Pre::TraitAlias,
    Pre::TryBlocks,
    Pre::MinSpecialization,
];

/// The warnings of the soft gates rustc stashed, by their spans, in its stash's order.
#[derive(Default)]
struct Stash {
    warnings: Vec<(Span, Diag)>,
}

impl Stash {
    /// `stash_diagnostic`: a warning at a span already stashed replaces it where it is.
    fn stash(&mut self, span: Span, diag: Diag) {
        match self.warnings.iter_mut().find(|it| it.0 == span) {
            Some(it) => it.1 = diag,
            None => self.warnings.push((span, diag)),
        }
    }

    /// `steal_non_err` of an error at `span`, which cancels the warning there: the stash is an
    /// `IndexMap` and rustc takes it out with `swap_remove`.
    fn steal(&mut self, span: &Span) {
        if let Some(at) = self.warnings.iter().position(|it| &it.0 == span) {
            self.warnings.swap_remove(at);
        }
    }
}

/// The soft gates' warnings, stashed in `check_crate`'s order.
fn soft(cx: &Cx<'_, '_>, features: &Features, spans: &pre::Spans) -> Stash {
    let soft_gates: [(Pre, &str, &str); 6] = [
        (Pre::AutoTraits, "auto_traits", "`auto` traits are unstable"),
        (Pre::DeclMacro, "decl_macro", "`macro` is experimental"),
        (Pre::NegativeImpls, "negative_impls", "negative impls are experimental"),
        (Pre::Specialization, "specialization", "specialization is experimental"),
        (Pre::TraitAlias, "trait_alias", "trait aliases are experimental"),
        (Pre::TryBlocks, "try_blocks", "`try` blocks are unstable"),
    ];
    let mut stash = Stash::default();
    let warn = |stash: &mut Stash, feature: &str, gated: &Gated, explain: &str| {
        let span = cx.span(gated.at);
        // clone: the warning's span, only when a soft gate is hit
        let mut diag = feature_warn(features, feature, span.clone(), explain);
        source_fact(cx, &mut diag, gated.node);
        stash.stash(span, diag);
    };
    for (pre, feature, explain) in soft_gates {
        for gated in &spans[pre as usize] {
            if is_gated(cx, features, feature, gated.at) {
                warn(&mut stash, feature, gated, explain);
            }
        }
    }
    for gated in &spans[Pre::MinSpecialization as usize] {
        if is_gated(cx, features, "specialization", gated.at) && is_gated(cx, features, "min_specialization", gated.at) {
            warn(&mut stash, "specialization", gated, "specialization is experimental");
        }
    }
    stash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Level;

    fn at(lo: u32) -> Span {
        Span { file: "a.rs".to_owned(), lo, hi: lo + 1, expansion: None }
    }

    fn warning(lo: u32) -> Diag {
        let mut diag = Diag::new(Level::Warning, None, format!("w{lo}"));
        diag.span = at(lo).into();
        diag
    }

    #[test]
    fn the_index_of_each_pre_is_its_place_in_the_table() {
        assert!(PRES.iter().enumerate().all(|(index, pre)| *pre as usize == index));
    }

    #[test]
    fn a_stolen_warning_is_swap_removed_and_a_restash_keeps_its_place() {
        let mut stash = Stash::default();
        for lo in [1, 2, 3, 4] {
            stash.stash(at(lo), warning(lo));
        }
        stash.stash(at(2), warning(20));
        stash.steal(&at(1));
        let order: Vec<&str> = stash.warnings.iter().map(|it| it.1.message.as_str()).collect();
        assert_eq!(order, ["w4", "w20", "w3"]);
    }

    #[test]
    fn only_the_rules_gates_have_no_feature_of_their_own() {
        assert_eq!(hard_gate(Pre::AssociatedConstEquality).map(|it| it.0), Some("gca_min_const_items"));
        assert_eq!(hard_gate(Pre::MgcaTypeConstSyntax).map(|it| it.0), Some("min_generic_const_args"));
        assert!(hard_gate(Pre::TryBlocks).is_none());
    }
}
