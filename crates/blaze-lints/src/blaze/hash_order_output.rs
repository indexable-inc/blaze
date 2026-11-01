//! `hash_order_output`: a hash map's or set's iteration order reaching output (AGENTS.md rule
//! 20: no output depends on hash-map iteration order). The order of `iter`, `keys`, `values`,
//! `drain`, `into_iter` (and the `_mut` and `into_` forms) of a hash collection
//! ([`facts::hash_collection`]) reaches output when, with no sort between:
//!
//! - the chain from it is `collect`ed into a `Vec`, `VecDeque` or `String` that is returned,
//!   stored in a field or a place, or handed to a call, itself or through the local it is bound
//!   to (a local some `sort*` method is called on is sorted);
//! - a `for` loop over it pushes into a `Vec` or `String` that is not sorted after (`push`,
//!   `push_back`, `push_str`, `extend`), writes (`write!`, `write_str`, `write_all`) or hashes
//!   (`hash`, a `Hasher`'s `write_*`).
//!
//! A chain that ends anywhere else (`sum`, `count`, `max`, a `collect` into a `BTreeMap` or
//! another hash map) has no order to leak. Calls are resolved, so a renamed map type, an alias
//! (`FxHashMap`) and a macro's loop are found; the sinks of a loop are known by their resolved
//! method's name.

use base_db::Db;
use hir::HirDisplay as _;
use hir_def::{
    DefWithBodyId,
    expr_store::{Body, scope::ExprScopes},
    hir::{BindingId, Expr, ExprId, LoopSource, Pat, Statement},
    resolver::{ValueNs, resolver_for_scope},
    signatures::FunctionSignature,
};
use hir_ty::InferenceResult;

use super::{
    Finding, Lint,
    alloc::written_callee,
    facts::{self, Parents},
};
use crate::check::Cx;

/// The methods that iterate a hash collection in its table's order.
const ITERATE: &[&str] =
    &["iter", "iter_mut", "keys", "values", "values_mut", "into_iter", "into_keys", "into_values", "drain"];

/// How many methods a chain from an iteration is followed through to its `collect`.
const CHAIN: usize = 16;
/// How many expressions up a collected value is followed to where it goes.
const SINK_DEPTH: usize = 8;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::HashOrderOutput) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// Where an order goes.
enum Sink {
    Returned,
    Stored,
    Passed(String),
    Pushed(String),
    Written,
    Hashed,
}

impl Sink {
    fn say(&self) -> String {
        match self {
            Sink::Returned => "the function's return value".to_owned(),
            Sink::Stored => "a value stored in a field or place".to_owned(),
            Sink::Passed(name) => format!("the argument of `{name}`"),
            Sink::Pushed(name) => format!("the order of `{name}`, never sorted after"),
            Sink::Written => "what the loop writes".to_owned(),
            Sink::Hashed => "what the loop hashes".to_owned(),
        }
    }
}

/// The body linted and what its walk needs.
struct Walk<'a, 'db> {
    db: &'db Db,
    def: DefWithBodyId,
    body: &'db Body,
    infer: &'db InferenceResult<'db>,
    analysis: &'a hir::BodyAnalysis<'db>,
    parents: Parents,
    ordered: [Option<hir::Adt>; 3],
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let body = analysis.body();
    let adt = |path: &[&str]| match cx.std_def(path) {
        Some(hir::ModuleDef::Adt(it)) => Some(it),
        _ => None,
    };
    let walk = Walk {
        db: cx.db(),
        def,
        body,
        infer: analysis.infer(),
        analysis,
        parents: Parents::of(body),
        ordered: [adt(&["alloc", "vec", "Vec"]), adt(&["alloc", "collections", "VecDeque"]), adt(&["alloc", "string", "String"])],
    };
    let target = cx.krate().to_display_target(walk.db);
    for (id, expr) in body.exprs() {
        let (iterated, written_for) = match expr {
            Expr::MethodCall { receiver, .. } => (receiver, false),
            Expr::Call { args: [arg], .. } => (*arg, true),
            _ => continue,
        };
        let Some((f, _)) = written_callee(walk.infer, body, id) else { continue };
        let name = FunctionSignature::of(walk.db, f).name.as_str();
        if !ITERATE.contains(&name) {
            continue;
        }
        let ty = analysis.ty(walk.infer.expr_ty(iterated));
        if !facts::hash_collection(cx, &ty) {
            continue;
        }
        let sink = match written_for {
            true => walk.for_loop(id),
            false => walk.chain(id),
        };
        let Some(sink) = sink else { continue };
        let Some(range) = analysis.expr_range(id).or_else(|| analysis.expr_range(iterated)) else { continue };
        let message = format!(
            "`{}` is iterated in its hasher's order, and that order reaches {}",
            ty.strip_references().display(walk.db, target),
            sink.say()
        );
        let note = "a hash table's order depends on its hasher and its history, so the output differs between runs \
                    and builds: sort what comes out, or keep it in a `BTreeMap` or an ordered `Vec`";
        facts::push(&mut found, Lint::HashOrderOutput, range, message, Some(note.to_owned()));
    }
    found
}

impl<'db> Walk<'_, 'db> {
    fn method_name(&self, call: ExprId) -> Option<&'db str> {
        let (f, _) = written_callee(self.infer, self.body, call)?;
        Some(FunctionSignature::of(self.db, f).name.as_str())
    }

    /// Where the chain of methods from the iteration `id` takes its order, if to output.
    fn chain(&self, id: ExprId) -> Option<Sink> {
        let mut child = id;
        for _ in 0..CHAIN {
            let parent = self.parents.of_expr(child)?;
            match self.body.expr(parent) {
                Expr::MethodCall { receiver, .. } if receiver == child => {}
                // `for x in map.iter()`: the loop's own `IntoIterator::into_iter` of the chain.
                Expr::Call { args: [arg], .. } if *arg == child && self.method_name(parent) == Some("into_iter") => {
                    return self.for_loop(parent);
                }
                _ => return None,
            }
            let name = self.method_name(parent)?;
            if name.starts_with("sorted") {
                return None;
            }
            if name == "collect" {
                let ty = self.analysis.ty(self.infer.expr_ty(parent));
                let adt = ty.as_adt()?;
                if !self.ordered.contains(&Some(adt)) {
                    return None;
                }
                return self.sink_of(parent);
            }
            child = parent;
        }
        None
    }

    /// Where the value of `value` goes, if to output: itself, or through the local it is bound
    /// to when that local is never sorted.
    fn sink_of(&self, value: ExprId) -> Option<Sink> {
        let mut child = value;
        for _ in 0..SINK_DEPTH {
            if child == self.body.root_expr() {
                return Some(Sink::Returned);
            }
            let parent = self.parents.of_expr(child)?;
            match self.body.expr(parent) {
                Expr::Return { expr: Some(it) } if it == child => return Some(Sink::Returned),
                Expr::Block { tail: Some(it), .. } if it == child => child = parent,
                Expr::Block { statements, .. } => {
                    let local = statements.iter().find_map(|it| match it {
                        Statement::Let { pat, initializer: Some(init), .. } if init == child => match self.body.pat(pat) {
                            Pat::Bind { id, subpat: None } => Some(id),
                            _ => None,
                        },
                        _ => None,
                    })?;
                    return self.sink_of_local(local);
                }
                Expr::Ref { .. } => child = parent,
                Expr::RecordLit { fields, .. } if fields.iter().any(|it| it.expr == child) => return Some(Sink::Stored),
                Expr::Assignment { value, .. } if value == child => return Some(Sink::Stored),
                Expr::Call { args, .. } | Expr::MethodCall { args, .. } if args.contains(&child) => {
                    let name = self.method_name(parent)?;
                    return (name != "into_iter").then(|| Sink::Passed(name.to_owned()));
                }
                _ => return None,
            }
        }
        None
    }

    /// The path expressions of the body that name `local`.
    fn uses(&self, local: BindingId) -> Vec<ExprId> {
        // alloc: the uses of one collected local, a few
        let mut uses = Vec::new();
        for (id, expr) in self.body.exprs() {
            if matches!(expr, Expr::Path(_)) && local_of(self.db, self.def, self.body, id) == Some(local) {
                uses.push(id);
            }
        }
        uses
    }

    /// Whether some `sort*` method is called on `uses`' local.
    fn sorted(&self, uses: &[ExprId]) -> bool {
        uses.iter().any(|&it| {
            let Some(parent) = self.parents.of_expr(it) else { return false };
            matches!(self.body.expr(parent), Expr::MethodCall { receiver, .. } if receiver == it)
                && self.method_name(parent).is_some_and(|name| name.starts_with("sort"))
        })
    }

    fn sink_of_local(&self, local: BindingId) -> Option<Sink> {
        let uses = self.uses(local);
        if self.sorted(&uses) {
            return None;
        }
        uses.into_iter().find_map(|it| self.sink_of(it))
    }

    /// Where the `for` loop whose `IntoIterator::into_iter` call is `id` takes its order, if to
    /// output.
    fn for_loop(&self, id: ExprId) -> Option<Sink> {
        let parent = self.parents.of_expr(id)?;
        let Expr::Match { arms: [arm], .. } = self.body.expr(parent) else { return None };
        let Expr::Loop { body, source: LoopSource::ForLoop, .. } = self.body.expr(arm.expr) else { return None };
        // alloc: the expressions of one loop's body, walked once
        let mut stack = vec![body];
        while let Some(at) = stack.pop() {
            self.body.walk_child_exprs(at, |it| stack.push(it));
            let Expr::MethodCall { receiver, .. } = self.body.expr(at) else { continue };
            let Some(name) = self.method_name(at) else { continue };
            match name {
                "write_fmt" | "write_str" | "write_all" => return Some(Sink::Written),
                "hash" => return Some(Sink::Hashed),
                _ if name.starts_with("write_") => return Some(Sink::Hashed),
                "push" | "push_back" | "push_str" | "extend" => {
                    let ty = self.analysis.ty(self.infer.expr_ty(receiver)).strip_references();
                    if !ty.as_adt().is_some_and(|it| self.ordered.contains(&Some(it))) {
                        continue;
                    }
                    let Some(local) = local_of(self.db, self.def, self.body, receiver) else {
                        return Some(Sink::Stored);
                    };
                    if !self.sorted(&self.uses(local)) {
                        return Some(Sink::Pushed(self.body[local].name.as_str().to_owned()));
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// The local the path expression `expr` of `def`'s body names, if it names one.
fn local_of(db: &Db, def: DefWithBodyId, body: &Body, expr: ExprId) -> Option<BindingId> {
    let Expr::Path(path) = body.expr(expr) else { return None };
    let scope = ExprScopes::of(db, def).scope_for(expr);
    match resolver_for_scope(db, def, scope).resolve_path_in_value_ns_fully(db, path, body.expr_path_hygiene(expr)) {
        Some(ValueNs::LocalBinding(it)) => Some(it),
        _ => None,
    }
}
