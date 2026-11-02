//! E0268, E0267, E0571, E0590, E0695 and E0696: a `break` or `continue` with no loop or labeled
//! block to leave, one inside a closure or a coroutine, a `break` with a value out of a `while`
//! or `for` loop, an unlabeled one in a `while` loop's condition or inside a labeled block, and
//! a `continue` to a labeled block.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/loops.rs` (`CheckLoopVisitor`: the stack of
//! contexts a body is visited with, `require_label_in_labeled_block`, `require_break_cx`,
//! `report_outside_loop_error`), `compiler/rustc_hir_typeck/src/diagnostics.rs`
//! (`OutsideLoop`, `BreakInsideClosure`, `BreakInsideCoroutine`, `BreakNonLoop`,
//! `UnlabeledInLabeledBlock`, `UnlabeledCfInWhileCondition`, `ContinueLabeledBlock`) and
//! `compiler/rustc_ast_lowering/src/expr.rs` (`lower_loop_destination`, the loop scope) (MIT or
//! Apache-2.0). The stack is rebuilt from the syntax: from the body the `break` is in down to
//! it, as rustc's visitor pushes it on the HIR that `rustc_ast_lowering` makes of it.

use hir::{HirFileId, InFile};
use syntax::{
    SyntaxKind, TextRange, TextSize, WalkEvent,
    view::{self as ast, AstNode as _, HasLoopBody as _},
};

use crate::{
    cx::{Cx, item_range_view as item_range, signature_range_view as signature_range},
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// A context of rustc's loop checker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Context {
    Normal,
    Fn,
    Loop,
    Closure(InFile<TextRange>),
    Coroutine { span: InFile<TextRange>, kind: &'static str, source: &'static str },
    /// A block with no label, by where it starts.
    UnlabeledBlock(InFile<TextSize>),
    UnlabeledIfBlock,
    LabeledBlock,
    AnonConst,
    ConstBlock,
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::BreakOutsideOfLoop) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let node = cx.syntax_view(at);
    // alloc: empty, allocates nothing
    let Some(mut diag) = diagnostic(cx, d, node) else { return Vec::new() };
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// What rustc's loop checker says of the `break` or `continue` at `node`, if anything.
fn diagnostic<'db>(cx: &Cx<'_, 'db>, d: &hir::BreakOutsideOfLoop, node: InFile<ast::Node<'db>>) -> Option<Diag> {
    let span = cx.span(node.with_value(node.value.text_range()));
    let name = if d.is_break { "break" } else { "continue" };
    let label = jump_label(node.value);
    let stack = contexts(cx, node);
    if label.is_none() && stack.last() == Some(&Context::LabeledBlock) {
        let mut diag =
            // alloc: clone: the error's parts, for an error hir found
            Diag::error(Some("E0695"), format!("unlabeled `{name}` inside of a labeled block"), span.clone());
        diag.span_label(
            span,
            // alloc: the error's text, for an error hir found
            format!("`{name}` statements that would diverge to or through a labeled block need to bear a label"),
        );
        return Some(diag);
    }
    if d.bad_value_break {
        if label.is_none() && matches!(target(cx, node, None), Some(Target::WhileCondition)) {
            let mut diag = Diag::error(
                Some("E0590"),
                "`break` or `continue` with no label in the condition of a `while` loop",
                // clone: a handle, on the error path, for an error hir found
                span.clone(),
            );
            // alloc: the error's text, for an error hir found
            diag.span_label(span, format!("unlabeled `{name}` in the condition of a `while` loop"));
            return Some(diag);
        }
        return break_non_loop(cx, node);
    }
    if !d.is_break
        && let Some(label) = label
        && let Some(to) = target_loop(cx, node, Some(label))
        && let ast::Expr::BlockExpr(block) = to.value
    {
        // clone: a handle, on the error path, for an error hir found
        let mut diag = Diag::error(Some("E0696"), "`continue` pointing to a labeled block", span.clone());
        diag.span_label(span, "labeled blocks cannot be `continue`'d");
        diag.span_label(cx.span(to.with_value(item_range(block.syntax()))), "labeled block the `continue` points to");
        return Some(diag);
    }
    match required(&stack, d.is_break)? {
        Context::Closure(closure) => {
            // alloc: clone: the error's parts, for an error hir found
            let mut diag = Diag::error(Some("E0267"), format!("`{name}` inside of a closure"), span.clone());
            // alloc: the error's text, for an error hir found
            diag.span_label(span, format!("cannot `{name}` inside of a closure"));
            diag.span_label(cx.span(closure), "enclosing closure");
            Some(diag)
        }
        Context::Coroutine { span: coroutine, kind, source } => {
            let mut diag =
                // alloc: clone: the error's parts, for an error hir found
                Diag::error(Some("E0267"), format!("`{name}` inside `{kind}` {source}"), span.clone());
            // alloc: the error's text, for an error hir found
            diag.span_label(span, format!("cannot `{name}` inside `{kind}` {source}"));
            // alloc: the error's text, for an error hir found
            diag.span_label(cx.span(coroutine), format!("enclosing `{kind}` {source}"));
            Some(diag)
        }
        Context::UnlabeledBlock(block) if same_file(node, &block) => block_breaks(cx, node, block),
        // alloc: the error's parts, for an error hir found
        _ => Some(outside_loop(vec![span], d.is_break)),
    }
}

/// The label a `break` or `continue` names.
fn jump_label(node: ast::Node<'_>) -> Option<&str> {
    let lifetime = match ast::Expr::cast(node)? {
        ast::Expr::BreakExpr(it) => it.lifetime(),
        ast::Expr::ContinueExpr(it) => it.lifetime(),
        _ => None,
    };
    Some(lifetime?.text())
}

/// rustc's `OutsideLoop`, of the `break`s or the `continue` at `spans`.
fn outside_loop(spans: Vec<crate::model::Span>, is_break: bool) -> Diag {
    let (name, or_block) = match is_break {
        true => ("break", " or labeled block"),
        false => ("continue", ""),
    };
    let mut spans = spans.into_iter();
    let first = spans.next().expect("a `break` to report");
    let mut diag =
        // alloc: clone: the error's parts, for an error hir found
        Diag::error(Some("E0268"), format!("`{name}` outside of a loop{or_block}"), first.clone());
    // alloc: the error's text, for an error hir found
    let label = format!("cannot `{name}` outside of a loop{or_block}");
    // clone: a handle, on the error path, for an error hir found
    diag.span_label(first, label.clone());
    for span in spans {
        // clone: a handle, on the error path, for an error hir found
        diag.primary(span.clone());
        // clone: a handle, on the error path, for an error hir found
        diag.span_label(span, label.clone());
    }
    diag
}

/// rustc's one `OutsideLoop` for all the `break`s whose context is the unlabeled block at
/// `block`, with its suggestion to label it: given with the first of them, none with the
/// others.
fn block_breaks<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>, block: InFile<TextSize>) -> Option<Diag> {
    let block_node = node
        .value
        .ancestors()
        .filter_map(ast::BlockExpr::cast)
        .find(|it| block_start(*it) == block.value)?;
    let breaks = breaks_of_block(cx, node, block_node, block)?;
    let first = breaks.first()?;
    if first.syntax() != node.value {
        return None;
    }
    // alloc: the error's parts, for an error hir found
    let spans = breaks.iter().map(|it| cx.span(node.with_value(it.syntax().text_range()))).collect();
    let mut diag = outside_loop(spans, true);
    // alloc: the error's text, for an error hir found
    let mut parts = vec![(cx.span(node.with_value(TextRange::empty(block.value))), "'block: ".to_owned())];
    for it in &breaks {
        let lo = match it.break_token() {
            Some(token) => token.text_range().end(),
            None => it.syntax().text_range().start() + TextSize::of("break"),
        };
        let hi = it.lifetime().map_or(lo, |label| label.syntax().text_range().end());
        // alloc: the error's text, for an error hir found
        parts.push((cx.span(node.with_value(TextRange::new(lo, hi))), " 'block".to_owned()));
    }
    diag.multipart_suggestion(
        "consider labeling this block to be able to break within it",
        parts,
        Applicability::MaybeIncorrect,
    );
    Some(diag)
}

/// The `break`s in `block_node` that rustc's `require_break_cx` finds in the context of the
/// unlabeled block at `block` (`block_node` itself), in the order the checker visits them;
/// none as soon as the first of them is not `node`, which then gives no diagnostic.
///
/// One pre-order walk of the block, the checker's stack pushed and popped as the walk goes
/// down and up, as rustc's `CheckLoopVisitor` visits it: one `visit` per node, and a subtree
/// skipped once nothing in it can be in the block's context (it is a body of its own, or a
/// loop, closure or labeled block is between). The stack at the block is made once, from its
/// ancestors.
fn breaks_of_block<'db>(
    cx: &Cx<'_, 'db>,
    node: InFile<ast::Node<'db>>,
    block_node: ast::BlockExpr<'db>,
    block: InFile<TextSize>,
) -> Option<Vec<ast::BreakExpr<'db>>> {
    /// What entering a node changed, undone on leaving it; `contents_only` and `live` are of
    /// the node, for its children.
    struct Entered {
        len: usize,
        floor: usize,
        contents_only: bool,
        live: bool,
    }
    let root = block_node.syntax();
    let (mut stack, root_contents_only) = state(cx, node.with_value(root));
    // The stack starts again at `floor` in a body of its own.
    let mut floor = 0;
    // alloc: two buffers per diagnostic, as deep as the block and as long as what it reports
    let mut entered: Vec<Entered> = Vec::new();
    // alloc: the error's parts, for an error hir found
    let mut breaks = Vec::new();
    let wanted = Context::UnlabeledBlock(block);
    let mut walk = root.preorder();
    while let Some(event) = walk.next() {
        let child = match event {
            WalkEvent::Enter(it) => it,
            WalkEvent::Leave(_) => {
                let Some(left) = entered.pop() else { break };
                stack.truncate(left.len);
                floor = left.floor;
                continue;
            }
        };
        let (skip, parent_live) = match entered.last().map(|it| (it.contents_only, it.live)) {
            Some(it) => it,
            None => {
                entered.push(Entered { len: stack.len(), floor, contents_only: root_contents_only, live: false });
                continue;
            }
        };
        let mut now = Entered { len: stack.len(), floor, contents_only: false, live: false };
        let Some(parent) = child.parent() else { break };
        let parent = node.with_value(parent);
        now.live = match base(parent, child) {
            Some(owner) => {
                floor = stack.len();
                stack.extend([Context::Normal, owner]);
                false
            }
            None => {
                let pushed = visit(&stack[floor..], parent, child, skip, &mut now.contents_only);
                if let Some(pushed) = pushed {
                    stack.push(pushed);
                    if pushed == Context::ConstBlock {
                        stack.push(Context::Normal);
                    }
                }
                match pushed {
                    Some(it) if it == wanted => true,
                    None | Some(Context::UnlabeledIfBlock) => parent_live,
                    Some(_) => false,
                }
            }
        };
        let live = now.live;
        entered.push(now);
        if !live {
            walk.skip_subtree();
            continue;
        }
        if let Some(it) = ast::BreakExpr::cast(child)
            && required(&stack[floor..], true) == Some(wanted)
        {
            if breaks.is_empty() && it.syntax() != node.value {
                return None;
            }
            breaks.push(it);
        }
    }
    Some(breaks)
}

/// rustc's `BreakNonLoop`: a `break` with a value whose target is a `while` or `for` loop.
fn break_non_loop<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Option<Diag> {
    let break_expr = ast::BreakExpr::cast(node.value)?;
    let value = break_expr.expr()?;
    let break_label = break_expr.lifetime().map(|it| it.text());
    let target = target_loop(cx, node, break_label)?;
    let (kind, head_end, loop_label) = match target.value {
        ast::Expr::WhileExpr(it) => {
            ("while", it.condition()?.syntax().text_range().end(), label_of(it.label()))
        }
        ast::Expr::ForExpr(it) => {
            ("for", it.iterable()?.syntax().text_range().end(), label_of(it.label()))
        }
        _ => return None,
    };
    let span = cx.span(node.with_value(break_expr.syntax().text_range()));
    let head = target.with_value(TextRange::new(item_range(target.value.syntax()).start(), head_end));
    let mut diag = Diag::error(
        Some("E0571"),
        // alloc: the error's text, for an error hir found
        format!("`break` with value from a `{kind}` loop"),
        // clone: a handle, on the error path, for an error hir found
        span.clone(),
    );
    // clone: a handle, on the error path, for an error hir found
    diag.span_label(span.clone(), "can only break with a value inside `loop` or breakable block");
    // alloc: the error's text, for an error hir found
    diag.span_label(cx.span(head), format!("you can't `break` with a value in a `{kind}` loop"));
    let suggestion = match break_label {
        // alloc: the error's text, for an error hir found
        Some(label) => format!("break {label}"),
        // alloc: the error's text, for an error hir found
        None => "break".to_owned(),
    };
    diag.suggestion(
        span,
        // alloc: the error's text, for an error hir found
        format!("use `break` on its own without a value inside this `{kind}` loop"),
        suggestion,
        Applicability::MaybeIncorrect,
    );
    if let (Some(label), None) = (loop_label, break_label) {
        let redundant = match value {
            ast::Expr::PathExpr(path) => path.path().is_some_and(|path| {
                path.qualifier().is_none()
                    && path.segment().is_some_and(|it| {
                        label.strip_prefix('\'').is_some_and(|name| it.syntax().text() == name)
                    })
                    && cx.vsema.resolve_path(path).is_none()
            }),
            _ => false,
        };
        if redundant {
            return None;
        }
        diag.suggestion(
            cx.span(node.with_value(value.syntax().text_range())),
            "alternatively, you might have meant to use the available loop label",
            label,
            Applicability::MaybeIncorrect,
        );
    }
    Some(diag)
}

fn label_of(label: Option<ast::Label<'_>>) -> Option<&str> {
    Some(label?.lifetime()?.text())
}

/// The loop a `break` at `node` leaves: the innermost one when it has no label, else the loop
/// or labeled block with its label, within the closure, coroutine, constant or body it is in
/// (rustc's loop scope, which `with_new_scopes` starts again, and its label ribs). None for
/// one with no label in a `while` loop's condition, which leaves no loop
/// (`LoopIdError::UnlabeledCfInWhileCondition`).
fn target_loop<'db>(
    cx: &Cx<'_, 'db>,
    node: InFile<ast::Node<'db>>,
    label: Option<&str>,
) -> Option<InFile<ast::Expr<'db>>> {
    match target(cx, node, label)? {
        Target::Loop(it) => Some(it),
        Target::WhileCondition => None,
    }
}

enum Target<'db> {
    Loop(InFile<ast::Expr<'db>>),
    WhileCondition,
}

fn target<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>, label: Option<&str>) -> Option<Target<'db>> {
    for (ancestor, child) in edges(cx, node) {
        let child = child.value;
        let kind = ancestor.value.kind();
        if ast::ClosureExpr::can_cast(kind) || is_item(kind) || ast::ConstArg::can_cast(kind) {
            return None;
        }
        let in_body = |body: Option<ast::BlockExpr<'_>>| body.is_some_and(|it| it.syntax() == child);
        let found = match ast::Expr::cast(ancestor.value) {
            Some(ast::Expr::LoopExpr(it)) if in_body(it.loop_body()) => Some((it.label(), false, ast::Expr::LoopExpr(it))),
            Some(ast::Expr::WhileExpr(it)) if it.condition().is_some_and(|it| it.syntax() == child) => {
                if label.is_none() {
                    return Some(Target::WhileCondition);
                }
                Some((it.label(), false, ast::Expr::WhileExpr(it)))
            }
            Some(ast::Expr::WhileExpr(it)) if in_body(it.loop_body()) => Some((it.label(), false, ast::Expr::WhileExpr(it))),
            Some(ast::Expr::ForExpr(it)) if in_body(it.loop_body()) => Some((it.label(), false, ast::Expr::ForExpr(it))),
            Some(ast::Expr::BlockExpr(it))
                if it.async_token().is_some() || it.gen_token().is_some() || it.const_token().is_some() =>
            {
                return None;
            }
            Some(ast::Expr::BlockExpr(it)) if it.label().is_some() => Some((it.label(), true, ast::Expr::BlockExpr(it))),
            Some(ast::Expr::ArrayExpr(it))
                if it.semicolon_token().is_some() && it.exprs().nth(1).is_some_and(|len| len.syntax() == child) =>
            {
                return None;
            }
            _ => None,
        };
        if let Some((own, is_block, expr)) = found {
            let named = |label: &str| own.and_then(ast::Label::lifetime).is_some_and(|it| it.text() == label);
            match label {
                None if !is_block => return Some(Target::Loop(ancestor.with_value(expr))),
                Some(label) if named(label) => return Some(Target::Loop(ancestor.with_value(expr))),
                _ => {}
            }
        }
    }
    None
}

/// Whether a node of `kind` is an item, a body's owner: not a macro call, which the syntax
/// tree counts as one.
pub(crate) fn is_item(kind: SyntaxKind) -> bool {
    ast::Item::can_cast(kind) && kind != SyntaxKind::MACRO_CALL
}

fn same_file(node: InFile<ast::Node<'_>>, block: &InFile<TextSize>) -> bool {
    node.file_id == block.file_id
}

/// What `require_break_cx` finds wrong with a `break` (or `continue`) in `stack`, if anything:
/// the context it reports, an `if` block standing for the context around it.
fn required(stack: &[Context], is_break: bool) -> Option<Context> {
    let mut at = stack.len().checked_sub(1)?;
    loop {
        match &stack[at] {
            Context::LabeledBlock | Context::Loop => return None,
            Context::UnlabeledIfBlock if is_break && at > 0 => at -= 1,
            Context::UnlabeledBlock(_) if !is_break => return Some(Context::Normal),
            other => return Some(*other),
        }
    }
}

/// The nodes from `node` up, through each macro expansion to its call, `node` first, one at a
/// time: a macro call's node is found (its file parsed or expanded) only when the walk goes
/// past the top of its expansion, so a walk that stops at the first node it wants pays only
/// for the nodes up to it, with nothing allocated.
pub(crate) fn up<'a, 'db>(cx: &'a Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Up<'a, 'db> {
    Up { vsema: &cx.vsema, file: node.file_id, next: Some(node.value) }
}

/// The iterator of [`up`].
pub(crate) struct Up<'a, 'db> {
    vsema: &'a hir::view::Semantics<'db>,
    file: HirFileId,
    next: Option<ast::Node<'db>>,
}

impl<'db> Iterator for Up<'_, 'db> {
    type Item = InFile<ast::Node<'db>>;

    fn next(&mut self) -> Option<InFile<ast::Node<'db>>> {
        let node = match self.next.take() {
            Some(it) => it,
            None => {
                let db = self.vsema.db;
                let call = self.file.macro_file()?.loc(db).to_node_view(db);
                // The call's tree is given out by `vsema`, so a caller may ask it of the call.
                self.vsema.parse_or_expand(call.file_id);
                self.file = call.file_id;
                call.value
            }
        };
        self.next = node.parent();
        Some(InFile::new(self.file, node))
    }
}

/// [`up`] from above `node`, each node with the one before it in the walk: its child, or for
/// a macro call the root of its expansion, in the expansion's file.
pub(crate) fn edges<'a, 'db>(cx: &'a Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Edges<'a, 'db> {
    let mut up = up(cx, node);
    let child = up.next();
    Edges { up, child }
}

/// The iterator of [`edges`].
pub(crate) struct Edges<'a, 'db> {
    up: Up<'a, 'db>,
    child: Option<InFile<ast::Node<'db>>>,
}

impl<'db> Iterator for Edges<'_, 'db> {
    type Item = (InFile<ast::Node<'db>>, InFile<ast::Node<'db>>);

    fn next(&mut self) -> Option<(InFile<ast::Node<'db>>, InFile<ast::Node<'db>>)> {
        let child = self.child.take()?;
        let node = self.up.next()?;
        self.child = Some(node);
        Some((node, child))
    }
}

/// The stack of contexts rustc's loop checker has at `node`.
fn contexts<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Vec<Context> {
    state(cx, node).0
}

/// [`contexts`] at `node`, and whether `node` is a block whose contents are visited as an
/// `if`'s, nothing pushed for the block itself. Walks up only to the owner of the body `node`
/// is in.
fn state<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> (Vec<Context>, bool) {
    // From `node` up to the owner of its body, the owner left out: replayed downwards.
    // alloc: one buffer per diagnostic, as deep as the body
    let mut chain = vec![node];
    let mut owner = Context::Normal;
    for it in up(cx, node).skip(1) {
        if let Some(found) = base(it, chain[chain.len() - 1].value) {
            owner = found;
            break;
        }
        chain.push(it);
    }
    // alloc: the checker's stack, one per diagnostic
    let mut stack = vec![Context::Normal, owner];
    let mut contents_only = false;
    for i in (1..chain.len()).rev() {
        let (node, child) = (chain[i], chain[i - 1]);
        let skip = std::mem::take(&mut contents_only);
        if let Some(pushed) = visit(&stack, node, child.value, skip, &mut contents_only) {
            // An inline const's body is a block of its own, visited with `ConstBlock` on top.
            let body = (pushed == Context::ConstBlock).then_some(Context::Normal);
            stack.push(pushed);
            stack.extend(body);
        }
    }
    (stack, contents_only)
}

/// The context a body starts in, when `node` is the owner of a body `child` is: rustc's
/// `check`, `Fn` but for an anonymous const; an `async` or `gen` function's body is the
/// coroutine it makes.
fn base(node: InFile<ast::Node<'_>>, child: ast::Node<'_>) -> Option<Context> {
    let value = node.value;
    if let Some(it) = ast::Fn::cast(value) {
        if !it.body().is_some_and(|body| body.syntax() == child) {
            return None;
        }
        let kind = match (it.async_token().is_some(), it.gen_token().is_some()) {
            (true, true) => "async gen",
            (true, false) => "async",
            (false, true) => "gen",
            (false, false) => return Some(Context::Fn),
        };
        let span = node.with_value(signature_range(it));
        return Some(Context::Coroutine { span, kind, source: "function" });
    }
    if ast::Const::can_cast(value.kind()) || ast::Static::can_cast(value.kind()) {
        return Some(Context::Fn);
    }
    // A discriminant, an array's length, a const generic argument.
    if ast::ConstArg::can_cast(value.kind()) {
        return Some(Context::AnonConst);
    }
    if let Some(it) = ast::ArrayExpr::cast(value)
        && it.semicolon_token().is_some()
        && it.exprs().nth(1).is_some_and(|len| len.syntax() == child)
    {
        return Some(Context::AnonConst);
    }
    if is_item(value.kind()) {
        return Some(Context::Normal);
    }
    None
}

/// What rustc's visitor pushes visiting `node` on its way to `child`, with `stack` as it is;
/// `skip` when `node` is a block whose contents are visited as an `if`'s, with nothing pushed
/// for the block itself. Sets `contents_only` when `child` is such a block.
fn visit(
    stack: &[Context],
    node: InFile<ast::Node<'_>>,
    child: ast::Node<'_>,
    skip: bool,
    contents_only: &mut bool,
) -> Option<Context> {
    let top = stack.last()?;
    let expr = ast::Expr::cast(node.value)?;
    let is = |it: Option<ast::BlockExpr<'_>>| it.is_some_and(|it| it.syntax() == child);
    match expr {
        ast::Expr::IfExpr(it) => {
            let branch = is(it.then_branch())
                || matches!(it.else_branch(), Some(ast::ElseBranch::Block(block)) if block.syntax() == child);
            let plain = ast::BlockExpr::cast(child).is_some_and(|it| it.label().is_none());
            if branch
                && plain
                && matches!(
                    top,
                    Context::Normal | Context::AnonConst | Context::UnlabeledBlock(_) | Context::UnlabeledIfBlock
                )
            {
                *contents_only = true;
                return Some(Context::UnlabeledIfBlock);
            }
            None
        }
        ast::Expr::LoopExpr(it) => is(it.loop_body()).then_some(Context::Loop),
        ast::Expr::WhileExpr(it) => {
            let in_condition = it.condition().is_some_and(|it| it.syntax() == child);
            (is(it.loop_body()) || in_condition).then_some(Context::Loop)
        }
        ast::Expr::ForExpr(it) => is(it.loop_body()).then_some(Context::Loop),
        ast::Expr::ClosureExpr(it) => {
            if !it.body().is_some_and(|body| body.syntax() == child) {
                return None;
            }
            let start = item_range(it.syntax()).start();
            let end = match it.ret_type() {
                Some(ret) => ret.syntax().text_range().end(),
                None => it.param_list()?.syntax().text_range().end(),
            };
            let span = node.with_value(TextRange::new(start, end));
            let kind = match (it.async_token().is_some(), it.gen_token().is_some()) {
                (true, true) => "async gen",
                (true, false) => "async",
                (false, true) => "gen",
                (false, false) => return Some(Context::Closure(span)),
            };
            Some(Context::Coroutine { span, kind, source: "closure" })
        }
        // A `let`'s `else` block is visited as a block, not as an expression.
        ast::Expr::BlockExpr(_) if node.value.parent().is_some_and(|it| it.kind() == SyntaxKind::LET_ELSE) => None,
        ast::Expr::BlockExpr(it) if !skip => block(top, node, it),
        _ => None,
    }
}

/// Where rustc's span of a block starts: at its label or keyword, after its attributes.
fn block_start(it: ast::BlockExpr<'_>) -> TextSize {
    item_range(it.syntax()).start()
}

/// The context of a block that is not a plain one: labeled, a coroutine, an inline const.
fn block_modifier(node: InFile<ast::Node<'_>>, it: ast::BlockExpr<'_>) -> Option<Context> {
    if it.label().is_some() {
        return Some(Context::LabeledBlock);
    }
    let kind = match (it.async_token().is_some(), it.gen_token().is_some()) {
        (true, true) => Some("async gen"),
        (true, false) => Some("async"),
        (false, true) => Some("gen"),
        (false, false) => None,
    };
    if let Some(kind) = kind {
        let start = block_start(it);
        let end = it
            .stmt_list()
            .and_then(|list| list.syntax().prev_sibling_or_token())
            .into_iter()
            .flat_map(|it| std::iter::successors(Some(it), |it| it.prev_sibling_or_token()))
            .find(|it| !it.kind().is_trivia())
            .map_or(start, |it| it.text_range().end());
        let span = node.with_value(TextRange::new(start, end));
        return Some(Context::Coroutine { span, kind, source: "block" });
    }
    if it.const_token().is_some() {
        return Some(Context::ConstBlock);
    }
    None
}

/// What visiting the block `it` as an expression pushes. A `try` block is lowered to a plain
/// block.
fn block(top: &Context, node: InFile<ast::Node<'_>>, it: ast::BlockExpr<'_>) -> Option<Context> {
    if let Some(context) = block_modifier(node, it) {
        return Some(context);
    }
    if matches!(top, Context::Fn | Context::ConstBlock) {
        return Some(Context::Normal);
    }
    let default = it.unsafe_token().is_none();
    if default && matches!(top, Context::Normal | Context::AnonConst | Context::UnlabeledBlock(_)) {
        return Some(Context::UnlabeledBlock(node.with_value(block_start(it))));
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use blaze_alloc_count::assert_at_most;

    use super::*;

    /// `check` given the context of the crate of the one file `source`, and the file's tree.
    pub(crate) fn with_file(source: &str, check: impl for<'db> FnOnce(&Cx<'_, 'db>, InFile<ast::Node<'db>>)) {
        let checked = crate::tests::checked(&format!("//- /main.rs crate:main\n{source}"));
        let db = &checked.db;
        hir::attach_db(db, || {
            let krate = hir::check::workspace_crates(db)[0];
            let frame = crate::tests::frame();
            let cx = Cx::new(db, krate, checked.files(), &frame);
            let file = krate.root_module(db).definition_source_file_id(db);
            check(&cx, InFile::new(file, cx.root(file)));
        });
    }

    /// Where rustc's loop checker has a `break` whose context is a plain block: in it, in an
    /// `if` in it, and through what it leaves the block's context for (a loop, a closure, a
    /// labeled block, an anonymous or inline const, an item, an `async` block, an `unsafe`
    /// block, a `let`'s `else`).
    const BLOCKS: &str = r#"
fn f(c: bool) {
    {
        break;
        if c { break; } else { { break; } }
        loop { break; }
        let _ = || { break; };
        'a: { break; break 'a; }
        let _ = [0; { break; 1 }];
        const { break; };
        fn g() { { break; } }
        unsafe { break; }
        async { break; };
        let Some(_) = None::<u8> else { break };
        { if c { break } }
        break;
    }
    { if c { break; } break; }
    { 'b: { if c { break 'b; } } }
}
fn main() {}
"#;

    /// [`breaks_of_block`] by the plain rule it stands for: each `break` of the block, its
    /// stack made again from its ancestors.
    fn plain<'db>(
        cx: &Cx<'_, 'db>,
        node: InFile<ast::Node<'db>>,
        block_node: ast::BlockExpr<'db>,
        block: InFile<TextSize>,
    ) -> Option<Vec<ast::BreakExpr<'db>>> {
        let breaks: Vec<ast::BreakExpr<'db>> = block_node
            .syntax()
            .descendants()
            .filter_map(ast::BreakExpr::cast)
            .filter(|it| {
                let stack = contexts(cx, node.with_value(it.syntax()));
                let in_labeled = it.lifetime().is_none() && stack.last() == Some(&Context::LabeledBlock);
                !in_labeled && required(&stack, true) == Some(Context::UnlabeledBlock(block))
            })
            .collect();
        match breaks.first() {
            Some(first) if first.syntax() != node.value => None,
            _ => Some(breaks),
        }
    }

    /// The one walk of a block finds the `break`s the plain rule does, for each `break` that
    /// is in a plain block's context.
    #[test]
    fn a_blocks_breaks_are_those_of_the_plain_rule() {
        with_file(BLOCKS, |cx, root| {
            let mut compared = 0;
            let mut reported = 0;
            for it in root.value.descendants().filter(|it| ast::BreakExpr::can_cast(it.kind())) {
                let node = root.with_value(it);
                let Some(Context::UnlabeledBlock(block)) = required(&contexts(cx, node), true) else { continue };
                let block_node = node
                    .value
                    .ancestors()
                    .filter_map(ast::BlockExpr::cast)
                    .find(|it| block_start(*it) == block.value)
                    .expect("the block of the context is around the `break`");
                let ranges = |breaks: Option<Vec<ast::BreakExpr<'_>>>| {
                    breaks.map(|it| it.iter().map(|it| it.syntax().text_range()).collect::<Vec<_>>())
                };
                let fast = ranges(breaks_of_block(cx, node, block_node, block));
                assert_eq!(fast, ranges(plain(cx, node, block_node, block)), "at {:?}", node.value);
                compared += 1;
                reported += usize::from(fast.is_some_and(|it| it.len() > 1));
            }
            assert!(compared >= 8, "the fixture has `break`s in plain blocks: {compared}");
            assert!(reported >= 2, "some block reports more than one `break`: {reported}");
        });
    }

    /// A walk up a file's tree reads the nodes in place: nothing is allocated.
    #[test]
    fn a_walk_up_allocates_nothing() {
        with_file(BLOCKS, |cx, root| {
            let deepest = root.value.descendants().max_by_key(|it| it.ancestors().count()).expect("a node");
            let node = root.with_value(deepest);
            let depth = node.value.ancestors().count();
            assert_eq!(up(cx, node).count(), depth);
            assert_eq!(edges(cx, node).count(), depth - 1);
            assert_at_most(0, "a walk up", || up(cx, node).count() + edges(cx, node).count());
        });
    }

    /// In a file with no macro call, the walk up is each node's chain of parents, `node` first,
    /// all in the file, ending at the file's root.
    #[test]
    fn a_walk_up_in_a_file_is_the_parent_chain() {
        with_file(BLOCKS, |cx, root| {
            let mut compared = 0;
            for node in root.value.descendants() {
                let key = |file, it: ast::Node<'_>| (file, it.kind(), it.text_range());
                let walk: Vec<_> = up(cx, root.with_value(node)).map(|it| key(it.file_id, it.value)).collect();
                let mut parents = Vec::new();
                let mut at = Some(node);
                while let Some(it) = at {
                    parents.push(key(root.file_id, it));
                    at = it.parent();
                }
                assert_eq!(walk, parents, "at {node:?}");
                assert_eq!(walk.last(), Some(&key(root.file_id, root.value)));
                compared += 1;
            }
            assert!(compared > 100, "the fixture has many nodes: {compared}");
        });
    }
}
