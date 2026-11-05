//! `arc_clone_only_borrowed`: an `Arc` cloned (`x.clone()`, `Arc::clone(&x)`), or an
//! `ArcSwap` loaded in full (`x.load_full()`), whose clone is only borrowed. A clone of an
//! `Arc` is an atomic increment of the count, and its drop an atomic decrement: two
//! read-modify-writes of a cache line every thread holding the `Arc` shares, bought for a value
//! that is only read through, while the `Arc` it came from was there to borrow all along.
//!
//! The clone is only borrowed when it is used inline as the receiver of a method of what it
//! points to (`x.clone().len()`), or bound by a plain `let y = ..` (not `mut`, not `ref`) and
//! every use of `y` is a shared borrow: a method or field of the value (`y.len()`, `y.name`),
//! `&y`, `*y`, a comparison. A use that moves it (a call's argument, a return, a struct field,
//! a channel's `send`, a spawned closure) or any use inside a closure leaves it alone: the
//! clone may be what keeps the value alive there.
//!
//! The `Arc` cloned must stay put while the clone lives, or the borrow would not do: it is a
//! place (a local, a field of one, a `static`), not a temporary (`m.lock().clone()`), and its
//! root is a shared reference (`&self`), a `static` that is not `mut`, or a local that is not
//! `mut` whose every use is itself a shared borrow (no move, no `&mut`, no assignment).
//!
//! The fix, machine-applicable, is offered for `let y = x.clone();` with no type written, when
//! every use of `y` is a method or field of the value or `&*y`: `x.clone()` becomes `&*x`, so
//! `y` is a `&T` and every use reads the same. An `ArcSwap`'s `load_full` gets help to take a
//! `load()` guard instead. Inside a loop the note names the pattern for reading a shared,
//! frequently replaced `Arc`: epoch or hazard-pointer reclamation (arc-swap's guards,
//! crossbeam-epoch), where a read is a load and no count is touched.

use base_db::Db;
use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{
        BinaryOp, BindingAnnotation, BindingId, Expr, ExprId, Pat, Statement, UnaryOp,
        type_ref::{Mutability, Rawness},
    },
    resolver::ValueNs,
    signatures::FunctionSignature,
};
use hir_ty::{Adjust, AutoBorrow};
use syntax::{NodeOrToken, TextRange};

use super::{Lint, alloc::written_callee, exempt, facts, hash_order_output::local_of};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::ArcCloneOnlyBorrowed;

/// How many expressions up a use is followed to the block of its `let`.
const UP: usize = 64;

/// How many projections a place is followed down to its root.
const PLACE: usize = 16;

/// What was cloned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Made {
    /// `Arc::clone`.
    Arc,
    /// arc-swap's `load_full`.
    LoadFull,
}

/// A finding, before it is reported.
struct Found {
    range: TextRange,
    message: String,
    note: Option<String>,
    fix: Option<(TextRange, String)>,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || cx.is_in_test(cx.root()) {
        return;
    }
    let found = find(cx);
    let root = cx.root();
    for it in found {
        let at = match root.text_range().contains_range(it.range) {
            true => match root.covering_element(it.range) {
                NodeOrToken::Node(node) => node,
                NodeOrToken::Token(token) => token.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(it.range);
        let fix = it.fix.map(|(at, text)| (cx.span(at), text));
        let diagnostic = cx.blaze(LINT, at, span, it.message);
        if let Some(note) = it.note {
            diagnostic.note(note);
        }
        if let Some(part) = fix {
            // alloc: the fix's one part
            diagnostic.multipart_suggestion(
                "borrow the `Arc` you have",
                vec![part],
                Applicability::MachineApplicable,
            );
        }
        diagnostic
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// What a body's walk knows.
struct Walk<'a, 'db> {
    db: &'db Db,
    def: DefWithBodyId,
    body: &'a Body,
    analysis: &'a hir::BodyAnalysis<'db>,
    parents: &'a facts::Parents,
}

/// How the clone's value is used.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Borrow {
    /// Through the `Arc`'s `Deref`: a method or field of the value, `*y`.
    Value,
    /// Another shared borrow: `&y`, a method of `Arc` taking `&self`, a comparison.
    Other,
}

fn find(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let Some(arc) = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("Arc")) else {
        return found;
    };
    let body = analysis.body();
    let infer = analysis.infer();
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let (source, method) = match expr {
            Expr::MethodCall { receiver, args: [], .. } => (receiver, true),
            Expr::Call { args: [arg], .. } => match body.expr(*arg) {
                Expr::Ref { expr, mutability: Mutability::Shared, .. } => (expr, false),
                _ => continue,
            },
            _ => continue,
        };
        let Some((callee, _)) = written_callee(infer, body, id) else { continue };
        let made = match FunctionSignature::of(db, callee).name.as_str() {
            "clone" if analysis.ty(infer.expr_ty(id)).as_adt() == Some(arc) => Made::Arc,
            "load_full"
                if method
                    && facts::impl_adt(db, callee).is_some_and(|it| {
                        facts::crate_is(db, it.module(db).krate(db), "arc_swap")
                    }) =>
            {
                Made::LoadFull
            }
            _ => continue,
        };
        let Some(range) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        let walk = Walk { db, def, body, analysis, parents };
        if !walk.stays_put(source, id) {
            continue;
        }
        let Some((uses, fixable)) = walk.only_borrowed(id) else { continue };
        let in_loop = walk.parents.up(id, UP).any(|it| matches!(body.expr(it), Expr::Loop { .. }));
        let what = match (made, method) {
            (Made::Arc, true) => "this `.clone()` of an `Arc`",
            (Made::Arc, false) => "this `Arc::clone`",
            (Made::LoadFull, _) => "this `load_full()`",
        };
        let message = format!(
            "{what} is only borrowed ({uses}): it pays two atomic updates of the shared count for nothing"
        );
        let mut note = match made {
            Made::Arc => "borrow through the `Arc` it was cloned from (`&*x`, or call the method on `x`): the `Arc` \
                          stays alive while the borrow lasts, and no count is touched"
                .to_owned(), // alloc: a message's part
            Made::LoadFull => "`load()` gives a guard that borrows the current value without touching its count; \
                               keep a guard short-lived, and `load_full()` for a value kept past the scope"
                .to_owned(), // alloc: a message's part
        };
        if in_loop {
            note.push_str(
                "; for reading a shared `Arc` that is replaced often, inside a loop, read it under epoch or hazard-pointer \
                 reclamation (arc-swap's guards, `crossbeam-epoch`), where a read is a load and no count is touched",
            );
        }
        let fix = match (made, method, fixable) {
            (Made::Arc, true, true) if cx.in_file() => walk.fix(source, id, cx),
            _ => None,
        };
        // alloc: reported at the edge
        found.push(Found { range, message, note: Some(note), fix });
    }
    found
}

impl Walk<'_, '_> {
    fn adjusted(&self, expr: ExprId) -> Option<Borrow> {
        let first = self.analysis.infer().expr_adjustment(expr)?.first()?;
        match &first.kind {
            Adjust::Deref(Some(deref)) if !deref.0.is_mut() => Some(Borrow::Value),
            Adjust::Borrow(AutoBorrow::Ref(_)) => {
                (!self.analysis.ty(first.target.as_ref()).is_mutable_reference())
                    .then_some(Borrow::Other)
            }
            _ => None,
        }
    }

    /// How the value at `at` is used, when it is only a shared borrow.
    fn borrowed_at(&self, at: ExprId) -> Option<Borrow> {
        let parent = self.parents.of_expr(at)?;
        match self.body.expr(parent) {
            Expr::Ref { expr, rawness: Rawness::Ref, mutability: Mutability::Shared }
                if expr == at =>
            {
                // `&*y` reads the value; `&y` borrows the `Arc`.
                Some(Borrow::Other)
            }
            Expr::UnaryOp { expr, op: UnaryOp::Deref } if expr == at => {
                let grand = self.parents.of_expr(parent);
                match grand.map(|it| self.body.expr(it)) {
                    Some(Expr::Ref { mutability: Mutability::Shared, .. }) => Some(Borrow::Value),
                    Some(Expr::MethodCall { receiver, .. }) if receiver == parent => {
                        self.adjusted(parent).map(|_| Borrow::Value)
                    }
                    Some(Expr::Field { .. }) => Some(Borrow::Value),
                    _ => None,
                }
            }
            Expr::MethodCall { receiver, .. } if receiver == at => self.adjusted(at),
            Expr::Field { expr, .. } if expr == at => self.adjusted(at),
            Expr::Index { base, .. } if base == at => self.adjusted(at),
            Expr::BinaryOp { op: Some(BinaryOp::CmpOp(_)), .. } => Some(Borrow::Other),
            _ => None,
        }
    }

    /// When the clone `clone` is only borrowed: what its uses are, and whether every one reads
    /// the value through `Deref` (so the clone can become a `&T`). A clone bound by a plain
    /// `let` has every use of its local checked, none inside a closure.
    fn only_borrowed(&self, clone: ExprId) -> Option<(&'static str, bool)> {
        let parent = self.parents.of_expr(clone)?;
        let Expr::Block { statements, .. } = self.body.expr(parent) else {
            // An inline use gets help only: its fix would rewrite the expression around it.
            self.borrowed_at(clone)?;
            return Some(("used once, inline", false));
        };
        let (pat, typed) = statements.iter().find_map(|it| match it {
            Statement::Let {
                pat, initializer: Some(init), else_branch: None, type_ref, ..
            } if init == clone => Some((pat, type_ref.is_some())),
            _ => None,
        })?;
        let Pat::Bind { id: local, subpat: None } = self.body.pat(pat) else { return None };
        if !matches!(self.body[local].mode, BindingAnnotation::Unannotated) {
            return None;
        }
        let uses = self.uses(local);
        if uses.is_empty() {
            return None;
        }
        let mut all_value = !typed;
        for &it in &uses {
            if !self.below(it, parent) {
                return None;
            }
            match self.borrowed_at(it)? {
                Borrow::Value => {}
                Borrow::Other => all_value = false,
            }
        }
        Some(("every use of the local is a shared borrow", all_value))
    }

    /// The path expressions of the body that name `local`.
    fn uses(&self, local: BindingId) -> Vec<ExprId> {
        // alloc: the uses of one local, a few
        let mut uses = Vec::new();
        for (id, expr) in self.body.exprs() {
            if matches!(expr, Expr::Path(_))
                && local_of(self.db, self.def, self.body, id) == Some(local)
            {
                uses.push(id);
            }
        }
        uses
    }

    /// Whether `expr` is inside `block` with no closure between them.
    fn below(&self, expr: ExprId, block: ExprId) -> bool {
        for it in self.parents.up(expr, UP) {
            if it == block {
                return true;
            }
            if matches!(self.body.expr(it), Expr::Closure { .. }) {
                return false;
            }
        }
        false
    }

    /// Whether the place `source`, cloned by `clone`, stays put while the clone lives: as the
    /// module docs say.
    fn stays_put(&self, source: ExprId, clone: ExprId) -> bool {
        let mut at = source;
        for _ in 0..PLACE {
            match self.body.expr(at) {
                Expr::Field { expr, .. } => at = expr,
                Expr::UnaryOp { expr, op: UnaryOp::Deref } => at = expr,
                Expr::Ref { expr, mutability: Mutability::Shared, .. } => at = expr,
                Expr::Path(_) => return self.root_stays_put(at, clone),
                _ => return false,
            }
        }
        false
    }

    fn root_stays_put(&self, root: ExprId, clone: ExprId) -> bool {
        let ty = self.analysis.ty(self.analysis.infer().expr_ty(root));
        if ty.is_reference() && !ty.is_mutable_reference() {
            return true;
        }
        if let Some(local) = local_of(self.db, self.def, self.body, root) {
            if !matches!(self.body[local].mode, BindingAnnotation::Unannotated) {
                return false;
            }
            let Some(clone_range) = self.analysis.expr_range(clone) else { return false };
            return self.uses(local).into_iter().all(|it| {
                let inside =
                    self.analysis.expr_range(it).is_some_and(|r| clone_range.contains_range(r));
                inside || self.place_borrowed(it)
            });
        }
        let Expr::Path(path) = self.body.expr(root) else { return false };
        let scope = hir_def::expr_store::scope::ExprScopes::of(self.db, self.def).scope_for(root);
        let value = hir_def::resolver::resolver_for_scope(self.db, self.def, scope)
            .resolve_path_in_value_ns_fully(self.db, path, self.body.expr_path_hygiene(root));
        matches!(value, Some(ValueNs::StaticId(it)) if !hir::Static::from(it).is_mut(self.db))
    }

    /// Whether the use `at` of a local, through the projections above it, is a shared borrow.
    fn place_borrowed(&self, at: ExprId) -> bool {
        let mut at = at;
        for _ in 0..PLACE {
            let Some(parent) = self.parents.of_expr(at) else { return false };
            match self.body.expr(parent) {
                Expr::Field { expr, .. } if expr == at && self.adjusted(at).is_none() => {
                    at = parent
                }
                _ => return self.borrowed_at(at).is_some(),
            }
        }
        false
    }

    /// The fix of `let y = x.clone();`: `x.clone()` to `&*x`, a `*` more for each reference
    /// `x`'s type is.
    fn fix(&self, source: ExprId, clone: ExprId, cx: &Cx<'_, '_>) -> Option<(TextRange, String)> {
        let file = self.analysis.tree().1;
        let source_map = self.analysis.source_map();
        let written = |it: ExprId| source_map.expr_syntax(it).is_ok_and(|it| it.file_id == file);
        if !written(source)
            || !written(clone)
            || !matches!(self.body.expr(source), Expr::Path(_) | Expr::Field { .. })
        {
            return None;
        }
        let mut ty = self.analysis.ty(self.analysis.infer().expr_ty(source));
        let mut stars = 1;
        while let Some((inner, _)) = ty.as_reference() {
            ty = inner;
            stars += 1;
        }
        let text =
            format!("&{}{}", "*".repeat(stars), cx.snippet(self.analysis.expr_range(source)?));
        Some((self.analysis.expr_range(clone)?, text))
    }
}

#[cfg(test)]
mod tests {
    use super::super::perf_tests::perf_only;

    const LINT: &str = "arc_clone_only_borrowed";
    const FOUND: &str = "is only borrowed";

    #[test]
    fn a_clone_only_read_through_is_found_and_fixed_and_one_moved_is_not() {
        let found = perf_only(
            LINT,
            r#"
use std::sync::Arc;
pub struct Config { pub limit: u32 }
impl Config { pub fn limit(&self) -> u32 { self.limit } }
pub fn read(shared: &Arc<Config>) -> u32 {
    let config = shared.clone();
    config.limit() + config.limit
}
pub fn kept(shared: &Arc<Config>) -> Arc<Config> {
    let config = shared.clone();
    config
}
pub fn handed(shared: &Arc<Config>) -> u32 {
    let config = shared.clone();
    take(config)
}
fn take(config: Arc<Config>) -> u32 { config.limit }
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("this `.clone()` of an `Arc` is only borrowed"), "{found}");
        assert!(
            found.contains("let config = &**shared;"),
            "the clone is fixed to a borrow:\n{found}"
        );
    }
}
