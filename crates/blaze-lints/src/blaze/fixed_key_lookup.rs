//! `fixed_key_lookup`: a lookup whose keys are all known at compile time, made at run time
//! (AGENTS.md rule standard-structures: one choice per need). A fixed set of names is a perfect
//! hash built by `const fn` (`blaze_phf::phf_table!`): no table filled on first use, no hasher
//! state, no run of string compares, and a key named twice fails to compile. A finding is:
//!
//! - a hash map or set ([`facts::hash_collection`]: std's with any hasher, hashbrown's,
//!   blaze_hash's) keyed by `&str` (or `&[u8]`), built only from string literals and `const`s
//!   or `static`s, and never changed after. Built: `HashMap::from([(lit, ..), ..])`,
//!   `FromIterator::from_iter` or `collect` of an array literal or a `const` or `static` through
//!   `iter`, `into_iter`, `copied` and `cloned`, or `new()` (`default`, `with_capacity`,
//!   `with_hasher`, ..) bound to a local and followed at once by `insert(lit, ..)` statements.
//!   Never changed after: the value goes into a `static`'s `LazyLock` (std's `LazyLock` and
//!   `LazyCell`, once_cell's `Lazy`), into the `get_or_init` of a `static`'s `OnceLock` or
//!   `OnceCell`, or into a local whose every other use is a method taking `&self`, a shared
//!   borrow, or a move into one of these. Anything else (a `&mut` use, a method taking `&mut
//!   self` or `self`, a return, a field) may change it, and is not a finding. papaya's and
//!   dashmap's maps insert through `&self`, so no use shows them fixed: they are not checked.
//! - a `match` on a `&str` (or a `&[u8]` whose byte-string patterns are all UTF-8, which
//!   `Phf::index` looks up) with more than [`KEYS`] string-literal patterns in arms with no
//!   guard, or an `if`/`else if` chain of more than [`KEYS`] `place == "lit"` compares of one
//!   place (a local or static, a field of one, or a method of one with no arguments). Not in a
//!   `const fn`, a `const`, a `static`'s initializer or a `const` block, which run at compile
//!   time, nor in code a macro writes (a `matches!`, a generated parser), where the keys are
//!   not the author's to move into a table.
//!
//! Calls, paths and types are resolved, so a renamed import, an alias (`FxHashMap`) and a
//! `const` key count. There is no fix: the keys move to a `const` slice and the values to a
//! `const` array in its order, which only the author can lay out.

use std::cell::OnceCell;

use base_db::Db;
use hir::AsAssocItem as _;
use hir_def::{
    DefWithBodyId,
    expr_store::{Body, scope::ExprScopes},
    hir::{
        Array, BinaryOp, BindingId, CmpOp, Expr, ExprId, Literal, LogicOp, Pat, PatId, Statement,
        UnaryOp,
    },
    resolver::{ValueNs, resolver_for_scope},
    signatures::FunctionSignature,
    type_ref::Mutability,
};
use hir_ty::InferenceResult;

use super::{
    Finding, Lint,
    alloc::written_callee,
    facts::{self, Parents},
};
use crate::check::Cx;

const LINT: Lint = Lint::FixedKeyLookup;

/// A `match` or `if` chain with more string-literal keys than this is a finding. rustc lowers
/// a `match` on a `&str` to one `==` per arm in order, each a length compare and a `memcmp`;
/// past about 16 keys that run costs more than a perfect hash's one hash of the bytes and one
/// compare, while a shorter `match` reads better than a table and costs about the same.
const KEYS: usize = 16;

/// The rule every finding states.
const RULE: &str = "keys known at compile time: build a perfect hash with `blaze_phf::phf_table!` \
                    (const fn, no build step, a duplicate key fails to compile), and index a const \
                    array of values with what it returns";

/// The constructors of an empty map that `insert(lit, ..)` statements may fill.
const CONSTRUCT: &[&str] =
    &["new", "default", "with_capacity", "with_hasher", "with_capacity_and_hasher"];
/// The methods a `collect` or `from_iter` source is followed through to its array or `const`.
const CHAIN: &[&str] = &["iter", "into_iter", "copied", "cloned"];
/// The cells a `static` keeps a value in, by crate and name: what is in one is only lent out.
const CELLS: &[(&str, &str)] = &[
    ("std", "LazyLock"),
    ("std", "OnceLock"),
    ("core", "LazyCell"),
    ("core", "OnceCell"),
    ("once_cell", "Lazy"),
    ("once_cell", "OnceCell"),
];
/// The crates whose maps insert through `&self`.
const SHARED_MUTABLE: &[&str] = &["papaya", "dashmap"];

/// How many expressions up a map is followed to where it is kept.
const SINK_DEPTH: usize = 8;
/// How many locals a map is followed through (`let a = ..; let b = a;`).
const LOCALS: u32 = 4;
/// How many methods a source is followed through to its array or `const`.
const CHAIN_DEPTH: usize = 8;
/// How many expressions up a `match` is looked at for a closure or `const` block around it.
const UP: usize = 64;
/// The most links of an `if` chain followed, and the depth a place is compared to.
const LINKS: usize = 4096;
const PLACE_DEPTH: u32 = 8;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// The body linted and what its walk needs.
struct Walk<'a, 'db> {
    db: &'db Db,
    def: DefWithBodyId,
    body: &'db Body,
    infer: &'db InferenceResult<'db>,
    analysis: &'a hir::BodyAnalysis<'db>,
    /// Made the first time a finding needs it.
    parents: OnceCell<Parents>,
}

/// Where a fixed map is kept.
enum Held {
    Static(hir::Static),
    Local(BindingId),
}

/// How many keys a fixed map has, as the message says it.
enum Count {
    Written(usize),
    /// Every key of the `const` or `static` named, its length when it is an array.
    Of(String, Option<usize>),
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let body = analysis.body();
    let walk = Walk {
        db: cx.db(),
        def,
        body,
        infer: analysis.infer(),
        analysis,
        parents: OnceCell::new(),
    };
    for (id, expr) in body.exprs() {
        match expr {
            Expr::Call { .. } | Expr::MethodCall { .. } => walk.map(cx, id, &mut found),
            Expr::Match { expr: scrutinee, arms } => {
                walk.matched(cx, id, scrutinee, arms, &mut found)
            }
            Expr::If { .. } => walk.chain(cx, id, &mut found),
            _ => {}
        }
    }
    found
}

impl<'db> Walk<'_, 'db> {
    fn parents(&self) -> &Parents {
        self.parents.get_or_init(|| Parents::of(self.body))
    }

    fn name_of(&self, f: hir_def::FunctionId) -> &'db str {
        FunctionSignature::of(self.db, f).name.as_str()
    }

    /// Whether `id` is written in the body's own file, not by a macro.
    fn written(&self, id: ExprId) -> bool {
        // The file the body's own expressions are in; one in another is a macro's.
        let file = self.analysis.tree().1;
        self.analysis.source_map().expr_syntax(id).is_ok_and(|it| it.file_id == file)
    }

    /// Whether `id` runs at compile time: in a `const fn`, a `const`, a `const` block, or a
    /// `static`'s initializer outside a closure.
    fn compile_time(&self, id: ExprId) -> bool {
        let (mut in_closure, mut in_const) = (false, false);
        for it in self.parents().up(id, UP) {
            in_closure |= matches!(self.body.expr(it), Expr::Closure { .. });
            in_const |= matches!(self.body.expr(it), Expr::Const(_));
        }
        in_const
            || match self.analysis.def() {
                hir::DefWithBody::Function(f) => f.is_const(self.db),
                hir::DefWithBody::Static(_) => !in_closure,
                hir::DefWithBody::Const(_) | hir::DefWithBody::EnumVariant(_) => true,
            }
    }

    /// The value the path expression `expr` names, if it is one.
    fn value_of(&self, expr: ExprId) -> Option<ValueNs> {
        let Expr::Path(path) = self.body.expr(expr) else { return None };
        let scope = ExprScopes::of(self.db, self.def).scope_for(expr);
        resolver_for_scope(self.db, self.def, scope).resolve_path_in_value_ns_fully(
            self.db,
            path,
            self.body.expr_path_hygiene(expr),
        )
    }

    /// The `static` that is not `mut` the path expression `expr` names.
    fn static_of(&self, expr: ExprId) -> Option<hir::Static> {
        match self.value_of(expr)? {
            ValueNs::StaticId(it) => Some(hir::Static::from(it)).filter(|it| !it.is_mut(self.db)),
            _ => None,
        }
    }

    /// Whether the key `key` is known at compile time: a string literal (a byte string that is
    /// UTF-8), or a `const` or a `static` that is not `mut`, maybe borrowed.
    fn fixed_key(&self, key: ExprId) -> bool {
        match self.body.expr(key) {
            Expr::Literal(literal) => text_literal(literal),
            Expr::Ref { expr, mutability: Mutability::Shared, .. } => self.fixed_key(expr),
            Expr::Path(_) => match self.value_of(key) {
                Some(ValueNs::ConstId(_)) => true,
                Some(ValueNs::StaticId(it)) => !hir::Static::from(it).is_mut(self.db),
                _ => false,
            },
            _ => false,
        }
    }

    /// The path expressions of the body that name `local`.
    fn uses(&self, local: BindingId) -> Vec<ExprId> {
        // alloc: the uses of one fixed map's local, a few
        let mut uses = Vec::new();
        for (id, expr) in self.body.exprs() {
            if matches!(expr, Expr::Path(_))
                && matches!(self.value_of(id), Some(ValueNs::LocalBinding(it)) if it == local)
            {
                uses.push(id);
            }
        }
        uses
    }

    /// A hash map or set built at `id` from keys known at compile time and never changed
    /// after: its finding.
    fn map(&self, cx: &Cx<'_, '_>, id: ExprId, found: &mut Vec<Finding>) {
        let db = self.db;
        let ty = self.analysis.ty(self.infer.expr_ty(id));
        if !facts::hash_collection(cx, &ty) {
            return;
        }
        let Some(adt) = ty.strip_references().as_adt() else { return };
        let krate = adt.module(db).krate(db);
        if SHARED_MUTABLE.iter().any(|it| facts::crate_is(db, krate, it)) {
            return;
        }
        if !ty.type_arguments().next().is_some_and(|key| text_key(&key)) {
            return;
        }
        let set = adt.name(db).as_str().ends_with("Set");
        let Some((f, _)) = written_callee(self.infer, self.body, id) else { return };
        let name = self.name_of(f);
        let (count, held) = match (name, self.body.expr(id)) {
            ("from" | "from_iter", Expr::Call { args: [source], .. }) => {
                let Some(count) = self.source_keys(*source, set) else { return };
                let Some(held) = self.kept(id, LOCALS) else { return };
                (count, held)
            }
            ("collect", Expr::MethodCall { receiver, .. }) => {
                let Some(count) = self.source_keys(receiver, set) else { return };
                let Some(held) = self.kept(id, LOCALS) else { return };
                (count, held)
            }
            _ if CONSTRUCT.contains(&name) => {
                let Some((local, inserts)) = self.filled(id, set) else { return };
                let Some(held) = self.local_kept(local, &inserts, LOCALS) else { return };
                (Count::Written(inserts.len()), held)
            }
            _ => return,
        };
        let Some(range) = self.analysis.expr_range(id) else { return };
        let holder = match held {
            Held::Static(it) => format!("the static `{}`", path_of(db, it.into())),
            Held::Local(it) => format!(
                "the local `{}` in {}",
                self.body[it].name.as_str(),
                item_name(db, self.analysis.def())
            ),
        };
        let count = match count {
            Count::Written(n) => format!("{n} keys"),
            Count::Of(name, Some(n)) => format!("the {n} keys of `{name}`"),
            Count::Of(name, None) => format!("the keys of `{name}`"),
        };
        let message = format!(
            "{holder} is a `{}` of {count}, filled at run time; {RULE}",
            adt.name(db).as_str()
        );
        facts::push(found, LINT, range, message, None);
    }

    /// The keys of the source `source` of a map's `from`, `from_iter` or `collect`, when all of
    /// them are known at compile time: an array literal of them (of `(key, value)` pairs for a
    /// map), or a `const` or `static`, through [`CHAIN`]'s methods.
    fn source_keys(&self, source: ExprId, set: bool) -> Option<Count> {
        let db = self.db;
        let mut at = source;
        for _ in 0..CHAIN_DEPTH {
            match self.body.expr(at) {
                Expr::MethodCall { receiver, .. } => {
                    let (f, _) = written_callee(self.infer, self.body, at)?;
                    if !CHAIN.contains(&self.name_of(f)) {
                        return None;
                    }
                    at = receiver;
                }
                Expr::Ref { expr, mutability: Mutability::Shared, .. } => at = expr,
                Expr::Array(Array::ElementList { elements }) => {
                    for &element in elements {
                        let key = match (set, self.body.expr(element)) {
                            (true, _) => element,
                            (false, Expr::Tuple { exprs: [key, _] }) => *key,
                            (false, _) => return None,
                        };
                        if !self.fixed_key(key) {
                            return None;
                        }
                    }
                    return (!elements.is_empty()).then_some(Count::Written(elements.len()));
                }
                Expr::Path(_) => {
                    let name = match self.value_of(at)? {
                        ValueNs::ConstId(it) => path_of(db, hir::Const::from(it).into()),
                        ValueNs::StaticId(it) if !hir::Static::from(it).is_mut(db) => {
                            path_of(db, hir::Static::from(it).into())
                        }
                        _ => return None,
                    };
                    let ty = self.analysis.ty(self.infer.expr_ty(at)).strip_references();
                    return Some(Count::Of(name, ty.as_array(db).map(|(_, len)| len)));
                }
                _ => return None,
            }
        }
        None
    }

    /// The local an empty map made at `id` is bound to, and the receivers of the
    /// `insert(key, ..)` statements right after the `let` that fill it with keys known at
    /// compile time, when there is at least one.
    fn filled(&self, id: ExprId, set: bool) -> Option<(BindingId, Vec<ExprId>)> {
        let block = self.parents().of_expr(id)?;
        let Expr::Block { statements, .. } = self.body.expr(block) else { return None };
        let (at, local) = statements.iter().enumerate().find_map(|(at, it)| match it {
            Statement::Let { pat, initializer: Some(init), .. } if init == id => {
                match self.body.pat(pat) {
                    Pat::Bind { id: local, subpat: None } => Some((at, local)),
                    _ => None,
                }
            }
            _ => None,
        })?;
        // alloc: the inserts that fill one map, a few dozen
        let mut inserts = Vec::new();
        for statement in statements.iter().skip(at + 1) {
            let Statement::Expr { expr, .. } = statement else { break };
            let Expr::MethodCall { receiver, args, .. } = self.body.expr(expr) else { break };
            let insert = written_callee(self.infer, self.body, expr)
                .is_some_and(|(f, _)| self.name_of(f) == "insert");
            let arity = if set { 1 } else { 2 };
            let on_local =
                matches!(self.value_of(receiver), Some(ValueNs::LocalBinding(it)) if it == local);
            if !insert || args.len() != arity || !on_local || !self.fixed_key(args[0]) {
                break;
            }
            inserts.push(receiver);
        }
        (!inserts.is_empty()).then_some((local, inserts))
    }

    /// Where the map that `value` is goes, when nothing can change it there: through block
    /// tails into a local ([`Walk::local_kept`]) or a closure a `static`'s cell runs
    /// ([`Walk::cell`]).
    fn kept(&self, value: ExprId, depth: u32) -> Option<Held> {
        let mut child = value;
        for _ in 0..SINK_DEPTH {
            let parent = self.parents().of_expr(child)?;
            match self.body.expr(parent) {
                Expr::Block { tail: Some(it), .. } if it == child => child = parent,
                Expr::Block { statements, .. } => {
                    let local = statements.iter().find_map(|it| match it {
                        Statement::Let { pat, initializer: Some(init), .. } if init == child => {
                            match self.body.pat(pat) {
                                Pat::Bind { id, subpat: None } => Some(id),
                                _ => None,
                            }
                        }
                        _ => None,
                    })?;
                    return self.local_kept(local, &[], depth);
                }
                Expr::Closure { body, .. } if body == child => return self.cell(parent),
                _ => return None,
            }
        }
        None
    }

    /// Whether nothing changes the map in `local`: each of its uses but `skip` is a method
    /// taking `&self`, a shared borrow, or a move to where [`Walk::kept`] says it is kept.
    fn local_kept(&self, local: BindingId, skip: &[ExprId], depth: u32) -> Option<Held> {
        let depth = depth.checked_sub(1)?;
        let mut held = Held::Local(local);
        for at in self.uses(local) {
            if skip.contains(&at) {
                continue;
            }
            let parent = self.parents().of_expr(at)?;
            match self.body.expr(parent) {
                Expr::MethodCall { receiver, .. } if receiver == at => {
                    let (f, _) = written_callee(self.infer, self.body, parent)?;
                    let access = hir::Function::from(f).self_param(self.db)?.access(self.db);
                    if !matches!(access, hir::Access::Shared) {
                        return None;
                    }
                }
                Expr::Ref { expr, mutability: Mutability::Shared, .. } if expr == at => {}
                _ => held = self.kept(at, depth)?,
            }
        }
        Some(held)
    }

    /// The `static` whose cell runs the closure `closure` to make its value once and then only
    /// lends it: `LazyLock::new(closure)` in a `static`'s initializer, or
    /// `CELL.get_or_init(closure)` of a `static` `CELL`.
    fn cell(&self, closure: ExprId) -> Option<Held> {
        let db = self.db;
        let call = self.parents().of_expr(closure)?;
        let (args, receiver) = match self.body.expr(call) {
            Expr::Call { args, .. } => (args, None),
            Expr::MethodCall { receiver, args, .. } => (args, Some(receiver)),
            _ => return None,
        };
        if !args.contains(&closure) {
            return None;
        }
        let (f, _) = written_callee(self.infer, self.body, call)?;
        let adt = facts::impl_adt(db, f)?;
        if !CELLS.iter().any(|(krate, name)| facts::adt_is(db, adt, krate, name)) {
            return None;
        }
        match self.name_of(f) {
            "new" => match self.analysis.def() {
                hir::DefWithBody::Static(it) if !it.is_mut(db) => Some(Held::Static(it)),
                _ => None,
            },
            "get_or_init" => {
                let mut cell = receiver.or_else(|| args.first().copied())?;
                while let Expr::Ref { expr, .. } = self.body.expr(cell) {
                    cell = expr;
                }
                self.static_of(cell).map(Held::Static)
            }
            _ => None,
        }
    }

    /// A `match` on a `&str` or `&[u8]` at `id` with more than [`KEYS`] string-literal keys:
    /// its finding.
    fn matched(
        &self,
        cx: &Cx<'_, '_>,
        id: ExprId,
        scrutinee: ExprId,
        arms: &[hir_def::hir::MatchArm],
        found: &mut Vec<Finding>,
    ) {
        if !text_key(&self.analysis.ty(self.infer.expr_ty(scrutinee))) {
            return;
        }
        let mut keys = 0;
        for arm in arms.iter().filter(|it| it.guard.is_none()) {
            let Some(n) = self.literal_pats(arm.pat) else { return };
            keys += n;
        }
        if keys <= KEYS || !self.written(id) || self.compile_time(id) {
            return;
        }
        let Some(range) =
            self.analysis.expr_range(scrutinee).or_else(|| self.analysis.expr_range(id))
        else {
            return;
        };
        let message = format!(
            "this `match` on `{}` in {} compares {keys} string literals one after another; {RULE}",
            cx.snippet(range),
            item_name(self.db, self.analysis.def())
        );
        facts::push(found, LINT, range, message, None);
    }

    /// The string literals `pat` matches, through `|` and `&`; `None` when one is a byte
    /// string that is not UTF-8, which a table of `&str` keys can't hold.
    fn literal_pats(&self, pat: PatId) -> Option<usize> {
        match self.body.pat(pat) {
            Pat::Lit(expr) => match self.body.expr(expr) {
                Expr::Literal(Literal::String(_)) => Some(1),
                Expr::Literal(Literal::ByteString(bytes)) => {
                    std::str::from_utf8(bytes).is_ok().then_some(1)
                }
                _ => Some(0),
            },
            Pat::Or(pats) => pats.iter().try_fold(0, |sum, &it| Some(sum + self.literal_pats(it)?)),
            Pat::Ref { pat, .. } => self.literal_pats(pat),
            _ => Some(0),
        }
    }

    /// An `if`/`else if` chain from `id` of more than [`KEYS`] compares of one place with
    /// string literals: its finding, at the chain's first `if`.
    fn chain(&self, cx: &Cx<'_, '_>, id: ExprId, found: &mut Vec<Finding>) {
        let mut place = None;
        let mut keys = 0;
        let mut at = id;
        for _ in 0..LINKS {
            let Expr::If { condition, else_branch, .. } = self.body.expr(at) else { break };
            let Some(n) = self.compares(condition, &mut place) else { break };
            keys += n;
            match else_branch {
                Some(next) => at = next,
                None => break,
            }
        }
        if keys <= KEYS {
            return;
        }
        // The chain is reported at its head, not again at each `else if`.
        if let Some(parent) = self.parents().of_expr(id)
            && matches!(self.body.expr(parent), Expr::If { else_branch: Some(it), .. } if it == id)
        {
            return;
        }
        if !self.written(id) || self.compile_time(id) {
            return;
        }
        let Some(place) = place else { return };
        let head = match self.body.expr(id) {
            Expr::If { condition, .. } => condition,
            _ => id,
        };
        let Some(range) = self.analysis.expr_range(head).or_else(|| self.analysis.expr_range(id))
        else {
            return;
        };
        let compared = self.analysis.expr_range(place).map_or("it", |it| cx.snippet(it));
        let message = format!(
            "this `if`/`else if` chain in {} compares `{compared}` with {keys} string literals one after another; {RULE}",
            item_name(self.db, self.analysis.def())
        );
        facts::push(found, LINT, range, message, None);
    }

    /// How many `place == "lit"` compares the condition `cond` is, joined by `||`, all of
    /// the one place `place` (set by the first).
    fn compares(&self, cond: ExprId, place: &mut Option<ExprId>) -> Option<usize> {
        match self.body.expr(cond) {
            Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::LogicOp(LogicOp::Or)) } => {
                Some(self.compares(lhs, place)? + self.compares(rhs, place)?)
            }
            Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(BinaryOp::CmpOp(CmpOp::Eq { negated: false })),
            } => {
                let other = match (self.is_text_literal(lhs), self.is_text_literal(rhs)) {
                    (false, true) => lhs,
                    (true, false) => rhs,
                    _ => return None,
                };
                if !text_place(self.db, &self.analysis.ty(self.infer.expr_ty(other))) {
                    return None;
                }
                let first = *place;
                match first {
                    None => *place = Some(other),
                    Some(it) if self.same_place(it, other, PLACE_DEPTH) => {}
                    Some(_) => return None,
                }
                Some(1)
            }
            _ => None,
        }
    }

    fn is_text_literal(&self, expr: ExprId) -> bool {
        match self.body.expr(expr) {
            Expr::Literal(literal) => text_literal(literal),
            Expr::Ref { expr, .. } => self.is_text_literal(expr),
            _ => false,
        }
    }

    /// Whether `a` and `b` read one place: one local or static, the same field of one, or the
    /// same method with no arguments of one.
    fn same_place(&self, a: ExprId, b: ExprId, depth: u32) -> bool {
        let Some(depth) = depth.checked_sub(1) else { return false };
        match (self.body.expr(a), self.body.expr(b)) {
            (Expr::Path(_), Expr::Path(_)) => match (self.value_of(a), self.value_of(b)) {
                (Some(ValueNs::LocalBinding(x)), Some(ValueNs::LocalBinding(y))) => x == y,
                (Some(ValueNs::StaticId(x)), Some(ValueNs::StaticId(y))) => x == y,
                _ => false,
            },
            (Expr::Field { expr: x, name: n }, Expr::Field { expr: y, name: m }) => {
                n == m && self.same_place(x, y, depth)
            }
            (
                Expr::UnaryOp { expr: x, op: UnaryOp::Deref },
                Expr::UnaryOp { expr: y, op: UnaryOp::Deref },
            )
            | (Expr::Ref { expr: x, .. }, Expr::Ref { expr: y, .. }) => {
                self.same_place(x, y, depth)
            }
            (
                Expr::MethodCall { receiver: x, args: [], .. },
                Expr::MethodCall { receiver: y, args: [], .. },
            ) => {
                let callee = |it: ExprId| written_callee(self.infer, self.body, it).map(|(f, _)| f);
                callee(a).is_some_and(|f| Some(f) == callee(b)) && self.same_place(x, y, depth)
            }
            _ => false,
        }
    }
}

/// A string literal, or a byte string that is UTF-8: what a table of `&str` keys holds.
fn text_literal(literal: &Literal) -> bool {
    match literal {
        Literal::String(_) => true,
        Literal::ByteString(bytes) => std::str::from_utf8(bytes).is_ok(),
        _ => false,
    }
}

/// Whether `ty`, its references peeled, is `str` or `[u8]`: a key `blaze_phf` looks up.
fn text_key(ty: &hir::Type<'_>) -> bool {
    let ty = ty.strip_references();
    ty.is_str()
        || ty
            .as_slice()
            .is_some_and(|it| it.as_builtin().is_some_and(|it| it.name().as_str() == "u8"))
}

/// [`text_key`], or a `String`: what is compared with a string literal.
fn text_place(db: &Db, ty: &hir::Type<'_>) -> bool {
    text_key(ty)
        || ty.strip_references().as_adt().is_some_and(|it| facts::adt_is(db, it, "alloc", "String"))
}

/// The item `def` is, with its path: "function `main::parse`".
fn item_name(db: &Db, def: hir::DefWithBody) -> String {
    let (kind, item): (&str, hir::ModuleDef) = match def {
        hir::DefWithBody::Function(it) => ("function", it.into()),
        hir::DefWithBody::Static(it) => ("static", it.into()),
        hir::DefWithBody::Const(it) => ("const", it.into()),
        hir::DefWithBody::EnumVariant(it) => ("variant", it.into()),
    };
    format!("{kind} `{}`", path_of(db, item))
}

/// `item`'s path from its crate's name, through the type of its impl for a method.
fn path_of(db: &Db, item: hir::ModuleDef) -> String {
    // alloc: a message's part
    let mut path = String::new();
    let mut push = |segment: &str| {
        if !path.is_empty() {
            path.push_str("::");
        }
        path.push_str(segment);
    };
    if let Some(module) = item.module(db) {
        if let Some(krate) = module.krate(db).display_name(db) {
            push(krate.crate_name().as_str());
        }
        for segment in module.path_segments(db) {
            push(segment.as_str());
        }
    }
    if let hir::ModuleDef::Function(f) = item
        && let Some(adt) = f
            .as_assoc_item(db)
            .and_then(|it| it.implementing_ty(db))
            .and_then(|it| it.strip_references().as_adt())
    {
        push(adt.name(db).as_str());
    }
    if let Some(name) = item.name(db) {
        push(name.as_str());
    }
    path
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const STD: &str = r#"
//- minicore: sized, copy, clone, option, fn, from, deref, builtin_impls
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod hash {
    pub struct RandomState;
}
pub mod collections {
    #[rustc_diagnostic_item = "HashMap"]
    pub struct HashMap<K, V, S = crate::hash::RandomState> { k: *const K, v: *const V, s: S }
    impl<K, V> HashMap<K, V, crate::hash::RandomState> {
        pub fn new() -> HashMap<K, V> { loop {} }
    }
    impl<K, V, S> HashMap<K, V, S> {
        pub fn insert(&mut self, k: K, v: V) -> Option<V> { None }
        pub fn get(&self, k: &K) -> Option<&V> { None }
    }
    impl<K, V, const N: usize> From<[(K, V); N]> for HashMap<K, V> {
        fn from(entries: [(K, V); N]) -> HashMap<K, V> { loop {} }
    }
}
pub mod sync {
    pub struct LazyLock<T, F = fn() -> T> { value: T, f: F }
    impl<T, F> LazyLock<T, F> {
        pub const fn new(f: F) -> LazyLock<T, F> { loop {} }
    }
}
//- /src/lib.rs crate:main deps:std
use std::{collections::HashMap, sync::LazyLock};
"#;

    const RULE: &str =
        "keys known at compile time: build a perfect hash with `blaze_phf::phf_table!`";

    fn lint_main(main: &str) -> String {
        lint(
            &format!("{STD}{}", main.trim_start_matches('\n')),
            "-A clippy::all --warn=blaze::fixed_key_lookup",
        )
    }

    /// `n` arms of a `match` on `word`, each a string literal.
    fn arms(n: usize) -> String {
        (0..n).map(|it| format!("        \"key{it}\" => {it},\n")).collect()
    }

    /// A static map of literals, from an array or filled by `insert`s in its `LazyLock`, and a
    /// local one only read, are found; a map inserted into after it is built is not.
    #[test]
    fn a_map_of_literal_keys_never_changed_is_found() {
        let found = lint_main(
            r#"
pub static NAMES: LazyLock<HashMap<&'static str, u32>> = LazyLock::new(|| HashMap::from([("a", 1), ("b", 2), ("c", 3)]));
pub static FILLED: LazyLock<HashMap<&'static str, u32>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    map.insert("x", 1);
    map.insert("y", 2);
    map
});
pub fn read(name: &'static str) -> bool {
    let names = HashMap::from([("a", 1u32), ("b", 2)]);
    names.get(&name).is_some()
}
pub fn grown(extra: &'static str) -> bool {
    let mut names = HashMap::from([("a", 1u32), ("b", 2)]);
    names.insert(extra, 3);
    names.get(&"a").is_some()
}
pub fn handed_out() -> HashMap<&'static str, u32> {
    HashMap::from([("a", 1), ("b", 2)])
}
"#,
        );
        assert_eq!(found.matches(RULE).count(), 3, "{found}");
        assert!(
            found.contains("the static `main::NAMES` is a `HashMap` of 3 keys, filled at run time"),
            "{found}"
        );
        assert!(
            found
                .contains("the static `main::FILLED` is a `HashMap` of 2 keys, filled at run time"),
            "{found}"
        );
        assert!(
            found.contains("the local `names` in function `main::read` is a `HashMap` of 2 keys"),
            "{found}"
        );
        assert!(
            !found.contains("in function `main::grown` is a"),
            "a map inserted into later is not fixed:\n{found}"
        );
        assert!(
            !found.contains("in function `main::handed_out` is a"),
            "a returned map may change in its caller:\n{found}"
        );
    }

    /// A `match` on a `&str` of 20 literal arms is found, one of 5 is not, and an `#[expect]`
    /// keeps one.
    #[test]
    fn a_long_match_on_str_is_found() {
        let main = format!(
            "pub fn long(word: &str) -> u32 {{\n    match word {{\n{long}        _ => 0,\n    }}\n}}\n\
             pub fn short(word: &str) -> u32 {{\n    match word {{\n{short}        _ => 0,\n    }}\n}}\n\
             #[expect(blaze::fixed_key_lookup, reason = \"the order of the arms is the priority\")]\n\
             pub fn kept(word: &str) -> u32 {{\n    match word {{\n{long}        _ => 0,\n    }}\n}}\n",
            long = arms(20),
            short = arms(5),
        );
        let found = lint_main(&main);
        assert_eq!(found.matches(RULE).count(), 1, "{found}");
        assert!(
            found.contains("this `match` on `word` in function `main::long` compares 20 string literals one after another"),
            "{found}"
        );
        assert!(!found.contains("unfulfilled"), "the `#[expect]` is met:\n{found}");
    }
}
