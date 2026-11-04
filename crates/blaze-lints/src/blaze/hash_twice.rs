//! `hash_twice`: a key looked up in a hash map or set and then written to it, on the same map
//! and the same key, with nothing changing either between: `contains_key` (`get`, `get_mut`,
//! `contains`, `get_key_value`) followed by `insert`, `remove`, `remove_entry` or `entry`. Each
//! call hashes the key and probes the table; the entry API (or the value a write returns) does
//! it once. On a concurrent map (papaya's, dashmap's) the two calls are also a race: another
//! thread may write the key between them.
//!
//! The maps are [`facts::hash_adt`]'s (std's `HashMap` and `HashSet` with any hasher,
//! hashbrown's, `blaze_hash`'s through their `Deref`, papaya's, dashmap's) and papaya's
//! `HashMapRef` and `HashSetRef`; the method is the one resolved, of the map's impl, never a
//! name on any type. The map is a place (a local or a `static`, its fields, derefs and
//! papaya's `pin()` of one) and the two calls name the same place; the keys are the same
//! literal, `const`, local or place, through `&`, `*` and conversions that keep the value
//! (`clone`, `to_owned`, `to_string`, `into`, `as_str`, `as_ref`, `borrow`).
//!
//! Not a finding: a write in another branch of an `if` or `match` than the lookup (each run
//! does one), one in a closure the other is not in, a write after anything that may change the
//! map or the key (a method taking `&mut self` of the map or a place holding it, a `&mut`
//! borrow, an assignment, a write of another key), and calls a macro wrote.
//!
//! The fix is offered for one shape only, `if !map.contains_key(&k) { map.insert(k, v); }`
//! on a map that is not concurrent: `map.entry(k).or_insert(v);` (`or_insert_with(|| v)`
//! when `v` calls anything, so it is still made only for a new key), and for a set
//! `if !set.contains(&k) { set.insert(k); }`, `set.insert(k);`. Machine-applicable when `k`
//! and `v` neither return, break nor await and do not name the map, and no comment is lost;
//! every other shape gets the advice as help.

use hir_def::{
    DefWithBodyId, FunctionId, StaticId,
    expr_store::Body,
    hir::{BinaryOp, BindingId, Expr, ExprId, Pat, Statement, UnaryOp},
    resolver::ValueNs,
    signatures::FunctionSignature,
    type_ref::Mutability,
};
use hir_expand::name::Name;
use syntax::{NodeOrToken, SyntaxKind, TextRange, TextSize};

use super::{
    Lint,
    alloc::written_callee,
    facts::{self, Parents},
    per_item::{mentions, value_of},
};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::HashTwice;

/// How many fields, derefs and indices deep a place is followed.
const PLACE_DEPTH: u32 = 16;
/// How many `&`, `*` and conversions a key is followed through.
const KEY_DEPTH: u32 = 8;
/// How many expressions up the common ancestor of two calls is looked for.
const UP: usize = 64;

/// The lookups.
const LOOKUPS: &[&str] = &["get", "get_mut", "contains_key", "contains", "get_key_value"];
/// The writes of one key that the entry API, or the write alone, replaces.
const WRITES: &[&str] = &["insert", "remove", "remove_entry", "entry"];
/// The methods of a concurrent map, through `&self`, that change it besides [`WRITES`].
const CONCURRENT_CHANGES: &[&str] = &[
    "clear",
    "retain",
    "compute",
    "update",
    "update_or_insert",
    "update_or_insert_with",
    "get_or_insert",
    "get_or_insert_with",
    "try_insert",
    "try_insert_with",
    "reserve",
    "alter",
    "alter_all",
    "remove_if",
    "remove_if_mut",
    "shrink_to_fit",
];
/// The conversions a key is followed through: each keeps the value it is called on.
const CONVERSIONS: &[&str] =
    &["clone", "to_owned", "to_string", "into", "as_str", "as_ref", "borrow"];
/// papaya's map and set, and what their `pin()` lends.
const PAPAYA: &[&str] = &["HashMap", "HashSet", "HashMapRef", "HashSetRef"];

/// What a call does to its map.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Lookup,
    Write,
    /// Anything else that may change the map.
    Change,
    /// Anything else, which reads it.
    Read,
}

/// Which kind of map a call is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    /// Changed through `&mut self`: std's, hashbrown's, blaze_hash's.
    Owned,
    /// Changed through `&self`: dashmap's.
    Concurrent,
    Papaya,
}

/// The local or `static` a place starts at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Root {
    Local(BindingId),
    Static(StaticId),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Field(Name),
    /// An index: which element is not known.
    Index,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Place {
    root: Root,
    steps: Vec<Step>,
}

impl Place {
    /// Whether `self` and `other` may overlap: one starts with the other, an index matching
    /// any step.
    fn overlaps(&self, other: &Place) -> bool {
        self.root == other.root
            && self
                .steps
                .iter()
                .zip(&other.steps)
                .all(|(a, b)| a == b || *a == Step::Index || *b == Step::Index)
    }

    /// Whether `self` and `other` are surely one place: equal, with no index.
    fn same(&self, other: &Place) -> bool {
        self == other && !self.steps.contains(&Step::Index)
    }
}

/// One call of a map's method.
struct MapCall {
    id: ExprId,
    range: TextRange,
    f: FunctionId,
    op: Op,
    family: Family,
    set: bool,
    place: Place,
    key: Option<ExprId>,
    /// `map.insert(..)`, not `HashMap::insert(&mut map, ..)`.
    method: bool,
}

/// A finding, before it is reported.
struct Found {
    range: TextRange,
    again: TextRange,
    message: String,
    help: String,
    fix: Option<(TextRange, String)>,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
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
        let again = cx.span(it.again);
        let fix = it.fix.map(|(at, to)| (cx.span(at), to));
        let diagnostic = cx.blaze(LINT, at, span, it.message);
        diagnostic.span_label(again, "and hashed and looked up again here");
        diagnostic.note(
            "each call hashes the key and probes the table; one call that both looks up and writes does it once",
        );
        match fix {
            Some(part) => {
                // alloc: the fix's one part
                diagnostic.multipart_suggestion(
                    "look the key up once",
                    vec![part],
                    Applicability::MachineApplicable,
                );
            }
            None => {
                diagnostic.help(it.help);
            }
        }
        diagnostic
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The body linted and what its walk needs.
struct Walk<'a, 'db, 't> {
    cx: &'a Cx<'db, 't>,
    def: DefWithBodyId,
    body: &'db Body,
    analysis: &'a hir::BodyAnalysis<'db>,
}

fn find(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let walk = Walk { cx, def, body: analysis.body(), analysis };
    // alloc: the map calls of one body, a few
    let mut calls: Vec<MapCall> =
        walk.body.exprs().filter_map(|(id, _)| walk.map_call(id)).collect();
    if !calls.iter().any(|it| it.op == Op::Lookup) {
        return found;
    }
    calls.sort_by_key(|it| (it.range.start(), it.range.end()));
    let parents = Parents::of(walk.body);
    for (at, lookup) in calls.iter().enumerate() {
        let Some(key) = lookup.key.filter(|_| lookup.op == Op::Lookup) else { continue };
        for write in &calls[at + 1..] {
            // A call inside the lookup (its key made by another lookup) runs before it.
            if write.range.start() < lookup.range.end() || !write.place.overlaps(&lookup.place) {
                continue;
            }
            if exclusive(walk.body, &parents, lookup.id, write.id) {
                continue;
            }
            match write.op {
                Op::Lookup | Op::Read => continue,
                Op::Change => break,
                Op::Write => {}
            }
            let same_key = write.key.is_some_and(|it| walk.same_key(key, it, KEY_DEPTH));
            if !write.place.same(&lookup.place) || !same_key {
                break;
            }
            // alloc: the places one pair is checked against, two
            let mut watched = vec![lookup.place.clone()]; // clone: one place per finding checked
            watched.extend(walk.place_of(walk.key_core(key, KEY_DEPTH), PLACE_DEPTH));
            if walk.changed_between(lookup.range.end(), write.range.start(), &watched) {
                break;
            }
            found.push(walk.found(&parents, lookup, write, key));
            break;
        }
    }
    found
}

impl<'db> Walk<'_, 'db, '_> {
    fn db(&self) -> &'db base_db::Db {
        self.cx.db()
    }

    fn name(&self, f: FunctionId) -> &'db str {
        FunctionSignature::of(self.db(), f).name.as_str()
    }

    /// Whether `id` is written in the body's own file, not by a macro.
    fn written(&self, id: ExprId) -> bool {
        let file = self.analysis.tree().1;
        self.analysis.source_map().expr_syntax(id).is_ok_and(|it| it.file_id == file)
    }

    /// The call `id` of a map's method, with what it does.
    fn map_call(&self, id: ExprId) -> Option<MapCall> {
        let (receiver, key, method) = match self.body.expr(id) {
            Expr::MethodCall { receiver, args, .. } => (receiver, args.first().copied(), true),
            Expr::Call { args: [first, rest @ ..], .. } => (*first, rest.first().copied(), false),
            _ => return None,
        };
        if !self.written(id) {
            return None;
        }
        let db = self.db();
        let (f, _) = written_callee(self.analysis.infer(), self.body, id)?;
        let adt = facts::impl_adt(db, f)?;
        let name = adt.name(db);
        let krate = adt.module(db).krate(db);
        let family = if facts::crate_is(db, krate, "papaya") && PAPAYA.contains(&name.as_str()) {
            Family::Papaya
        } else if !facts::hash_adt(self.cx, adt) {
            return None;
        } else if facts::crate_is(db, krate, "dashmap") {
            Family::Concurrent
        } else {
            Family::Owned
        };
        let place = self.place_of(receiver, PLACE_DEPTH)?;
        let method_name = self.name(f);
        let op = if LOOKUPS.contains(&method_name) {
            Op::Lookup
        } else if WRITES.contains(&method_name) {
            Op::Write
        } else if match family {
            Family::Owned => self.changes_self(f),
            Family::Concurrent | Family::Papaya => {
                CONCURRENT_CHANGES.contains(&method_name) || self.changes_self(f)
            }
        } {
            Op::Change
        } else {
            Op::Read
        };
        let range = self.analysis.expr_range(id)?;
        let set = name.as_str().contains("Set");
        Some(MapCall { id, range, f, op, family, set, place, key, method })
    }

    /// Whether `f` takes `&mut self` or `self`.
    fn changes_self(&self, f: FunctionId) -> bool {
        let db = self.db();
        hir::Function::from(f)
            .self_param(db)
            .is_some_and(|it| matches!(it.access(db), hir::Access::Exclusive | hir::Access::Owned))
    }

    /// The place `expr` names: a local or `static`, its fields, indices and derefs, and
    /// papaya's `pin()` of one.
    fn place_of(&self, expr: ExprId, depth: u32) -> Option<Place> {
        let depth = depth.checked_sub(1)?;
        match self.body.expr(expr) {
            Expr::Path(_) => {
                let root = match value_of(self.db(), self.def, self.body, expr)? {
                    ValueNs::LocalBinding(it) => Root::Local(it),
                    ValueNs::StaticId(it) => Root::Static(it),
                    _ => return None,
                };
                Some(Place { root, steps: Vec::new() }) // alloc: a place's steps, a few
            }
            Expr::Field { expr: inner, name } => {
                let mut place = self.place_of(inner, depth)?;
                place.steps.push(Step::Field(*name));
                Some(place)
            }
            Expr::Index { base, .. } => {
                let mut place = self.place_of(base, depth)?;
                place.steps.push(Step::Index);
                Some(place)
            }
            Expr::UnaryOp { expr: inner, op: UnaryOp::Deref } | Expr::Ref { expr: inner, .. } => {
                self.place_of(inner, depth)
            }
            Expr::MethodCall { receiver, args: [], .. } => {
                let (f, _) = written_callee(self.analysis.infer(), self.body, expr)?;
                let db = self.db();
                let adt = facts::impl_adt(db, f)?;
                let pin = matches!(self.name(f), "pin" | "pin_owned")
                    && facts::crate_is(db, adt.module(db).krate(db), "papaya");
                pin.then(|| self.place_of(receiver, depth)).flatten()
            }
            _ => None,
        }
    }

    /// `key` through `&`, `*` and [`CONVERSIONS`].
    fn key_core(&self, key: ExprId, depth: u32) -> ExprId {
        let Some(depth) = depth.checked_sub(1) else { return key };
        match self.body.expr(key) {
            Expr::Ref { expr, .. } | Expr::UnaryOp { expr, op: UnaryOp::Deref } => {
                self.key_core(expr, depth)
            }
            Expr::MethodCall { receiver, args: [], .. } => {
                let converts = written_callee(self.analysis.infer(), self.body, key)
                    .is_some_and(|(f, _)| CONVERSIONS.contains(&self.name(f)));
                match converts {
                    true => self.key_core(receiver, depth),
                    false => key,
                }
            }
            _ => key,
        }
    }

    /// Whether the keys `a` and `b` are surely one value: the same literal, `const`, local or
    /// place, through [`Walk::key_core`].
    fn same_key(&self, a: ExprId, b: ExprId, depth: u32) -> bool {
        let (a, b) = (self.key_core(a, depth), self.key_core(b, depth));
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
            _ => match (self.place_of(a, PLACE_DEPTH), self.place_of(b, PLACE_DEPTH)) {
                (Some(x), Some(y)) => x.same(&y),
                _ => false,
            },
        }
    }

    /// Whether something written between `from` and `to` may change one of `watched`: a call
    /// taking `&mut self` of a place overlapping one, a `&mut` borrow of one, or an assignment
    /// to one (or to what is not a place it can tell apart).
    fn changed_between(&self, from: TextSize, to: TextSize, watched: &[Place]) -> bool {
        let overlaps = |expr: ExprId| {
            self.place_of(expr, PLACE_DEPTH)
                .is_some_and(|it| watched.iter().any(|w| w.overlaps(&it)))
        };
        for (id, expr) in self.body.exprs() {
            let Some(range) = self.analysis.expr_range(id) else { continue };
            if range.start() < from || range.end() > to {
                continue;
            }
            let changes = match expr {
                Expr::Ref { expr, mutability: Mutability::Mut, .. } => overlaps(expr),
                Expr::BinaryOp { lhs, op: Some(BinaryOp::Assignment { .. }), .. } => overlaps(lhs),
                Expr::MethodCall { receiver, .. } => {
                    written_callee(self.analysis.infer(), self.body, id)
                        .is_some_and(|(f, _)| self.changes_self(f))
                        && overlaps(receiver)
                }
                Expr::Assignment { target, .. } => {
                    let mut hit = false;
                    self.body.walk_pats(target, &mut |pat| match self.body.pat(pat) {
                        Pat::Expr(it) => {
                            hit |= self
                                .place_of(it, PLACE_DEPTH)
                                .is_none_or(|place| watched.iter().any(|w| w.overlaps(&place)))
                        }
                        Pat::Wild
                        | Pat::Tuple { .. }
                        | Pat::Slice { .. }
                        | Pat::Rest
                        | Pat::Missing => {}
                        _ => hit = true,
                    });
                    hit
                }
                _ => false,
            };
            if changes {
                return true;
            }
        }
        false
    }

    fn found(&self, parents: &Parents, lookup: &MapCall, write: &MapCall, key: ExprId) -> Found {
        let snippet = |id: ExprId| self.analysis.expr_range(id).map(|it| self.cx.snippet(it));
        let map = match self.body.expr(write.id) {
            Expr::MethodCall { receiver, .. } => snippet(receiver),
            _ => None,
        };
        let (looked, wrote) = (self.name(lookup.f), self.name(write.f));
        // The key itself, as both calls name it through `&`, `*` and conversions (`k` of
        // `contains_key(&k)` and `insert(k.clone(), ..)`): what is hashed twice.
        let key_text = snippet(self.key_core(key, KEY_DEPTH)).unwrap_or("the key");
        let message = match map {
            Some(map) => format!(
                "`{map}` hashes and looks up `{key_text}` twice: `{looked}` here, then `{wrote}`"
            ),
            None => format!(
                "this map hashes and looks up `{key_text}` twice: `{looked}` here, then `{wrote}`"
            ),
        };
        Found {
            range: lookup.range,
            again: write.range,
            message,
            help: advice(write.family, write.set, wrote),
            fix: self.classic_fix(parents, lookup, write),
        }
    }

    /// The fix of `if !map.contains_key(&k) { map.insert(k, v); }` (and of a set's
    /// `contains` and `insert`): the `if`'s range and what replaces it.
    fn classic_fix(
        &self,
        parents: &Parents,
        lookup: &MapCall,
        write: &MapCall,
    ) -> Option<(TextRange, String)> {
        let body = self.body;
        let wanted = if write.set { "contains" } else { "contains_key" };
        if write.family != Family::Owned
            || !write.method
            || !lookup.method
            || self.name(lookup.f) != wanted
        {
            return None;
        }
        if self.name(write.f) != "insert" {
            return None;
        }
        let not = parents.of_expr(lookup.id)?;
        let Expr::UnaryOp { op: UnaryOp::Not, .. } = body.expr(not) else { return None };
        let if_ = parents.of_expr(not)?;
        let Expr::If { condition, then_branch, else_branch: None } = body.expr(if_) else {
            return None;
        };
        if condition != not {
            return None;
        }
        // The `if`'s block is the write alone.
        let Expr::Block { statements, tail, label: None, .. } = body.expr(then_branch) else {
            return None;
        };
        let alone = match (statements.len(), tail) {
            (0, Some(it)) => it == write.id,
            (1, None) => statements
                .into_iter()
                .next()
                .is_some_and(|it| matches!(it, Statement::Expr { expr, .. } if expr == write.id)),
            _ => false,
        };
        if !alone {
            return None;
        }
        // The `if` is a statement of a block, or its tail.
        let Expr::Block { statements: around, tail: around_tail, .. } =
            body.expr(parents.of_expr(if_)?)
        else {
            return None;
        };
        let semi = match around.into_iter().find_map(|it| match it {
            Statement::Expr { expr, has_semi } if expr == if_ => Some(has_semi),
            _ => None,
        }) {
            Some(has_semi) => has_semi,
            None if around_tail == Some(if_) => false,
            None => return None,
        };
        let file = self.analysis.tree().1;
        let written_if = self
            .analysis
            .source_map()
            .expr_syntax(if_)
            .is_ok_and(|it| it.file_id == file && it.value.kind() == SyntaxKind::IF_EXPR);
        if !written_if {
            return None;
        }
        let Expr::MethodCall { receiver, args, .. } = body.expr(write.id) else { return None };
        let Root::Local(map_root) = write.place.root else { return None };
        let movable =
            |it: ExprId| !self.flows(it) && !mentions(self.db(), self.def, body, it, &[map_root]);
        if !args.iter().all(|&it| movable(it)) {
            return None;
        }
        let range_of = |it: ExprId| self.analysis.expr_range(it);
        let if_range = range_of(if_)?;
        let whole = self.cx.snippet(if_range);
        if whole.contains("//") || whole.contains("/*") {
            return None;
        }
        let text = |it: ExprId| range_of(it).map(|range| self.cx.snippet(range));
        let map = text(receiver)?;
        let end = if semi { "" } else { ";" };
        let replaced = match (write.set, args) {
            (true, [k]) => format!("{map}.insert({}){end}", text(*k)?),
            (false, [k, v]) => {
                let (k, value) = (text(*k)?, text(*v)?);
                match self.plain(*v) {
                    true => format!("{map}.entry({k}).or_insert({value}){end}"),
                    false => format!("{map}.entry({k}).or_insert_with(|| {value}){end}"),
                }
            }
            _ => return None,
        };
        Some((if_range, replaced))
    }

    /// Whether `expr` may leave where it is evaluated: a `return`, `break`, `continue`, `?`,
    /// `.await` or `yield` in it.
    fn flows(&self, expr: ExprId) -> bool {
        let mut stack = vec![expr]; // alloc: the expressions of one argument
        while let Some(id) = stack.pop() {
            if matches!(
                self.body.expr(id),
                Expr::Return { .. }
                    | Expr::Break { .. }
                    | Expr::Continue { .. }
                    | Expr::Become { .. }
                    | Expr::Yield { .. }
                    | Expr::Yeet { .. }
                    | Expr::Await { .. }
            ) {
                return true;
            }
            self.body.walk_child_exprs(id, |child| stack.push(child));
        }
        false
    }

    /// Whether `expr` calls nothing: literals, paths, fields, borrows, operators and literals
    /// of tuples, arrays and structs, so making it whether or not the key is new is the same.
    fn plain(&self, expr: ExprId) -> bool {
        let mut stack = vec![expr]; // alloc: the expressions of one argument
        while let Some(id) = stack.pop() {
            let plain = matches!(
                self.body.expr(id),
                Expr::Literal(_)
                    | Expr::Path(_)
                    | Expr::Field { .. }
                    | Expr::Ref { .. }
                    | Expr::UnaryOp { .. }
                    | Expr::Tuple { .. }
                    | Expr::Array(_)
                    | Expr::RecordLit { .. }
            ) || matches!(self.body.expr(id), Expr::BinaryOp { op: Some(op), .. } if !matches!(op, BinaryOp::Assignment { .. }));
            if !plain {
                return false;
            }
            self.body.walk_child_exprs(id, |child| stack.push(child));
        }
        true
    }
}

/// Whether the calls `a` and `b` never both run in one evaluation of what holds them: they sit
/// in different branches of an `if` or different arms of a `match`, or one of them is in a
/// closure the other is not in (it may run any number of times, or never). Two calls with no
/// common ancestor within [`UP`] expressions are counted as exclusive, so nothing is claimed.
fn exclusive(body: &Body, parents: &Parents, a: ExprId, b: ExprId) -> bool {
    // alloc: the parents of two calls, at most UP each
    let up_a: Vec<ExprId> = std::iter::once(a).chain(parents.up(a, UP)).collect();
    let up_b: Vec<ExprId> = std::iter::once(b).chain(parents.up(b, UP)).collect(); // alloc: as above
    let Some(at_b) = up_b.iter().position(|it| up_a.contains(it)) else { return true };
    let common = up_b[at_b];
    let Some(at_a) = up_a.iter().position(|&it| it == common) else { return true };
    let (below_a, below_b) = (&up_a[..at_a], &up_b[..at_b]);
    if below_a.iter().chain(below_b).any(|&it| matches!(body.expr(it), Expr::Closure { .. })) {
        return true;
    }
    let (Some(&child_a), Some(&child_b)) = (below_a.last(), below_b.last()) else { return false };
    if child_a == child_b {
        return false;
    }
    match body.expr(common) {
        Expr::If { then_branch, else_branch, .. } => {
            let branch = |it: ExprId| it == then_branch || else_branch == Some(it);
            branch(child_a) && branch(child_b)
        }
        Expr::Match { arms, .. } => {
            let arm =
                |it: ExprId| arms.iter().position(|arm| arm.expr == it || arm.guard == Some(it));
            matches!((arm(child_a), arm(child_b)), (Some(x), Some(y)) if x != y)
        }
        _ => false,
    }
}

/// What looks the key up once instead, for a write `wrote` of a map of `family` (a set when
/// `set`).
fn advice(family: Family, set: bool, wrote: &str) -> String {
    let said = match (family, set, wrote) {
        (Family::Papaya, ..) => {
            "papaya looks a key up once in `get_or_insert_with(key, || value)`, and reads and writes it in one atomic \
             step in `compute(key, |entry| ..)`; a lookup and then a write is two lookups, and another thread may \
             write the key between them"
        }
        (Family::Concurrent, ..) => {
            "`entry(key)` looks the key up once, holding its shard; a lookup and then a write is two lookups, and \
             another thread may write the key between them"
        }
        (Family::Owned, true, "insert") => {
            "`insert` says whether the value was new: `if set.insert(key) { .. }` looks it up once"
        }
        (Family::Owned, true, _) => {
            "`remove` says whether the value was there: `if set.remove(&key) { .. }` looks it up once"
        }
        (Family::Owned, false, "remove" | "remove_entry") => {
            "`remove` hands back the value it took: `if let Some(value) = map.remove(&key) { .. }` looks the key up \
             once, as does `OccupiedEntry::remove` on `map.entry(key)`"
        }
        (Family::Owned, false, "entry") => {
            "`entry` looks the key up itself: match on what it returns (`Entry::Occupied`, `Entry::Vacant`) instead of \
             looking it up first"
        }
        (Family::Owned, false, _) => {
            "`map.entry(key)` looks the key up once: `.or_insert(value)`, `.or_insert_with(|| ..)`, \
             `.and_modify(|value| ..).or_insert(..)`, or a match on `Entry::Occupied` and `Entry::Vacant`"
        }
    };
    said.to_owned() // alloc: a finding's help, reported at the edge
}
