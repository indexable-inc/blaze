//! What match checking lints: `unreachable_patterns`, `irrefutable_let_patterns`,
//! `bindings_with_variant_name` (E0170), `overlapping_range_endpoints` and
//! `non_contiguous_range_endpoints`.
//!
//! Follows `compiler/rustc_mir_build/src/thir/pattern/check_match.rs`
//! (`report_unreachable_pattern`, `report_arm_reachability`, `report_irrefutable_let_patterns`,
//! `check_let`, `check_for_bindings_named_same_as_variants`),
//! `compiler/rustc_mir_build/src/diagnostics.rs` (`UnreachablePatternInner`, `WantedConstant`,
//! `IrrefutableLetPatterns*`, `LetElseReplacementSuggestion`, `BindingsWithVariantName`),
//! `compiler/rustc_pattern_analysis/src/rustc.rs` (`lint_overlapping_range_endpoints`,
//! `lint_non_contiguous_range_endpoints`) and `compiler/rustc_pattern_analysis/src/diagnostics.rs`
//! (`OverlappingRangeEndpoints`, `ExclusiveRangeMissingMax`, `ExclusiveRangeMissingGap`) (MIT or
//! Apache-2.0). Which patterns are redundant and why is match checking's
//! (`hir::DefWithBody::match_lints`); rustc checks no body that failed type checking.

use hir::{DefWithBody, InFile, IrrefutableLet, LetSource, MatchLint, PatternTypo, view::HasSourceView as _};
use syntax::{
    AstPtr, SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode, HasName as _},
};

use super::BodyCx;
use crate::{
    cx::def_range_view,
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span},
};

/// rustc's `CAP_COVERED_BY_MANY`.
const COVERED_BY_MANY: usize = 4;

/// The lints this module gives: it runs only when one may fire.
pub(super) const LINTS: super::LintSet = super::lint_set([
    "unreachable_patterns",
    "irrefutable_let_patterns",
    "bindings_with_variant_name",
    "overlapping_range_endpoints",
    "non_contiguous_range_endpoints",
]);

pub(super) fn lints(b: &BodyCx<'_, '_, '_>) -> Vec<Found> {
    if b.tainted() {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let lints = DefWithBody::from(b.owner).match_lints(b.db());
    lints
        .iter()
        .filter_map(|lint| match lint {
            MatchLint::UnreachablePattern(it) => unreachable_pattern(b, it),
            MatchLint::IrrefutableLet(it) => irrefutable_let(b, it),
            MatchLint::BindingWithVariantName(it) => binding_with_variant_name(b, it),
            MatchLint::OverlappingRangeEndpoints(it) => overlapping_range_endpoints(b, it),
            MatchLint::NonContiguousRangeEndpoints(it) => non_contiguous_range_endpoints(b, it),
        })
        // alloc: the lints found, empty until one is
        .collect()
}

fn unreachable_pattern<'db>(b: &BodyCx<'_, '_, 'db>, d: &hir::UnreachablePattern<'db>) -> Option<Found> {
    let cx = b.cx;
    let at = pat_node(d.pat);
    let pat = cx.span_of(at);
    // clone: a lint's span, for a lint found
    let mut diag = Diag::lint("unreachable_patterns", "unreachable pattern", pat.clone());
    // alloc: the lint's text, for a lint found
    let covering: Vec<Span> = d.covered_by.iter().map(|it| cx.span_of_ptr(*it)).collect();
    match covering.as_slice() {
        [] => {
            // alloc: the lint's text, for a lint found
            diag.span_label(pat.clone(), format!("matches no values because `{}` is uninhabited", cx.ty(&d.ty)));
            diag.note(
                "to learn more about uninhabited types, see https://doc.rust-lang.org/nomicon/exotic-sizes.html#empty-types",
            );
        }
        [covering] if d.catchall => {
            // clone: a lint's span, for a lint found
            diag.span_label(pat.clone(), "no value can reach this");
            // clone: a lint's span, for a lint found
            diag.span_label(covering.clone(), "matches any value");
            if let Some(typo) = &d.typo {
                pattern_typo(b, &mut diag, &pat, typo);
            }
        }
        [covering] => {
            // clone: a lint's span, for a lint found
            diag.span_label(pat.clone(), "no value can reach this");
            // clone: a lint's span, for a lint found
            diag.span_label(covering.clone(), "matches all the relevant values");
        }
        covering => {
            // clone: a lint's span, for a lint found
            diag.span_label(pat.clone(), "no value can reach this");
            // clone: a lint's span, for a lint found
            let mut spans = MultiSpan::of(pat.clone());
            for it in covering.iter().take(COVERED_BY_MANY) {
                // alloc: the lint's text, for a lint found
                spans.labels.push((it.clone(), "matches some of the same values".to_owned()));
            }
            let label = match covering.len().saturating_sub(COVERED_BY_MANY) {
                // alloc: the lint's text, for a lint found
                0 => "collectively making this unreachable".to_owned(),
                // alloc: the lint's text, for a lint found
                remain => format!("...and {remain} other patterns collectively make this unreachable"),
            };
            // clone: a lint's span, for a lint found
            spans.labels.push((pat.clone(), label));
            diag.span_note(spans, "multiple earlier patterns match some of the same values");
        }
    }
    if covering.is_empty()
        && let Some(arm) = d.arm
    {
        diag.suggestion(whole_arm(b, arm)?, "remove the match arm", "", Applicability::MachineApplicable);
    }
    Some(b.lint(diag, Phase::Mir, at))
}

/// `find_fallback_pattern_typo`'s part of the lint.
fn pattern_typo(b: &BodyCx<'_, '_, '_>, diag: &mut Diag, pat: &Span, typo: &PatternTypo) {
    let cx = b.cx;
    match typo {
        PatternTypo::WantedConstant { is_typo, const_name, const_path } => {
            let what = if *is_typo { "similarly named constant" } else { "constant" };
            diag.suggestion(
                // clone: a lint's span, for a lint found
                pat.clone(),
                // alloc: the lint's text, for a lint found
                format!(
                    "you might have meant to pattern match against the value of {what} `{const_name}` instead of introducing a new catch-all binding"
                ),
                // clone: a lint's span, for a lint found
                const_path.clone(),
                Applicability::MachineApplicable,
            );
        }
        PatternTypo::AccessibleConstant(name) => {
            diag.span_note(
                cx.span(*name),
                "there is a constant of the same name imported in another scope, which could have been used to pattern match against its value instead of introducing a new catch-all binding, but it needs to be imported in the pattern's scope",
            );
        }
        PatternTypo::InaccessibleConstant(konst) => {
            let Some(source) = konst.source_view(cx.db) else { return };
            let node = source.map(|it| it.syntax());
            let range = def_range_view(node.value);
            diag.span_note(
                cx.span(InFile::new(node.file_id, range)),
                "there is a constant of the same name, which could have been used to pattern match against its value instead of introducing a new catch-all binding, but it is not accessible from this scope",
            );
        }
        PatternTypo::PatternLetBinding(name) => {
            diag.span_note(
                cx.span(*name),
                "there is a binding of the same name; if you meant to pattern match against the value of that binding, that is a feature of constants that is not available for `let` bindings",
            );
        }
    }
}

/// rustc's `whole_arm_span`: the arm, from its pattern to its value, with the `,` after it.
fn whole_arm(b: &BodyCx<'_, '_, '_>, arm: InFile<AstPtr<syntax::ast::MatchArm>>) -> Option<Span> {
    let node = b.cx.node_view(arm);
    let start = node.value.pat()?.syntax().text_range().start();
    let end = match node.value.comma_token() {
        Some(comma) => comma.text_range().end(),
        None => node.value.expr()?.syntax().text_range().end(),
    };
    Some(b.cx.span(InFile::new(node.file_id, TextRange::new(start, end))))
}

fn irrefutable_let(b: &BodyCx<'_, '_, '_>, d: &IrrefutableLet) -> Option<Found> {
    let cx = b.cx;
    let (message, note, help) = match d.source {
        LetSource::IfLet => (
            "irrefutable `if let` pattern",
            "this pattern will always match, so the `if let` is useless",
            "consider replacing the `if let` with a `let`",
        ),
        LetSource::IfLetGuard => (
            "irrefutable `if let` guard pattern",
            "this pattern will always match, so the guard is useless",
            "consider removing the guard and adding a `let` inside the match arm",
        ),
        LetSource::WhileLet => (
            "irrefutable `while let` pattern",
            "this pattern will always match, so the loop will never exit",
            "consider instead using a `loop { ... }` with a `let` inside it",
        ),
        LetSource::LetElse => return let_else(b, d),
    };
    let mut diag = Diag::lint("irrefutable_let_patterns", message, cx.span_of(d.at));
    diag.note(note).help(help);
    Some(b.lint(diag, Phase::Mir, d.at))
}

/// `LetSource::LetElse`: at `else`, with the `let` labelled, unless the `let` is a macro's.
fn let_else(b: &BodyCx<'_, '_, '_>, d: &IrrefutableLet) -> Option<Found> {
    let cx = b.cx;
    let node = cx.syntax_view(d.at);
    let stmt = ast::LetStmt::cast(node.value)?;
    let start = stmt.let_token()?.text_range().start();
    let end = stmt.initializer()?.syntax().text_range().end();
    let span = cx.span(InFile::new(node.file_id, TextRange::new(start, end)));
    let mut diag = match span.expansion.is_some() {
        // clone: a lint's span, for a lint found
        true => Diag::lint("irrefutable_let_patterns", "unreachable `else` clause", span.clone()),
        false => {
            let keyword = cx.span(InFile::new(node.file_id, stmt.let_else()?.else_token()?.text_range()));
            let text = cx.file_text(cx.file_of(&keyword)?).get(keyword.lo as usize..)?;
            let len = text.find(char::is_whitespace).unwrap_or(text.len());
            let keyword = keyword.with_hi(keyword.lo + len as u32);
            let mut diag = Diag::lint("irrefutable_let_patterns", "unreachable `else` clause", keyword);
            // clone: a lint's span, for a lint found
            diag.span_label(span.clone(), "assigning to binding pattern will always succeed");
            diag
        }
    };
    diag.note("this pattern always matches, so the else clause is unreachable");
    if let Some((pat, inner)) = d.replacement
        && let Some(lhs) = cx.snippet(&cx.span_of_ptr(pat))
        && let Some(rhs) = cx.snippet(&cx.span_of_ptr(inner))
    {
        diag.suggestion(
            span,
            // alloc: the lint's text, for a lint found
            format!("consider using `let Some({lhs}) = {rhs}` to match on a specific variant"),
            // alloc: the lint's text, for a lint found
            format!("let Some({lhs}) = {rhs}"),
            Applicability::MachineApplicable,
        );
    }
    Some(b.lint(diag, Phase::Mir, d.at))
}

fn binding_with_variant_name(b: &BodyCx<'_, '_, '_>, d: &hir::BindingWithVariantName) -> Option<Found> {
    let cx = b.cx;
    let at = d.pat.map(|it| it.syntax_node_ptr());
    // alloc: the lint's text, for a lint found
    let name = cx.node_view(d.pat).value.name()?.text();
    let span = cx.span_of(at);
    let ty_path = crate::print::def_path_untrimmed(cx, hir::ModuleDef::Adt(d.enum_.into()));
    let mut diag = Diag::lint(
        "bindings_with_variant_name",
        // alloc: the lint's text, for a lint found
        format!("pattern binding `{name}` is named the same as one of the variants of the type `{ty_path}`"),
        // clone: a lint's span, for a lint found
        span.clone(),
    );
    if d.suggest {
        diag.suggestion(
            span,
            "to match on the variant, qualify the path",
            // alloc: the lint's text, for a lint found
            format!("{ty_path}::{name}"),
            Applicability::MachineApplicable,
        );
    }
    Some(b.lint(diag, Phase::Mir, at))
}

fn overlapping_range_endpoints(b: &BodyCx<'_, '_, '_>, d: &hir::OverlappingRangeEndpoints) -> Option<Found> {
    let cx = b.cx;
    let range = cx.span_of_ptr(d.pat);
    let message = "multiple patterns overlap on their endpoints";
    let mut diag = Diag::lint("overlapping_range_endpoints", message, range.clone()); // clone: the lint's span, the edge
    diag.span_label(range, "... with this range");
    for it in &d.overlaps_with {
        // alloc: a label of the lint, the edge
        diag.span_label(cx.span_of_ptr(*it), format!("this range overlaps on `{}`...", d.overlap));
    }
    diag.note("you likely meant to write mutually exclusive ranges");
    Some(b.lint(diag, Phase::Mir, match_level(b, d.pat)))
}

/// `ExclusiveRangeMissingMax` when no range starts at the gap (it is the type's maximum), else
/// `ExclusiveRangeMissingGap`.
fn non_contiguous_range_endpoints(b: &BodyCx<'_, '_, '_>, d: &hir::NonContiguousRangeEndpoints) -> Option<Found> {
    let cx = b.cx;
    let first_range = cx.span_of_ptr(d.pat);
    let gap = &d.gap;
    let message = match d.gapped_with.is_empty() {
        true => format!("exclusive range missing `{gap}`"), // alloc: the lint's message, the edge
        false => "multiple ranges are one apart".to_owned(), // alloc: as above
    };
    // clone: the lint's span, the edge
    let mut diag = Diag::lint("non_contiguous_range_endpoints", message, first_range.clone());
    let label = format!("this range doesn't match `{gap}` because `..` is an exclusive range"); // alloc: a label, the edge
    diag.span_label(first_range.clone(), label); // clone: as above
    if !d.gapped_with.is_empty() {
        // rustc's `PatRange` printed with its own end: `suggestion` is it with `..=`.
        let printed = d.suggestion.replacen("..=", "..", 1);
        for it in &d.gapped_with {
            diag.span_label(
                cx.span_of_ptr(*it),
                // alloc: a label, the edge
                format!("this could appear to continue range `{printed}`, but `{gap}` isn't matched by either of them"),
            );
        }
    }
    let suggestion = d.suggestion.as_str();
    diag.suggestion(first_range, "use an inclusive range instead", suggestion, Applicability::MaybeIncorrect);
    Some(b.lint(diag, Phase::Mir, match_level(b, d.pat)))
}

/// rustc's `match_lint_level`: the `match`, `let` or parameter the pattern is part of, whose
/// level the range lints take (an arm's own attributes don't count).
fn match_level(b: &BodyCx<'_, '_, '_>, pat: InFile<AstPtr<syntax::ast::Pat>>) -> InFile<SyntaxNodePtr> {
    let node = b.cx.syntax_view(pat_node(pat));
    for ancestor in b.cx.vsema.ancestors_with_macros_file(node).skip(1) {
        match ancestor.value.kind() {
            SyntaxKind::MATCH_ARM
            | SyntaxKind::MATCH_ARM_LIST
            | SyntaxKind::RECORD_PAT_FIELD
            | SyntaxKind::RECORD_PAT_FIELD_LIST
            | SyntaxKind::MACRO_CALL
            | SyntaxKind::TOKEN_TREE => {}
            kind if ast::Pat::can_cast(kind) => {}
            _ => return ancestor.map(ast::ptr),
        }
    }
    pat_node(pat)
}

fn pat_node(pat: InFile<AstPtr<syntax::ast::Pat>>) -> InFile<SyntaxNodePtr> {
    pat.map(|it| it.syntax_node_ptr())
}
