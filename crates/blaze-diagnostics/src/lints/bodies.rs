//! What rustc lints in the crate's bodies (functions, consts, statics, enum discriminants, the
//! closures and inline consts in them): what type checking, match checking, unsafety checking,
//! liveness and the MIR lints find, whatever else failed, and what its late lint passes find,
//! only when nothing failed.
//!
//! - `reachable`: `unreachable_code` (typeck).
//! - `unstable_names`: `unstable_name_collisions` (typeck).
//! - `patterns`: what match checking lints (`unreachable_patterns`, `bindings_with_variant_name`,
//!   `irrefutable_let_patterns`).
//! - `recursion`: `unconditional_recursion` (the MIR lints of calls, and of drops after drop
//!   elaboration).
//! - `unsafety`: `unused_unsafe` (`check_unsafety`).
//! - `liveness`: `unused_variables`, `unused_assignments` (`check_liveness`).
//! - `panics`: `arithmetic_overflow`, `unconditional_panic` (the known-panics lint).
//! - `types`, `calls`: rustc's late lint passes of expressions, statements and patterns.
//!
//! Follows rustc's `compiler/rustc_interface/src/passes.rs` (`run_required_analyses`: each body
//! owner's `check_unsafety`, which builds its MIR and so checks its matches first, then
//! `mir_borrowck`, which first runs the MIR lints of `mir_promoted`, `check_liveness` and, when
//! rustc makes code, `mir_drops_elaborated_and_const_checked`, in the order of
//! `hir_crate_items(()).body_owners`), `compiler/rustc_lint/src/late.rs` (`LateContextAndPass`:
//! each node's `check_*` before its children's, every pass at each node) and
//! `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedLateLintModPass`: the order of the passes)
//! (MIT or Apache-2.0).
//!
//! Speed (`docs/v2/plan/lanes/diagnostics/speed-of-light.md`, "Bodies"): a pass whose every lint
//! is allowed in the crate is not run (rustc's `lints_that_dont_need_to_run`). The late passes
//! are one walk per body, each node's tag indexing a table, built once per crate, of the passes
//! that look at that kind of node; a pass is a variant of [`Pass`], called by `match`, never
//! through a vtable. What a body needs only for some passes (its preorder, its taint, its MIR)
//! is computed the first time one asks, into buffers reused from body to body.

#[cfg(test)]
mod tests;
mod calls;
mod liveness;
mod panics;
mod patterns;
mod reachable;
mod recursion;
mod types;
mod unsafety;
mod unstable_names;

use std::cell::{Cell, OnceCell};

use base_db::Db;
use blaze_hash::HashMap;
use blaze_ids::radix;
use blaze_items::{Fact, ItemId};
use hir::{DefWithBody, InFile, db::HirDatabase};
use hir_def::{
    AdtId, DefWithBodyId, Lookup, ModuleDefId,
    expr_store::{Body, BodySourceMap, ExpressionStore, ExpressionStoreDiagnostics},
    hir::{Array, BindingId, Expr, ExprId, Pat, PatId, Statement},
    nameres::demand::every_body,
    signatures::FunctionSignature,
    src::HasSource as _,
};
use hir_expand::MacroCallKind;
use hir_ty::{
    InferBodyId, InferenceDiagnostic, InferenceResult, PathLoweringDiagnostic,
    TyLoweringDiagnostic,
    mir::{MirBody, MirSpan},
};
use span::FileId;
use syntax::{
    SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lints::registry::{LintId, LintSet},
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// What type checking, match checking, unsafety checking, liveness and the MIR lints lint in
/// the crate's bodies, given whatever else failed: each body in rustc's order, its typeck's
/// lints at [`Phase::Bodies`], the rest at [`Phase::Mir`]. `codegen` is whether rustc makes
/// code of the crate (`--emit` asks for more than metadata: `cargo build`, not `cargo check`),
/// which runs the lints after drop elaboration on every body, not only on those of consts.
pub(crate) fn before_abort(cx: &Cx<'_, '_>, codegen: bool) -> Vec<Found> {
    let runs = Runs::of(cx);
    if !runs.any() {
        // alloc: an empty `Vec` allocates nothing.
        return Vec::new();
    }
    // alloc: the lints found, empty until one is.
    let mut found = Vec::new();
    let mut scratch = Scratch::default();
    let mut pool = liveness::Pool::default();
    for owner in body_owners(cx) {
        let Some(mut b) = BodyCx::new(cx, owner) else { continue };
        b.lend(std::mem::take(&mut scratch));
        b.elaborated = codegen || b.is_const_context();
        if runs.unstable_names {
            found.extend(unstable_names::lints(&b));
        }
        if runs.reachable {
            found.extend(reachable::lints(&b));
        }
        if runs.patterns {
            found.extend(patterns::lints(&b));
        }
        if runs.unsafety {
            found.extend(unsafety::lints(&b));
        }
        if runs.reachable {
            found.extend(reachable::after_calls(&b));
        }
        if runs.recursion {
            found.extend(recursion::calls(&b));
        }
        if runs.liveness {
            found.extend(liveness::lints(&b, &mut pool));
        }
        if runs.recursion {
            found.extend(recursion::drops(&b));
        }
        if runs.panics {
            found.extend(panics::lints(&b));
        }
        scratch = b.into_scratch();
    }
    found
}

/// What rustc's late lint passes lint in bodies (`unused_comparisons`, `path_statements`,
/// `overflowing_literals`, ..), run only when nothing failed: each pass at each node of each
/// body, in rustc's order.
pub(crate) fn after_abort(cx: &Cx<'_, '_>) -> Vec<Found> {
    let mut passes = Passes::new(cx);
    if !passes.any {
        // alloc: an empty `Vec` allocates nothing.
        return Vec::new();
    }
    // alloc: the lints found, empty until one is.
    let mut found = Vec::new();
    let mut scratch = Scratch::default();
    for owner in body_owners(cx) {
        let Some(mut b) = BodyCx::new(cx, owner) else { continue };
        b.lend(std::mem::take(&mut scratch));
        Late { b: &b, passes: &mut passes, out: &mut found }.body();
        scratch = b.into_scratch();
    }
    found
}

/// Whether one of `lints` can be given in the crate: rustc's `lints_that_dont_need_to_run`
/// leaves it out. One AND per word of the set.
pub(crate) fn may_fire(cx: &Cx<'_, '_>, lints: &LintSet) -> bool {
    cx.any_may_fire(lints)
}

/// The builtin lints `names`, as a set made at compile time: a name of no builtin lint fails
/// to compile.
pub(crate) const fn lint_set<const N: usize>(names: [&str; N]) -> LintSet {
    let mut ids = [LintId::WARNINGS; N];
    let mut at = 0;
    while at < N {
        ids[at] = LintId::named(names[at]);
        at += 1;
    }
    LintSet::of(&ids)
}

/// Which passes of [`before_abort`] run in the crate: those one of whose lints may fire.
#[derive(Clone, Copy)]
struct Runs {
    reachable: bool,
    unstable_names: bool,
    patterns: bool,
    unsafety: bool,
    recursion: bool,
    liveness: bool,
    panics: bool,
}

impl Runs {
    fn of(cx: &Cx<'_, '_>) -> Runs {
        Runs {
            reachable: may_fire(cx, &reachable::LINTS),
            unstable_names: may_fire(cx, &unstable_names::LINTS),
            patterns: may_fire(cx, &patterns::LINTS),
            unsafety: may_fire(cx, &unsafety::LINTS),
            recursion: may_fire(cx, &recursion::LINTS),
            liveness: may_fire(cx, &liveness::LINTS),
            panics: may_fire(cx, &panics::LINTS),
        }
    }

    fn any(self) -> bool {
        self.reachable
            || self.unstable_names
            || self.patterns
            || self.unsafety
            || self.recursion
            || self.liveness
            || self.panics
    }
}

/// The buffers a body's [`BodyCx::exprs`] are built in, handed
/// from body to body so that a crate's bodies grow them to the largest body's size once.
#[derive(Default)]
pub(crate) struct Scratch {
    exprs: Vec<ExprId>,
    stack: Vec<ExprId>,
}

/// One body being linted, with what it was lowered to and inferred as.
pub(crate) struct BodyCx<'a, 'c, 'db> {
    pub(crate) cx: &'a Cx<'c, 'db>,
    pub(crate) owner: DefWithBodyId,
    pub(crate) body: &'db Body,
    pub(crate) source_map: &'db BodySourceMap,
    pub(crate) infer: &'db InferenceResult<'db>,
    /// rustc runs `mir_drops_elaborated_and_const_checked` on the body before it aborts, so
    /// its lints (the known-panics lint, a `Drop` impl's `unconditional_recursion`): it makes
    /// code, or the body is in a const context. Set by [`before_abort`].
    pub(crate) elaborated: bool,
    /// [`BodyCx::tainted`] and [`BodyCx::mir_tainted`], once asked.
    tainted: Cell<Option<bool>>,
    mir_tainted: Cell<Option<bool>>,
    /// [`BodyCx::mir`] and [`BodyCx::runtime_mir`], once asked: one query fetch per body.
    mir: OnceCell<Option<&'db MirBody<'db>>>,
    runtime_mir: OnceCell<Option<&'db MirBody<'db>>>,
    /// The expressions of the body in the order rustc visits them, each before those in it;
    /// those of an anonymous const in it (an array length) are the const's. Built the first
    /// time a pass asks ([`BodyCx::exprs`]), in `scratch`'s buffers.
    exprs: OnceCell<Vec<ExprId>>,
    scratch: Cell<Scratch>,
}

impl<'a, 'c, 'db> BodyCx<'a, 'c, 'db> {
    /// The body of `owner`, if it has one written (a variant has one only when its
    /// discriminant is).
    pub(crate) fn new(cx: &'a Cx<'c, 'db>, owner: DefWithBodyId) -> Option<BodyCx<'a, 'c, 'db>> {
        let db = cx.db;
        let (body, source_map) = Body::with_source_map(db, owner);
        let body: &'db Body = body;
        if matches!(body.expr(body.root_expr()), Expr::Missing) && body.params.is_empty() {
            return None;
        }
        let infer = InferenceResult::of(db, owner);
        Some(BodyCx {
            cx,
            owner,
            body,
            source_map,
            infer,
            elaborated: false,
            tainted: Cell::new(None),
            mir_tainted: Cell::new(None),
            mir: OnceCell::new(),
            runtime_mir: OnceCell::new(),
            exprs: OnceCell::new(),
            scratch: Cell::new(Scratch::default()),
        })
    }

    /// Builds [`BodyCx::exprs`] in `scratch`'s buffers, those of the body linted before.
    fn lend(&mut self, scratch: Scratch) {
        *self.scratch.get_mut() = scratch;
    }

    /// The buffers back, for the next body.
    fn into_scratch(self) -> Scratch {
        let mut scratch = self.scratch.into_inner();
        if let Some(exprs) = self.exprs.into_inner() {
            scratch.exprs = exprs;
        }
        scratch
    }

    pub(crate) fn db(&self) -> &'db Db {
        self.cx.db
    }

    pub(crate) fn store(&self) -> &'db ExpressionStore {
        &self.body.store
    }

    /// rustc's `tainted_by_errors`: type checking the body gave an error, or AST lowering gave
    /// it an error expression or loop destination, so rustc builds no THIR of it and runs none
    /// of its MIR lints (match checking, unsafety, liveness, known panics).
    pub(crate) fn tainted(&self) -> bool {
        if let Some(tainted) = self.tainted.get() {
            return tainted;
        }
        let tainted = self.infer.diagnostics().iter().any(taints) || lowering_failed(self.db(), self.source_map);
        self.tainted.set(Some(tainted));
        tainted
    }

    /// rustc's `mir_built` is `construct_error`'s body, with no lint to give: `tainted`, or
    /// match checking failed (E0004, E0005), which leaves unsafety checking of the THIR.
    pub(crate) fn mir_tainted(&self) -> bool {
        if let Some(tainted) = self.mir_tainted.get() {
            return tainted;
        }
        let tainted = self.tainted() || DefWithBody::from(self.owner).match_check_failed(self.db());
        self.mir_tainted.set(Some(tainted));
        tainted
    }

    /// The expressions of the body, each before those in it, in the order rustc visits them.
    pub(crate) fn exprs(&self) -> &[ExprId] {
        self.exprs.get_or_init(|| {
            let Scratch { mut exprs, mut stack } = self.scratch.take();
            preorder(self.store(), self.body.root_expr(), &mut exprs, &mut stack);
            // alloc: an empty `Vec` allocates nothing; the scratch's buffer is put back after.
            self.scratch.set(Scratch { exprs: Vec::new(), stack });
            exprs
        })
    }

    /// The node `expr` is written as, if it is written (not made by lowering).
    pub(crate) fn expr_node(&self, expr: ExprId) -> Option<InFile<SyntaxNodePtr>> {
        let source = self.source_map.expr_syntax(expr).ok()?;
        Some(source.map(|it| it.syntax_node_ptr()))
    }

    pub(crate) fn pat_node(&self, pat: PatId) -> Option<InFile<SyntaxNodePtr>> {
        let source = self.source_map.pat_syntax(pat).ok()?;
        Some(source.map(|it| it.syntax_node_ptr()))
    }

    /// The pattern a binding is first bound by, or `self`.
    pub(crate) fn binding_node(&self, binding: BindingId) -> Option<InFile<SyntaxNodePtr>> {
        let is_self = self.body.self_param.is_some_and(|it| it.formal == binding || it.user_written == binding);
        if is_self {
            return self.source_map.self_param_syntax().map(|it| it.map(|it| it.syntax_node_ptr()));
        }
        let pat = *self.source_map.patterns_for_binding(binding).first()?;
        self.pat_node(pat)
    }

    /// rustc's span of `expr`: where it is written, with the parentheses around it, which rustc's
    /// lowering gives the expression in them when their span contains its (`rustc_ast_lowering`,
    /// `ExprKind::Paren`). Parentheses a macro put around an expression it was handed do not
    /// contain it, and are left out.
    pub(crate) fn expr_span(&self, expr: ExprId) -> Option<Span> {
        let node = self.expr_node(expr)?;
        let mut span = self.cx.span_of(node);
        let InFile { file_id, value } = self.cx.syntax_view(node);
        let mut at = value;
        while let Some(parent) = at.parent().filter(|it| ast::ParenExpr::can_cast(it.kind())) {
            let outer = self.cx.span(InFile::new(file_id, parent.text_range()));
            let contains = outer.file == span.file && outer.lo <= span.lo && span.hi <= outer.hi;
            let parenthesized =
                self.cx.snippet(&outer).is_some_and(|it| it.starts_with('(') && it.ends_with(')'));
            if !contains || !parenthesized {
                break;
            }
            span = outer;
            at = parent;
        }
        Some(span)
    }

    pub(crate) fn pat_span(&self, pat: PatId) -> Option<Span> {
        Some(self.cx.span_of(self.pat_node(pat)?))
    }

    /// The item the body is of (the function, const, static or variant): where rustc emits a
    /// lint of the body as a whole.
    pub(crate) fn owner_node(&self) -> InFile<SyntaxNodePtr> {
        owner_node(self.db(), self.owner)
    }

    /// The MIR the body is built to, before rustc's runtime passes, if it can be built.
    pub(crate) fn mir(&self) -> Option<&'db MirBody<'db>> {
        *self.mir.get_or_init(|| self.db().mir_body(InferBodyId::DefWithBodyId(self.owner)).ok())
    }

    /// The MIR of the body after drop elaboration (`MirPhase::Runtime`), if it can be built.
    pub(crate) fn runtime_mir(&self) -> Option<&'db MirBody<'db>> {
        *self.runtime_mir.get_or_init(|| self.db().runtime_mir_body(InferBodyId::DefWithBodyId(self.owner)).ok())
    }

    /// rustc's `hir_body_const_context` is some: a const, a static, a discriminant or a
    /// `const fn`.
    pub(crate) fn is_const_context(&self) -> bool {
        match self.owner {
            DefWithBodyId::FunctionId(it) => FunctionSignature::of(self.db(), it).is_const(),
            DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => true,
        }
    }

    /// The span rustc's MIR has for what `span` is of: an expression (with its parentheses, as
    /// [`BodyCx::expr_span`]), the end of one (the last character, rustc's `end_point` of a
    /// scope), a pattern, a binding or `self`.
    pub(crate) fn mir_span(&self, span: MirSpan) -> Option<Span> {
        if let MirSpan::ExprId(expr) = span {
            return self.expr_span(expr);
        }
        let node = self.mir_node(span)?;
        let MirSpan::ExprEnd(_) = span else { return Some(self.cx.span_of(node)) };
        let last = self.cx.syntax_view(node).value.last_token()?;
        let end = last.text_range().end();
        let len = TextSize::of(last.text().chars().next_back()?);
        Some(self.cx.span(InFile::new(node.file_id, TextRange::new(end - len, end))))
    }

    /// The node what `span` is of is written as (of [`MirSpan::ExprEnd`], the whole expression).
    pub(crate) fn mir_node(&self, span: MirSpan) -> Option<InFile<SyntaxNodePtr>> {
        match span {
            MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => self.expr_node(expr),
            MirSpan::PatId(pat) => self.pat_node(pat),
            MirSpan::BindingId(binding) => self.binding_node(binding),
            MirSpan::SelfParam => {
                let param = self.source_map.self_param_syntax()?;
                Some(param.map(|it| it.syntax_node_ptr()))
            }
            // Lints read built MIR, which inlines nothing.
            MirSpan::Unknown | MirSpan::Inlined(_) => None,
        }
    }

    /// A type of the body as rustc prints it, through the type of its owner.
    pub(crate) fn ty(&self, ty: hir_ty::next_solver::Ty<'db>) -> String {
        let db = self.db();
        let owner = match self.owner {
            DefWithBodyId::FunctionId(it) => hir::Function::from(it).ret_type(db),
            DefWithBodyId::ConstId(it) => hir::Const::from(it).ty(db),
            DefWithBodyId::StaticId(it) => hir::Static::from(it).ty(db),
            DefWithBodyId::VariantId(it) => hir::EnumVariant::from(it).parent_enum(db).ty(db),
        };
        self.cx.ty(&owner.derived_from_next_solver(ty))
    }

    /// rustc's `Span::in_external_macro` of `node`: written by a macro of another crate, or
    /// by an attribute or derive macro, rather than handed to it.
    pub(crate) fn in_external_macro(&self, node: InFile<SyntaxNodePtr>) -> bool {
        let db = self.db();
        let Some(call) = node.file_id.macro_file() else { return false };
        if let Some((_, ctxt)) = node.map(|it| it.text_range()).original_node_file_range_opt(db)
            && ctxt.is_root()
        {
            return false;
        }
        let loc = call.loc(db);
        match loc.kind {
            MacroCallKind::FnLike { .. } => hir::Crate::from(loc.def.krate) != self.cx.krate,
            _ => true,
        }
    }

    /// The item whose code the body is, with what to call it in a fact.
    fn item(&self) -> (ItemId, hir::ModuleDef) {
        match self.owner {
            DefWithBodyId::FunctionId(it) => (ItemId::Function(it), ModuleDefId::FunctionId(it).into()),
            DefWithBodyId::ConstId(it) => (ItemId::Const(it), ModuleDefId::ConstId(it).into()),
            DefWithBodyId::StaticId(it) => (ItemId::Static(it), ModuleDefId::StaticId(it).into()),
            DefWithBodyId::VariantId(it) => {
                let parent = it.lookup(self.db()).parent;
                (ItemId::Adt(AdtId::EnumId(parent)), ModuleDefId::EnumVariantId(it).into())
            }
        }
    }

    /// Records on `diag` that it read the code of the body.
    pub(crate) fn code_fact(&self, diag: &mut Diag) {
        let (item, def) = self.item();
        // alloc: the fact's text, for a lint found
        let what = format!("the code of `{}`", self.cx.def_path(def));
        diag.fact(Fact::Code(self.cx.key(item)), what);
    }

    /// A lint of the body found, its level decided at `at`, with the fact that it read the
    /// body's code.
    pub(crate) fn lint(&self, mut diag: Diag, phase: Phase, at: InFile<SyntaxNodePtr>) -> Found {
        self.code_fact(&mut diag);
        Found::lint(diag, phase, at)
    }
}

/// The expressions from `root` down, in rustc's order ([`Late`]'s): one pass over an explicit
/// stack, each node's children pushed and then reversed in place, so nothing is allocated per
/// node and deep bodies need no native stack. Both buffers are the caller's, reused from body
/// to body; `out` is reserved to the store's count.
fn preorder(store: &ExpressionStore, root: ExprId, out: &mut Vec<ExprId>, stack: &mut Vec<ExprId>) {
    out.clear();
    out.reserve(store.expr_count());
    stack.clear();
    stack.push(root);
    while let Some(expr) = stack.pop() {
        out.push(expr);
        let start = stack.len();
        child_exprs(store, expr, stack);
        stack[start..].reverse();
    }
}

/// Whether rustc's type checking of a body taints it for an inference diagnostic: every one
/// is an error but an elided lifetime in a path, which is the `elided_lifetimes_in_paths` lint
/// where it is not a hard error.
fn taints(diagnostic: &InferenceDiagnostic) -> bool {
    let path = match diagnostic {
        InferenceDiagnostic::PathDiagnostic { diag, .. } => diag,
        InferenceDiagnostic::TyDiagnostic { diag: TyLoweringDiagnostic::PathDiagnostic { diag, .. } } => diag,
        _ => return true,
    };
    !matches!(path, PathLoweringDiagnostic::ElidedLifetimesInPath { hard_error: false, .. })
}

/// Whether AST lowering put an error node in the body, which taints its type checking: a
/// `hir::ExprKind::Err` for `.await` outside `async` (E0728) or for a macro call that did not
/// resolve or expand (`DummyResult`, `DummyExpander`), and a `break` or `continue` destination
/// of `LoopIdError::UnresolvedLabel` (E0426, E0767). A `break` outside a loop (E0267, E0268) is
/// inference's.
fn lowering_failed(db: &Db, source_map: &BodySourceMap) -> bool {
    let errors = source_map.diagnostics().iter().any(|it| {
        matches!(
            it,
            ExpressionStoreDiagnostics::AwaitOutsideOfAsync { .. }
                | ExpressionStoreDiagnostics::UndeclaredLabel { .. }
                | ExpressionStoreDiagnostics::UnreachableLabel { .. }
                | ExpressionStoreDiagnostics::UnresolvedMacroCall { .. }
                | ExpressionStoreDiagnostics::IntLiteralTooLarge { .. }
        )
    });
    errors
        || source_map.macro_calls().any(|(_, call)| {
            call.parse_macro_expansion_error(db).is_some_and(|it| {
                !it.value.is_empty() || it.err.is_some_and(|err| err.render_to_string(db).error)
            })
        })
}

/// The expressions directly in `expr` that are of its body, in the order rustc's HIR visitor
/// walks them (`rustc_hir::intravisit::walk_expr`), those in patterns included: a `let`'s
/// initializer before its pattern, a `match` arm's pattern before its guard and value. An array
/// length is an anonymous const of its own, and not of the body.
fn child_exprs(store: &ExpressionStore, expr: ExprId, out: &mut Vec<ExprId>) {
    let pat = |pat: PatId, out: &mut Vec<ExprId>| store.walk_exprs_in_pat(pat, |it| out.push(it));
    match store.expr(expr) {
        Expr::Let { pat: p, expr } => {
            out.push(expr);
            pat(p, out);
        }
        Expr::Block { statements, tail, .. } => {
            for statement in statements {
                match statement {
                    Statement::Let { pat: p, initializer, else_branch, .. } => {
                        out.extend(initializer);
                        pat(p, out);
                        out.extend(else_branch);
                    }
                    Statement::Expr { expr, .. } => out.push(expr),
                    Statement::Item(_) => {}
                }
            }
            out.extend(tail);
        }
        Expr::Match { expr, arms } => {
            out.push(expr);
            for arm in arms.iter() {
                pat(arm.pat, out);
                out.extend(arm.guard);
                out.push(arm.expr);
            }
        }
        Expr::Closure { args, body, .. } => {
            for arg in args.iter() {
                pat(*arg, out);
            }
            out.push(body);
        }
        Expr::Assignment { target, value } => {
            pat(target, out);
            out.push(value);
        }
        Expr::Array(Array::Repeat { initializer, .. }) => out.push(initializer),
        _ => store.walk_child_exprs_without_pats(expr, |it| out.push(it)),
    }
}

/// The bodies of the crate in the order rustc gives their lints (`body_owners`, each before
/// the items in it): by where they are written, the files in the order of the modules they
/// hold.
fn body_owners(cx: &Cx<'_, '_>) -> Vec<DefWithBodyId> {
    let db = cx.db;
    let mut rank: HashMap<FileId, usize> = HashMap::default();
    for module in crate::checks::modules(cx) {
        let file = module.definition_source_file_id(db).original_file(db).file_id(db);
        let next = rank.len();
        rank.entry(file).or_insert(next);
    }
    let mut owners: Vec<(usize, TextSize, usize, DefWithBodyId)> = every_body(db, cx.krate.base())
        .into_iter()
        .enumerate()
        .map(|(index, owner)| {
            let at = owner_node(db, owner).map(|it| it.text_range());
            let at = at.original_node_file_range_rooted(db);
            let file = at.file_id.file_id(db);
            (rank.get(&file).copied().unwrap_or(usize::MAX), at.range.start(), index, owner)
        })
        // alloc: the bodies to sort, once per crate
        .collect();
    radix::sort_by_key(&mut owners, |&(file, at, _, _)| {
        radix::pack_64(file as u64, u64::from(u32::from(at)))
    });
    // alloc: the bodies in order, once per crate
    owners.into_iter().map(|(.., owner)| owner).collect()
}

fn owner_node(db: &Db, owner: DefWithBodyId) -> InFile<SyntaxNodePtr> {
    match owner {
        DefWithBodyId::FunctionId(it) => it.lookup(db).ast_ptr(db).map(|it| it.syntax_node_ptr()),
        DefWithBodyId::ConstId(it) => it.lookup(db).ast_ptr(db).map(|it| it.syntax_node_ptr()),
        DefWithBodyId::StaticId(it) => it.lookup(db).ast_ptr(db).map(|it| it.syntax_node_ptr()),
        DefWithBodyId::VariantId(it) => it.lookup(db).ast_ptr(db).map(|it| it.syntax_node_ptr()),
    }
}

/// A late lint pass of rustc's, of what it checks in bodies (`rustc_lint::LateLintPass`): each
/// method is called at each node of each body, before the methods at the nodes in it, as
/// `LateContextAndPass` calls them, but only at the nodes the pass's constants say it looks
/// at. Its state is made once per crate and reset by [`LatePass::check_body`], not remade per
/// body.
pub(crate) trait LatePass: Default {
    /// The lints the pass gives: it doesn't run in a crate where none of them may fire.
    const LINTS: LintSet;
    /// The methods below, other than `check_expr` and `check_pat`, the pass has.
    const HOOKS: Hooks = Hooks::NONE;
    /// The kinds of expression `check_expr` is called at.
    const EXPRS: ExprKinds = ExprKinds::NONE;
    /// The kinds of pattern `check_pat` is called at.
    const PATS: PatKinds = PatKinds::NONE;
    /// The body, before anything in it.
    fn check_body(&mut self, _b: &BodyCx<'_, '_, '_>, _out: &mut Vec<Found>) {}
    fn check_expr(&mut self, _b: &BodyCx<'_, '_, '_>, _expr: ExprId, _out: &mut Vec<Found>) {}
    /// Statement `index` of the block `block`, before what is in it.
    fn check_stmt(&mut self, _b: &BodyCx<'_, '_, '_>, _block: ExprId, _index: usize, _out: &mut Vec<Found>) {}
    /// The `let` statement `index` of the block `block`, after [`LatePass::check_stmt`] of it.
    fn check_local(&mut self, _b: &BodyCx<'_, '_, '_>, _block: ExprId, _index: usize, _out: &mut Vec<Found>) {}
    /// Arm `index` of the `match` `expr`, before its pattern.
    fn check_arm(&mut self, _b: &BodyCx<'_, '_, '_>, _expr: ExprId, _index: usize, _out: &mut Vec<Found>) {}
    fn check_pat(&mut self, _b: &BodyCx<'_, '_, '_>, _pat: PatId, _out: &mut Vec<Found>) {}
}

/// Which of [`LatePass`]'s `check_body`, `check_stmt`, `check_local` and `check_arm` a pass
/// has, a bit each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Hooks(u8);

impl Hooks {
    pub(crate) const NONE: Hooks = Hooks(0);
    pub(crate) const BODY: Hooks = Hooks(1 << 0);
    pub(crate) const STMT: Hooks = Hooks(1 << 1);
    pub(crate) const LOCAL: Hooks = Hooks(1 << 2);
    pub(crate) const ARM: Hooks = Hooks(1 << 3);

    /// Both sets: `const`, as a pass's [`LatePass::HOOKS`] is.
    pub(crate) const fn and(self, other: Hooks) -> Hooks {
        Hooks(self.0 | other.0)
    }

    const fn has(self, hook: Hooks) -> bool {
        self.0 & hook.0 != 0
    }
}

pub(crate) use hir_def::hir::{ExprKind, PatKind};

const EXPR_TAGS: usize = ExprKind::ALL.len();
const PAT_TAGS: usize = PatKind::ALL.len();
const _: () = assert!(EXPR_TAGS <= u64::BITS as usize && PAT_TAGS <= u32::BITS as usize);

/// A set of [`ExprKind`]s, a bit each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ExprKinds(u64);

impl ExprKinds {
    pub(crate) const NONE: ExprKinds = ExprKinds(0);

    pub(crate) const fn of(tags: &[ExprKind]) -> ExprKinds {
        let mut bits = 0u64;
        let mut at = 0;
        while at < tags.len() {
            bits |= 1u64 << tags[at] as u8;
            at += 1;
        }
        ExprKinds(bits)
    }

    const fn has(self, tag: usize) -> bool {
        self.0 & (1u64 << tag) != 0
    }
}

/// A set of [`PatKind`]s, a bit each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PatKinds(u32);

impl PatKinds {
    pub(crate) const NONE: PatKinds = PatKinds(0);

    pub(crate) const fn of(tags: &[PatKind]) -> PatKinds {
        let mut bits = 0u32;
        let mut at = 0;
        while at < tags.len() {
            bits |= 1u32 << tags[at] as u8;
            at += 1;
        }
        PatKinds(bits)
    }

    const fn has(self, tag: usize) -> bool {
        self.0 & (1u32 << tag) != 0
    }
}

/// The late passes: one variant of [`Pass`] each, listed in the order
/// `BuiltinCombinedLateLintModPass` lists them, which is the order rustc gives their lints at
/// one node. A call into one is a `match` on the variant that the compiler can inline, and
/// their constants are tables indexed by the variant's position.
macro_rules! late_passes {
    ($($pass:ident: $ty:ty,)*) => {
        enum Pass {
            $($pass($ty),)*
        }

        const PASSES: usize = [$(stringify!($pass),)*].len();

        /// Of each pass, by its index: [`LatePass::LINTS`], `HOOKS`, `EXPRS` and `PATS`.
        const PASS_LINTS: [LintSet; PASSES] = [$(<$ty as LatePass>::LINTS,)*];
        const PASS_HOOKS: [Hooks; PASSES] = [$(<$ty as LatePass>::HOOKS,)*];
        const PASS_EXPRS: [ExprKinds; PASSES] = [$(<$ty as LatePass>::EXPRS,)*];
        const PASS_PATS: [PatKinds; PASSES] = [$(<$ty as LatePass>::PATS,)*];

        impl Pass {
            fn all() -> [Pass; PASSES] {
                [$(Pass::$pass(<$ty>::default()),)*]
            }

            fn check_body(&mut self, b: &BodyCx<'_, '_, '_>, out: &mut Vec<Found>) {
                match self {
                    $(Pass::$pass(it) => it.check_body(b, out),)*
                }
            }

            fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
                match self {
                    $(Pass::$pass(it) => it.check_expr(b, expr, out),)*
                }
            }

            fn check_stmt(&mut self, b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize, out: &mut Vec<Found>) {
                match self {
                    $(Pass::$pass(it) => it.check_stmt(b, block, index, out),)*
                }
            }

            fn check_local(&mut self, b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize, out: &mut Vec<Found>) {
                match self {
                    $(Pass::$pass(it) => it.check_local(b, block, index, out),)*
                }
            }

            fn check_arm(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, index: usize, out: &mut Vec<Found>) {
                match self {
                    $(Pass::$pass(it) => it.check_arm(b, expr, index, out),)*
                }
            }

            fn check_pat(&mut self, b: &BodyCx<'_, '_, '_>, pat: PatId, out: &mut Vec<Found>) {
                match self {
                    $(Pass::$pass(it) => it.check_pat(b, pat, out),)*
                }
            }
        }
    };
}

late_passes! {
    ForLoopsOverFallibles: calls::desugar::for_loops_over_fallibles::ForLoopsOverFallibles,
    DropForgetUseless: calls::items::drop_forget_useless::DropForgetUseless,
    InvalidFromUtf8: calls::items::invalid_from_utf8::InvalidFromUtf8,
    PathStatements: calls::statements::PathStatements,
    LetUnderscore: calls::statements::LetUnderscore,
    InvalidReferenceCasting: types::pointers::reference_casting::InvalidReferenceCasting,
    ImplicitAutorefs: types::pointers::autorefs::ImplicitAutorefs,
    UnusedResults: calls::statements::UnusedResults,
    UnitBindings: calls::statements::UnitBindings,
    NonShorthandFieldPatterns: calls::statements::NonShorthandFieldPatterns,
    UnusedAllocation: calls::statements::UnusedAllocation,
    PtrNullChecks: types::pointers::ptr_nulls::PtrNullChecks,
    MutableTransmutes: types::values::transmutes::MutableTransmutes,
    TypeLimits: types::limits::TypeLimits,
    InvalidValue: types::values::invalid::InvalidValue,
    DerefNullPtr: types::pointers::deref_null::DerefNullPtr,
    ShadowedIntoIter: calls::desugar::shadowed_into_iter::ShadowedIntoIter,
    DanglingPointers: types::pointers::dangling::DanglingPointers,
    NonPanicFmt: calls::methods::non_fmt_panic::NonPanicFmt,
    NoopMethodCall: calls::methods::noop_method_call::NoopMethodCall,
    EnumIntrinsicsNonEnums: types::values::intrinsics::EnumIntrinsicsNonEnums,
    InvalidAtomicOrdering: calls::items::invalid_atomic_ordering::InvalidAtomicOrdering,
    AsmLabels: calls::methods::asm_labels::AsmLabels,
    MapUnitFn: calls::methods::map_unit_fn::MapUnitFn,
    InteriorMutableConsts: types::values::interior::InteriorMutableConsts,
    IfLetRescope: calls::desugar::if_let_rescope::IfLetRescope,
    StaticMutRefs: types::casts::static_mut_refs::StaticMutRefs,
    FunctionCastsAsInteger: types::casts::function_casts::FunctionCastsAsInteger,
    CheckTransmutes: types::values::transmutes::CheckTransmutes,
    ImplicitProvenanceCasts: types::casts::provenance::ImplicitProvenanceCasts,
    RawBorrowsViaReferences: types::casts::raw_borrows::RawBorrowsViaReferences,
}

const _: () = assert!(PASSES <= u8::MAX as usize);

/// Indices of passes, in [`Pass`]'s order: those a hook calls. A fixed array, so the tables
/// are built without allocating.
#[derive(Clone, Copy)]
struct Ids {
    len: u8,
    at: [u8; PASSES],
}

impl Ids {
    const EMPTY: Ids = Ids { len: 0, at: [0; PASSES] };

    fn push(&mut self, pass: u8) {
        self.at[usize::from(self.len)] = pass;
        self.len += 1;
    }

    #[inline]
    fn ids(&self) -> &[u8] {
        &self.at[..usize::from(self.len)]
    }
}

/// The passes, and for each hook, expression tag and pattern tag the indices of the passes that
/// run there, in rustc's order: what [`Late`] calls at a node is the slice its tag indexes.
/// Built once per crate; a pass none of whose lints may fire is in no list.
struct Passes {
    all: [Pass; PASSES],
    body: Ids,
    stmt: Ids,
    local: Ids,
    arm: Ids,
    expr: [Ids; EXPR_TAGS],
    pat: [Ids; PAT_TAGS],
    /// Whether any pass runs.
    any: bool,
}

impl Passes {
    fn new(cx: &Cx<'_, '_>) -> Passes {
        let mut passes = Passes {
            all: Pass::all(),
            body: Ids::EMPTY,
            stmt: Ids::EMPTY,
            local: Ids::EMPTY,
            arm: Ids::EMPTY,
            expr: [Ids::EMPTY; EXPR_TAGS],
            pat: [Ids::EMPTY; PAT_TAGS],
            any: false,
        };
        for index in 0..PASSES {
            if !may_fire(cx, &PASS_LINTS[index]) {
                continue;
            }
            passes.any = true;
            let id = index as u8;
            let hooks = PASS_HOOKS[index];
            for (hook, ids) in [
                (Hooks::BODY, &mut passes.body),
                (Hooks::STMT, &mut passes.stmt),
                (Hooks::LOCAL, &mut passes.local),
                (Hooks::ARM, &mut passes.arm),
            ] {
                if hooks.has(hook) {
                    ids.push(id);
                }
            }
            for (tag, ids) in passes.expr.iter_mut().enumerate() {
                if PASS_EXPRS[index].has(tag) {
                    ids.push(id);
                }
            }
            for (tag, ids) in passes.pat.iter_mut().enumerate() {
                if PASS_PATS[index].has(tag) {
                    ids.push(id);
                }
            }
        }
        passes
    }
}

/// rustc's `LateContextAndPass` over one body, in its visitor's order: at each node, the passes
/// its tag's list names. Each node is decoded once, for its tag and its children.
struct Late<'s, 'a, 'db> {
    b: &'s BodyCx<'s, 'a, 'db>,
    passes: &'s mut Passes,
    out: &'s mut Vec<Found>,
}

impl Late<'_, '_, '_> {
    fn body(&mut self) {
        let b = self.b;
        let Passes { all, body, .. } = &mut *self.passes;
        for &i in body.ids() {
            all[usize::from(i)].check_body(b, self.out);
        }
        for param in b.body.params.iter() {
            self.pat(param.user_written);
        }
        self.expr(b.body.root_expr());
    }

    fn expr(&mut self, expr: ExprId) {
        let b = self.b;
        let decoded = b.store().expr(expr);
        let Passes { all, expr: on, .. } = &mut *self.passes;
        for &i in on[ExprKind::of(&decoded) as usize].ids() {
            all[usize::from(i)].check_expr(b, expr, self.out);
        }
        stdx::ensure_sufficient_stack(|| self.expr_children(expr, decoded));
    }

    fn expr_children(&mut self, expr: ExprId, decoded: Expr<'_>) {
        let b = self.b;
        let store = b.store();
        match decoded {
            Expr::Let { pat, expr } => {
                self.expr(expr);
                self.pat(pat);
            }
            Expr::Block { statements, tail, .. } => {
                for (index, statement) in statements.iter().enumerate() {
                    if let Statement::Item(_) = statement {
                        continue;
                    }
                    let Passes { all, stmt, .. } = &mut *self.passes;
                    for &i in stmt.ids() {
                        all[usize::from(i)].check_stmt(b, expr, index, self.out);
                    }
                    match statement {
                        Statement::Let { pat, initializer, else_branch, .. } => {
                            let Passes { all, local, .. } = &mut *self.passes;
                            for &i in local.ids() {
                                all[usize::from(i)].check_local(b, expr, index, self.out);
                            }
                            if let Some(it) = initializer {
                                self.expr(it);
                            }
                            self.pat(pat);
                            if let Some(it) = else_branch {
                                self.expr(it);
                            }
                        }
                        Statement::Expr { expr, .. } => self.expr(expr),
                        Statement::Item(_) => {}
                    }
                }
                if let Some(tail) = tail {
                    self.expr(tail);
                }
            }
            Expr::Match { expr: scrutinee, arms } => {
                self.expr(scrutinee);
                for (index, arm) in arms.iter().enumerate() {
                    let Passes { all, arm: on, .. } = &mut *self.passes;
                    for &i in on.ids() {
                        all[usize::from(i)].check_arm(b, expr, index, self.out);
                    }
                    self.pat(arm.pat);
                    if let Some(guard) = arm.guard {
                        self.expr(guard);
                    }
                    self.expr(arm.expr);
                }
            }
            Expr::Closure { args, body, .. } => {
                for arg in args.iter() {
                    self.pat(*arg);
                }
                self.expr(body);
            }
            Expr::Assignment { target, value } => {
                self.pat(target);
                self.expr(value);
            }
            Expr::Array(Array::Repeat { initializer, .. }) => self.expr(initializer),
            _ => {
                store.walk_child_exprs_without_pats(expr, |it| self.expr(it));
            }
        }
    }

    fn pat(&mut self, pat: PatId) {
        let b = self.b;
        let store = b.store();
        let decoded = store.pat(pat);
        let Passes { all, pat: on, .. } = &mut *self.passes;
        for &i in on[PatKind::of(&decoded) as usize].ids() {
            all[usize::from(i)].check_pat(b, pat, self.out);
        }
        match decoded {
            Pat::Lit(expr) | Pat::Expr(expr) => self.expr(expr),
            Pat::Range { start, end, .. } => {
                if let Some(it) = start {
                    self.expr(it);
                }
                if let Some(it) = end {
                    self.expr(it);
                }
            }
            _ => {
                store.walk_pats_shallow(pat, |it| self.pat(it));
            }
        }
    }
}