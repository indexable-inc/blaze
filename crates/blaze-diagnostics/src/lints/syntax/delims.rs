//! rustc's `unused_parens`, `unused_braces` and `unused_import_braces`: parentheses and braces
//! around an expression, a pattern, a type or an import that the program reads the same without.
//!
//! rustc's early lint pass sees the expanded AST: a macro call as what it expands to
//! ([`expanded`]), and what a statement macro expands to as statements of the block it is in.
//!
//! Cost: one pass over the walk's nodes, one `match` on each node's kind, and nothing allocated
//! unless a lint is emitted. A node that is not delimited is one tag compare past that match.
//! The file of every node is carried along, never looked up from the node. Spans, strings and
//! suggestions are built only for what is emitted. This is rustc's own shape
//! (`BuiltinCombinedEarlyLintPass`: one walk, static dispatch, every lint's `check_*` per
//! node), without its per-lint `Vec`s of arguments and patterns.
//!
//! Follows rustc's `compiler/rustc_lint/src/unused.rs` (`UnusedDelimLint`, `UnusedParens`,
//! `UnusedBraces`, `UnusedImportBraces`) and `compiler/rustc_lint/src/lints.rs` (`UnusedDelim`,
//! `UnusedDelimSuggestion`, `UnusedImportBracesDiag`) (MIT or Apache-2.0).

mod precedence;

use hir::{HirFileId, InFile};
use span::{Edition, SyntaxContext};
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasArgList as _, HasLoopBody as _, HasName as _},
};

use super::{Emitted, Pass, Tree, Walk};
use crate::{
    cx::Cx,
    lints::registry::{LintId, LintSet},
    lower::Phase,
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// How many macro calls that expand to a macro call are followed: rustc's default
/// `recursion_limit`.
const MACRO_DEPTH: usize = 128;

pub(crate) fn lints<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Emitted> {
    const GIVES: LintSet = LintSet::of(&[
        LintId::named("unused_parens"),
        LintId::named("unused_braces"),
        LintId::named("unused_import_braces"),
    ]);
    if !cx.any_may_fire(&GIVES) {
        return Vec::new(); // alloc: empty, nothing is allocated
    }
    let mut check = Check {
        cx,
        walk,
        here: None,
        call: CallMemo::default(),
        last: None,
        import_braces: cx.lint_may_fire(LintId::named("unused_import_braces")),
        // alloc: empty until a lint fires; holds only what fires
        emitted: Vec::new(), // alloc: grows only by an emitted diagnostic
    };
    for tree in &walk.trees {
        check.here = Some(tree);
        for &node in &tree.nodes {
            check.node(tree.file, node);
        }
    }
    check.emitted
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lint {
    Parens,
    Braces,
}

impl Lint {
    fn name(self) -> &'static str {
        match self {
            Lint::Parens => "unused_parens",
            Lint::Braces => "unused_braces",
        }
    }

    fn delims(self) -> &'static str {
        match self {
            Lint::Parens => "parentheses",
            Lint::Braces => "braces",
        }
    }

    /// rustc's `LINT_EXPR_IN_PATTERN_MATCHING_CTX`: braces make a copy a `ref` pattern then
    /// borrows (`let ref b = { a };`), so they are not linted where a pattern matches a value.
    fn in_pattern_matching(self) -> bool {
        self == Lint::Parens
    }
}

/// Where a delimited value is: rustc's `UnusedDelimsCtx`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Ctx {
    FunctionArg,
    MethodArg,
    AssignedValue,
    AssignedValueLetElse,
    IfCond,
    WhileCond,
    ForIterExpr,
    MatchScrutineeExpr,
    ReturnValue,
    BlockRetValue,
    BreakValue,
    LetScrutineeExpr,
    ArrayLenExpr,
    AnonConst,
    MatchArmExpr,
    IndexExpr,
    ClosureBody,
}

impl Ctx {
    fn describe(self) -> &'static str {
        match self {
            Ctx::FunctionArg => "function argument",
            Ctx::MethodArg => "method argument",
            Ctx::AssignedValue | Ctx::AssignedValueLetElse => "assigned value",
            Ctx::IfCond => "`if` condition",
            Ctx::WhileCond => "`while` condition",
            Ctx::ForIterExpr => "`for` iterator expression",
            Ctx::MatchScrutineeExpr => "`match` scrutinee expression",
            Ctx::ReturnValue => "`return` value",
            Ctx::BlockRetValue => "block return value",
            Ctx::BreakValue => "`break` value",
            Ctx::LetScrutineeExpr => "`let` scrutinee expression",
            Ctx::ArrayLenExpr | Ctx::AnonConst => "const expression",
            Ctx::MatchArmExpr => "match arm expression",
            Ctx::IndexExpr => "index expression",
            Ctx::ClosureBody => "closure body",
        }
    }
}

/// What rustc's `check_unused_delims_expr` is told of a value: where it is, whether a block
/// follows it, where the keyword before it ends and the block after it starts (`left_pos`,
/// `right_pos`: offsets in the file of the expression checked, made spans only when a lint is
/// emitted), and whether a keyword is before it (`is_kw`).
#[derive(Clone, Copy)]
struct Site {
    ctx: Ctx,
    followed_by_block: bool,
    left: Option<TextSize>,
    right: Option<TextSize>,
    is_kw: bool,
}

const _: () = assert!(size_of::<Lint>() == 1 && size_of::<Ctx>() == 1 && size_of::<Site>() == 20);

impl Site {
    fn of(ctx: Ctx) -> Site {
        Site { ctx, followed_by_block: false, left: None, right: None, is_kw: false }
    }
}

/// Where a parenthesized trait object or `impl Trait` is one whose bounds a `+` after it would
/// run into: rustc's `NoBoundsException`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NoBounds {
    /// The type must be parenthesized.
    None,
    /// The type is the last bound: with one bound, its parentheses are not needed.
    OneBound,
}

/// A delimited node's span and, as rustc's `UnusedDelimSuggestion` takes them, the spans from
/// its opening delimiter to what it encloses and from the end of that to its closing one.
struct Delimited {
    value: Span,
    spans: Option<(Span, Span)>,
}

/// What both lints ask of the call expression being checked, worked out once for it.
#[derive(Default)]
struct CallMemo {
    nested: Option<bool>,
    callee_from_expansion: Option<bool>,
}

type At = InFile<SyntaxNodePtr>;

struct Check<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    walk: &'c Walk<'db>,
    /// The tree being walked, whose `cfg` facts answer [`Check::kept`] without a lookup.
    here: Option<&'c Tree<'db>>,
    call: CallMemo,
    /// The last macro call in a value position and what it expands to: both lints check the
    /// same values in turn, so the second finds the first's expansion here.
    last: Option<(ast::Node<'db>, InFile<ast::Expr<'db>>)>,
    /// Whether `unused_import_braces`, allowed by default, may fire: `use` items are not read
    /// otherwise.
    import_braces: bool,
    emitted: Vec<Emitted>,
}

impl<'db> Check<'_, '_, 'db> {
    /// What rustc's passes check as their walk comes to `node` of `file`: `check_expr`,
    /// `check_pat`, `check_ty`, `check_stmt`, `check_param`, `check_arm`, `check_item`,
    /// `check_variant` and `check_generic_arg`.
    fn node(&mut self, file: HirFileId, node: ast::Node<'db>) {
        let at = InFile::new(file, ast::ptr(node));
        match node.kind() {
            SyntaxKind::LET_STMT => {
                if let Some(it) = ast::LetStmt::cast(node) {
                    self.let_stmt(it, file, at);
                }
            }
            SyntaxKind::STMT_LIST => {
                if let Some(tail) = ast::StmtList::cast(node).and_then(|it| it.tail_expr()) {
                    self.statement(tail, file);
                }
            }
            SyntaxKind::EXPR_STMT => {
                if let Some(it) = ast::ExprStmt::cast(node)
                    && it.semicolon_token().is_none()
                    && let Some(e) = it.expr()
                {
                    self.statement(e, file);
                }
            }
            SyntaxKind::MACRO_STMTS => {
                if let Some(tail) = ast::MacroStmts::cast(node).and_then(|it| it.expr()) {
                    self.statement(tail, file);
                }
            }
            SyntaxKind::PARAM => {
                if let Some(pat) = ast::Param::cast(node).and_then(|it| it.pat()) {
                    self.parens_pat(pat, file, true, false, (false, false), at);
                }
            }
            SyntaxKind::MATCH_ARM => {
                if let Some(pat) = ast::MatchArm::cast(node).and_then(|it| it.pat()) {
                    self.parens_pat(pat, file, false, false, (false, false), at);
                }
            }
            SyntaxKind::CONST if !in_assoc_list(node) => {
                let body = ast::Const::cast(node).and_then(|it| it.body());
                self.item_value(body, file, at);
            }
            SyntaxKind::STATIC if !in_assoc_list(node) => {
                let body = ast::Static::cast(node).and_then(|it| it.body());
                self.item_value(body, file, at);
            }
            SyntaxKind::USE => {
                if self.import_braces
                    && let Some(item) = ast::Use::cast(node)
                    && let Some(tree) = item.use_tree()
                {
                    self.use_tree(tree, file, item, at);
                }
            }
            SyntaxKind::CONST_ARG => {
                if node.parent().is_some_and(|it| it.kind() == SyntaxKind::GENERIC_ARG_LIST)
                    && let Some(e) = ast::ConstArg::cast(node).and_then(|it| it.expr())
                {
                    self.braces_value(e, file, &Site::of(Ctx::AnonConst), at);
                }
            }
            SyntaxKind::VARIANT => {
                if let Some(e) = ast::Variant::cast(node).and_then(|it| it.const_arg()).and_then(|it| it.expr()) {
                    self.braces_value(e, file, &Site::of(Ctx::AnonConst), at);
                }
            }
            kind if ast::Expr::can_cast(kind) => {
                if let Some(e) = ast::Expr::cast(node) {
                    self.call = CallMemo::default();
                    self.parens_expr(e, file, at);
                    self.braces_expr(e, file, at);
                }
            }
            kind if ast::Pat::can_cast(kind) => {
                if let Some(pat) = ast::Pat::cast(node) {
                    self.pat(pat, file, at);
                }
            }
            kind if ast::Type::can_cast(kind) => {
                if let Some(ty) = ast::Type::cast(node) {
                    self.parens_ty(ty, file, at);
                    self.braces_ty(ty, file, at);
                }
            }
            _ => {}
        }
    }

    /// `UnusedParens::check_expr`: the patterns of `let` and `for`, the scrutinee of an
    /// `if let`, the bodies of match arms, then what both lints check.
    fn parens_expr(&mut self, e: ast::Expr<'db>, file: HirFileId, at: At) {
        match e {
            ast::Expr::LetExpr(it) => {
                if let Some(pat) = it.pat() {
                    self.parens_pat(pat, file, false, false, (true, true), at);
                }
            }
            ast::Expr::ForExpr(it) => {
                if let Some(pat) = it.pat() {
                    self.parens_pat(pat, file, false, false, (true, true), at);
                }
            }
            ast::Expr::IfExpr(it) => {
                // `if (let ..)` is an error of lowering; the scrutinee is still linted.
                if let Some(cond) = it.condition().map(peel_parens)
                    && matches!(cond, ast::Expr::LetExpr(_))
                {
                    let site = Site { followed_by_block: true, is_kw: true, ..Site::of(Ctx::LetScrutineeExpr) };
                    self.parens_value(cond, file, &site, at);
                    return;
                }
            }
            ast::Expr::MatchExpr(it) => {
                let site = Site { is_kw: true, ..Site::of(Ctx::MatchArmExpr) };
                for arm in it.match_arm_list().into_iter().flat_map(|it| it.arms()) {
                    if self.kept(file, arm.syntax())
                        && let Some(body) = arm.expr()
                    {
                        self.parens_value(body, file, &site, at);
                    }
                }
            }
            _ => {}
        }
        self.delim_expr(Lint::Parens, e, file, at);
    }

    /// `UnusedBraces::check_expr`: what both lints check, and the length of `[x; { 3 }]`.
    fn braces_expr(&mut self, e: ast::Expr<'db>, file: HirFileId, at: At) {
        self.delim_expr(Lint::Braces, e, file, at);
        if let ast::Expr::ArrayExpr(it) = e
            && let ast::ArrayExprKind::Repeat { repeat: Some(len), .. } = it.kind()
        {
            self.braces_value(len, file, &Site::of(Ctx::AnonConst), at);
        }
    }

    /// `UnusedDelimLint::check_expr`: the value of an expression that takes one.
    fn delim_expr(&mut self, lint: Lint, e: ast::Expr<'db>, file: HirFileId, at: At) {
        let (value, site) = match e {
            ast::Expr::IfExpr(it) => {
                let Some(cond) = it.condition() else { return };
                if matches!(cond, ast::Expr::LetExpr(_)) && !lint.in_pattern_matching() {
                    return;
                }
                let left = it.if_token().map(|it| it.text_range().start() + TextSize::from(2));
                let right = it.then_branch().map(|it| it.syntax().text_range().start());
                (cond, Site { ctx: Ctx::IfCond, followed_by_block: true, left, right, is_kw: true })
            }
            ast::Expr::WhileExpr(it) => {
                let Some(cond) = it.condition() else { return };
                if matches!(cond, ast::Expr::LetExpr(_)) && !lint.in_pattern_matching() {
                    return;
                }
                let start = it
                    .label()
                    .map(|it| it.syntax().text_range().start())
                    .or_else(|| it.while_token().map(|it| it.text_range().start()));
                let left = start.map(|it| it + TextSize::from(5));
                let right = it.loop_body().map(|it| it.syntax().text_range().start());
                (cond, Site { ctx: Ctx::WhileCond, followed_by_block: true, left, right, is_kw: true })
            }
            ast::Expr::ForExpr(it) => {
                let Some(iter) = it.iterable() else { return };
                let right = it.loop_body().map(|it| it.syntax().text_range().start());
                (iter, Site { ctx: Ctx::ForIterExpr, followed_by_block: true, left: None, right, is_kw: true })
            }
            ast::Expr::MatchExpr(it) if lint.in_pattern_matching() => {
                let Some(head) = it.expr() else { return };
                let left = it.match_token().map(|it| it.text_range().start() + TextSize::from(5));
                let site = Site { ctx: Ctx::MatchScrutineeExpr, followed_by_block: true, left, right: None, is_kw: true };
                (head, site)
            }
            ast::Expr::ReturnExpr(it) => {
                let Some(value) = it.expr() else { return };
                let left = it.return_token().map(|it| it.text_range().start() + TextSize::from(3));
                (value, Site { left, is_kw: true, ..Site::of(Ctx::ReturnValue) })
            }
            ast::Expr::BreakExpr(it) => {
                let Some(value) = it.expr() else { return };
                // `break 'label ({ .. })` needs them: without, the block would be the label's.
                if it.lifetime().is_some()
                    && let ast::Expr::ParenExpr(paren) = expanded(self.cx, value)
                    && paren.expr().is_some_and(|it| precedence::is_block(expanded(self.cx, it)))
                {
                    return;
                }
                (value, Site { is_kw: true, ..Site::of(Ctx::BreakValue) })
            }
            ast::Expr::IndexExpr(it) => {
                let Some(index) = it.index() else { return };
                (index, Site::of(Ctx::IndexExpr))
            }
            ast::Expr::BinExpr(it) if matches!(it.op_kind(), Some(ast::BinaryOp::Assignment { .. })) => {
                let Some(rhs) = it.rhs() else { return };
                (rhs, Site::of(Ctx::AssignedValue))
            }
            _ => {
                self.args(lint, e, file, at);
                return;
            }
        };
        self.value(lint, value, file, &site, at);
    }

    /// The arguments of a call, a method call or the body of a closure with no return type.
    fn args(&mut self, lint: Lint, e: ast::Expr<'db>, file: HirFileId, at: At) {
        let (ctx, list) = match e {
            ast::Expr::CallExpr(it) => (Ctx::FunctionArg, it.arg_list()),
            ast::Expr::MethodCallExpr(it) => (Ctx::MethodArg, it.arg_list()),
            ast::Expr::ClosureExpr(it) if it.ret_type().is_none() => {
                if let Some(body) = it.body()
                    && self.kept(file, body.syntax())
                    && !self.nested(file, e.syntax())
                {
                    self.value(lint, body, file, &Site::of(Ctx::ClosureBody), at);
                }
                return;
            }
            _ => return,
        };
        let Some(list) = list else { return };
        // A parenthesized token tree one macro hands another as an expression is not the
        // second macro's author's to care about (rust-lang/rust#47775).
        if self.nested(file, e.syntax()) {
            return;
        }
        let site = Site::of(ctx);
        for arg in list.args() {
            if !self.kept(file, arg.syntax()) {
                continue;
            }
            // Braces can choose which `macro_rules!` arm is taken (rust-lang/rust#158747).
            if self.callee_from_expansion(e, file) && self.block_wraps_expanded_expr(arg, file) {
                continue;
            }
            self.value(lint, arg, file, &site, at);
        }
    }

    /// `check_stmt` of an expression statement with no `;`: a block's tail, or a block-like
    /// expression before more statements. A statement macro is checked as the statements it
    /// expands to, in its expansion.
    fn statement(&mut self, e: ast::Expr<'db>, file: HirFileId) {
        if matches!(e, ast::Expr::MacroExpr(_)) || !self.kept(file, e.syntax()) {
            return;
        }
        let at = InFile::new(file, ast::ptr(e.syntax()));
        let site = Site::of(Ctx::BlockRetValue);
        self.parens_value(e, file, &site, at);
        self.braces_value(e, file, &site, at);
    }

    /// `UnusedParens::check_stmt` of a `let`: its pattern, then its initializer.
    fn let_stmt(&mut self, it: ast::LetStmt<'db>, file: HirFileId, at: At) {
        if let Some(pat) = it.pat() {
            self.parens_pat(pat, file, true, false, (true, false), at);
        }
        let Some(init) = it.initializer() else { return };
        let ctx = match it.let_else() {
            None => Ctx::AssignedValue,
            Some(_) => {
                // `let Some(_) = (x!{}) else { .. }`: what the parentheses hold is a macro's.
                let paren = expanded_in(self.cx, file, init);
                if let ast::Expr::ParenExpr(it) = paren.value
                    && let Some(inner) = it.expr()
                {
                    let inner = expanded_in(self.cx, paren.file_id, inner);
                    if self.ctxt(paren.file_id, it.syntax()) != self.ctxt(inner.file_id, inner.value.syntax()) {
                        return;
                    }
                }
                Ctx::AssignedValueLetElse
            }
        };
        self.parens_value(init, file, &Site::of(ctx), at);
    }

    /// `UnusedDelimLint::check_item`: the value of a `const` or `static` item.
    fn item_value(&mut self, body: Option<ast::Expr<'db>>, file: HirFileId, at: At) {
        let Some(body) = body else { return };
        let site = Site::of(Ctx::AssignedValue);
        self.parens_value(body, file, &site, at);
        self.braces_value(body, file, &site, at);
    }

    fn value(&mut self, lint: Lint, value: ast::Expr<'db>, file: HirFileId, site: &Site, at: At) {
        match lint {
            Lint::Parens => self.parens_value(value, file, site, at),
            Lint::Braces => self.braces_value(value, file, site, at),
        }
    }

    /// `UnusedParens::check_unused_delims_expr`.
    fn parens_value(&mut self, value: ast::Expr<'db>, file: HirFileId, site: &Site, at: At) {
        if !matches!(value, ast::Expr::ParenExpr(_) | ast::Expr::LetExpr(_) | ast::Expr::MacroExpr(_)) {
            return;
        }
        let site_file = file;
        let InFile { file_id: file, value } = self.expand(file, value);
        match value {
            ast::Expr::ParenExpr(paren) => {
                let Some(inner) = paren.expr() else { return };
                let inner = expanded(self.cx, inner);
                let unused = !(site.ctx == Ctx::LetScrutineeExpr && precedence::is_lazy(inner))
                    && !(matches!(site.ctx, Ctx::ReturnValue | Ctx::BreakValue) && precedence::is_assign(inner))
                    && !has_outer_attrs(paren.syntax())
                    && !self.from_expansion(file, paren.syntax())
                    && !precedence::delims_necessary(self.cx, inner, site.ctx, site.followed_by_block);
                if !unused {
                    return;
                }
                let Some(delimited) = self.delimited(file, paren.syntax(), SyntaxKind::L_PAREN, SyntaxKind::R_PAREN)
                else {
                    return;
                };
                self.emit_value(Lint::Parens, delimited, site, site_file, at);
            }
            ast::Expr::LetExpr(it) => {
                if let Some(scrutinee) = it.expr() {
                    let site = Site { followed_by_block: site.followed_by_block, ..Site::of(Ctx::LetScrutineeExpr) };
                    self.parens_value(scrutinee, file, &site, at);
                }
            }
            _ => {}
        }
    }

    /// `UnusedBraces::check_unused_delims_expr`: a block of one expression, unlabelled and not
    /// `unsafe`, on one line.
    fn braces_value(&mut self, value: ast::Expr<'db>, file: HirFileId, site: &Site, at: At) {
        if !matches!(value, ast::Expr::BlockExpr(_) | ast::Expr::LetExpr(_) | ast::Expr::MacroExpr(_)) {
            return;
        }
        let site_file = file;
        let InFile { file_id: file, value } = self.expand(file, value);
        match value {
            ast::Expr::BlockExpr(block) if site.ctx != Ctx::ClosureBody && precedence::is_plain_block(block) => {
                let Some(list) = block.stmt_list() else { return };
                if has_attrs(block.syntax())
                    || has_attrs(list.syntax())
                    || self.from_expansion(file, block.syntax())
                    || self.from_expansion(file, list.syntax())
                {
                    return;
                }
                let Some(expr) = self.single_expr(file, list) else { return };
                if (site.ctx == Ctx::AnonConst
                    && !(matches!(expr.value, ast::Expr::Literal(_))
                        && !self.from_expansion(expr.file_id, expr.value.syntax())))
                    || precedence::delims_necessary(self.cx, expr.value, site.ctx, site.followed_by_block)
                {
                    return;
                }
                let Some(delimited) = self.delimited(file, list.syntax(), SyntaxKind::L_CURLY, SyntaxKind::R_CURLY)
                else {
                    return;
                };
                if self.cx.snippet(&delimited.value).is_none_or(|it| it.contains('\n')) {
                    return;
                }
                self.emit_value(Lint::Braces, delimited, site, site_file, at);
            }
            ast::Expr::LetExpr(it) => {
                if let Some(scrutinee) = it.expr() {
                    let site = Site { followed_by_block: site.followed_by_block, ..Site::of(Ctx::LetScrutineeExpr) };
                    self.braces_value(scrutinee, file, &site, at);
                }
            }
            _ => {}
        }
    }

    /// [`expanded_in`] of a value of `file`, through [`Check::last`] for a macro call.
    fn expand(&mut self, file: HirFileId, value: ast::Expr<'db>) -> InFile<ast::Expr<'db>> {
        let ast::Expr::MacroExpr(call) = value else { return InFile::new(file, value) };
        if let Some((node, expansion)) = self.last
            && node == call.syntax()
        {
            return expansion;
        }
        let expansion = expanded_in(self.cx, file, value);
        self.last = Some((call.syntax(), expansion));
        expansion
    }

    /// The one statement of a block of `file` when it is an expression with no `;` (rustc's
    /// `[StmtKind::Expr(expr)]`), as rustc sees it: a statement macro's expansion in place.
    fn single_expr(&self, file: HirFileId, list: ast::StmtList<'db>) -> Option<InFile<ast::Expr<'db>>> {
        let empty_stmt = list.syntax().children_with_tokens().any(|it| it.kind() == SyntaxKind::SEMICOLON);
        if empty_stmt || list.statements().any(|it| self.kept(file, it.syntax())) {
            return None;
        }
        let mut e = InFile::new(file, list.tail_expr().filter(|it| self.kept(file, it.syntax()))?);
        for _ in 0..MACRO_DEPTH {
            let ast::Expr::MacroExpr(call) = e.value else { break };
            let root = call.macro_call().and_then(|it| self.cx.vsema.expand_macro_call(it))?;
            let next = match ast::MacroStmts::cast(root.value) {
                Some(stmts) if stmts.statements().next().is_some() => return None,
                Some(stmts) => stmts.expr()?,
                None => ast::Expr::cast(root.value)?,
            };
            e = InFile::new(root.file_id, next);
        }
        Some(e)
    }

    /// `block_wraps_expanded_expr`: a block written in the source whose one expression is a
    /// macro's.
    fn block_wraps_expanded_expr(&self, value: ast::Expr<'db>, file: HirFileId) -> bool {
        let InFile { file_id: file, value } = expanded_in(self.cx, file, value);
        let ast::Expr::BlockExpr(block) = value else { return false };
        precedence::is_plain_block(block)
            && !self.from_expansion(file, block.syntax())
            && block
                .stmt_list()
                .and_then(|it| self.single_expr(file, it))
                .is_some_and(|it| self.from_expansion(it.file_id, it.value.syntax()))
    }

    /// `UnusedParens::check_pat`: the parentheses around a pattern's subpatterns.
    fn pat(&mut self, pat: ast::Pat<'db>, file: HirFileId, at: At) {
        let no_space = (false, false);
        match pat {
            ast::Pat::TupleStructPat(it) => {
                for sub in it.fields() {
                    self.subpattern(sub, file, at);
                }
            }
            ast::Pat::TuplePat(it) => {
                for sub in it.fields() {
                    self.subpattern(sub, file, at);
                }
            }
            ast::Pat::SlicePat(it) => {
                for sub in it.pats() {
                    self.subpattern(sub, file, at);
                }
            }
            ast::Pat::OrPat(it) => {
                for sub in it.pats() {
                    self.subpattern(sub, file, at);
                }
            }
            ast::Pat::RecordPat(it) => {
                for field in it.record_pat_field_list().into_iter().flat_map(|it| it.fields()) {
                    if self.kept(file, field.syntax())
                        && let Some(sub) = field.pat()
                    {
                        self.subpattern(sub, file, at);
                    }
                }
            }
            // Not `i @ (a | b)` or `box (a | b)` (rust-lang/rust#64106).
            ast::Pat::IdentPat(it) => {
                if let Some(sub) = it.pat() {
                    self.parens_pat(sub, file, true, false, no_space, at);
                }
            }
            ast::Pat::BoxPat(it) => {
                if let Some(sub) = it.pat() {
                    self.parens_pat(sub, file, true, false, no_space, at);
                }
            }
            ast::Pat::DerefPat(it) => {
                if let Some(sub) = it.pat() {
                    self.parens_pat(sub, file, true, false, no_space, at);
                }
            }
            // Not `&(mut x)`: `&mut x` means another thing (rust-lang/rust#55342).
            ast::Pat::RefPat(it) => {
                if let Some(sub) = it.pat() {
                    self.parens_pat(sub, file, true, it.mut_token().is_none(), no_space, at);
                }
            }
            _ => {}
        }
    }

    fn subpattern(&mut self, sub: ast::Pat<'db>, file: HirFileId, at: At) {
        if self.kept(file, sub.syntax()) {
            self.parens_pat(sub, file, false, false, (false, false), at);
        }
    }

    /// `check_unused_parens_pat`.
    fn parens_pat(
        &mut self,
        value: ast::Pat<'db>,
        file: HirFileId,
        avoid_or: bool,
        avoid_mut: bool,
        keep_space: (bool, bool),
        at: At,
    ) {
        if !matches!(value, ast::Pat::ParenPat(_) | ast::Pat::MacroPat(_)) {
            return;
        }
        let InFile { file_id: file, value } = expanded_pat_in(self.cx, file, value);
        let ast::Pat::ParenPat(paren) = value else { return };
        let Some(inner) = paren.pat() else { return };
        match expanded_pat(self.cx, inner) {
            // Kept for readability wherever they are, as in `&(a..=b)`.
            ast::Pat::RangePat(_) => return,
            ast::Pat::OrPat(_) if avoid_or => return,
            ast::Pat::IdentPat(it) if avoid_mut && it.mut_token().is_some() && it.ref_token().is_none() => return,
            _ => {}
        }
        let Some(delimited) = self.delimited(file, paren.syntax(), SyntaxKind::L_PAREN, SyntaxKind::R_PAREN) else {
            return;
        };
        self.emit(Lint::Parens, delimited, "pattern", keep_space, false, at);
    }

    /// `UnusedParens::check_ty`: a parenthesized type, the length of an array type, and the
    /// parenthesized bounds of a trait object or `impl Trait`.
    fn parens_ty(&mut self, ty: ast::Type<'db>, file: HirFileId, at: At) {
        match ty {
            ast::Type::ParenType(paren) => {
                let Some(inner) = paren.ty() else { return };
                if self.cast_followed_by_lt(file, paren) {
                    return;
                }
                let unused = match expanded_ty(self.cx, inner) {
                    ast::Type::ImplTraitType(it) => self.bounds_unused(file, paren, it.type_bound_list()),
                    ast::Type::DynTraitType(it) => self.bounds_unused(file, paren, it.type_bound_list()),
                    ast::Type::ForType(it) if matches!(it.ty(), Some(ast::Type::FnPtrType(_))) => {
                        let no_params = it
                            .for_binder()
                            .and_then(|it| it.generic_param_list())
                            .is_none_or(|it| it.generic_params().next().is_none());
                        no_params || !self.with_self_ty_parens(file, paren)
                    }
                    _ => true,
                };
                if !unused {
                    return;
                }
                let Some(delimited) = self.delimited(file, paren.syntax(), SyntaxKind::L_PAREN, SyntaxKind::R_PAREN)
                else {
                    return;
                };
                self.emit(Lint::Parens, delimited, "type", (false, false), false, at);
            }
            ast::Type::ArrayType(it) => {
                if let Some(len) = it.const_arg().and_then(|it| it.expr()) {
                    self.parens_value(len, file, &Site::of(Ctx::ArrayLenExpr), at);
                }
            }
            ast::Type::DynTraitType(it) => self.paren_bounds(it.type_bound_list(), file, true, at),
            ast::Type::ImplTraitType(it) => self.paren_bounds(it.type_bound_list(), file, false, at),
            _ => {}
        }
    }

    /// `UnusedBraces::check_ty`: the length of an array type.
    fn braces_ty(&mut self, ty: ast::Type<'db>, file: HirFileId, at: At) {
        if let ast::Type::ArrayType(it) = ty
            && let Some(len) = it.const_arg().and_then(|it| it.expr())
        {
            self.braces_value(len, file, &Site::of(Ctx::ArrayLenExpr), at);
        }
    }

    /// Whether the parentheses `paren` around a trait object or `impl Trait` of `bounds` are
    /// not needed where it is.
    fn bounds_unused(&self, file: HirFileId, paren: ast::ParenType<'db>, bounds: Option<ast::TypeBoundList<'db>>) -> bool {
        match self.no_bounds(InFile::new(file, paren.syntax())) {
            Some(NoBounds::None) => false,
            Some(NoBounds::OneBound) => bounds.is_none_or(|it| it.bounds().nth(1).is_none()),
            None => true,
        }
    }

    /// rustc's `in_no_bounds_pos` of a type: what a `&` or `*const` before it, or being the
    /// return type of a `Fn` bound of a trait object or `impl Trait`, makes of its bounds.
    fn no_bounds(&self, ty: InFile<ast::Node<'db>>) -> Option<NoBounds> {
        let InFile { file_id: file, value: ty } = self.stand_in(ty);
        let parent = ty.parent()?;
        let pointee = match ast::RefType::cast(parent) {
            Some(it) => it.ty(),
            None => ast::PtrType::cast(parent).and_then(|it| it.ty()),
        };
        if let Some(pointee) = pointee {
            return (pointee.syntax() == ty)
                .then(|| self.no_bounds(InFile::new(file, parent)).unwrap_or(NoBounds::OneBound));
        }
        let ret = ast::RetType::cast(parent)?;
        if ret.ty().is_none_or(|it| it.syntax() != ty) {
            return None;
        }
        let segment = ast::PathSegment::cast(ret.syntax().parent()?)?;
        segment.parenthesized_arg_list()?;
        let path = segment.syntax().parent()?;
        let path_ty = path.parent()?;
        if !ast::Path::can_cast(path.kind()) || !ast::PathType::can_cast(path_ty.kind()) {
            return None;
        }
        let mut bound = path_ty.parent()?;
        if ast::ForType::can_cast(bound.kind()) {
            bound = bound.parent()?;
        }
        let bound = ast::TypeBound::cast(bound)?;
        let list = ast::TypeBoundList::cast(bound.syntax().parent()?)?;
        let owner = list.syntax().parent()?;
        if !ast::DynTraitType::can_cast(owner.kind()) && !ast::ImplTraitType::can_cast(owner.kind()) {
            return None;
        }
        match list.bounds().last() == Some(bound) {
            true => Some(NoBounds::OneBound),
            false => Some(NoBounds::None),
        }
    }

    /// rustc's `with_self_ty_parens`: `paren` is the bounded type of a `where` predicate with
    /// no `for<..>` of its own, where `(for<'a> fn(&'a u8)): Trait` needs them.
    fn with_self_ty_parens(&self, file: HirFileId, paren: ast::ParenType<'db>) -> bool {
        let node = self.stand_in(InFile::new(file, paren.syntax())).value;
        node.parent()
            .and_then(ast::WherePred::cast)
            .is_some_and(|pred| pred.for_binder().is_none() && pred.ty().is_some_and(|it| it.syntax() == node))
    }

    /// rustc's `parens_in_cast_in_lt`: `1 as (i32) < 2` is `1 as i32<2..` without them.
    fn cast_followed_by_lt(&self, file: HirFileId, paren: ast::ParenType<'db>) -> bool {
        let node = self.stand_in(InFile::new(file, paren.syntax()));
        let Some(cast) = node.value.parent().and_then(ast::CastExpr::cast) else { return false };
        if cast.ty().is_none_or(|it| it.syntax() != node.value) {
            return false;
        }
        let mut current = self.stand_in(InFile::new(node.file_id, cast.syntax()));
        loop {
            let Some(bin) = current.value.parent().and_then(ast::BinExpr::cast) else { return false };
            let Some(op) = bin.op_kind() else { return false };
            if matches!(op, ast::BinaryOp::Assignment { .. }) {
                return false;
            }
            if bin.lhs().is_some_and(|it| it.syntax() == current.value) {
                return matches!(
                    op,
                    ast::BinaryOp::CmpOp(ast::CmpOp::Ord { ordering: ast::Ordering::Less, strict: true })
                        | ast::BinaryOp::ArithOp(ast::ArithOp::Shl)
                );
            }
            // clone: a cursor's refcount (no allocation), only for a cast to a parenthesized type
            current = self.stand_in(InFile::new(current.file_id, bin.syntax()));
        }
    }

    /// The parenthesized bounds of a trait object or `impl Trait` that need no parentheses:
    /// not a `Fn` bound with a return type before another, nor, in 2015, a first bound of a
    /// path from the root (`dyn (::a::B)`, where `dyn::a::B` is a path).
    fn paren_bounds(&mut self, list: Option<ast::TypeBoundList<'db>>, file: HirFileId, is_dyn: bool, at: At) {
        let Some(list) = list else { return };
        let mut bounds = list.bounds().enumerate().peekable();
        while let Some((i, bound)) = bounds.next() {
            let is_last = bounds.peek().is_none();
            if !bound.syntax().children_with_tokens().any(|it| it.kind() == SyntaxKind::L_PAREN) {
                continue;
            }
            let Some(path) = trait_bound_path(bound) else { continue };
            let fn_with_explicit_ret_ty = path
                .segment()
                .is_some_and(|it| it.parenthesized_arg_list().is_some() && it.ret_type().is_some());
            let dyn2015_exception = is_dyn
                && i == 0
                && self.cx.edition == Edition::Edition2015
                && path.first_segment().is_some_and(|it| it.coloncolon_token().is_some());
            if (!is_last && fn_with_explicit_ret_ty) || dyn2015_exception || self.from_expansion(file, bound.syntax()) {
                continue;
            }
            let span = self.cx.span(InFile::new(file, bound.syntax().text_range()));
            if !self.cx.snippet(&span).is_some_and(|it| it.starts_with('(') && it.ends_with(')')) {
                continue;
            }
            let spans = Some((span.with_hi(span.lo + 1), span.with_lo(span.hi - 1)));
            self.emit(Lint::Parens, Delimited { value: span, spans }, "type", (false, false), false, at);
        }
    }

    /// `UnusedImportBraces::check_use_tree`: a list of one name or glob, inner lists first.
    fn use_tree(&mut self, tree: ast::UseTree<'db>, file: HirFileId, item: ast::Use<'db>, at: At) {
        let Some(list) = tree.use_tree_list() else { return };
        let mut only = None;
        let mut count = 0usize;
        for it in list.use_trees() {
            self.use_tree(it, file, item, at);
            count += 1;
            only = Some(it);
        }
        let (1, Some(only)) = (count, only) else { return };
        if only.use_tree_list().is_some() {
            return;
        }
        let name = if only.star_token().is_some() {
            // alloc: the lint's text, for a lint found
            "*".to_owned() // alloc: the name in an emitted diagnostic
        } else {
            let Some(name_ref) = only.path().and_then(|it| it.segment()).and_then(|it| it.name_ref()) else {
                return;
            };
            if name_ref.self_token().is_some() {
                return;
            }
            match only.rename() {
                // alloc: the lint's text, for a lint found
                // alloc: the name in an emitted diagnostic
                Some(rename) => rename.name().map_or_else(|| "_".to_owned(), |it| self.ident(it.text())),
                None => self.ident(name_ref.text()),
            }
        };
        let span = self.cx.span(InFile::new(file, crate::cx::item_range_view(item.syntax())));
        // alloc: the lint's text, for a lint found
        // alloc: an emitted diagnostic's message
        let mut diag = Diag::lint("unused_import_braces", format!("braces around {name} is unnecessary"), span);
        super::source_fact(self.cx, &mut diag, at);
        self.emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Early, at));
    }

    /// A name as rustc prints a `Symbol`: `r#` only before a keyword.
    fn ident(&self, text: &str) -> String {
        match text.strip_prefix("r#") {
            // alloc: the lint's text, for a lint found
            Some(name) if !syntax::utils::is_raw_identifier(name, self.cx.edition) => name.to_owned(), // alloc: emitted
            _ => text.to_owned(), // alloc: the name in an emitted diagnostic
        }
    }

    /// `emit_unused_delims_expr`: whether to keep a space where the keyword before the value
    /// or the block after it touches it; `site`'s offsets are of `site_file`.
    fn emit_value(&mut self, lint: Lint, delimited: Delimited, site: &Site, site_file: HirFileId, at: At) {
        let value = &delimited.value;
        let span = |pos: TextSize| self.cx.span(InFile::new(site_file, TextRange::empty(pos)));
        let keep_space = (
            site.left.map(span).is_some_and(|it| it.file == value.file && it.lo >= value.lo),
            site.right.map(span).is_some_and(|it| it.file == value.file && it.lo <= value.hi),
        );
        self.emit(lint, delimited, site.ctx.describe(), keep_space, site.is_kw, at);
    }

    /// `emit_unused_delims`: the lint on both delimiters, and their removal, with a space where
    /// what was around them would otherwise run together.
    fn emit(&mut self, lint: Lint, delimited: Delimited, item: &str, keep_space: (bool, bool), is_kw: bool, at: At) {
        // alloc: the lint's text, for a lint found
        let message = format!("unnecessary {} around {item}", lint.delims()); // alloc: an emitted diagnostic's message
        let mut diag = match &delimited.spans {
            Some((lo, hi)) => {
                if hi.is_empty() {
                    return;
                }
                // clone: a lint's span, for a lint found
                let mut diag = Diag::lint(lint.name(), message, lo.clone()); // clone: a span the diagnostic keeps
                diag.primary(hi.clone()); // clone: a span the diagnostic keeps
                diag
            }
            None => Diag::lint(lint.name(), message, delimited.value.clone()), // clone: a span the diagnostic keeps
        };
        if let Some((lo, hi)) = delimited.spans {
            let text = self.cx.file_of(&lo).map_or("", |it| self.cx.file_text(it));
            let before = text.get(..lo.lo as usize).unwrap_or("");
            let after = text.get(hi.hi as usize..).unwrap_or("");
            let lo_replace = match ((keep_space.0 || is_kw) && !before.ends_with(' '))
                || before.ends_with(char::is_alphanumeric)
            {
                true => " ",
                false => "",
            };
            let hi_replace = match (keep_space.1 && !after.starts_with(' ')) || after.starts_with(char::is_alphanumeric) {
                true => " ",
                false => "",
            };
            diag.suggestion_with_style(
                // alloc: the lint's text, for a lint found
                vec![(lo, lo_replace.to_owned()), (hi, hi_replace.to_owned())], // alloc: an emitted suggestion
                format!("remove these {}", lint.delims()), // alloc: an emitted suggestion's message
                Applicability::MachineApplicable,
                SuggestionStyle::ShowCode,
            );
        }
        super::source_fact(self.cx, &mut diag, at);
        self.emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Early, at));
    }

    /// The span of `node` of `file` and, when it is written in the source, those of its
    /// delimiters `open` and `close` with what is between them and what they enclose (rustc's
    /// `span_with_attrs.find_ancestor_inside(value.span)`); `None` when `close` is missing, as
    /// the parser's recovery leaves it, or nothing is between them. One pass over the node's
    /// children: the first `open`, the last `close` after it, and the first and last element
    /// that is not trivia between them.
    fn delimited(&self, file: HirFileId, node: ast::Node<'_>, open: SyntaxKind, close: SyntaxKind) -> Option<Delimited> {
        let mut open_at: Option<TextSize> = None;
        let mut first: Option<TextSize> = None;
        let mut last: Option<TextSize> = None;
        let mut closing: Option<(TextSize, Option<TextSize>)> = None;
        for element in node.children_with_tokens() {
            let kind = element.kind();
            let range = element.text_range();
            if open_at.is_none() {
                if kind == open {
                    open_at = Some(range.start());
                }
                continue;
            }
            if kind.is_trivia() {
                continue;
            }
            if kind == close {
                closing = Some((range.end(), last));
            }
            first.get_or_insert(range.start());
            last = Some(range.end());
        }
        let (open_at, (close_end, enclosed_end)) = (open_at?, closing?);
        let (first, enclosed_end) = (first?, enclosed_end?);
        let span = |range: TextRange| self.cx.span(InFile::new(file, range));
        let value = span(TextRange::new(open_at, close_end));
        let spans = match self.from_expansion(file, node) {
            true => None,
            false => {
                let inner = span(TextRange::new(first, enclosed_end));
                (inner.file == value.file && value.lo <= inner.lo && inner.hi <= value.hi)
                    .then(|| (value.with_hi(inner.lo), value.with_lo(inner.hi)))
            }
        };
        Some(Delimited { value, spans })
    }

    /// rustc's `e.span.ctxt().outer_expn_data().call_site.from_expansion()` for the call
    /// expression being checked: it was made by a macro whose call was itself made by one.
    fn nested(&mut self, file: HirFileId, node: ast::Node<'_>) -> bool {
        if file.macro_file().is_none() {
            return false;
        }
        if let Some(it) = self.call.nested {
            return it;
        }
        let it = self.ctxt(file, node).outer_expn(self.cx.db).is_some_and(|call| {
            let call = hir_expand::MacroCallId::from(call).loc(self.cx.db).to_node_view(self.cx.db);
            super::from_expansion(self.cx, call)
        });
        self.call.nested = Some(it);
        it
    }

    /// Whether the callee of the call expression being checked (its function, or its method's
    /// name) is a macro's.
    fn callee_from_expansion(&mut self, e: ast::Expr<'_>, file: HirFileId) -> bool {
        if file.macro_file().is_none() {
            return false;
        }
        if let Some(it) = self.call.callee_from_expansion {
            return it;
        }
        let it = match e {
            ast::Expr::CallExpr(it) => it.expr().is_some_and(|it| self.from_expansion(file, it.syntax())),
            ast::Expr::MethodCallExpr(it) => it.name_ref().is_some_and(|it| self.from_expansion(file, it.syntax())),
            _ => false,
        };
        self.call.callee_from_expansion = Some(it);
        it
    }

    /// The syntax context of rustc's span of `node` of `file`: that of its first token.
    fn ctxt(&self, file: HirFileId, node: ast::Node<'_>) -> SyntaxContext {
        file.span_map(self.cx.db).span_for_range(node.text_range()).ctx
    }

    /// The node rustc's AST has where `node` is: when `node` is all of what a type, pattern or
    /// expression macro expands to, the place of the call.
    fn stand_in(&self, node: InFile<ast::Node<'db>>) -> InFile<ast::Node<'db>> {
        let mut node = node;
        for _ in 0..MACRO_DEPTH {
            if node.value.parent().is_some() {
                break;
            }
            let Some(call) = node.file_id.macro_file() else { break };
            let call = call.loc(self.cx.db).to_node_view(self.cx.db);
            match call.value.parent() {
                Some(wrapper)
                    if matches!(
                        wrapper.kind(),
                        SyntaxKind::MACRO_TYPE | SyntaxKind::MACRO_PAT | SyntaxKind::MACRO_EXPR
                    ) =>
                {
                    node = InFile::new(call.file_id, wrapper);
                }
                _ => break,
            }
        }
        node
    }

    fn from_expansion(&self, file: HirFileId, node: ast::Node<'_>) -> bool {
        super::from_expansion(self.cx, InFile::new(file, node))
    }

    /// Whether rustc's walk has `node` of `file`: `cfg` keeps it. The tree being walked answers
    /// without a lookup; an expansion's tree is found by its file.
    fn kept(&self, file: HirFileId, node: ast::Node<'_>) -> bool {
        let tree = match self.here {
            Some(here) if here.file == file => Some(here),
            _ => self.walk.tree(file),
        };
        tree.is_none_or(|tree| !tree.skipped(node.text_range()))
    }
}

/// [`expanded`] of `e` of `file`, with the file of what it expands to.
fn expanded_in<'db>(cx: &Cx<'_, 'db>, file: HirFileId, e: ast::Expr<'db>) -> InFile<ast::Expr<'db>> {
    let (expansion, e) = peel(cx, e);
    InFile::new(expansion.unwrap_or(file), e)
}

/// What rustc's expanded AST has for `e`: what a macro call expands to, through macro calls
/// that expand to macro calls.
pub(super) fn expanded<'db>(cx: &Cx<'_, 'db>, e: ast::Expr<'db>) -> ast::Expr<'db> {
    peel(cx, e).1
}

/// [`expanded`], and the file of the last expansion when there is one.
fn peel<'db>(cx: &Cx<'_, 'db>, e: ast::Expr<'db>) -> (Option<HirFileId>, ast::Expr<'db>) {
    let mut e = e;
    let mut file = None;
    for _ in 0..MACRO_DEPTH {
        let ast::Expr::MacroExpr(call) = e else { break };
        let Some(root) = call.macro_call().and_then(|it| cx.vsema.expand_macro_call(it)) else { break };
        let next = match ast::MacroStmts::cast(root.value) {
            Some(stmts) if stmts.statements().next().is_none() => stmts.expr(),
            Some(_) => None,
            None => ast::Expr::cast(root.value),
        };
        let Some(next) = next else { break };
        file = Some(root.file_id);
        e = next;
    }
    (file, e)
}

/// [`expanded_pat`] of `pat` of `file`, with the file of what it expands to.
fn expanded_pat_in<'db>(cx: &Cx<'_, 'db>, file: HirFileId, pat: ast::Pat<'db>) -> InFile<ast::Pat<'db>> {
    let mut pat = InFile::new(file, pat);
    for _ in 0..MACRO_DEPTH {
        let ast::Pat::MacroPat(call) = pat.value else { break };
        let Some(root) = call.macro_call().and_then(|it| cx.vsema.expand_macro_call(it)) else { break };
        let Some(next) = ast::Pat::cast(root.value) else { break };
        pat = InFile::new(root.file_id, next);
    }
    pat
}

/// [`expanded`] of a pattern.
fn expanded_pat<'db>(cx: &Cx<'_, 'db>, pat: ast::Pat<'db>) -> ast::Pat<'db> {
    let mut pat = pat;
    for _ in 0..MACRO_DEPTH {
        let ast::Pat::MacroPat(call) = pat else { break };
        let Some(next) =
            call.macro_call().and_then(|it| cx.vsema.expand_macro_call(it)).and_then(|it| ast::Pat::cast(it.value))
        else {
            break;
        };
        pat = next;
    }
    pat
}

/// [`expanded`] of a type.
pub(super) fn expanded_ty<'db>(cx: &Cx<'_, 'db>, ty: ast::Type<'db>) -> ast::Type<'db> {
    let mut ty = ty;
    for _ in 0..MACRO_DEPTH {
        let ast::Type::MacroType(call) = ty else { break };
        let Some(next) =
            call.macro_call().and_then(|it| cx.vsema.expand_macro_call(it)).and_then(|it| ast::Type::cast(it.value))
        else {
            break;
        };
        ty = next;
    }
    ty
}

/// rustc's `peel_parens`.
fn peel_parens(e: ast::Expr<'_>) -> ast::Expr<'_> {
    let mut e = e;
    while let ast::Expr::ParenExpr(paren) = e
        && let Some(inner) = paren.expr()
    {
        e = inner;
    }
    e
}

/// The path of a trait bound (rustc's `GenericBound::Trait`), `for<..>` or not.
fn trait_bound_path(bound: ast::TypeBound<'_>) -> Option<ast::Path<'_>> {
    match bound.ty()? {
        ast::Type::PathType(it) => it.path(),
        ast::Type::ForType(it) => match it.ty()? {
            ast::Type::PathType(it) => it.path(),
            _ => None,
        },
        _ => None,
    }
}

/// Whether an item is an associated one, which rustc's `check_item` is not called on.
fn in_assoc_list(item: ast::Node<'_>) -> bool {
    item.parent().is_some_and(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST)
}

fn is_attr(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT)
}

/// Whether `node` has attributes of its own (rustc's `value.attrs`).
fn has_attrs(node: ast::Node<'_>) -> bool {
    node.children().any(|it| is_attr(it.kind()))
}

/// Whether a parenthesized expression has attributes before its `(`: those after it are of
/// what it encloses.
fn has_outer_attrs(paren: ast::Node<'_>) -> bool {
    paren
        .children_with_tokens()
        .take_while(|it| it.kind() != SyntaxKind::L_PAREN)
        .any(|it| is_attr(it.kind()))
}

#[cfg(test)]
mod tests {
    use crate::lints::{quiet_crate, registry::LintId, with_cx};

    use super::*;

    /// With its lints on, the pass reads every node of the walk, and for a crate it finds
    /// nothing in it allocates nothing: nothing per node, so nothing that grows with the crate.
    #[test]
    fn delims_on_allocate_nothing_at_any_size() {
        let on = ["-W", "unused_parens", "-W", "unused_braces", "-W", "unused_import_braces"];
        for fns in [4, 400] {
            with_cx(&quiet_crate(fns), &on, |cx| {
                for lint in ["unused_parens", "unused_braces", "unused_import_braces"] {
                    assert!(cx.lint_may_fire(LintId::named(lint)), "{lint} is on");
                }
                let walk = cx.walk();
                assert!(walk.trees.iter().map(|it| it.nodes.len()).sum::<usize>() > fns * 10);
                // The first run settles the lint levels it asks for.
                assert!(lints(cx, walk).is_empty());
                let found = blaze_alloc_count::assert_at_most(0, &format!("delims on, {fns} functions"), || {
                    lints(cx, walk)
                });
                assert!(found.is_empty());
            });
        }
    }

    /// The view `Semantics` expands every macro call of every tree the walk has but those `cfg`
    /// removes: into the call's own macro file, whose call is the node, at the root it hands
    /// out for that file; a call `cfg` removes is `None`.
    #[test]
    fn the_view_expansions_are_the_calls_files() {
        const FIXTURE: &str = r#"
//- /lib.rs crate:main edition:2021
macro_rules! e { ($e:expr) => { ($e) }; }
macro_rules! nest { ($e:expr) => { e!($e) }; }
macro_rules! p { () => { (_) }; }
macro_rules! t { () => { (u8) }; }
macro_rules! s { () => { let _s = 1; }; }
pub fn f(x: t!()) -> u8 {
    let p!() = x;
    s!();
    let y = nest!(x + 1);
    #[cfg(FALSE)]
    let z = e!(1);
    match y { p!() => e!({ y }) }
}
"#;
        with_cx(FIXTURE, &[], |cx| {
            let walk = cx.walk();
            let (mut expanded, mut not) = (0, 0);
            for tree in &walk.trees {
                for node in tree.root.descendants().filter(|it| it.kind() == SyntaxKind::MACRO_CALL) {
                    let call = ast::MacroCall::cast(node).expect("checked by kind");
                    let at = (tree.file, node.text_range());
                    let views = cx.vsema.expand_macro_call(call);
                    assert_eq!(views.is_none(), tree.skipped(node.text_range()), "{at:?}");
                    let Some(expansion) = views else {
                        not += 1;
                        continue;
                    };
                    let file = expansion.file_id.macro_file().expect("an expansion is a macro's file");
                    let called = file.loc(cx.db).to_node_view(cx.db);
                    assert_eq!((called.file_id, called.value.kind(), called.value.text_range()), (tree.file, node.kind(), node.text_range()));
                    let root = cx.root(expansion.file_id);
                    assert_eq!((expansion.value.kind(), expansion.value.text_range()), (root.kind(), root.text_range()), "{at:?}");
                    assert!(expansion.value.parent().is_none(), "{at:?}");
                    expanded += 1;
                }
            }
            // `t!`, both `p!`, `s!`, `nest!` and the `e!` it expands to, `e!({ y })`.
            assert_eq!(expanded, 7, "{expanded} calls expanded");
            assert!(not >= 1, "the call `cfg` removes is expanded by neither");
        });
    }
}
