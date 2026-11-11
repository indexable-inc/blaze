//! `async_fn` (in `blaze::edition`, allowed by default): an `async fn`, an `async` block or an
//! `async` closure, each of which the blaze edition removes (docs/v2/plan/design/edition.md):
//! a call that waits parks the fiber it runs on, and the may-suspend effect is inferred from
//! the body, so neither `async` nor `.await` is written.
//!
//! One finding per outermost `async` construct: an `async` block or closure inside an `async
//! fn`, block or closure is not found again, and neither is an `.await`, which the language
//! allows only inside one of them, so each `.await` goes with the finding of the construct it
//! is in. The search up stops at the item the node is in: an `async fn` declared inside
//! another function is found on its own. What `async` is comes from the parsed tree (the
//! `async` keyword of the item, block or closure), and a construct a macro wrote with no
//! input of the file (the expansion of a derive, a macro's own `async` block) is not found.
//! There is no fix: a fiber's body is the author's to write.

use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::Lint;
use crate::check::Cx;

const LINT: Lint = Lint::AsyncFn;

/// The note every finding has: what the edition writes in place of it.
const NOTE: &str = "the blaze edition has no `async` or `.await`: the code runs on a fiber, a call that waits parks \
                    the fiber, and the may-suspend effect is inferred from the body";

/// An `async fn`.
pub(super) fn check_fn<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) || f.async_token().is_none() || !written(cx, f.syntax()) {
        return;
    }
    let Some(name) = f.name() else { return };
    let Some(range) = cx.range(name.syntax()) else { return };
    let message =
        format!("`async fn {}` is `async`, which the blaze edition removes", name.text());
    let span = cx.span(range);
    cx.blaze(LINT, f.syntax(), span, message)
        .note(NOTE)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// An `async` block or closure, `node` a block or closure expression, not inside another
/// `async` construct of its item.
pub(super) fn check_expr<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let what = if let Some(block) = ast::BlockExpr::cast(node) {
        if block.async_token().is_none() {
            return;
        }
        "an `async` block"
    } else if let Some(closure) = ast::ClosureExpr::cast(node) {
        if closure.async_token().is_none() {
            return;
        }
        "an `async` closure"
    } else {
        return;
    };
    if !written(cx, node) || inside_async(node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let message =
        format!("{what} in {} is `async`, which the blaze edition removes", item_of(node));
    let span = cx.span(range);
    cx.blaze(LINT, node, span, message)
        .note(NOTE)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// Whether `node` is code of the file: not what a derive expanded to, and, in what a macro
/// expanded to, handed to the macro.
fn written(cx: &Cx<'_, '_>, node: ast::Node<'_>) -> bool {
    cx.expansion().is_none() && cx.is_written(node)
}

/// Whether an `async` block, closure or `fn` holds `node`, up to the item `node` is in.
fn inside_async(node: ast::Node<'_>) -> bool {
    for at in node.ancestors().skip(1) {
        if let Some(block) = ast::BlockExpr::cast(at)
            && block.async_token().is_some()
        {
            return true;
        }
        if let Some(closure) = ast::ClosureExpr::cast(at)
            && closure.async_token().is_some()
        {
            return true;
        }
        if let Some(item) = ast::Item::cast(at) {
            return matches!(item, ast::Item::Fn(f) if f.async_token().is_some());
        }
    }
    false
}

/// How a message names the item `node` is written in: `` `fn run` ``, `` `const C` ``,
/// `` `static S` ``, or "this item".
fn item_of(node: ast::Node<'_>) -> String {
    let named = node.ancestors().find_map(ast::Item::cast).and_then(|item| match item {
        ast::Item::Fn(it) => it.name().map(|name| ("fn", name)),
        ast::Item::Const(it) => it.name().map(|name| ("const", name)),
        ast::Item::Static(it) => it.name().map(|name| ("static", name)),
        _ => None,
    });
    match named {
        // alloc: a finding's message part
        Some((kind, name)) => format!("`{kind} {}`", name.text()),
        None => "this item".to_owned(), // alloc: a finding's message part
    }
}
