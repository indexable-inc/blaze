//! `option_question_mark_hides`: a function returning `Option` that takes a second lookup's
//! `None` up with `?`. The first lookup's `None` is the function's answer ("there is no such
//! key"); a later one's means the first found something the second can't (an index pointing at
//! no item, a name with no definition): a broken invariant, which the `?` turns into the same
//! `None` the caller reads as "absent". Nobody can tell the two apart, so the bug is answered as
//! a miss.
//!
//! Found by resolution and types: a `?` whose operand is an `Option`, in a function or closure
//! that returns `Option`, the operand a method call of one of the lookups (`get`, `get_mut`,
//! `find`, `find_map`, `first`, `last`, `nth`, `position`, `rposition`, `strip_prefix`,
//! `strip_suffix`, `split_once`, `rsplit_once`, `parent`) of any type, whose value is an
//! `Option`; and an earlier `?` of the same body (not of a closure in it) on another such
//! lookup. The first is left alone. `.ok()?`, a `Result`'s error made `None`, is
//! `dropped_error`'s and not looked at here; neither is test code. No fix.
//!
//! The right form: return `Result<Option<T>, E>` (or `Result<T, E>` where absent is itself an
//! error) and answer the later lookup's miss with an error naming the key it missed
//! (`.ok_or(Error::Dangling { index })?`); or, where the miss is truly unreachable, say so with
//! `.expect("why it is there")`.

use syntax::{
    SyntaxKind, TextRange, WalkEvent,
    view::{self as ast, AstNode as _},
};

use super::{
    super::{Lint, std_mistakes::report, test_code},
    question_mark_converts::returns_into,
};
use crate::check::Cx;

const LINT: Lint = Lint::OptionQuestionMarkHides;

/// The lookups whose `None` is "not there".
const LOOKUPS: &[&str] = &[
    "get",
    "get_mut",
    "find",
    "find_map",
    "first",
    "last",
    "nth",
    "position",
    "rposition",
    "strip_prefix",
    "strip_suffix",
    "split_once",
    "rsplit_once",
    "parent",
];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if node.kind() != SyntaxKind::TRY_EXPR || !cx.on_blaze(LINT) || cx.analysis().is_none() {
        return;
    }
    if test_code::is_test_code(cx, node) {
        return;
    }
    let Some(name) = lookup(cx, node) else { return };
    let option = cx.famous().core_option_Option().map(hir::Adt::Enum);
    if option.is_none() || returns_into(cx, node).and_then(|it| it.as_adt()) != option {
        return;
    }
    let Some(body) = node
        .ancestors()
        .skip(1)
        .find(|it| matches!(it.kind(), SyntaxKind::FN | SyntaxKind::CLOSURE_EXPR))
    else {
        return;
    };
    let Some((first, first_range)) = first_lookup(cx, body, node) else { return };
    let Some(range) = cx.range(node) else { return };
    let line = |at: TextRange| {
        cx.text().get(..usize::from(at.start())).map_or(0, |it| it.matches('\n').count()) + 1
    };
    let message = format!(
        "this `?` answers a missing `.{name}(..)` with the same `None` as the `.{first}(..)?` on line {}: a caller \
         can't tell \"not there\" from \"the first lookup found something the second can't\"",
        line(first_range)
    );
    let note = "return a `Result` and name the later miss (`.ok_or(Error::Dangling { key })?`), or say why it can't \
                miss with `.expect(\"..\")`";
    report(cx, LINT, node, range, message, note, None);
}

/// The lookup the `?` at `node` takes the `None` of: its method's name.
fn lookup<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<&'static str> {
    let operand = ast::TryExpr::cast(node)?.expr()?.peel_parens();
    let ast::Expr::MethodCallExpr(call) = operand else { return None };
    let f = cx.method(call)?;
    let name = f.name(cx.db());
    let found = LOOKUPS.iter().find(|it| **it == name.as_str())?;
    let option = cx.famous().core_option_Option().map(hir::Adt::Enum)?;
    (cx.ty(operand)?.as_adt() == Some(option)).then_some(*found)
}

/// The first `?` of `body` (not of a function or closure in it) on a lookup, when it is before
/// `node`: its lookup's name and its range.
fn first_lookup<'t>(
    cx: &Cx<'_, 't>,
    body: ast::Node<'t>,
    node: ast::Node<'t>,
) -> Option<(&'static str, TextRange)> {
    let mut events = body.preorder();
    while let Some(event) = events.next() {
        let WalkEvent::Enter(at) = event else { continue };
        if at == body {
            continue;
        }
        if matches!(at.kind(), SyntaxKind::FN | SyntaxKind::CLOSURE_EXPR) {
            events.skip_subtree();
            continue;
        }
        if at == node {
            return None;
        }
        if at.kind() == SyntaxKind::TRY_EXPR
            && let Some(name) = lookup(cx, at)
        {
            return Some((name, cx.range(at)?));
        }
    }
    None
}
