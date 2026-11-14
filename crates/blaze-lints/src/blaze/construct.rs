//! Correct by construction (AGENTS.md: type, then lint, then rule; verify once): the lints
//! that find where a check at run time, a convention or a comment is standing in for a type.
//! Each finding says three things: what was found, the type that would carry it, and what that
//! type removes (the check, the convention, the class of mistake).
//!
//! Every lint here is decided on the typed tree (HIR, inference, name resolution, the crate's
//! field facts and `hir_ty_pure::reach`'s proof of the body's MIR), never on text: a name is
//! read as the name a binding, a field or a method has, and only where its type agrees.
//!
//! - [`proof_forgotten`]: an `unwrap`, `expect` or index blaze proves cannot fail, because a
//!   check of the same local just before it already decided it: the check's proof is dropped
//!   and taken again at run time. The type is the one the check hands out (`if let Some(x)`,
//!   `let Ok(x) = .. else`, `v.get(i)`).
//! - [`units`]: `unit_mismatch` (a sum, difference or comparison of two values whose names
//!   carry different units) and `primitive_meaning` (a parameter or field whose name carries a
//!   unit or an identity and whose type is a bare integer). The type is `Duration`, a byte
//!   count newtype, or a typed id.
//! - [`sentinel_value`]: a value compared with `-1`, `MAX` or `""` as a stand-in for "none".
//!   The type is `Option<T>` (with a niche: `Option<NonZero>`).
//! - [`flags`]: `state_in_flags` (two or more state-word `bool` fields of one struct: a state
//!   machine kept as flags) and `bool_pair_state` (a `(bool, bool)`, an `Option<bool>`, or a
//!   `match` on a tuple of `bool`s). The type is an enum of the states.
//! - [`out_parameter`]: a `&mut` parameter only ever written through, beside a `bool` or `()`
//!   return. The type is the return value: `Option<T>` or `Result<T, E>`.
//! - [`parallel_collections`]: two `Vec` fields of one value pushed one after the other, or
//!   indexed by the same index: two collections kept the same length by convention. The type
//!   is one `Vec` of a struct.
//! - [`untyped_index`]: an integer field the crate only uses to index one collection. The type
//!   is a typed index (`la_arena::Idx<T>` or a newtype per collection).
//! - [`validated_but_raw`]: a function that checks its primitive parameter and then hands the
//!   same raw value back: the check's result is thrown away. The type is a validated newtype
//!   whose constructor is the check (`TryFrom`).
//!
//! The ranged-integer and invariant lanes (`Int<LO, HI>`, `#[blaze::invariant]`, `requires`)
//! are the destinations that turn several of these into proofs; a message names them where
//! they apply, and nothing here depends on their code.

mod flags;
mod out_parameter;
mod parallel_collections;
mod proof_forgotten;
mod sentinel_value;
#[cfg(test)]
mod tests;
mod units;
mod untyped_index;
mod validated_but_raw;

use hir_def::hir::ExprId;
use syntax::view::{self as ast, AstNode as _};

use super::{Finding, Lint, facts, test_code};
use crate::{check::Cx, diag::Applicability};

/// The item lints: run at each node of the file's walk.
pub(super) fn check_node<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if let Some(item) = ast::Struct::cast(node) {
        flags::check_struct(cx, item);
        untyped_index::check(cx, item);
        units::check_struct(cx, item);
    }
    if let Some(f) = ast::Fn::cast(node) {
        units::check_fn(cx, f);
        flags::check_fn(cx, f);
    }
}

/// The body lints: run once per body, at its root.
pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if test_code::is_test_code(cx, cx.root()) {
        return;
    }
    proof_forgotten::check(cx);
    units::check_body(cx);
    sentinel_value::check(cx);
    flags::check_body(cx);
    out_parameter::check(cx);
    parallel_collections::check(cx);
    validated_but_raw::check(cx);
}

/// A finding of this module: the three parts every message has, and the fix where one is
/// mechanical.
struct Found {
    lint: Lint,
    range: syntax::TextRange,
    /// What was found.
    what: String,
    /// The type that would carry it.
    carry: String,
    /// What that type removes.
    removes: &'static str,
    fix: Option<Fix>,
}

/// A fix: its message, its parts (each a range of the file and the text written there) and
/// how sure it is.
struct Fix {
    message: &'static str,
    parts: Vec<(syntax::TextRange, String)>,
    applicability: Applicability,
}

/// Reports `found`, each with its three parts and its fix.
fn report(cx: &mut Cx<'_, '_>, found: Vec<Found>) {
    for it in found {
        let parts = it.fix.map(|fix| {
            let parts: Vec<_> = fix.parts.into_iter().map(|(at, text)| (cx.span(at), text)).collect(); // alloc: one fix's parts
            (fix.message, parts, fix.applicability)
        });
        let finding = Finding {
            lint: it.lint,
            range: it.range,
            message: it.what,
            note: Some(format!("carry it in a type: {}, which removes {}", it.carry, it.removes)), // alloc: a message's part
        };
        let diagnostic = facts::report_one(cx, finding);
        if let Some((message, parts, applicability)) = parts {
            diagnostic.multipart_suggestion(message, parts, applicability);
        }
    }
}

/// Reports one item finding at `node`, with its three parts.
fn report_item<'t>(
    cx: &mut Cx<'_, 't>,
    lint: Lint,
    node: ast::Node<'t>,
    what: String,
    carry: String,
    removes: &'static str,
) {
    let Some(range) = cx.range(node) else { return };
    let span = cx.span(range);
    cx.blaze(lint, node, span, what)
        .note(format!("carry it in a type: {carry}, which removes {removes}")) // alloc: a message's part
        .help(lint.help())
        .help(format!("for further information visit {}", lint.docs_url())); // alloc: a help line
}

/// Whether the item lint `lint` looks at `node`: on, written in the file, not a macro's, not
/// test code.
fn item_on<'t>(cx: &Cx<'_, 't>, lint: Lint, node: ast::Node<'t>) -> bool {
    cx.on_blaze(lint)
        && cx.expansion().is_none()
        && cx.is_written(node)
        && !cx.is_in_test(node)
}

/// Whether `expr` of `analysis`'s body is written in the body's own file, not by a macro.
fn written(analysis: &hir::BodyAnalysis<'_>, expr: ExprId) -> bool {
    let file = analysis.tree().1;
    analysis.source_map().expr_syntax(expr).is_ok_and(|it| it.file_id == file)
}

/// The `_`-separated words of `name`, compared by the caller in any case.
fn words(name: &str) -> impl Iterator<Item = &str> {
    name.split('_').filter(|it| !it.is_empty())
}
