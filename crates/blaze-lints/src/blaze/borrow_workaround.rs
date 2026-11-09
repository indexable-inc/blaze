//! `borrow_workaround` and `nll_workaround`: a value looked up twice, first only to learn
//! whether it is there and then again in the branch that found it, where one lookup kept in an
//! `if let` does the same. Code is written so when the borrow checker it was written for
//! rejects the direct form: stable Rust's (NLL) does when the borrow is returned from the branch
//! (NLL problem case #3, rust-lang/rust#54663, #21906), and blaze's default
//! checker, the Polonius alpha, accepts it.
//!
//! The shapes found:
//!
//! - `if map.contains_key(&k) { .. map.get(&k) .. }` (or `get_mut`, `&map[&k]`, `map[&k]`), and
//!   `if map.get(&k).is_some() { .. }` with the same: `if let Some(found) = map.get(&k)`. The
//!   maps are std's `HashMap` and `BTreeMap`, hashbrown's and `blaze_hash`'s ([`facts::hash_adt`]
//!   less its sets and the concurrent maps, whose `get` hands back a guard).
//! - `if o.is_some() { .. o.as_mut() .. }` (or `as_ref`, `as_deref`, `as_deref_mut`) on an
//!   `Option` place: `if let Some(found) = o.as_mut()`.
//!
//! An `unwrap()` or `expect("..")` of the second lookup becomes `found`; any other use of it
//! becomes `Some(found)`. The map or option is the same place (a local or `static`, its fields
//! and derefs) in both calls, and the keys are the same literal, `const`, local or place.
//!
//! A finding is made only when the direct form is proven:
//!
//! - It passes the borrow checker on this body: the body's MIR is rewritten, the second lookup's
//!   call moved before the `if`'s switch with the statements that make its operands, and that
//!   MIR is checked with [`hir_ty_borrowck::borrowck_rewritten`] in the database's mode
//!   ([`hir_ty_borrowck::mode`], `BLAZE_BORROWCK`). Checked again in `Mode::Nll`, it says which
//!   lint: `borrow_workaround` when stable Rust's checker accepts the direct form too (so the
//!   change is right for every crate), `nll_workaround` when only blaze's (and nightly's) does.
//! - It behaves the same: nothing runs between the `if`'s test and the second lookup but
//!   statements that only borrow, copy or name a value (no call, no assignment to a variable, no
//!   drop, nothing that may panic); the lookup returns a reference or an `Option` of one, which
//!   has nothing to drop; and the first lookup runs no code of its own beyond the second's. std
//!   writes `contains_key(k)` as `get_inner(k).is_some()` (hashbrown's) and `get(k).is_some()`
//!   (`BTreeMap`'s), so the direct form makes a subset of the same `Hash` and `Eq` calls, whose
//!   number std leaves unspecified, and `Option::is_some` and `as_*` run no user code.
//!   Places and keys are reached with no `Deref` impl but the standard library's and
//!   `blaze_hash`'s, so dropping the first lookup drops no call of the crate's own.
//!
//! `blaze check --fix` applies the fix and checks the crate again (`fix.rs`'s settling), undoing
//! it if it adds an error.
//!
//! Not yet found (docs/v2/plan/design/lint-borrow-workarounds.md): a `clone()` or `to_owned()`
//! made only to end a borrow, an index loop or `unsafe { &mut *ptr }` standing in for a cursor
//! (rust-lang/rust#57165, #63908, #108704, which the alpha still rejects), `mem::take` and put
//! back, and a call of the crate's own function twice (it may have effects).

use base_db::Db;
use hir_def::{
    DefWithBodyId, FunctionId, StaticId,
    expr_store::Body,
    hir::{
        BindingId, Expr, ExprId, UnaryOp,
        type_ref::{Mutability, Rawness},
    },
    resolver::ValueNs,
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use hir_ty::{Adjust, InferBodyId, db::HirDatabase as _};
use hir_ty_borrowck::{BorrowckResult, Mode};
use hir_ty_mir::mir::{
    BasicBlock, BasicBlockId, Local, LocalId, MirBody, MirSpan, Operand, OperandKind, Rvalue,
    Statement, StatementKind, StoredPlace, Terminator, TerminatorKind, return_slot,
};
use syntax::{NodeOrToken, SyntaxKind, TextRange};

use super::{
    Lint,
    alloc::written_callee,
    facts::{self, Parents},
    per_item::value_of,
};
use crate::{check::Cx, diag::Applicability};

/// How many fields and derefs deep a place is followed.
const PLACE_DEPTH: u32 = 16;
/// How many `&` and `*` a key is followed through.
const KEY_DEPTH: u32 = 8;
/// How many blocks the path from the `if`'s switch to the second lookup may have.
const PATH: usize = 32;
/// How many names `found`, `found1`, .. are tried for the binding.
const NAMES: u32 = 64;

/// The crates whose `Deref` impls run no code of the user's.
const PURE_DEREF: &[&str] = &["core", "alloc", "std", "blaze_hash"];
/// The lookups of a map that the probe repeats.
const MAP_LOOKUPS: &[&str] = &["get", "get_mut"];
/// The lookups of an `Option` that `is_some` repeats.
const OPTION_LOOKUPS: &[&str] = &["as_ref", "as_mut", "as_deref", "as_deref_mut"];

/// The local or `static` a place starts at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Root {
    Local(BindingId),
    Static(StaticId),
}

/// A place: its root and the fields under it, its derefs and borrows looked through.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Place {
    root: Root,
    fields: Vec<Name>,
}

/// What the `if` tests.
enum Probe {
    /// `map.contains_key(key)` or `map.get(key).is_some()`.
    Map { place: Place, key: ExprId },
    /// `option.is_some()`.
    Option { place: Place },
}

/// The second lookup and how the fix writes it.
struct Lookup {
    /// The expression whose MIR call is the lookup: the method call, or the `Index`.
    call: ExprId,
    /// The range the fix replaces, and with what (`{}` standing for the binding's name).
    replaced: TextRange,
    with: Use,
    /// The `if let`'s scrutinee.
    scrutinee: String,
}

/// What the lookup's use becomes.
#[derive(Clone, Copy)]
enum Use {
    /// `found`: an `unwrap()`, `expect("..")` or `&map[k]` replaced.
    Found,
    /// `Some(found)`: the lookup itself replaced.
    Wrapped,
    /// `*found`, or `(*found)` under a postfix operator: `map[k]` replaced.
    Deref { parens: bool },
}

/// A finding, before it is reported.
struct Found {
    lint: Lint,
    range: TextRange,
    again: TextRange,
    message: String,
    parts: [(TextRange, String); 2],
    applicability: Applicability,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    let (general, nll) = (cx.on_blaze(Lint::BorrowWorkaround), cx.on_blaze(Lint::NllWorkaround));
    if !general && !nll {
        return;
    }
    let found = find(cx, general, nll);
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
        let again = cx.span(it.again);
        let [(a, a_text), (b, b_text)] = it.parts;
        // alloc: the fix's two parts
        let parts = vec![(cx.span(a), a_text), (cx.span(b), b_text)];
        let lint = it.lint;
        let diagnostic = cx.blaze(lint, at, span, it.message);
        diagnostic.span_label(again, "and looked up again here");
        match lint {
            Lint::NllWorkaround => diagnostic.note(
                "blaze's borrow checker accepts one lookup kept in an `if let` here, which stable Rust's (NLL) \
                 rejects: the crate then builds only with blaze or nightly Rust",
            ),
            _ => diagnostic.note("one lookup kept in an `if let` does the same, and every borrow checker accepts it here"),
        };
        diagnostic.multipart_suggestion("look it up once", parts, it.applicability);
        diagnostic
            .help(lint.help())
            .help(format!("for further information visit {}", lint.docs_url()));
    }
}

/// The body linted and what its walk needs.
struct Walk<'a, 'db, 't> {
    cx: &'a Cx<'db, 't>,
    def: DefWithBodyId,
    body: &'db Body,
    analysis: &'a hir::BodyAnalysis<'db>,
    parents: Parents,
}

fn find(cx: &Cx<'_, '_>, general: bool, nll: bool) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let db = cx.db();
    let mode = hir_ty_borrowck::mode(db);
    if mode == Mode::Off {
        return found;
    }
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let body = analysis.body();
    if !body.exprs().any(|(_, expr)| matches!(expr, Expr::If { .. })) {
        return found;
    }
    let walk = Walk { cx, def, body, analysis, parents: Parents::of(body) };
    let mut checked = None;
    let mut mir = None;
    for (id, expr) in body.exprs() {
        let Expr::If { condition, then_branch, .. } = expr else { continue };
        let Some((probe, lookup)) = walk.shape(id, condition, then_branch) else { continue };
        // The body as written passes: a body with errors is not linted.
        if !*checked.get_or_insert_with(|| passes(hir_ty_borrowck::borrowck(db, def))) {
            return found;
        }
        let mir = match mir.get_or_insert_with(|| db.mir_body(InferBodyId::DefWithBodyId(def)).ok())
        {
            Some(it) => *it,
            None => return found,
        };
        let Some(rewritten) = hoisted(mir, id, condition, lookup.call) else { continue };
        if !passes(&hir_ty_borrowck::borrowck_rewritten(db, def, &rewritten, mode)) {
            continue;
        }
        let stable = mode == Mode::Nll
            || passes(&hir_ty_borrowck::borrowck_rewritten(db, def, &rewritten, Mode::Nll));
        let lint = match stable {
            true if general => Lint::BorrowWorkaround,
            false if nll => Lint::NllWorkaround,
            _ => continue,
        };
        if let Some(it) = walk.found(lint, id, condition, &probe, &lookup) {
            found.push(it);
        }
    }
    found
}

/// Whether a borrow check found no error and checked the whole body.
fn passes(result: &BorrowckResult) -> bool {
    result.skipped.is_none()
        && result.not_checked.is_none()
        && !result.diagnostics.iter().any(|it| it.is_error())
}

impl<'db> Walk<'_, 'db, '_> {
    fn db(&self) -> &'db Db {
        self.cx.db()
    }

    fn name(&self, f: FunctionId) -> &'db str {
        FunctionSignature::of(self.db(), f).name.as_str()
    }

    fn range(&self, id: ExprId) -> Option<TextRange> {
        self.analysis.expr_range(id)
    }

    fn text(&self, id: ExprId) -> Option<&str> {
        self.range(id).map(|it| self.cx.snippet(it))
    }

    /// The probe of the `if` `id` and the lookup it repeats in `then`, when both are written.
    fn shape(&self, id: ExprId, condition: ExprId, then: ExprId) -> Option<(Probe, Lookup)> {
        let file = self.analysis.tree().1;
        let written_if = self
            .analysis
            .source_map()
            .expr_syntax(id)
            .is_ok_and(|it| it.file_id == file && it.value.kind() == SyntaxKind::IF_EXPR);
        if !written_if || !super::loops::written(self.analysis, condition) {
            return None;
        }
        let probe = self.probe(condition)?;
        let then_range = self.range(then)?;
        // The lookups in the branch, the first written first.
        // alloc: the candidates of one branch, a few
        let mut candidates: Vec<(TextRange, Lookup)> = self
            .body
            .exprs()
            .filter_map(|(at, _)| {
                let range = self.range(at)?;
                if !then_range.contains_range(range) || !super::loops::written(self.analysis, at) {
                    return None;
                }
                Some((range, self.lookup(&probe, at)?))
            })
            .collect();
        candidates.sort_by_key(|(range, _)| (range.start(), std::cmp::Reverse(range.end())));
        let (_, lookup) = candidates.into_iter().next()?;
        Some((probe, lookup))
    }

    /// What `condition` tests, when it is a probe this lint knows.
    fn probe(&self, condition: ExprId) -> Option<Probe> {
        let Expr::MethodCall { receiver, args, .. } = self.body.expr(condition) else {
            return None;
        };
        let db = self.db();
        let (f, _) = written_callee(self.analysis.infer(), self.body, condition)?;
        let adt = facts::impl_adt(db, f)?;
        match (self.name(f), args) {
            ("contains_key", [key])
                if self.map(adt) && self.receiver_adt(receiver).is_some_and(|it| self.map(it)) =>
            {
                Some(Probe::Map {
                    place: self.place_of(receiver, PLACE_DEPTH)?,
                    key: self.plain_key(*key)?,
                })
            }
            ("is_some", []) if facts::adt_is(db, adt, "core", "Option") => {
                match self.map_get(receiver, &["get"]) {
                    Some((place, key)) => Some(Probe::Map { place, key }),
                    None if self
                        .receiver_adt(receiver)
                        .is_some_and(|it| facts::adt_is(db, it, "core", "Option")) =>
                    {
                        Some(Probe::Option { place: self.place_of(receiver, PLACE_DEPTH)? })
                    }
                    None => None,
                }
            }
            _ => None,
        }
    }

    /// Whether `adt` is a map whose `get` hands back a plain reference: std's `HashMap` and
    /// `BTreeMap`, hashbrown's, `blaze_hash`'s; not papaya's or dashmap's.
    fn map(&self, adt: hir::Adt) -> bool {
        let db = self.db();
        let krate = adt.module(db).krate(db);
        if facts::crate_is(db, krate, "papaya") || facts::crate_is(db, krate, "dashmap") {
            return false;
        }
        (facts::hash_adt(self.cx, adt) && adt.name(db).as_str().contains("Map"))
            || facts::adt_is(db, adt, "alloc", "BTreeMap")
    }

    /// `map.<one of names>(key)` on a map place: the place and the key.
    fn map_get(&self, id: ExprId, names: &[&str]) -> Option<(Place, ExprId)> {
        let Expr::MethodCall { receiver, args: [key], .. } = self.body.expr(id) else {
            return None;
        };
        let (f, _) = written_callee(self.analysis.infer(), self.body, id)?;
        if !names.contains(&self.name(f)) || !self.map(facts::impl_adt(self.db(), f)?) {
            return None;
        }
        // Reached with no `Deref` of the crate's own on the way, which could run its code.
        if !self.receiver_adt(receiver).is_some_and(|it| self.map(it)) {
            return None;
        }
        Some((self.place_of(receiver, PLACE_DEPTH)?, self.plain_key(*key)?))
    }

    /// `at` as the lookup `probe` repeats.
    fn lookup(&self, probe: &Probe, at: ExprId) -> Option<Lookup> {
        match (probe, self.body.expr(at)) {
            (Probe::Map { place, key }, Expr::MethodCall { .. }) => {
                let (there, again) = self.map_get(at, MAP_LOOKUPS)?;
                (there == *place && self.same_key(*key, again)).then_some(())?;
                self.method_lookup(at)
            }
            (
                Probe::Map { place, key },
                Expr::Ref { expr: index, rawness: Rawness::Ref, mutability: Mutability::Shared },
            ) => {
                let Expr::Index { base, index: again } = self.body.expr(index) else { return None };
                self.index_of(place, *key, base, again)?;
                Some(Lookup {
                    call: index,
                    replaced: self.range(at)?,
                    with: Use::Found,
                    scrutinee: self.get_text(base, again)?,
                })
            }
            (Probe::Map { place, key }, Expr::Index { base, index: again }) => {
                // `&map[k]` is found as the `Ref`.
                let parent = self.parents.of_expr(at).map(|it| self.body.expr(it));
                if matches!(parent, Some(Expr::Ref { .. })) {
                    return None;
                }
                self.index_of(place, *key, base, again)?;
                let parens = match parent {
                    Some(Expr::Field { .. } | Expr::Await { .. }) => true,
                    Some(Expr::MethodCall { receiver, .. }) => receiver == at,
                    Some(Expr::Index { base: outer, .. }) => outer == at,
                    _ => false,
                };
                Some(Lookup {
                    call: at,
                    replaced: self.range(at)?,
                    with: Use::Deref { parens },
                    scrutinee: self.get_text(base, again)?,
                })
            }
            (Probe::Option { place }, Expr::MethodCall { receiver, args: [], .. }) => {
                let (f, _) = written_callee(self.analysis.infer(), self.body, at)?;
                let db = self.db();
                let option = facts::adt_is(db, facts::impl_adt(db, f)?, "core", "Option")
                    && self
                        .receiver_adt(receiver)
                        .is_some_and(|it| facts::adt_is(db, it, "core", "Option"));
                if !option
                    || !OPTION_LOOKUPS.contains(&self.name(f))
                    || self.place_of(receiver, PLACE_DEPTH)? != *place
                {
                    return None;
                }
                self.method_lookup(at)
            }
            _ => None,
        }
    }

    /// The ADT of `expr`'s type, its references peeled: what a method call on it reaches with
    /// no autoderef through a `Deref` impl.
    fn receiver_adt(&self, expr: ExprId) -> Option<hir::Adt> {
        self.analysis.ty(self.analysis.infer().expr_ty(expr)).strip_references().as_adt()
    }

    /// Whether using `expr` goes through a `Deref` impl that may run code of the crate's own:
    /// an overloaded deref among its adjustments, but for the standard library's types
    /// (`String` to `str`, `Box`) and `blaze_hash`'s maps, which only hand out what they hold.
    fn runs_deref(&self, expr: ExprId) -> bool {
        let Some(adjustments) = self.analysis.infer().expr_adjustment(expr) else { return false };
        let db = self.db();
        let known = self.receiver_adt(expr).is_some_and(|adt| {
            PURE_DEREF.iter().any(|it| facts::crate_is(db, adt.module(db).krate(db), it))
        });
        !known && adjustments.iter().any(|it| matches!(it.kind, Adjust::Deref(Some(_))))
    }

    /// Whether `*expr` is the built-in deref of a reference, which runs no code.
    fn builtin_deref(&self, expr: ExprId) -> bool {
        self.analysis.ty(self.analysis.infer().expr_ty(expr)).is_reference()
    }

    /// `base[index]` on the map `place` with the key `key`.
    fn index_of(&self, place: &Place, key: ExprId, base: ExprId, index: ExprId) -> Option<()> {
        let ty = self.analysis.ty(self.analysis.infer().expr_ty(base));
        let map = ty.strip_references().as_adt().is_some_and(|adt| self.map(adt));
        (map && self.place_of(base, PLACE_DEPTH)? == *place && self.same_key(key, index))
            .then_some(())
    }

    /// `base.get(index)`, `base` in parentheses unless it is a path or a field.
    fn get_text(&self, base: ExprId, index: ExprId) -> Option<String> {
        let (map, key) = (self.text(base)?, self.text(index)?);
        Some(match self.body.expr(base) {
            Expr::Path(_) | Expr::Field { .. } => format!("{map}.get({key})"),
            _ => format!("({map}).get({key})"),
        })
    }

    /// The method call `at` as the lookup: its `unwrap()` or `expect("..")` becomes the
    /// binding, anything else `Some` of it.
    fn method_lookup(&self, at: ExprId) -> Option<Lookup> {
        let scrutinee = self.text(at)?.to_owned(); // alloc: a finding's text
        let unwrapped = self.parents.of_expr(at).filter(|&parent| match self.body.expr(parent) {
            Expr::MethodCall { receiver, args, .. } if receiver == at => {
                let unwraps = written_callee(self.analysis.infer(), self.body, parent).is_some_and(
                    |(f, _)| {
                        let db = self.db();
                        let option = facts::impl_adt(db, f)
                            .is_some_and(|adt| facts::adt_is(db, adt, "core", "Option"));
                        option
                            && match (self.name(f), args) {
                                ("unwrap", []) => true,
                                ("expect", [message]) => {
                                    matches!(self.body.expr(*message), Expr::Literal(_))
                                }
                                _ => false,
                            }
                    },
                );
                unwraps && super::loops::written(self.analysis, parent)
            }
            _ => false,
        });
        let (replaced, with) = match unwrapped {
            Some(parent) => (self.range(parent)?, Use::Found),
            None => (self.range(at)?, Use::Wrapped),
        };
        Some(Lookup { call: at, replaced, with, scrutinee })
    }

    /// The place `expr` names: a local or `static` and its fields, through derefs and borrows.
    fn place_of(&self, expr: ExprId, depth: u32) -> Option<Place> {
        let depth = depth.checked_sub(1)?;
        if self.runs_deref(expr) {
            return None;
        }
        match self.body.expr(expr) {
            Expr::Path(_) => {
                let root = match value_of(self.db(), self.def, self.body, expr)? {
                    ValueNs::LocalBinding(it) => Root::Local(it),
                    ValueNs::StaticId(it) => Root::Static(it),
                    _ => return None,
                };
                Some(Place { root, fields: Vec::new() }) // alloc: a place's fields, a few
            }
            Expr::Field { expr: inner, name } => {
                let mut place = self.place_of(inner, depth)?;
                place.fields.push(*name);
                Some(place)
            }
            Expr::UnaryOp { expr: inner, op: UnaryOp::Deref } if self.builtin_deref(inner) => {
                self.place_of(inner, depth)
            }
            Expr::Ref { expr: inner, rawness: Rawness::Ref, .. } => self.place_of(inner, depth),
            _ => None,
        }
    }

    /// `key` when evaluating it runs nothing: a literal, a path or a place, through `&` and `*`.
    fn plain_key(&self, key: ExprId) -> Option<ExprId> {
        let core = self.key_core(key, KEY_DEPTH);
        if self.runs_deref(key) || self.runs_deref(core) {
            return None;
        }
        match self.body.expr(core) {
            Expr::Literal(_) | Expr::Path(_) => Some(key),
            Expr::Field { .. } => self.place_of(core, PLACE_DEPTH).map(|_| key),
            _ => None,
        }
    }

    /// `key` through `&` and `*`.
    fn key_core(&self, key: ExprId, depth: u32) -> ExprId {
        let Some(depth) = depth.checked_sub(1) else { return key };
        match self.body.expr(key) {
            Expr::Ref { expr, rawness: Rawness::Ref, .. } => self.key_core(expr, depth),
            Expr::UnaryOp { expr, op: UnaryOp::Deref } if self.builtin_deref(expr) => {
                self.key_core(expr, depth)
            }
            _ => key,
        }
    }

    /// Whether the keys `a` and `b` are surely one value: the same literal, `const`, local or
    /// place, through `&` and `*`, the same number of each deep.
    fn same_key(&self, a: ExprId, b: ExprId) -> bool {
        if self.borrows(a) != self.borrows(b) {
            return false;
        }
        let (a, b) = (self.key_core(a, KEY_DEPTH), self.key_core(b, KEY_DEPTH));
        match (self.body.expr(a), self.body.expr(b)) {
            (Expr::Literal(x), Expr::Literal(y)) => x == y,
            (Expr::Path(_), Expr::Path(_)) => {
                let (x, y) = (
                    value_of(self.db(), self.def, self.body, a),
                    value_of(self.db(), self.def, self.body, b),
                );
                x.is_some()
                    && x == y
                    && matches!(
                        x,
                        Some(ValueNs::LocalBinding(_) | ValueNs::ConstId(_) | ValueNs::StaticId(_))
                    )
            }
            (Expr::Field { .. }, Expr::Field { .. }) => {
                matches!((self.place_of(a, PLACE_DEPTH), self.place_of(b, PLACE_DEPTH)), (Some(x), Some(y)) if x == y)
            }
            _ => false,
        }
    }

    /// The `&` and `*` around a key's core, outermost first: `true` for a `&`.
    fn borrows(&self, key: ExprId) -> Vec<bool> {
        // alloc: a key's few borrows
        let mut out = Vec::new();
        let mut at = key;
        for _ in 0..KEY_DEPTH {
            match self.body.expr(at) {
                Expr::Ref { expr, .. } => {
                    out.push(true);
                    at = expr;
                }
                Expr::UnaryOp { expr, op: UnaryOp::Deref } => {
                    out.push(false);
                    at = expr;
                }
                _ => break,
            }
        }
        out
    }

    /// The finding of the `if` `id`, its fix written.
    fn found(
        &self,
        lint: Lint,
        id: ExprId,
        condition: ExprId,
        probe: &Probe,
        lookup: &Lookup,
    ) -> Option<Found> {
        let (if_range, condition_range) = (self.range(id)?, self.range(condition)?);
        let name = fresh(self.cx.snippet(if_range))?;
        let with = match lookup.with {
            Use::Found => name.clone(), // clone: a finding's text
            Use::Wrapped => format!("Some({name})"),
            Use::Deref { parens: true } => format!("(*{name})"),
            Use::Deref { parens: false } => format!("*{name}"),
        };
        let what = match probe {
            Probe::Map { key, .. } => format!("the key `{}`", self.text(*key)?),
            Probe::Option { .. } => "the option's value".to_owned(), // alloc: a finding's text
        };
        let whole = self.cx.snippet(condition_range).to_owned() + self.cx.snippet(lookup.replaced); // alloc: as above
        let applicability = match whole.contains("//") || whole.contains("/*") {
            true => Applicability::MaybeIncorrect,
            false => Applicability::MachineApplicable,
        };
        let message = match lint {
            Lint::NllWorkaround => format!(
                "{what} is looked up twice so the borrow of the second lookup ends where the branch is not taken; \
                 blaze's borrow checker accepts one lookup, `if let Some({name}) = {}`",
                lookup.scrutinee
            ),
            _ => format!(
                "{what} is looked up twice; one lookup, `if let Some({name}) = {}`, does the same",
                lookup.scrutinee
            ),
        };
        Some(Found {
            lint,
            range: condition_range,
            again: lookup.replaced,
            message,
            parts: [
                (condition_range, format!("let Some({name}) = {}", lookup.scrutinee)),
                (lookup.replaced, with),
            ],
            applicability,
        })
    }
}

/// A name for the binding that no identifier in `text` (the `if`, where it would be in scope)
/// is: `found`, else `found1`, `found2`, ..
fn fresh(text: &str) -> Option<String> {
    let taken = |name: &str| {
        text.split(|it: char| !(it.is_alphanumeric() || it == '_')).any(|it| it == name)
    };
    (0..NAMES)
        .map(|n| match n {
            0 => "found".to_owned(), // alloc: a finding's text
            _ => format!("found{n}"),
        })
        .find(|it| !taken(it))
}

/// `mir` with the lookup `lookup` (its call's span) moved out of the `then` branch of the `if`
/// `if_` to just before the `if`'s switch, its result kept in a new local the branch then
/// reads: the borrows the direct form, `if let Some(found) = lookup`, makes. None when the path
/// from the switch to the call is not one the move keeps the meaning of: a block with another
/// way in, a cleanup block, a statement that writes a variable, may panic or drops, or a
/// terminator other than a `Goto`.
fn hoisted<'db>(
    mir: &MirBody<'db>,
    if_: ExprId,
    condition: ExprId,
    lookup: ExprId,
) -> Option<MirBody<'db>> {
    let blocks = &mir.basic_blocks;
    // The `if`'s switch on its condition's value: the only one.
    // alloc: the switches of one `if`, one
    let switches: Vec<(BasicBlockId, StoredPlace, BasicBlockId)> = blocks
        .iter()
        .filter_map(|(id, block)| {
            let terminator = block.terminator.as_ref()?;
            let TerminatorKind::SwitchInt { discr, targets } = &terminator.kind else {
                return None;
            };
            let OperandKind::Copy(flag) = &discr.kind else { return None };
            let ours = terminator.span == MirSpan::ExprId(if_)
                && discr.span == Some(MirSpan::ExprId(condition))
                && targets.values.as_slice() == [1]
                && targets.targets.len() == 2;
            ours.then(|| (id, flag.clone(), targets.targets[0])) // clone: one place
        })
        .collect();
    let [(switch, flag, then)] = &switches[..] else { return None };
    let (switch, then) = (*switch, *then);
    // The probe's call writes the flag; what the lookup unwinds to is what it did.
    let probe_cleanups: Vec<Option<BasicBlockId>> = blocks
        .iter()
        .filter_map(|(_, block)| {
            let terminator = block.terminator.as_ref()?;
            match &terminator.kind {
                TerminatorKind::Call { destination, cleanup, .. }
                    if terminator.span == MirSpan::ExprId(condition) && destination == flag =>
                {
                    Some(*cleanup)
                }
                _ => None,
            }
        })
        .collect(); // alloc: the probe's calls, one
    let &[cleanup] = &probe_cleanups[..] else { return None };
    // How many ways into each block.
    let mut preds = vec![0u32; blocks.len()]; // alloc: one count per block of one body
    for (_, block) in blocks.iter() {
        if let Some(terminator) = &block.terminator {
            for it in terminator.kind.all_successors() {
                preds[it.into_raw().into_u32() as usize] += 1;
            }
        }
    }
    // The path from the branch's start to the lookup's call.
    // alloc: the blocks of one path, at most PATH
    let mut path = Vec::new();
    let mut at = then;
    let call = loop {
        let block = &blocks[at];
        if path.len() >= PATH
            || preds[at.into_raw().into_u32() as usize] != 1
            || block.is_cleanup
            || path.contains(&at)
        {
            return None;
        }
        path.push(at);
        let terminator = block.terminator.as_ref()?;
        match &terminator.kind {
            TerminatorKind::Goto { target } => at = *target,
            TerminatorKind::Call { .. } if terminator.span == MirSpan::ExprId(lookup) => break at,
            _ => return None,
        }
    };
    if mir.panic_unwinds.iter().any(|(block, _, _)| path.contains(block)) {
        return None;
    }
    // The statements before the call only borrow, copy or name a value, into temporaries.
    let temporary = |local: LocalId| {
        local != return_slot()
            && !mir.param_locals.contains(&local)
            && !mir.binding_locals.values().any(|&it| it == local)
    };
    // alloc: the temporaries one path writes, a few
    let mut written: Vec<LocalId> = Vec::new();
    // A constant is inert in any of its forms: `Allocation` is how lowering writes a constant
    // value it has the bytes of (a call's `FnDef` is one, with none), `Static` the address of a
    // `static`.
    let readable = |operand: &Operand, written: &[LocalId]| match &operand.kind {
        OperandKind::Copy(_)
        | OperandKind::Constant { .. }
        | OperandKind::Allocation { .. }
        | OperandKind::Static(_) => true,
        OperandKind::Move(place) => place.projection.is_empty() && written.contains(&place.local),
    };
    for &block in &path {
        for statement in &blocks[block].statements {
            match &statement.kind {
                StatementKind::StorageLive(_) | StatementKind::Nop | StatementKind::FakeRead(_) => {
                }
                StatementKind::Assign(place, value)
                    if place.projection.is_empty() && temporary(place.local) =>
                {
                    let fine = match value {
                        Rvalue::Use(operand) => readable(operand, &written),
                        Rvalue::Ref(..) | Rvalue::CopyForDeref(_) => true,
                        _ => false,
                    };
                    if !fine {
                        return None;
                    }
                    written.push(place.local);
                }
                _ => return None,
            }
        }
    }
    let terminator = blocks[call].terminator.as_ref()?;
    let TerminatorKind::Call { func, args, destination, target: Some(target), .. } =
        &terminator.kind
    else {
        return None;
    };
    if !destination.projection.is_empty() || !temporary(destination.local) {
        return None;
    }
    if !readable(func, &written) || !args.iter().all(|it| readable(it, &written)) {
        return None;
    }
    let (target, span) = (*target, terminator.span);
    let mut out = mir.clone(); // clone: the one body a rewrite is checked on
    let ty = out.locals[destination.local].ty.clone(); // clone: one type
    let kept = out.locals.alloc(Local { ty });
    let kept_place = StoredPlace { local: kept, projection: destination.projection.clone() }; // clone: empty
    // alloc: the statements moved, a few
    let mut moved = Vec::new();
    for &block in &path {
        moved.append(&mut out.basic_blocks[block].statements);
    }
    let mut call_kind = out.basic_blocks[call].terminator.take()?.kind;
    let switch_terminator = out.basic_blocks[switch].terminator.take()?;
    let rest = out.basic_blocks.alloc(BasicBlock {
        statements: Vec::new(), // alloc: the switch's own block, no statements
        terminator: Some(switch_terminator),
        is_cleanup: false,
    });
    if let TerminatorKind::Call { destination: to, target: next, cleanup: unwind, .. } =
        &mut call_kind
    {
        *to = kept_place.clone(); // clone: one place
        *next = Some(rest);
        *unwind = cleanup;
    }
    let switch_block = &mut out.basic_blocks[switch];
    switch_block.statements.extend(moved);
    switch_block.terminator = Some(Terminator { span, kind: call_kind });
    let call_block = &mut out.basic_blocks[call];
    call_block.statements.push(Statement {
        kind: StatementKind::Assign(
            destination.clone(), // clone: one place
            Rvalue::Use(Operand { kind: OperandKind::Move(kept_place), span: None }),
        ),
        span,
    });
    call_block.terminator = Some(Terminator { span, kind: TerminatorKind::Goto { target } });
    Some(out)
}
