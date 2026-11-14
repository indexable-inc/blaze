//! The suspicious-code lints: code that compiles and reads as right but leaves something implicit
//! that a reader (a person or an LLM editing it later) has to reconstruct, and gets wrong. An
//! early exit that skips the second half of a pair, a write left half done before a `?`, a
//! conversion whose target only inference knows, a fallback that hides the failure it answers,
//! a test that checks nothing, a doc that names an item that is not there. Each is found by name
//! resolution, inferred types or the MIR's control flow, never by text.
//!
//! One module per lint under `suspicious/`; this one dispatches them. Each lint's module says what
//! it finds, what it leaves alone and the right form; docs/lints.md says the same for people.
//! Findings are reported through `std_mistakes::report`, the one home of a finding with its fix.

mod doc_names_nothing;
mod fallback;
mod glob_import;
mod half_done;
mod hidden_exit;
mod ignored_without_reason;
mod option_question_mark_hides;
mod placeholder_body;
mod question_mark_converts;
mod shadow_changes_meaning;
mod skipped_pair;
mod test_asserts_nothing;
mod test_misses_its_subject;
#[cfg(test)]
mod tests;
mod unseen_target;
mod unused_parameter;
mod versioned_name;

use syntax::view as ast;

use super::Lint;
use crate::check::Cx;

/// The lints of this module.
const LINTS: &[Lint] = &[
    Lint::SkippedPair,
    Lint::HalfDone,
    Lint::HiddenExit,
    Lint::QuestionMarkConverts,
    Lint::OptionQuestionMarkHides,
    Lint::UnseenTarget,
    Lint::GlobImport,
    Lint::ShadowChangesMeaning,
    Lint::VersionedName,
    Lint::UnusedParameter,
    Lint::Fallback,
    Lint::PlaceholderBody,
    Lint::TestAssertsNothing,
    Lint::AssertionCannotFail,
    Lint::TestMissesItsSubject,
    Lint::IgnoredWithoutReason,
    Lint::DocNamesNothing,
];

/// The whole-body lints, once per body, from `blaze.rs`'s body pass (the root is the body's
/// item and the walk is in the file's own tree).
pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !LINTS.iter().any(|it| cx.on_blaze(*it)) || cx.analysis().is_none() {
        return;
    }
    skipped_pair::check_body(cx);
    half_done::check_body(cx);
    placeholder_body::check_body(cx);
    test_asserts_nothing::check_body(cx);
    test_misses_its_subject::check_body(cx);
    unused_parameter::check_body(cx);
    shadow_changes_meaning::check_body(cx);
}

/// Every node of a file and of each body: each lint looks at the kinds it finds and returns at
/// once otherwise. Nothing a macro made is looked at.
pub(super) fn check_node<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !LINTS.iter().any(|it| cx.on_blaze(*it)) || cx.expansion().is_some() || !cx.is_written(node)
    {
        return;
    }
    hidden_exit::check(cx, node);
    question_mark_converts::check(cx, node);
    option_question_mark_hides::check(cx, node);
    unseen_target::check(cx, node);
    glob_import::check(cx, node);
    versioned_name::check(cx, node);
    fallback::check(cx, node);
    ignored_without_reason::check(cx, node);
    doc_names_nothing::check(cx, node);
}
