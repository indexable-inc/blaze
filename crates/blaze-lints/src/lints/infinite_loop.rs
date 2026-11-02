//! `infinite_loop`: a `loop` that nothing leaves, in a function that does not return `!`: the
//! signature says it ends and the loop says it doesn't. A loop that waits for progress needs a
//! bound and an outcome (AGENTS.md rule 13); one meant to run forever says so with `-> !`.
//!
//! Follows clippy's `clippy_lints/src/loops/infinite_loop.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: a `loop` written in the file, in a
//! function or closure whose written return type is not `!` (an `async` block is not the
//! owner, the function around it is), not in an `async` block or closure unless the loop is
//! inside an `.await`; the loop is left by a `break` not inside an inner loop or naming it, a
//! `continue` of an outer loop, a `return`, a `?`, or a call of a function that returns `!`,
//! none of them in a closure, an `async` or `const` block or an item. Unlike clippy, which
//! stops at the macros it can't see into, the walk goes into what macros expand to (a
//! `panic!` is a call that returns `!`), and the message names the function. The `-> !` fix is
//! offered, maybe incorrect, where the function has no return type and the loop is its tail.

use syntax::{
    SyntaxKind as K, TextRange, WalkEvent,
    view::{self as ast, AstNode as _, HasLoopBody as _, HasName as _},
};

use super::{Implemented, unwrap_in_result::written_ancestors};
use crate::{check::Cx, diag::Applicability};

const LINT: Implemented = Implemented::InfiniteLoop;

/// How deep the walk goes into macro expansions.
const DEEPEST: u32 = 8;

/// The function or closure a loop is in.
enum Owner<'t> {
    Fn(ast::Fn<'t>),
    Closure(ast::ClosureExpr<'t>),
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, lp: ast::LoopExpr<'t>) {
    if !cx.on(LINT) || !cx.is_written(lp.syntax()) {
        return;
    }
    let Some(owner) = owner(cx, lp.syntax()) else { return };
    let ret = match owner {
        Owner::Fn(f) => f.ret_type(),
        Owner::Closure(c) => c.ret_type(),
    };
    if ret.and_then(|it| it.ty()).is_some_and(|it| matches!(it, ast::Type::NeverType(_))) {
        return;
    }
    let Some(body) = lp.loop_body() else { return };
    let mut visitor = Leaves {
        cx,
        label: label_of(lp.label()),
        inner: Vec::new(), // alloc: the labels of the loops nested in one loop
        depth: 0,
        left: false,
    };
    if let Some(label) = visitor.label {
        visitor.inner.push(label);
    }
    visitor.walk(body.syntax(), 0);
    if visitor.left {
        return;
    }
    let Some(range) = cx.range(lp.syntax()) else { return };
    let name = match owner {
        Owner::Fn(f) => f.name().map_or_else(|| "this function".into(), |it| format!("`{}`", it.text())),
        Owner::Closure(_) => "this closure".into(),
    };
    let fix = match owner {
        Owner::Fn(f) if f.ret_type().is_none() && is_tail(f, lp) => f
            .param_list()
            .and_then(|it| cx.range(it.syntax()))
            .map(|it| cx.span(TextRange::empty(it.end()))),
        _ => None,
    };
    let span = cx.span(range);
    let message = format!(
        "this `loop` in {name} never ends: nothing in it breaks, returns or calls a function that doesn't return"
    );
    let diagnostic = cx.lint(LINT, lp.syntax(), span, message);
    match fix {
        Some(at) => diagnostic.suggestion(at, "if it is meant to run forever, say so", " -> !", Applicability::MaybeIncorrect),
        None => diagnostic.note("if it is meant to run forever, the function's return type says so: `-> !`"),
    };
    diagnostic.help(
        "a loop that waits for progress needs a bound (fuel, a deadline) and ends in a typed outcome (AGENTS.md rule 13)",
    );
}

/// The function or closure `node` runs in: `None` when that is a constant or static, the loop
/// is in an `async` block not awaited, or the function returns `!` by what it resolves to.
fn owner<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<Owner<'t>> {
    let mut in_async = false;
    let mut awaited = false;
    for it in written_ancestors(cx, node).skip(1) {
        match it.kind() {
            K::AWAIT_EXPR => awaited = true,
            K::BLOCK_EXPR if ast::BlockExpr::cast(it).is_some_and(|it| it.async_token().is_some()) => in_async = true,
            K::CLOSURE_EXPR => {
                let closure = ast::ClosureExpr::cast(it)?;
                if closure.async_token().is_some() {
                    in_async = true;
                }
                return (!in_async || awaited).then_some(Owner::Closure(closure));
            }
            K::FN => return (!in_async || awaited).then_some(Owner::Fn(ast::Fn::cast(it)?)),
            K::CONST | K::STATIC => return None,
            _ => {}
        }
    }
    None
}

/// Whether `lp` is what `f`'s body evaluates to: its tail, or its last statement unended.
fn is_tail(f: ast::Fn<'_>, lp: ast::LoopExpr<'_>) -> bool {
    let Some(stmts) = f.body().and_then(|it| it.stmt_list()) else { return false };
    match stmts.tail_expr() {
        Some(tail) => tail.syntax() == lp.syntax(),
        None => stmts.statements().last().is_some_and(|it| match it {
            ast::Stmt::ExprStmt(stmt) => stmt.expr().is_some_and(|it| it.syntax() == lp.syntax()),
            _ => false,
        }),
    }
}

fn label_of(label: Option<ast::Label<'_>>) -> Option<&str> {
    Some(label?.lifetime()?.text())
}

/// clippy's `LoopVisitor`: whether anything leaves the loop.
struct Leaves<'c, 'q, 't> {
    cx: &'c Cx<'q, 't>,
    /// The loop's own label.
    label: Option<&'t str>,
    /// The labels of the loop and the loops it is in, at this point of the walk.
    inner: Vec<&'t str>,
    /// How many loops inside the loop the walk is.
    depth: u32,
    left: bool,
}

impl<'t> Leaves<'_, '_, 't> {
    fn walk(&mut self, node: ast::Node<'t>, expansions: u32) {
        let mut events = node.preorder();
        while let Some(event) = events.next() {
            if self.left {
                return;
            }
            let WalkEvent::Enter(it) = event else { continue };
            match it.kind() {
                K::BREAK_EXPR => {
                    let label = ast::BreakExpr::cast(it).and_then(|it| it.lifetime()).map(|it| it.text());
                    if self.depth == 0 || (label.is_some() && label == self.label) {
                        self.left = true;
                    }
                }
                K::CONTINUE_EXPR => {
                    let label = ast::ContinueExpr::cast(it).and_then(|it| it.lifetime()).map(|it| it.text());
                    if label.is_some_and(|it| !self.inner.contains(&it)) {
                        self.left = true;
                    }
                }
                K::RETURN_EXPR | K::TRY_EXPR | K::BECOME_EXPR => self.left = true,
                K::LOOP_EXPR | K::WHILE_EXPR | K::FOR_EXPR => {
                    events.skip_subtree();
                    let label = ast::LoopExpr::cast(it)
                        .and_then(|it| it.label())
                        .or_else(|| ast::WhileExpr::cast(it).and_then(|it| it.label()))
                        .or_else(|| ast::ForExpr::cast(it).and_then(|it| it.label()));
                    let label = label_of(label);
                    label.iter().for_each(|it| self.inner.push(it));
                    self.depth += 1;
                    for child in it.children() {
                        self.walk(child, expansions);
                    }
                    self.depth -= 1;
                    if label.is_some() {
                        self.inner.pop();
                    }
                }
                K::CALL_EXPR | K::METHOD_CALL_EXPR => {
                    let never = ast::Expr::cast(it).and_then(|it| self.cx.ty(it)).is_some_and(|it| it.is_never());
                    if never {
                        self.left = true;
                    }
                }
                K::MACRO_CALL => {
                    events.skip_subtree();
                    let Some(call) = ast::MacroCall::cast(it) else { continue };
                    match self.cx.expand(call) {
                        Some(expansion) if expansions < DEEPEST => self.walk(expansion, expansions + 1),
                        // What it does can't be seen: not called infinite.
                        _ => self.left = true,
                    }
                }
                K::CLOSURE_EXPR | K::FN | K::CONST | K::STATIC | K::IMPL | K::TRAIT | K::MODULE => {
                    events.skip_subtree();
                }
                K::BLOCK_EXPR => {
                    if ast::BlockExpr::cast(it).is_some_and(|it| it.async_token().is_some() || it.const_token().is_some()) {
                        events.skip_subtree();
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "infinite_loop";
    const FOUND: &str = "never ends";

    #[test]
    fn a_loop_nothing_leaves_is_found_and_one_left_by_break_return_or_a_never_call_is_not() {
        let found = lint_only(
            LINT,
            r#"
pub fn work() {}
pub fn more() -> bool { false }
pub fn spin() {
    loop { work(); }
}
pub fn nested() -> u8 {
    loop {
        loop { break; }
        work();
    }
}
pub fn breaks() { loop { break; } }
pub fn labelled() { 'outer: loop { loop { break 'outer; } } }
pub fn continues() { 'outer: while more() { loop { continue 'outer; } } }
pub fn returns() -> u8 { loop { return 1; } }
pub fn exits() { loop { std::process::exit(0); } }
pub fn forever() -> ! { loop { work(); } }
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("in `spin` never ends"), "{found}");
        assert!(found.contains("in `nested` never ends"), "{found}");
    }

    #[test]
    fn a_loop_in_an_async_block_not_awaited_is_not_found() {
        let found = lint_only(
            LINT,
            r#"
pub fn work() {}
pub fn spawned() { let _job = async { loop { work(); } }; }
pub async fn awaited() { async { loop { work(); } }.await }
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("in `awaited` never ends"), "{found}");
    }

    #[test]
    fn a_loop_through_a_renamed_import_and_a_loop_a_macro_is_handed_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod jobs { pub fn work() {} pub fn stop() -> ! { std::process::exit(0) } }
use jobs::{stop as halt, work as run};
macro_rules! id { ($($t:tt)*) => { $($t)* }; }
pub fn f() { id! { loop { run(); } } }
pub fn g() { loop { halt(); } }
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("in `f` never ends"), "{found}");
    }

    /// `-> !` is offered where the loop is the tail of a function with no return type, maybe
    /// incorrect (a trait's method or a `fn()` pointer can't change its return type), so
    /// `blaze check --fix` leaves it; elsewhere a note says it.
    #[test]
    fn a_tail_loop_is_offered_a_never_return_type() {
        let found = lint_only(
            LINT,
            r#"
pub fn work() {}
pub fn serve() {
    loop { work(); }
}
pub fn after() {
    loop { work(); }
    work();
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert_eq!(found.matches("if it is meant to run forever, say so: `-> !`").count(), 1, "{found}");
        assert!(found.contains("in `serve` never ends"), "{found}");
        assert_eq!(found.matches("the function's return type says so: `-> !`").count(), 1, "{found}");
    }
}
