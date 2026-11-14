//! The suggestion lints (`blaze::suggest`): where a plain Rust project would gain from a blaze
//! feature or a tighter type, one finding at a time, each saying what adopting it buys. They are
//! the second step of the adoption ladder (`blaze_findings::ladder::Step::Suggest`, set by
//! `step = "suggest"` in `[workspace.metadata.blaze.lints]`), so a project that starts as plain
//! Rust takes blaze's checks one step at a time.
//!
//! The group is `blaze::Kind::Suggest`, disjoint from the other kinds: these four and
//! `could_be_program_lifetime` and `could_be_ref32`, which live beside `program_lifetime`. The
//! `unmarked_*` lints are the edition's (they write a mark the edition requires).
//! This module dispatches the four it holds; each module says what it finds, what it leaves
//! alone, its class in the findings scheduler (`blaze_findings::pass::Class`) and why.

mod could_be_linear;
mod could_be_narrower;
mod could_be_sans_io;
mod could_have_invariant;
#[cfg(test)]
mod tests;

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use super::Lint;
use crate::check::Cx;

/// The lints of this module.
const LINTS: &[Lint] = &[Lint::CouldBeNarrower, Lint::CouldBeSansIo, Lint::CouldBeLinear, Lint::CouldHaveInvariant];

/// Every node of a file or a body: each lint looks at the kinds it finds. Nothing a macro made
/// is looked at.
pub(super) fn check_node<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !matches!(node.kind(), SyntaxKind::FN | SyntaxKind::SOURCE_FILE | SyntaxKind::IMPL) {
        return;
    }
    if !LINTS.iter().any(|it| cx.on_blaze(*it)) || cx.expansion().is_some() || !cx.is_written(node)
    {
        return;
    }
    match node.kind() {
        SyntaxKind::FN => {
            // A function is linted by its own body's query, whose tree's root it is.
            if node != cx.root() || cx.analysis().is_none() {
                return;
            }
            let Some(f) = ast::Fn::cast(node) else { return };
            could_be_narrower::check(cx, f);
            could_be_linear::check(cx, f);
        }
        SyntaxKind::SOURCE_FILE => {
            if node == cx.root() && cx.in_file() && cx.body_def().is_none() {
                could_be_sans_io::check(cx, node);
            }
        }
        SyntaxKind::IMPL => {
            if let Some(imp) = ast::Impl::cast(node) {
                could_have_invariant::check(cx, imp);
            }
        }
        _ => {}
    }
}
