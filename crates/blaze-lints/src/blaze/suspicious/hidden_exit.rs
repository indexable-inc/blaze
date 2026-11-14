//! `hidden_exit`: a call of a workspace macro whose expansion leaves the caller: a `return`, a
//! `?`, or a `break` or `continue` of a loop the caller wrote. `check!(len > 0);` reads as an
//! expression and returns from the function; a reader (or an LLM) moving the line, or looking
//! for the exits of the function, does not see it.
//!
//! Found on the expansion of each call of a `macro_rules!` or `macro` defined in a crate of the
//! workspace (std's, core's and dependencies' are their own business: `assert!` panics, which
//! every reader knows). An exit acts on the caller when it is outside every closure, `async`
//! block, `try` block and nested function of the expansion, and (for `break` and `continue`)
//! outside every loop of the expansion. An exit inside the arguments the caller wrote
//! (`log!(f()?)`) is the caller's own, in plain sight, and is left alone. A nested macro call
//! in the expansion is not looked into. Test code is left alone.
//!
//! No fix: the right form is the exit written at the call, the macro making a value or a
//! `Result` (`return Err(error!(..))`, `ensure(len > 0)?`), so every exit of a function is in
//! its own text.

use syntax::{
    SyntaxKind, TextRange, WalkEvent,
    view::{self as ast, AstNode as _},
};

use super::super::{Lint, std_mistakes::report, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::HiddenExit;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if node.kind() != SyntaxKind::MACRO_CALL || !cx.on_blaze(LINT) {
        return;
    }
    let Some(call) = ast::MacroCall::cast(node) else { return };
    let db = cx.db();
    let Some(mac) = cx.macro_def(call) else { return };
    if cx.macro_source(mac).is_none() || !mac.module(db).krate(db).origin(db).is_local() {
        return;
    }
    if test_code::is_test_code(cx, node) {
        return;
    }
    let Some(exit) = exit_of(cx, call) else { return };
    let Some(range) = cx.range(node) else { return };
    let name = mac.name(db).as_str().to_owned(); // alloc: a message's part
    report(
        cx,
        LINT,
        node,
        range,
        format!("`{name}!` leaves the function it is called in: its expansion holds {exit}"),
        "an exit a macro hides is one a reader of the caller does not see: write the `return` or the \
         `?` at the call and have the macro make the value",
        None,
    );
}

/// The exit `call`'s expansion has that acts on the caller, as a message names it.
fn exit_of<'t>(cx: &Cx<'_, 't>, call: ast::MacroCall<'t>) -> Option<&'static str> {
    let expansion = cx.expand(call)?;
    // alloc: the caller's arguments in the expansion, a few
    let mut args = Vec::new();
    cx.macro_args(call, &mut args);
    let written: Vec<TextRange> = args.iter().map(|(_, it)| it.syntax().text_range()).collect(); // alloc: as above
    let mut events = expansion.preorder();
    while let Some(event) = events.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if written.iter().any(|it| it.contains_range(node.text_range())) || is_own_body(node) {
            events.skip_subtree();
            continue;
        }
        // A `break` or `continue` inside a loop of the expansion is that loop's.
        let in_loop = || node.ancestors().skip(1).any(|it| is_loop(it.kind()));
        match node.kind() {
            SyntaxKind::RETURN_EXPR => return Some("a `return`"),
            SyntaxKind::TRY_EXPR => return Some("a `?`"),
            SyntaxKind::BREAK_EXPR if !in_loop() => return Some("a `break` of the caller's loop"),
            SyntaxKind::CONTINUE_EXPR if !in_loop() => {
                return Some("a `continue` of the caller's loop");
            }
            _ => {}
        }
    }
    None
}

fn is_loop(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::LOOP_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::FOR_EXPR)
}

/// Whether an exit inside `node` acts on `node`, not on the caller: a closure, a nested
/// function, an `async` or `try` block.
fn is_own_body(node: ast::Node<'_>) -> bool {
    match node.kind() {
        SyntaxKind::CLOSURE_EXPR | SyntaxKind::FN => true,
        SyntaxKind::BLOCK_EXPR => ast::BlockExpr::cast(node)
            .is_some_and(|it| it.async_token().is_some() || it.try_block_modifier().is_some()),
        _ => false,
    }
}
