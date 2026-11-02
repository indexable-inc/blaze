//! rustc's late lint passes of statements, `let`s and patterns in bodies: `PathStatements`
//! (`path_statements`), `LetUnderscore` (`let_underscore_drop`, `let_underscore_lock`),
//! `UnusedResults` (`unused_results`; its `unused_must_use` is `lower/unused_must_use.rs`'s),
//! `UnitBindings` (`unit_bindings`), `NonShorthandFieldPatterns`
//! (`non_shorthand_field_patterns`) and `UnusedAllocation` (`unused_allocation`).
//!
//! Follows rustc's `compiler/rustc_lint/src/unused.rs` (`PathStatements`, `UnusedAllocation`),
//! `compiler/rustc_lint/src/unused/must_use.rs` (`UnusedResults::check_stmt`, `is_ty_must_use`,
//! `check_fn_must_use`), `compiler/rustc_lint/src/let_underscore.rs`,
//! `compiler/rustc_lint/src/unit_bindings.rs`, `compiler/rustc_lint/src/builtin.rs`
//! (`NonShorthandFieldPatterns`), `compiler/rustc_lint/src/lints.rs` (`PathStatementDrop`,
//! `NonBindingLet`, `NonBindingLetSub`, `UnusedResult`, `UnitBindingsDiag`,
//! `BuiltinNonShorthandFieldPatterns`, `UnusedAllocationDiag`) and
//! `compiler/rustc_ast_lowering/src/expr.rs` (`lower_expr_assign`: a destructuring assignment
//! is a `let` of `LocalSource::AssignDesugar`) (MIT or Apache-2.0).

use std::sync::OnceLock;

use base_db::Db;
use hir::InFile;
use intern::Symbol;
use hir_def::{
    AdtId, AttrDefId, EnumId, HasModule as _, Lookup as _,
    attrs::AttrFlags,
    hir::{BindingAnnotation, BinaryOp, Expr, ExprId, Pat, PatId, Statement},
    lang_item::LangItems,
    resolver::ValueNs,
    signatures::FunctionSignature,
};
use hir_ty::{
    Adjust, AutoBorrow, ImplTraitId,
    db::HirDatabase,
    drop::{DropGlue, has_drop_glue},
    infer::AutoBorrowMutability,
    next_solver::{AliasTy, AliasTyKind, SolverDefId, Ty, TyKind},
};
use rustc_type_ir::{
    ClauseKind, Interner as _,
    elaborate::elaborate,
    inherent::{BoundExistentialPredicates as _, GenericArgs as _, IntoKind as _},
};
use smallvec::SmallVec;
use syntax::{
    SyntaxKind, SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::item_range_view,
    lints::bodies::{
        BodyCx, ExprKinds, ExprKind, Hooks, LatePass, LintSet, PatKinds, PatKind, lint_set,
        types::util::{Solver, diagnostic_item, from_expansion, path_item, path_value},
    },
    lower::{Found, Phase, break_outside_of_loop::up},
    model::{Applicability, Diag, Span},
};

/// Statement `index` of the block `block`.
fn statement<'db>(b: &BodyCx<'_, '_, 'db>, block: ExprId, index: usize) -> Option<Statement<'db>> {
    match b.store().expr(block) {
        Expr::Block { statements, .. } => statements.get(index),
        _ => None,
    }
}

/// The expression of statement `index` of `block` when it ends with `;` (rustc's
/// `StmtKind::Semi`).
fn semi_expr(b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize) -> Option<ExprId> {
    match statement(b, block, index)? {
        Statement::Expr { expr, has_semi: true } => Some(expr),
        _ => None,
    }
}

/// The span of a statement or a `let` as rustc gives it (`Stmt::span`): from its first token
/// after its attributes to its `;`.
fn stmt_span(b: &BodyCx<'_, '_, '_>, node: &InFile<ast::Node<'_>>) -> Span {
    b.cx.span(node.with_value(item_range_view(node.value)))
}

/// The expression statement `expr` is the expression of, as written.
fn expr_stmt<'db>(b: &BodyCx<'_, '_, 'db>, expr: ExprId) -> Option<InFile<ast::Node<'db>>> {
    let node = b.cx.syntax_view(b.expr_node(expr)?);
    up(b.cx, node).find(|it| it.value.kind() == SyntaxKind::EXPR_STMT)
}

/// rustc's `Ty::needs_drop` in the typing environment of the body's item.
fn needs_drop<'db>(solver: &Solver<'db>, ty: Ty<'db>) -> bool {
    has_drop_glue(&solver.infcx, ty, solver.env) != DropGlue::None
}

fn is_must_use(db: &Db, def: AttrDefId) -> bool {
    AttrFlags::must_use_message(db, def).is_some()
}

/// The diagnostic items these passes look for, interned once, so that a check is a pointer
/// compare.
struct Items {
    result: Symbol,
    control_flow: Symbol,
    box_new: Symbol,
    /// rustc's guards of a lock, which `let _ = ` drops at once.
    sync_guards: [Symbol; 3],
}

fn items() -> &'static Items {
    static ITEMS: OnceLock<Items> = OnceLock::new();
    ITEMS.get_or_init(|| Items {
        result: Symbol::intern("Result"),
        control_flow: Symbol::intern("ControlFlow"),
        box_new: Symbol::intern("box_new"),
        sync_guards: ["MutexGuard", "RwLockReadGuard", "RwLockWriteGuard"].map(Symbol::intern),
    })
}

/// Whether `id` is core's enum `name` (`Result`, `ControlFlow`): its diagnostic item, or the
/// enum of its lang-item variant `variant` (`Ok`, `Continue`).
fn is_enum(b: &BodyCx<'_, '_, '_>, id: EnumId, name: Symbol, variant: Option<hir_def::EnumVariantId>) -> bool {
    let db = b.db();
    variant.is_some_and(|it| it.lookup(db).parent == id)
        || diagnostic_item(db, AttrDefId::AdtId(AdtId::EnumId(id))) == Some(name)
}

fn lang_items<'db>(solver: &Solver<'db>) -> &'db LangItems {
    solver.interner().lang_items()
}

/// `PathStatements`: a statement that is a path and nothing else.
#[derive(Default)]
pub(crate) struct PathStatements;

impl LatePass for PathStatements {
    const LINTS: LintSet = lint_set(["path_statements"]);
    const HOOKS: Hooks = Hooks::STMT;

    fn check_stmt(&mut self, b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize, out: &mut Vec<Found>) {
        let Some(expr) = semi_expr(b, block, index) else { return };
        let Expr::Path(_) = b.store().expr(expr) else { return };
        let (Some(stmt), Some(at)) = (expr_stmt(b, expr), b.expr_node(expr)) else { return };
        let span = stmt_span(b, &stmt);
        let solver = Solver::of(b);
        let diag = if needs_drop(&solver, b.infer.expr_ty(expr)) {
            // clone: a span, for the lint's diagnostic
            let mut diag = Diag::lint("path_statements", "path statement drops value", span.clone());
            let message = "use `drop` to clarify the intent";
            match b.expr_span(expr).and_then(|it| b.cx.snippet(&it)) {
                Some(snippet) => {
                    // alloc: the lint's diagnostic
                    diag.suggestion(span, message, format!("drop({snippet});"), Applicability::MachineApplicable);
                }
                None => {
                    diag.span_help(span, message);
                }
            }
            diag
        } else {
            Diag::lint("path_statements", "path statement with no effect", span)
        };
        out.push(b.lint(diag, Phase::Late, at));
    }
}

/// A `let` as rustc's `LetStmt` is: a `let` statement, or the one a destructuring assignment
/// is lowered to.
struct Local {
    pat: PatId,
    init: Option<ExprId>,
    /// A type is written on it.
    typed: bool,
    /// The assignment it is lowered from: rustc's `LocalSource::AssignDesugar`.
    assign: Option<ExprId>,
}

/// Where a [`Local`] is written: its span, and where its lints are emitted (the statement,
/// or the assignment).
struct Written {
    span: Span,
    at: InFile<SyntaxNodePtr>,
}

impl Local {
    /// The `let` statement `index` of `block`.
    fn of_stmt(b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize) -> Option<Local> {
        let Statement::Let { pat, type_ref, initializer, .. } = statement(b, block, index)? else { return None };
        Some(Local { pat, init: initializer, typed: type_ref.is_some(), assign: None })
    }

    /// The `let` a destructuring assignment `expr` is lowered to (`(a, _) = ..`, `_ = ..`):
    /// one whose target is not a place expression (rustc's `is_ordinary`).
    fn of_assignment(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<Local> {
        let Expr::Assignment { target, value } = b.store().expr(expr) else { return None };
        if matches!(b.store().pat(target), Pat::Expr(_) | Pat::Missing) {
            return None;
        }
        Some(Local { pat: target, init: Some(value), typed: false, assign: Some(expr) })
    }

    /// Where it is written; a `let` hir-def made (the moves of an `async fn`'s arguments,
    /// rustc's `LocalSource::AsyncFn`) is not.
    fn written(&self, b: &BodyCx<'_, '_, '_>) -> Option<Written> {
        if let Some(expr) = self.assign {
            return Some(Written { span: b.expr_span(expr)?, at: b.expr_node(expr)? });
        }
        let node = b.cx.syntax_view(b.pat_node(self.pat)?);
        let stmt = up(b.cx, node).find(|it| ast::LetStmt::can_cast(it.value.kind()))?;
        Some(Written { span: stmt_span(b, &stmt), at: stmt.map(ast::ptr) })
    }
}

/// `LetUnderscore`: a `_` in a `let` that drops a value with a destructor at once.
#[derive(Default)]
pub(crate) struct LetUnderscore;

impl LatePass for LetUnderscore {
    const LINTS: LintSet = lint_set(["let_underscore_drop", "let_underscore_lock"]);
    const HOOKS: Hooks = Hooks::LOCAL;
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Assignment]);

    fn check_local(&mut self, b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize, out: &mut Vec<Found>) {
        if let Some(local) = Local::of_stmt(b, block, index) {
            let_underscore(b, &local, out);
        }
    }

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        if let Some(local) = Local::of_assignment(b, expr) {
            let_underscore(b, &local, out);
        }
    }
}

/// rustc's `LetUnderscore::check_local`: each `_` of the pattern, outermost first; a lock
/// wherever it is, a value with a destructor only at the top.
fn let_underscore(b: &BodyCx<'_, '_, '_>, local: &Local, out: &mut Vec<Found>) {
    let store = b.store();
    let mut wild: SmallVec<[(usize, PatId); 2]> = SmallVec::new();
    let mut index = 0;
    store.walk_pats(local.pat, &mut |it| {
        if matches!(store.pat(it), Pat::Wild) {
            wild.push((index, it));
        }
        index += 1;
    });
    if wild.is_empty() {
        return;
    }
    let solver = Solver::of(b);
    let mut written = None;
    for (index, pat) in wild {
        let ty = b.infer.pat_ty(pat);
        if !needs_drop(&solver, ty) {
            continue;
        }
        let Some(local_at) = written.get_or_insert_with(|| local.written(b)) else { return };
        let lang = lang_items(&solver);
        let lock = match ty.as_adt() {
            Some((AdtId::EnumId(id), args)) if is_enum(b, id, items().result, lang.ResultOk) => args.type_at(0),
            _ => ty,
        };
        let is_sync_lock = lock.as_adt().is_some_and(|(adt, _)| {
            diagnostic_item(b.db(), AttrDefId::AdtId(adt)).is_some_and(|it| items().sync_guards.contains(&it))
        });
        let Some(pat_span) = b.pat_span(pat) else { continue };
        let init = if index == 0 { local.init.and_then(|it| b.expr_span(it)) } else { None };
        let drop_fn = init.as_ref().filter(|it| it.file == local_at.span.file).map(|init| {
            (local_at.span.with_hi(init.lo), init.shrink_to_hi())
        });
        let assign = local.assign.is_some();
        if is_sync_lock {
            let mut diag =
                // clone: a span, for the lint's diagnostic
                Diag::lint("let_underscore_lock", "non-binding let on a synchronization lock", pat_span.clone());
            // clone: a span, for the lint's diagnostic
            diag.span_label(pat_span.clone(), "this lock is not assigned to a binding and is immediately dropped");
            non_binding_let_sub(&mut diag, pat_span, drop_fn, assign);
            out.push(b.lint(diag, Phase::Late, local_at.at));
        } else if init.is_some() {
            let mut diag = Diag::lint(
                "let_underscore_drop",
                "non-binding let on a type that has a destructor",
                // clone: a span, for the lint's diagnostic
                local_at.span.clone(),
            );
            non_binding_let_sub(&mut diag, pat_span, drop_fn, assign);
            out.push(b.lint(diag, Phase::Late, local_at.at));
        }
    }
}

/// rustc's `NonBindingLetSub`: bind the value to `_unused`, or drop it with `drop(..)`.
fn non_binding_let_sub(diag: &mut Diag, pat: Span, drop_fn: Option<(Span, Span)>, assign: bool) {
    let binding = "consider binding to an unused variable to avoid immediately dropping the value";
    if drop_fn.is_some() || !assign {
        let prefix = if assign { "let " } else { "" };
        // alloc: the lint's diagnostic
        diag.suggestion_verbose(pat, binding, format!("{prefix}_unused"), Applicability::MachineApplicable);
    } else {
        diag.span_help(pat, binding);
    }
    match drop_fn {
        Some((start, end)) => {
            diag.multipart_suggestion(
                "consider immediately dropping the value",
                // alloc: the lint's diagnostic
                vec![(start, "drop(".to_owned()), (end, ")".to_owned())],
                Applicability::MachineApplicable,
            );
        }
        None => {
            diag.help("consider immediately dropping the value using `drop(..)` after the `let` statement");
        }
    }
}

/// What rustc's `is_ty_must_use` says of a type.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MustUse {
    /// `#[must_use]`, through what holds it: `unused_must_use`'s.
    Yes,
    /// An ordinary type: `unused_results`'s.
    No,
    /// `()`, an uninhabited type, or a tuple of them: never linted.
    Trivial,
}

/// `UnusedResults`: of rustc's pass, `unused_results`: a statement's value of a type that is
/// neither trivial nor `#[must_use]`, from no `#[must_use]` function or operator.
#[derive(Default)]
pub(crate) struct UnusedResults;

impl LatePass for UnusedResults {
    const LINTS: LintSet = lint_set(["unused_results"]);
    const HOOKS: Hooks = Hooks::STMT;

    fn check_stmt(&mut self, b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize, out: &mut Vec<Found>) {
        let Some(stmt_expr) = semi_expr(b, block, index) else { return };
        let store = b.store();
        let db = b.db();
        let mut expr = stmt_expr;
        while let Expr::Block { tail: Some(tail), .. } = store.expr(expr) {
            expr = tail;
        }
        match store.expr(expr) {
            Expr::Return { .. } => return,
            Expr::Await { expr: future } if awaits_must_use_async_fn(b, future) => return,
            _ => {}
        }
        let ty = b.infer.expr_ty(expr);
        if ty.is_unit() || ty_must_use(b, &Solver::of(b), ty, Some(expr)) != MustUse::No {
            return;
        }
        let callee = match store.expr(expr) {
            Expr::Call { callee, .. } => match path_value(b, callee) {
                Some(ValueNs::FunctionId(it)) => Some(it),
                _ => None,
            },
            Expr::MethodCall { .. } => b.infer.method_resolution(expr).map(|(it, _)| it),
            _ => None,
        };
        if callee.is_some_and(|it| is_must_use(db, AttrDefId::FunctionId(it))) || is_must_use_op(b, expr) {
            return;
        }
        let (Some(stmt), Some(at), Some(node)) = (expr_stmt(b, stmt_expr), b.expr_node(stmt_expr), b.expr_node(expr))
        else {
            return;
        };
        let Some(written) = ast::Expr::cast(b.cx.syntax_view(node).value) else { return };
        let Some(ty) = b.cx.vsema.type_of_expr(written) else { return };
        let ty = b.cx.ty(&ty.original);
        // alloc: the lint's diagnostic
        let diag = Diag::lint("unused_results", format!("unused result of type `{ty}`"), stmt_span(b, &stmt));
        out.push(b.lint(diag, Phase::Late, at));
    }
}

/// `future.await` of the future an `async fn` that is `#[must_use]` returns: rustc gives
/// `unused_must_use` of it and nothing else.
fn awaits_must_use_async_fn(b: &BodyCx<'_, '_, '_>, future: ExprId) -> bool {
    let db = b.db();
    let TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { def_id }, .. }) = b.infer.expr_ty(future).kind() else {
        return false;
    };
    let ImplTraitId::ReturnTypeImplTrait(function, _) = def_id.0.loc(db) else { return false };
    FunctionSignature::of(db, function).is_async() && is_must_use(db, AttrDefId::FunctionId(function))
}

/// rustc's operators whose value is to be used (`UnusedResults::check_stmt`'s `must_use_op`):
/// comparisons, arithmetic, logic, bit operations, borrows, unary operators and `offset_of!`.
fn is_must_use_op(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
    let store = b.store();
    match store.expr(expr) {
        Expr::BinaryOp { op: Some(op), .. } => !matches!(op, BinaryOp::Assignment { .. }),
        Expr::Ref { .. } | Expr::UnaryOp { .. } | Expr::OffsetOf(_) => true,
        Expr::Const(body) => matches!(
            store.expr(body),
            Expr::Block { tail: Some(tail), .. } if matches!(store.expr(tail), Expr::OffsetOf(_))
        ),
        _ => false,
    }
}

/// rustc's `is_ty_must_use` of `ty`, the type of `expr` (whose elements, when it is a tuple,
/// are the elements' expressions).
fn ty_must_use<'db>(b: &BodyCx<'_, '_, 'db>, solver: &Solver<'db>, ty: Ty<'db>, expr: Option<ExprId>) -> MustUse {
    if ty.is_unit() {
        return MustUse::Trivial;
    }
    let db = b.db();
    let module = b.owner.module(db);
    let uninhabited =
        |ty: Ty<'db>| hir_ty::inhabitedness::is_ty_uninhabited_from(&solver.infcx, ty, module, solver.env);
    if uninhabited(ty) {
        return MustUse::Trivial;
    }
    let yes = |it: bool| if it { MustUse::Yes } else { MustUse::No };
    match ty.kind() {
        TyKind::Adt(def, args) => {
            if let Some(boxed) = ty.boxed_ty() {
                return ty_must_use(b, solver, boxed, expr);
            }
            let lang = lang_items(solver);
            let adt = def.def_id();
            if lang.Pin.is_some_and(|it| adt == AdtId::StructId(it)) {
                return ty_must_use(b, solver, args.type_at(0), expr);
            }
            if let AdtId::EnumId(id) = adt {
                if is_enum(b, id, items().result, lang.ResultOk) && uninhabited(args.type_at(1)) {
                    return ty_must_use(b, solver, args.type_at(0), expr);
                }
                if is_enum(b, id, items().control_flow, lang.ControlFlowContinue) && uninhabited(args.type_at(0)) {
                    return ty_must_use(b, solver, args.type_at(1), expr);
                }
            }
            yes(is_must_use(db, AttrDefId::AdtId(adt)))
        }
        TyKind::Alias(_, alias) => {
            let def_id: SolverDefId<'_> = match alias.kind {
                AliasTyKind::Opaque { def_id } => def_id.into(),
                AliasTyKind::Projection { def_id } => def_id.into(),
                _ => return MustUse::No,
            };
            let interner = solver.interner();
            let bounds = interner.item_self_bounds(def_id).skip_binder();
            yes(elaborate(interner, bounds).filter_only_self().any(|clause| match clause.kind().skip_binder() {
                ClauseKind::Trait(pred) => is_must_use(db, AttrDefId::TraitId(pred.def_id().0)),
                _ => false,
            }))
        }
        TyKind::Dynamic(predicates, _) => {
            yes(predicates.principal_def_id().is_some_and(|it| is_must_use(db, AttrDefId::TraitId(it.0))))
        }
        TyKind::Tuple(tys) => {
            let elements: Option<&[ExprId]> = match expr.map(|it| b.store().expr(it)) {
                Some(Expr::Tuple { exprs }) if exprs.len() == tys.len() => Some(exprs),
                _ => None,
            };
            let mut all_trivial = true;
            let mut nested = false;
            for (index, ty) in tys.iter().enumerate() {
                let at = elements.map_or(expr, |it| Some(it[index]));
                match ty_must_use(b, solver, ty, at) {
                    MustUse::Trivial => {}
                    MustUse::Yes => {
                        all_trivial = false;
                        nested = true;
                    }
                    MustUse::No => all_trivial = false,
                }
            }
            match (all_trivial, nested) {
                (true, _) => MustUse::Trivial,
                (false, true) => MustUse::Yes,
                (false, false) => MustUse::No,
            }
        }
        TyKind::Array(element, len) => match hir_ty::consteval::try_const_usize(db, len) {
            Some(0) | None => MustUse::No,
            Some(_) => ty_must_use(b, solver, element, expr),
        },
        TyKind::Closure(..) | TyKind::CoroutineClosure(..) | TyKind::Coroutine(..) => MustUse::Yes,
        _ => MustUse::No,
    }
}

/// `UnitBindings`: a `let` that binds a value of `()` with no type written, neither side
/// written as `()`, outside of macros.
#[derive(Default)]
pub(crate) struct UnitBindings;

impl LatePass for UnitBindings {
    const LINTS: LintSet = lint_set(["unit_bindings"]);
    const HOOKS: Hooks = Hooks::LOCAL;
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Assignment]);

    fn check_local(&mut self, b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize, out: &mut Vec<Found>) {
        if let Some(local) = Local::of_stmt(b, block, index) {
            unit_binding(b, &local, out);
        }
    }

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        if let Some(local) = Local::of_assignment(b, expr) {
            unit_binding(b, &local, out);
        }
    }
}

fn unit_binding(b: &BodyCx<'_, '_, '_>, local: &Local, out: &mut Vec<Found>) {
    let store = b.store();
    let Some(init) = local.init else { return };
    if local.typed || !b.infer.expr_ty(init).is_unit() || !b.infer.pat_ty(local.pat).is_unit() {
        return;
    }
    if matches!(store.expr(init), Expr::Tuple { exprs } if exprs.is_empty()) {
        return;
    }
    if matches!(store.pat(local.pat), Pat::Tuple { args, .. } if args.is_empty()) {
        return;
    }
    let Some(written) = local.written(b) else { return };
    if from_expansion(&written.span) {
        return;
    }
    let Some(pat) = b.pat_span(local.pat) else { return };
    let mut diag = Diag::lint("unit_bindings", "binding has unit type `()`", written.span);
    diag.span_label(pat, "this pattern is inferred to be the unit type `()`");
    out.push(b.lint(diag, Phase::Late, written.at));
}

/// `NonShorthandFieldPatterns`: `Struct { x: x }` in a pattern, outside of macros.
#[derive(Default)]
pub(crate) struct NonShorthandFieldPatterns;

impl LatePass for NonShorthandFieldPatterns {
    const LINTS: LintSet = lint_set(["non_shorthand_field_patterns"]);
    const PATS: PatKinds = PatKinds::of(&[PatKind::Record]);

    fn check_pat(&mut self, b: &BodyCx<'_, '_, '_>, pat: PatId, out: &mut Vec<Found>) {
        let store = b.store();
        let Pat::Record { args, .. } = store.pat(pat) else { return };
        if b.tainted() {
            return;
        }
        let Some(at) = b.pat_node(pat) else { return };
        for field in args.iter() {
            let Pat::Bind { id, subpat: None } = store.pat(field.pat) else { continue };
            let binding = &store[id];
            if binding.name != field.name {
                continue;
            }
            let Some(node) = b.pat_node(field.pat) else { continue };
            let node = b.cx.syntax_view(node);
            let Some(written) = node.value.parent().and_then(ast::RecordPatField::cast) else { continue };
            if written.colon_token().is_none() {
                continue;
            }
            let span = b.cx.span(node.with_value(item_range_view(written.syntax())));
            if from_expansion(&span) {
                continue;
            }
            let prefix = match binding.mode {
                BindingAnnotation::Unannotated => "",
                BindingAnnotation::Mutable => "mut ",
                BindingAnnotation::Ref => "ref ",
                BindingAnnotation::RefMut => "ref mut ",
            };
            // alloc: the lint's diagnostic
            let name = binding.name.display_no_db(b.cx.edition).to_string();
            let mut diag = Diag::lint(
                "non_shorthand_field_patterns",
                // alloc: the lint's diagnostic
                format!("the `{name}:` in this pattern is redundant"),
                // clone: a span, for the lint's diagnostic
                span.clone(),
            );
            diag.suggestion(
                span,
                "use shorthand field pattern",
                // alloc: the lint's diagnostic
                format!("{prefix}{name}"),
                Applicability::MachineApplicable,
            );
            out.push(b.lint(diag, Phase::Late, at));
        }
    }
}

/// `UnusedAllocation`: `Box::new(x)` borrowed at once, by an adjustment to `&` or `&mut` of
/// what it holds.
#[derive(Default)]
pub(crate) struct UnusedAllocation;

impl LatePass for UnusedAllocation {
    const LINTS: LintSet = lint_set(["unused_allocation"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Call]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Expr::Call { callee, args } = b.store().expr(expr) else { return };
        if args.len() != 1 {
            return;
        }
        let Some(adjustments) = b.infer.expr_adjustment(expr) else { return };
        if path_item(b, callee) != Some(items().box_new) {
            return;
        }
        for adjustment in adjustments {
            let Adjust::Borrow(AutoBorrow::Ref(mutability)) = &adjustment.kind else { continue };
            if let TyKind::Ref(_, inner, _) = adjustment.target.as_ref().kind()
                && inner.is_box()
            {
                continue;
            }
            let message = match mutability {
                AutoBorrowMutability::Not => "unnecessary allocation, use `&` instead",
                AutoBorrowMutability::Mut { .. } => "unnecessary allocation, use `&mut` instead",
            };
            let (Some(span), Some(at)) = (b.expr_span(expr), b.expr_node(expr)) else { return };
            out.push(b.lint(Diag::lint("unused_allocation", message, span), Phase::Late, at));
        }
    }
}
