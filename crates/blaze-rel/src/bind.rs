//! The planner: blaze-sql's query tree to the operators of [`crate::plan`], every name bound to
//! a column (query.md, "The pipeline").
//!
//! Names resolve innermost first: a query's CTEs, then the prelude's views, then the catalog's
//! tables. A view that is `select * from read_parquet(@t)` is the scan of `t` itself, so a
//! filter over it reaches the scan and its statistics; any other view and every CTE is planned
//! once and materialized once a query ([`Plan::Shared`]). Columns resolve without regard to
//! ASCII case, as DuckDB's do.

use core::alloc::Allocator;

use blaze_own::{Box, Vec};
use blaze_sql::ast::{self, CallArgs, ChainOp, CompareOp, ResultColumn, Span, UnaryOp};
use blaze_sql::query::{
    self as q, Analytic, Constraint, Core, FromItem, FrameUnits, GroupBy, Query, SetExpr, SetOp,
    Statement,
};

use crate::datum::{Datum, Store};
use crate::error::{ErrorKind, Fail, Run, fail};
use crate::exec::{Planned, Sub, SubKind};
use crate::plan::{
    Agg, AggFunc, Arith, Cmp, Expr, ExprKind, Frame, FrameBound, Func, JoinKind, Key, Loc, Plan,
    Prune, SetKind, SortKey, Source, WinFunc, Window,
};
use crate::table::{Bound, Catalog};
use crate::{Arg, Session};

/// A window's column before the aggregate's width is known: `WINDOW_BASE + i`, renumbered
/// once the level is planned.
const WINDOW_BASE: u32 = 1 << 30;

/// A column in scope.
#[derive(Clone, Copy, Debug)]
struct Col<'n> {
    qual: Option<&'n str>,
    name: &'n str,
    /// Kept from unqualified names and `*`: the right side's column of `using`.
    hidden: bool,
    /// A later column of one relation under a name it already has (`select t.*, u.job as job`
    /// where `t` has a `job`): DuckDB renames it `job_1`, so `job` names the first.
    shadowed: bool,
}

/// Marks each column of one relation whose name an earlier column of it has.
fn shadow(scope: &mut [Col<'_>]) {
    for at in 1..scope.len() {
        let name = scope.get(at).map_or("", |it| it.name);
        if scope.get(..at).is_some_and(|earlier| earlier.iter().any(|it| eq(it.name, name)))
            && let Some(col) = scope.get_mut(at)
        {
            col.shadowed = true;
        }
    }
}

type Scope<'n, A> = Vec<Col<'n>, A>;

/// What a name in `from` stands for.
#[derive(Clone, Copy, Debug)]
enum Target {
    Shared(u32),
    Work(u32),
    Scan { table: u32, width: u32 },
}

struct Named<'n, A: Allocator> {
    name: &'n str,
    target: Target,
    names: Vec<&'n str, A>,
}

/// The grouping of a level: its keys and the aggregates found so far.
struct Grouping<A: Allocator> {
    keys: Vec<Expr<A>, A>,
    /// Where each key's expression is written: a select item a key names by alias or position
    /// is that key, though a subquery in it binds anew each time.
    spans: Vec<Span, A>,
    aggs: Vec<Agg<A>, A>,
}

/// Where an expression binds: its columns, and what it may hold.
struct Level<'l, 'n, A: Allocator> {
    scope: &'l [Col<'n>],
    group: Option<&'l mut Grouping<A>>,
    windows: Option<&'l mut Vec<Window<A>, A>>,
}

impl<'n, A: Allocator> Level<'_, 'n, A> {
    fn plain<'l>(scope: &'l [Col<'n>]) -> Level<'l, 'n, A> {
        Level { scope, group: None, windows: None }
    }
}

struct Binder<'q, 's: 'n, 'n, 'c, 't: 'n, B: Allocator, A: Allocator + Copy> {
    catalog: &'c Catalog<'t, B, A>,
    views: &'q [Statement<'s, A>],
    view_text: &'s str,
    params: &'q [(&'s str, Arg<'s>)],
    store: &'c mut Store<A>,
    alloc: A,
    shared: Vec<Plan<'n, A>, A>,
    subs: Vec<Sub<'n, A>, A>,
    fixpoints: u32,
    joins: u32,
    /// Each view's target and names once bound; `binding` marks one being bound.
    bound_views: Vec<Option<(Target, Vec<&'n str, A>)>, A>,
    binding: Vec<bool, A>,
    ctes: Vec<Named<'n, A>, A>,
    work_reads: u32,
    source: Source,
    text: &'s str,
}

/// Plans `query` (of text `text`) against `tables` and `session`'s views and parameters.
pub(crate) fn plan<'q, 's: 'n, 'n, 't: 'n, B: Allocator, A: Allocator + Copy>(
    query: &'q Query<'s, A>,
    text: &'s str,
    session: &'q Session<'s, '_, A>,
    tables: &Catalog<'t, B, A>,
    store: &mut Store<A>,
    alloc: A,
) -> Run<'s, Planned<'n, A>, A> {
    let views = session.prelude.statements.as_slice();
    let mut bound_views = Vec::with_capacity_in(views.len(), alloc);
    let mut binding = Vec::with_capacity_in(views.len(), alloc);
    for _ in views {
        bound_views.push(None);
        binding.push(false);
    }
    let mut binder = Binder {
        catalog: tables,
        views,
        view_text: session.prelude.text,
        params: session.params,
        store,
        alloc,
        shared: Vec::new_in(alloc),
        subs: Vec::new_in(alloc),
        fixpoints: 0,
        joins: 0,
        bound_views,
        binding,
        ctes: Vec::new_in(alloc),
        work_reads: 0,
        source: Source::Query,
        text,
    };
    let (root, names) = binder.query(query)?;
    Ok(Planned {
        root,
        shared: binder.shared,
        subs: binder.subs,
        fixpoints: binder.fixpoints,
        joins: binder.joins,
        names,
    })
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The conjuncts of `e`: the operands of a top-level `and` chain, or `e` itself.
fn conjuncts<'q, 's, A: Allocator>(e: &'q ast::Expr<'s, A>, out: &mut Vec<&'q ast::Expr<'s, A>, A>) {
    if let ast::ExprKind::Chain { first, rest } = &e.kind
        && rest.iter().all(|link| link.op == ChainOp::And)
    {
        conjuncts(first, out);
        for link in rest.iter() {
            conjuncts(&link.operand, out);
        }
        return;
    }
    out.push(e);
}

/// Calls `f` on each child expression of `e`, not entering subqueries.
fn children<'q, 's, A: Allocator>(e: &'q ast::Expr<'s, A>, f: &mut dyn FnMut(&'q ast::Expr<'s, A>)) {
    use ast::ExprKind as K;
    match &e.kind {
        K::Null | K::Boolean(_) | K::Integer(_) | K::Real(_) | K::Text(_) | K::Blob(_) => {}
        K::Column { .. } | K::Param(_) => {}
        K::Unary { operand, .. } | K::IsNull { operand, .. } | K::Cast { operand, .. } => f(operand),
        K::InParam { operand, .. } => f(operand),
        K::Chain { first, rest } => {
            f(first);
            for link in rest.iter() {
                f(&link.operand);
            }
        }
        K::Compare { left, right, .. } => {
            f(left);
            f(right);
        }
        K::InList { operand, list, .. } => {
            f(operand);
            for it in list.iter() {
                f(it);
            }
        }
        K::Between { operand, low, high, .. } => {
            f(operand);
            f(low);
            f(high);
        }
        K::Like { operand, pattern, .. } => {
            f(operand);
            f(pattern);
        }
        K::Case { operand, arms, otherwise } => {
            if let Some(it) = operand {
                f(it);
            }
            for arm in arms.iter() {
                f(&arm.when);
                f(&arm.then);
            }
            if let Some(it) = otherwise {
                f(it);
            }
        }
        K::Call { args, .. } => {
            if let CallArgs::List { args, .. } = args {
                for it in args.iter() {
                    f(it);
                }
            }
        }
        K::Analytic(analytic) => match &**analytic {
            Analytic::Subquery(_) | Analytic::Exists(_) | Analytic::Files(_) => {}
            Analytic::InQuery { operand, .. } | Analytic::Convert { operand, .. } => f(operand),
            Analytic::Aggregate(agg) => {
                if let CallArgs::List { args, .. } = &agg.args {
                    for it in args.iter() {
                        f(it);
                    }
                }
                if let Some(it) = &agg.filter {
                    f(it);
                }
                for o in agg.order_by.iter() {
                    f(&o.expr);
                }
                if let Some(w) = &agg.over {
                    for it in w.partition_by.iter() {
                        f(it);
                    }
                    for o in w.order_by.iter() {
                        f(&o.expr);
                    }
                }
            }
        },
    }
}

/// Whether `e` holds an aggregate outside any window or subquery (or inside a window's
/// arguments, as `sum(count(*)) over ()`).
fn has_aggregate<A: Allocator>(e: &ast::Expr<'_, A>) -> bool {
    match &e.kind {
        ast::ExprKind::Call { function, .. } if AggFunc::named(function.text).is_some() => return true,
        ast::ExprKind::Analytic(analytic) => {
            if let Analytic::Aggregate(agg) = &**analytic
                && agg.over.is_none()
            {
                return true;
            }
        }
        _ => {}
    }
    let mut found = false;
    children(e, &mut |child| found = found || has_aggregate(child));
    found
}

/// Whether `e` holds a window function outside any subquery.
fn has_window<A: Allocator>(e: &ast::Expr<'_, A>) -> bool {
    if let ast::ExprKind::Analytic(analytic) = &e.kind
        && let Analytic::Aggregate(agg) = &**analytic
        && agg.over.is_some()
    {
        return true;
    }
    let mut found = false;
    children(e, &mut |child| found = found || has_window(child));
    found
}

fn same_datum(a: Datum, b: Datum) -> bool {
    match (a, b) {
        (Datum::Null, Datum::Null) => true,
        (Datum::Bool(x), Datum::Bool(y)) => x == y,
        (Datum::Int(x), Datum::Int(y)) => x == y,
        (Datum::UInt(x), Datum::UInt(y)) => x == y,
        (Datum::Float(x), Datum::Float(y)) => x.to_bits() == y.to_bits(),
        (Datum::Text(x), Datum::Text(y)) => x == y,
        _ => false,
    }
}

fn same_all<A: Allocator>(a: &[Expr<A>], b: &[Expr<A>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_expr(x, y))
}

/// Whether two bound expressions compute the same value: a `group by` key and a select item.
fn same_expr<A: Allocator>(a: &Expr<A>, b: &Expr<A>) -> bool {
    use ExprKind as K;
    match (&a.kind, &b.kind) {
        (K::Lit(x), K::Lit(y)) => same_datum(*x, *y),
        (K::Col(x), K::Col(y)) => x == y,
        (K::Neg(x), K::Neg(y)) | (K::Not(x), K::Not(y)) => same_expr(x, y),
        (K::And(x), K::And(y)) | (K::Or(x), K::Or(y)) => same_all(x, y),
        (K::Arith { op: o1, left: l1, right: r1 }, K::Arith { op: o2, left: l2, right: r2 }) => {
            o1 == o2 && same_expr(l1, l2) && same_expr(r1, r2)
        }
        (K::Concat { left: l1, right: r1 }, K::Concat { left: l2, right: r2 }) => {
            same_expr(l1, l2) && same_expr(r1, r2)
        }
        (K::Compare { op: o1, left: l1, right: r1 }, K::Compare { op: o2, left: l2, right: r2 }) => {
            o1 == o2 && same_expr(l1, l2) && same_expr(r1, r2)
        }
        (K::IsNull { operand: x, negated: n1 }, K::IsNull { operand: y, negated: n2 }) => {
            n1 == n2 && same_expr(x, y)
        }
        (K::Cast { operand: x, to: t1 }, K::Cast { operand: y, to: t2 }) => t1 == t2 && same_expr(x, y),
        (K::Call { func: f1, args: a1 }, K::Call { func: f2, args: a2 }) => f1 == f2 && same_all(a1, a2),
        (K::Case { operand: o1, arms: a1, otherwise: e1 }, K::Case { operand: o2, arms: a2, otherwise: e2 }) => {
            let opt = |x: &Option<Box<Expr<A>, A>>, y: &Option<Box<Expr<A>, A>>| match (x, y) {
                (None, None) => true,
                (Some(x), Some(y)) => same_expr(x, y),
                _ => false,
            };
            opt(o1, o2)
                && opt(e1, e2)
                && a1.len() == a2.len()
                && a1.iter().zip(a2.iter()).all(|((w1, t1), (w2, t2))| same_expr(w1, w2) && same_expr(t1, t2))
        }
        _ => false,
    }
}

/// Renumbers the window placeholders of `e` to follow a level of `base` columns.
fn renumber<A: Allocator>(e: &mut Expr<A>, base: u32) {
    use ExprKind as K;
    match &mut e.kind {
        K::Col(c) if *c >= WINDOW_BASE => *c = *c - WINDOW_BASE + base,
        K::Lit(_) | K::Col(_) | K::Scalar(_) => {}
        K::Neg(x) | K::Not(x) => renumber(x, base),
        K::And(xs) | K::Or(xs) | K::Call { args: xs, .. } => {
            for x in xs.iter_mut() {
                renumber(x, base);
            }
        }
        K::Arith { left, right, .. } | K::Concat { left, right } | K::Compare { left, right, .. } => {
            renumber(left, base);
            renumber(right, base);
        }
        K::Distinct { left, right, .. } => {
            renumber(left, base);
            renumber(right, base);
        }
        K::IsNull { operand, .. } | K::Cast { operand, .. } | K::InSet { operand, .. } => renumber(operand, base),
        K::InList { operand, list, .. } => {
            renumber(operand, base);
            for x in list.iter_mut() {
                renumber(x, base);
            }
        }
        K::Between { operand, low, high, .. } => {
            renumber(operand, base);
            renumber(low, base);
            renumber(high, base);
        }
        K::Like { operand, pattern, .. } => {
            renumber(operand, base);
            renumber(pattern, base);
        }
        K::Case { operand, arms, otherwise } => {
            if let Some(x) = operand {
                renumber(x, base);
            }
            for (w, t) in arms.iter_mut() {
                renumber(w, base);
                renumber(t, base);
            }
            if let Some(x) = otherwise {
                renumber(x, base);
            }
        }
        K::Exists { keys, .. } => {
            for x in keys.iter_mut() {
                renumber(x, base);
            }
        }
    }
}

/// The text of a `'..'` literal with its doubled quotes made single.
fn unquote<A: Allocator + Copy>(raw: &str, alloc: A) -> blaze_own::String<A> {
    let mut out = blaze_own::String::with_capacity_in(raw.len(), alloc);
    let mut rest = raw;
    while let Some(at) = rest.find("''") {
        out.push_str(rest.get(..at + 1).unwrap_or(""));
        rest = rest.get(at + 2..).unwrap_or("");
    }
    out.push_str(rest);
    out
}

impl<'q, 's: 'n, 'n, 'c, 't: 'n, B: Allocator, A: Allocator + Copy> Binder<'q, 's, 'n, 'c, 't, B, A> {
    fn loc(&self, span: Span) -> Loc {
        Loc { source: self.source, start: span.start, end: span.end }
    }

    fn expr(&self, kind: ExprKind<A>, span: Span) -> Expr<A> {
        Expr { kind, at: self.loc(span) }
    }

    fn boxed(&self, e: Expr<A>) -> Box<Expr<A>, A> {
        Box::new_in(e, self.alloc)
    }

    fn slice(&self, span: Span) -> &'s str {
        self.text.get(span.range()).unwrap_or("?").trim()
    }

    fn intern(&mut self, text: &str) -> Run<'s, Datum, A> {
        match self.store.intern(text) {
            Ok(sym) => Ok(Datum::Text(sym)),
            Err(full) => Err(Fail::nowhere(ErrorKind::Bound { what: full.what, most: full.most })),
        }
    }

    // ---- queries ----

    fn query(&mut self, query: &'q Query<'s, A>) -> Run<'s, (Plan<'n, A>, Vec<&'n str, A>), A> {
        let depth = self.ctes.len();
        let out = self.query_in(query);
        self.ctes.truncate(depth);
        out
    }

    fn query_in(&mut self, query: &'q Query<'s, A>) -> Run<'s, (Plan<'n, A>, Vec<&'n str, A>), A> {
        if let Some(with) = &query.with {
            for cte in with.ctes.iter() {
                self.cte(cte, with.recursive)?;
            }
        }
        let (mut plan, names) = match &query.body {
            SetExpr::Core(core) => self.core(core, &query.order_by)?,
            body => {
                let (plan, names) = self.set_expr(body)?;
                if query.order_by.is_empty() {
                    (plan, names)
                } else {
                    let mut keys = Vec::new_in(self.alloc);
                    for order in query.order_by.iter() {
                        let Some(at) = self.output_index(&order.expr, &names) else {
                            return fail(
                                self.loc(order.expr.span),
                                ErrorKind::Unsupported("an `order by` over a set operation that is not an output column"),
                            );
                        };
                        keys.push(SortKey {
                            expr: self.expr(ExprKind::Col(at), order.expr.span),
                            descending: order.descending,
                            nulls_first: order.nulls_first.unwrap_or(false),
                        });
                    }
                    (Plan::Sort { input: Box::new_in(plan, self.alloc), keys }, names)
                }
            }
        };
        if query.limit.is_some() || query.offset.is_some() {
            let limit = match &query.limit {
                Some(e) => Some(self.constant_u64(e)?),
                None => None,
            };
            let offset = match &query.offset {
                Some(e) => self.constant_u64(e)?,
                None => 0,
            };
            plan = Plan::Limit { input: Box::new_in(plan, self.alloc), limit, offset };
        }
        Ok((plan, names))
    }

    /// Output column `e` names: an alias or column by name, or a 1-based position.
    fn output_index(&self, e: &ast::Expr<'s, A>, names: &[&'n str]) -> Option<u32> {
        match &e.kind {
            ast::ExprKind::Column { table: None, column } => {
                names.iter().position(|it| eq(it, column.text)).and_then(|it| u32::try_from(it).ok())
            }
            ast::ExprKind::Integer(n) => {
                let n = usize::try_from(*n).ok()?;
                (n >= 1 && n <= names.len()).then(|| u32::try_from(n - 1).ok()).flatten()
            }
            _ => None,
        }
    }

    fn constant(&mut self, e: &'q ast::Expr<'s, A>) -> Run<'s, Datum, A> {
        let scope: [Col<'n>; 0] = [];
        let bound = self.bind(e, &mut Level::plain(&scope))?;
        match bound.kind {
            ExprKind::Lit(d) => Ok(d),
            ExprKind::Neg(inner) => match inner.kind {
                ExprKind::Lit(Datum::Int(n)) => Ok(Datum::Int(-n)),
                ExprKind::Lit(Datum::Float(f)) => Ok(Datum::Float(-f)),
                _ => fail(self.loc(e.span), ErrorKind::Unsupported("a computed constant")),
            },
            _ => fail(self.loc(e.span), ErrorKind::Unsupported("a computed constant")),
        }
    }

    fn constant_u64(&mut self, e: &'q ast::Expr<'s, A>) -> Run<'s, u64, A> {
        let d = self.constant(e)?;
        match d.as_i128().and_then(|it| u64::try_from(it).ok()) {
            Some(n) => Ok(n),
            None => fail(self.loc(e.span), ErrorKind::Type { op: "limit", left: d.kind(), right: crate::datum::Kind::Int }),
        }
    }

    fn cte(&mut self, cte: &'q q::Cte<'s, A>, recursive: bool) -> Run<'s, (), A> {
        let rename = |names: Vec<&'n str, A>, cols: &'q [ast::Name<'s>], alloc: A| -> Result<Vec<&'n str, A>, (usize, usize)> {
            if cols.is_empty() {
                return Ok(names);
            }
            if cols.len() != names.len() {
                return Err((cols.len(), names.len()));
            }
            let mut out = Vec::with_capacity_in(cols.len(), alloc);
            for c in cols {
                out.push(c.text);
            }
            Ok(out)
        };
        let width = |(left, right)| Fail { at: None, kind: ErrorKind::Width { left, right } };
        if recursive
            && let SetExpr::SetOp { op: SetOp::Union, all, by_name: false, left, right } = &cte.query.body
            && cte.query.order_by.is_empty()
            && cte.query.limit.is_none()
            && cte.query.with.is_none()
        {
            let (base, names) = self.set_expr(left)?;
            let names = rename(names, &cte.columns, self.alloc).map_err(width)?;
            let id = self.fixpoints;
            self.fixpoints += 1;
            self.ctes.push(Named { name: cte.name.text, target: Target::Work(id), names: names.clone() });
            let before = self.work_reads;
            let step = self.set_expr(right);
            self.ctes.pop();
            let (step, step_names) = step?;
            if step_names.len() != names.len() {
                return Err(width((names.len(), step_names.len())));
            }
            let plan = if self.work_reads > before {
                Plan::Fixpoint {
                    id,
                    name: cte.name.text,
                    distinct: !*all,
                    base: Box::new_in(base, self.alloc),
                    step: Box::new_in(step, self.alloc),
                    at: self.loc(cte.query.span),
                }
            } else {
                Plan::Set {
                    kind: SetKind::Union,
                    all: *all,
                    left: Box::new_in(base, self.alloc),
                    right: Box::new_in(step, self.alloc),
                }
            };
            let sid = self.share(plan);
            self.ctes.push(Named { name: cte.name.text, target: Target::Shared(sid), names });
            return Ok(());
        }
        let (plan, names) = self.query(&cte.query)?;
        let names = rename(names, &cte.columns, self.alloc).map_err(width)?;
        let sid = self.share(plan);
        self.ctes.push(Named { name: cte.name.text, target: Target::Shared(sid), names });
        Ok(())
    }

    fn share(&mut self, plan: Plan<'n, A>) -> u32 {
        let id = u32::try_from(self.shared.len()).unwrap_or(u32::MAX);
        self.shared.push(plan);
        id
    }

    fn set_expr(&mut self, body: &'q SetExpr<'s, A>) -> Run<'s, (Plan<'n, A>, Vec<&'n str, A>), A> {
        match body {
            SetExpr::Core(core) => self.core(core, &[]),
            SetExpr::Nested(query) => self.query(query),
            SetExpr::SetOp { op, all, by_name, left, right } => {
                let (lplan, lnames) = self.set_expr(left)?;
                let (rplan, rnames) = self.set_expr(right)?;
                let kind = match op {
                    SetOp::Union => SetKind::Union,
                    SetOp::Except => SetKind::Except,
                    SetOp::Intersect => SetKind::Intersect,
                };
                if *by_name {
                    // The left's columns, then the right's it lacks; each side null where it
                    // has no such column.
                    let mut names = lnames.clone();
                    for name in rnames.iter() {
                        if !names.iter().any(|it| eq(it, name)) {
                            names.push(name);
                        }
                    }
                    let lplan = self.align(lplan, &lnames, &names);
                    let rplan = self.align(rplan, &rnames, &names);
                    let plan = Plan::Set { kind, all: *all, left: Box::new_in(lplan, self.alloc), right: Box::new_in(rplan, self.alloc) };
                    return Ok((plan, names));
                }
                if lnames.len() != rnames.len() {
                    return Err(Fail::nowhere(ErrorKind::Width { left: lnames.len(), right: rnames.len() }));
                }
                let plan = Plan::Set { kind, all: *all, left: Box::new_in(lplan, self.alloc), right: Box::new_in(rplan, self.alloc) };
                Ok((plan, lnames))
            }
        }
    }

    /// `plan`'s columns `have` projected to `want`, null where it has none.
    fn align(&mut self, plan: Plan<'n, A>, have: &[&'n str], want: &[&'n str]) -> Plan<'n, A> {
        let mut exprs = Vec::with_capacity_in(want.len(), self.alloc);
        let nowhere = Loc { source: self.source, start: 0, end: 0 };
        for name in want {
            let kind = match have.iter().position(|it| eq(it, name)) {
                Some(at) => ExprKind::Col(u32::try_from(at).unwrap_or(u32::MAX)),
                None => ExprKind::Lit(Datum::Null),
            };
            exprs.push(Expr { kind, at: nowhere });
        }
        Plan::Project { input: Box::new_in(plan, self.alloc), exprs }
    }

    // ---- from ----

    fn resolve(&mut self, name: &'q ast::Name<'s>) -> Run<'s, (Target, Vec<&'n str, A>), A> {
        if let Some(named) = self.ctes.iter().rev().find(|it| eq(it.name, name.text)) {
            let out = (named.target, named.names.clone());
            if let Target::Work(_) = named.target {
                self.work_reads += 1;
            }
            return Ok(out);
        }
        let view = self.views.iter().enumerate().rev().find_map(|(at, statement)| match statement {
            Statement::CreateView { name: view, query, .. } if eq(view.text, name.text) => Some((at, query)),
            _ => None,
        });
        if let Some((at, query)) = view {
            return self.view(at, query, name);
        }
        if let Some((at, table)) = self.catalog.find(name.text) {
            return Ok(self.scan_target(at, table.fields()));
        }
        fail(self.loc(name.span), ErrorKind::UnknownTable(name.text))
    }

    fn scan_target(&self, at: usize, fields: &'t [blaze_columns::Field<B>]) -> (Target, Vec<&'n str, A>) {
        let mut names = Vec::with_capacity_in(fields.len(), self.alloc);
        for field in fields {
            names.push(field.name());
        }
        let table = u32::try_from(at).unwrap_or(u32::MAX);
        (Target::Scan { table, width: u32::try_from(fields.len()).unwrap_or(u32::MAX) }, names)
    }

    /// The table a `read_parquet(@t, ..)` call reads.
    fn files_table(&self, args: &'q [ast::Expr<'s, A>]) -> Option<&'s str> {
        let first = args.first()?;
        if let ast::ExprKind::Analytic(analytic) = &first.kind
            && let Analytic::Files(name) = &**analytic
        {
            return Some(name.text);
        }
        None
    }

    fn view(&mut self, at: usize, query: &'q Query<'s, A>, name: &'q ast::Name<'s>) -> Run<'s, (Target, Vec<&'n str, A>), A> {
        if let Some(Some((target, names))) = self.bound_views.get(at) {
            return Ok((*target, names.clone()));
        }
        if self.binding.get(at).copied().unwrap_or(false) {
            return fail(self.loc(name.span), ErrorKind::Unsupported("a view that refers to itself"));
        }
        // `select * from read_parquet(@t, ..)` is the table itself.
        if let SetExpr::Core(core) = &query.body
            && query.with.is_none()
            && query.order_by.is_empty()
            && query.limit.is_none()
            && !core.distinct
            && core.filter.is_none()
            && matches!(core.group_by, GroupBy::None)
            && core.qualify.is_none()
            && core.having.is_none()
            && matches!(core.columns.as_slice(), [ResultColumn::Star { .. }])
            && let [FromItem::Function { function, args, .. }] = core.from.as_slice()
            && eq(function.text, "read_parquet")
            && let Some(table) = self.files_table(args)
            && let Some((index, found)) = self.catalog.find(table)
        {
            let out = self.scan_target(index, found.fields());
            if let Some(slot) = self.bound_views.get_mut(at) {
                *slot = Some((out.0, out.1.clone()));
            }
            return Ok(out);
        }
        if let Some(flag) = self.binding.get_mut(at) {
            *flag = true;
        }
        // A view sees no CTE of the query, and its positions are the prelude's.
        let ctes = core::mem::replace(&mut self.ctes, Vec::new_in(self.alloc));
        let (source, text) = (self.source, self.text);
        self.source = Source::View(u32::try_from(at).unwrap_or(u32::MAX));
        self.text = self.view_text;
        let bound = self.query(query);
        self.source = source;
        self.text = text;
        self.ctes = ctes;
        if let Some(flag) = self.binding.get_mut(at) {
            *flag = false;
        }
        let (plan, names) = bound?;
        let target = Target::Shared(self.share(plan));
        if let Some(slot) = self.bound_views.get_mut(at) {
            *slot = Some((target, names.clone()));
        }
        Ok((target, names))
    }

    fn named_scope(&self, names: &[&'n str], qual: &'n str) -> Scope<'n, A> {
        let mut scope = Vec::with_capacity_in(names.len(), self.alloc);
        for name in names {
            scope.push(Col { qual: Some(qual), name, hidden: false, shadowed: false });
        }
        shadow(&mut scope);
        scope
    }

    fn from_item(&mut self, item: &'q FromItem<'s, A>) -> Run<'s, (Plan<'n, A>, Scope<'n, A>), A> {
        match item {
            FromItem::Table { name, alias } => {
                let (target, names) = self.resolve(name)?;
                let qual = alias.as_ref().map_or(name.text, |it| it.text);
                Ok((self.plan_of(target), self.named_scope(&names, qual)))
            }
            FromItem::Function { function, args, alias, span } => {
                if !eq(function.text, "read_parquet") {
                    return fail(self.loc(*span), ErrorKind::Unsupported("a table function but `read_parquet(@table)`"));
                }
                let Some(table) = self.files_table(args) else {
                    return fail(self.loc(*span), ErrorKind::Unsupported("`read_parquet` of anything but `@table`"));
                };
                let Some((at, found)) = self.catalog.find(table) else {
                    return fail(self.loc(*span), ErrorKind::UnknownTable(table));
                };
                let (target, names) = self.scan_target(at, found.fields());
                let qual = alias.as_ref().map_or(table, |it| it.text);
                Ok((self.plan_of(target), self.named_scope(&names, qual)))
            }
            FromItem::Subquery { query, alias } => {
                let (plan, names) = self.query(query)?;
                let mut scope = Vec::with_capacity_in(names.len(), self.alloc);
                for name in names.iter() {
                    scope.push(Col { qual: alias.as_ref().map(|it| it.text), name, hidden: false, shadowed: false });
                }
                shadow(&mut scope);
                Ok((plan, scope))
            }
            FromItem::Join(join) => self.join(join),
        }
    }

    fn plan_of(&self, target: Target) -> Plan<'n, A> {
        match target {
            Target::Shared(id) => Plan::Shared { id },
            Target::Work(id) => Plan::Work { id },
            Target::Scan { table, width } => Plan::Scan { table, width, filter: None, prune: Vec::new_in(self.alloc) },
        }
    }

    fn join_id(&mut self) -> u32 {
        let id = self.joins;
        self.joins += 1;
        id
    }

    /// Splits `on`'s conjuncts into equality keys across the two sides and the rest.
    #[expect(clippy::type_complexity, reason = "the keys, the rest, and the asof inequality")]
    fn split_on(
        &mut self,
        on: Option<&'q ast::Expr<'s, A>>,
        extra: &mut Vec<&'q ast::Expr<'s, A>, A>,
        left: &[Col<'n>],
        right: &[Col<'n>],
        asof: bool,
    ) -> Run<'s, (Vec<Key<A>, A>, Vec<&'q ast::Expr<'s, A>, A>, Option<(Cmp, Expr<A>, Expr<A>)>), A> {
        let mut parts = Vec::new_in(self.alloc);
        if let Some(on) = on {
            conjuncts(on, &mut parts);
        }
        parts.append(extra);
        let mut keys = Vec::new_in(self.alloc);
        let mut rest = Vec::new_in(self.alloc);
        let mut inequality = None;
        for part in parts.iter() {
            if let ast::ExprKind::Compare { op, left: l, right: r } = &part.kind {
                let cmp = match op {
                    CompareOp::Eq | CompareOp::Is => Some(Cmp::Eq),
                    CompareOp::Lt => Some(Cmp::Lt),
                    CompareOp::Le => Some(Cmp::Le),
                    CompareOp::Gt => Some(Cmp::Gt),
                    CompareOp::Ge => Some(Cmp::Ge),
                    CompareOp::Ne | CompareOp::IsNot => None,
                };
                if let Some(cmp) = cmp
                    && (cmp == Cmp::Eq || (asof && inequality.is_none()))
                {
                    let pair = match (self.try_bind(l, left), self.try_bind(r, right)) {
                        (Some(x), Some(y)) => Some((x, y, cmp)),
                        _ => match (self.try_bind(r, left), self.try_bind(l, right)) {
                            (Some(x), Some(y)) => Some((x, y, flip(cmp))),
                            _ => None,
                        },
                    };
                    if let Some((x, y, cmp)) = pair {
                        if cmp == Cmp::Eq {
                            keys.push(Key { left: x, right: y, null_equal: *op == CompareOp::Is });
                        } else {
                            inequality = Some((cmp, x, y));
                        }
                        continue;
                    }
                }
            }
            rest.push(*part);
        }
        Ok((keys, rest, inequality))
    }

    /// `e` bound against `scope` alone, if it binds there.
    fn try_bind(&mut self, e: &'q ast::Expr<'s, A>, scope: &[Col<'n>]) -> Option<Expr<A>> {
        if has_aggregate(e) || has_window(e) {
            return None;
        }
        self.bind(e, &mut Level::plain(scope)).ok()
    }

    fn concat(&self, left: &[Col<'n>], right: &[Col<'n>]) -> Scope<'n, A> {
        let mut scope = Vec::with_capacity_in(left.len() + right.len(), self.alloc);
        scope.extend_from_slice(left);
        scope.extend_from_slice(right);
        scope
    }

    fn join(&mut self, join: &'q q::Join<'s, A>) -> Run<'s, (Plan<'n, A>, Scope<'n, A>), A> {
        let (lplan, lscope) = self.from_item(&join.left)?;
        let (rplan, mut rscope) = self.from_item(&join.right)?;
        let at = self.loc(join.span);
        let mut keys = Vec::new_in(self.alloc);
        let mut residual_parts = Vec::new_in(self.alloc);
        let mut inequality = None;
        let asof = matches!(join.kind, q::JoinKind::Asof | q::JoinKind::AsofLeft);
        match &join.constraint {
            Constraint::None => {}
            Constraint::On(on) => {
                let mut none = Vec::new_in(self.alloc);
                let (k, rest, ineq) = self.split_on(Some(on), &mut none, &lscope, &rscope, asof)?;
                keys = k;
                residual_parts = rest;
                inequality = ineq;
            }
            Constraint::Using(names) => {
                for name in names.iter() {
                    let l = self.column(&lscope, None, name)?;
                    let r = self.column(&rscope, None, name)?;
                    if let Some(col) = rscope.get_mut(r as usize) {
                        col.hidden = true;
                    }
                    keys.push(Key {
                        left: self.expr(ExprKind::Col(l), name.span),
                        right: self.expr(ExprKind::Col(r), name.span),
                        null_equal: false,
                    });
                }
            }
        }
        let scope = self.concat(&lscope, &rscope);
        let residual = if residual_parts.is_empty() {
            None
        } else {
            Some(self.and_of(&residual_parts, &scope)?)
        };
        let id = self.join_id();
        let plan = match join.kind {
            q::JoinKind::Asof | q::JoinKind::AsofLeft => {
                let Some((op, probe, build)) = inequality else {
                    return fail(at, ErrorKind::Unsupported("an asof join without one inequality across its sides"));
                };
                if residual.is_some() {
                    return fail(at, ErrorKind::Unsupported("an asof join condition beyond equalities and one inequality"));
                }
                Plan::Asof {
                    outer: join.kind == q::JoinKind::AsofLeft,
                    left: Box::new_in(lplan, self.alloc),
                    right: Box::new_in(rplan, self.alloc),
                    keys,
                    op,
                    probe,
                    build,
                    id,
                    at,
                }
            }
            q::JoinKind::Right => {
                // The right join is the left join of the sides swapped, its columns put back.
                let lw = lscope.len();
                let rw = rscope.len();
                let mut swapped = Vec::with_capacity_in(keys.len(), self.alloc);
                for key in keys {
                    swapped.push(Key { left: key.right, right: key.left, null_equal: key.null_equal });
                }
                let residual = match residual {
                    Some(mut e) => {
                        swap_sides(&mut e, lw, rw);
                        Some(e)
                    }
                    None => None,
                };
                let joined = Plan::Join {
                    kind: JoinKind::Left,
                    left: Box::new_in(rplan, self.alloc),
                    right: Box::new_in(lplan, self.alloc),
                    keys: swapped,
                    residual,
                    id,
                    at,
                };
                let mut exprs = Vec::with_capacity_in(lw + rw, self.alloc);
                for c in 0..lw {
                    exprs.push(Expr { kind: ExprKind::Col(u32::try_from(rw + c).unwrap_or(u32::MAX)), at });
                }
                for c in 0..rw {
                    exprs.push(Expr { kind: ExprKind::Col(u32::try_from(c).unwrap_or(u32::MAX)), at });
                }
                Plan::Project { input: Box::new_in(joined, self.alloc), exprs }
            }
            kind => {
                let kind = match kind {
                    q::JoinKind::Left => JoinKind::Left,
                    q::JoinKind::Full => JoinKind::Full,
                    _ => JoinKind::Inner,
                };
                // Not an asof join: no inequality was split off (`split_on` with `asof` false).
                let residual = match inequality {
                    Some((op, l, r)) => {
                        let lw = u32::try_from(lscope.len()).unwrap_or(u32::MAX);
                        let mut r = r;
                        shift(&mut r, lw);
                        let cmp = Expr { kind: ExprKind::Compare { op, left: self.boxed(l), right: self.boxed(r) }, at };
                        Some(match residual {
                            Some(e) => {
                                let mut items = Vec::with_capacity_in(2, self.alloc);
                                items.push(cmp);
                                items.push(e);
                                Expr { kind: ExprKind::And(items), at }
                            }
                            None => cmp,
                        })
                    }
                    None => residual,
                };
                Plan::Join {
                    kind,
                    left: Box::new_in(lplan, self.alloc),
                    right: Box::new_in(rplan, self.alloc),
                    keys,
                    residual,
                    id,
                    at,
                }
            }
        };
        Ok((plan, scope))
    }

    fn and_of(&mut self, parts: &[&'q ast::Expr<'s, A>], scope: &[Col<'n>]) -> Run<'s, Expr<A>, A> {
        let mut items = Vec::with_capacity_in(parts.len(), self.alloc);
        for part in parts {
            items.push(self.bind(part, &mut Level::plain(scope))?);
        }
        if items.len() == 1
            && let Some(only) = items.pop()
        {
            return Ok(only);
        }
        let at = items.first().map_or(Loc { source: self.source, start: 0, end: 0 }, |it| it.at);
        Ok(Expr { kind: ExprKind::And(items), at })
    }

    /// The `from` list, comma-joined left to right: equalities of `filter` across a comma
    /// become the join's keys and leave `filter`.
    fn from_list(
        &mut self,
        items: &'q [FromItem<'s, A>],
        filter: &mut Vec<&'q ast::Expr<'s, A>, A>,
    ) -> Run<'s, (Plan<'n, A>, Scope<'n, A>), A> {
        let Some((first, rest)) = items.split_first() else {
            return Ok((Plan::Unit, Vec::new_in(self.alloc)));
        };
        let (mut plan, mut scope) = self.from_item(first)?;
        for item in rest {
            let (rplan, rscope) = self.from_item(item)?;
            let mut keys = Vec::new_in(self.alloc);
            let mut kept = Vec::new_in(self.alloc);
            for part in filter.iter() {
                if let ast::ExprKind::Compare { op: op @ (CompareOp::Eq | CompareOp::Is), left: l, right: r } = &part.kind {
                    let pair = match (self.try_bind(l, &scope), self.try_bind(r, &rscope)) {
                        (Some(x), Some(y)) => Some((x, y)),
                        _ => match (self.try_bind(r, &scope), self.try_bind(l, &rscope)) {
                            (Some(x), Some(y)) => Some((x, y)),
                            _ => None,
                        },
                    };
                    if let Some((x, y)) = pair {
                        keys.push(Key { left: x, right: y, null_equal: *op == CompareOp::Is });
                        continue;
                    }
                }
                kept.push(*part);
            }
            *filter = kept;
            let id = self.join_id();
            let at = Loc { source: self.source, start: 0, end: 0 };
            plan = Plan::Join {
                kind: JoinKind::Inner,
                left: Box::new_in(plan, self.alloc),
                right: Box::new_in(rplan, self.alloc),
                keys,
                residual: None,
                id,
                at,
            };
            scope = self.concat(&scope, &rscope);
        }
        Ok((plan, scope))
    }

    // ---- select ----

    #[expect(clippy::too_many_lines, reason = "the clauses of one select, in their order")]
    fn core(&mut self, core: &'q Core<'s, A>, order_by: &'q [q::Order<'s, A>]) -> Run<'s, (Plan<'n, A>, Vec<&'n str, A>), A> {
        let mut filter = Vec::new_in(self.alloc);
        if let Some(e) = &core.filter {
            conjuncts(e, &mut filter);
        }
        let (mut plan, scope) = self.from_list(&core.from, &mut filter)?;
        if !filter.is_empty() {
            let predicate = self.and_of(&filter, &scope)?;
            plan = self.filtered(plan, predicate);
        }
        let grouped = !matches!(core.group_by, GroupBy::None)
            || core.columns.iter().any(|c| matches!(c, ResultColumn::Expr { expr, .. } if has_aggregate(expr)))
            || core.having.as_ref().is_some_and(|it| has_aggregate(it))
            || core.qualify.as_ref().is_some_and(|it| has_aggregate(it))
            || order_by.iter().any(|o| has_aggregate(&o.expr));
        let mut grouping =
            Grouping { keys: Vec::new_in(self.alloc), spans: Vec::new_in(self.alloc), aggs: Vec::new_in(self.alloc) };
        if grouped {
            match &core.group_by {
                GroupBy::None => {}
                GroupBy::Exprs(exprs) => {
                    for e in exprs.iter() {
                        let target = self.group_target(e, core, &scope);
                        let key = self.bind(target, &mut Level::plain(&scope))?;
                        grouping.keys.push(key);
                        grouping.spans.push(target.span);
                    }
                }
                GroupBy::All(_) => {
                    for c in core.columns.iter() {
                        if let ResultColumn::Expr { expr, .. } = c
                            && !has_aggregate(expr)
                            && !has_window(expr)
                        {
                            let key = self.bind(expr, &mut Level::plain(&scope))?;
                            grouping.keys.push(key);
                            grouping.spans.push(expr.span);
                        }
                    }
                }
            }
        }
        let mut windows = Vec::new_in(self.alloc);
        let mut exprs = Vec::new_in(self.alloc);
        let mut names: Vec<&'n str, A> = Vec::new_in(self.alloc);
        for column in core.columns.iter() {
            match column {
                ResultColumn::Star { span } | ResultColumn::TableStar { table: ast::Name { span, .. } } => {
                    let only = match column {
                        ResultColumn::TableStar { table } => Some(table.text),
                        _ => None,
                    };
                    let mut any = false;
                    for (at, col) in scope.iter().enumerate() {
                        let wanted = match only {
                            Some(t) => col.qual.is_some_and(|q| eq(q, t)),
                            None => !col.hidden,
                        };
                        if !wanted {
                            continue;
                        }
                        any = true;
                        let mut e = self.expr(ExprKind::Col(u32::try_from(at).unwrap_or(u32::MAX)), *span);
                        if grouped {
                            match grouping.keys.iter().position(|k| same_expr(k, &e)) {
                                Some(k) => e = self.expr(ExprKind::Col(u32::try_from(k).unwrap_or(u32::MAX)), *span),
                                None => return fail(self.loc(*span), ErrorKind::NotGrouped(self.slice(*span))),
                            }
                        }
                        exprs.push(e);
                        names.push(col.name);
                    }
                    if !any && let Some(t) = only {
                        return fail(self.loc(*span), ErrorKind::UnknownTable(t));
                    }
                }
                ResultColumn::Expr { expr, alias } => {
                    let mut level = Level {
                        scope: &scope,
                        group: grouped.then_some(&mut grouping),
                        windows: Some(&mut windows),
                    };
                    let e = self.bind(expr, &mut level)?;
                    exprs.push(e);
                    let name: &'n str = match (alias, &expr.kind) {
                        (Some(alias), _) => alias.text,
                        (None, ast::ExprKind::Column { column, .. }) => column.text,
                        (None, _) => self.slice(expr.span),
                    };
                    names.push(name);
                }
            }
        }
        let having = match &core.having {
            Some(e) => {
                let mut level = Level { scope: &scope, group: grouped.then_some(&mut grouping), windows: None };
                Some(self.bind(e, &mut level)?)
            }
            None => None,
        };
        let mut qualify = match &core.qualify {
            Some(e) => {
                let mut level = Level { scope: &scope, group: grouped.then_some(&mut grouping), windows: Some(&mut windows) };
                Some(self.bind(e, &mut level)?)
            }
            None => None,
        };
        // `order by`: an output column by alias or position, else an expression of the level,
        // carried as a hidden column.
        let visible = exprs.len();
        let mut sort = Vec::new_in(self.alloc);
        for order in order_by {
            let at = match self.output_index(&order.expr, &names) {
                Some(at) => at,
                None => {
                    let mut level = Level { scope: &scope, group: grouped.then_some(&mut grouping), windows: Some(&mut windows) };
                    let e = self.bind(&order.expr, &mut level)?;
                    let at = u32::try_from(exprs.len()).unwrap_or(u32::MAX);
                    exprs.push(e);
                    at
                }
            };
            sort.push(SortKey {
                expr: self.expr(ExprKind::Col(at), order.expr.span),
                descending: order.descending,
                nulls_first: order.nulls_first.unwrap_or(false),
            });
        }
        let base = if grouped {
            u32::try_from(grouping.keys.len() + grouping.aggs.len()).unwrap_or(u32::MAX)
        } else {
            u32::try_from(scope.len()).unwrap_or(u32::MAX)
        };
        if grouped {
            plan = Plan::Aggregate { input: Box::new_in(plan, self.alloc), keys: grouping.keys, aggs: grouping.aggs };
            if let Some(h) = having {
                plan = Plan::Filter { input: Box::new_in(plan, self.alloc), predicate: h };
            }
        } else if let Some(h) = having {
            plan = Plan::Filter { input: Box::new_in(plan, self.alloc), predicate: h };
        }
        if !windows.is_empty() {
            for w in windows.iter_mut() {
                renumber_window(w, base);
            }
            plan = Plan::Window { input: Box::new_in(plan, self.alloc), windows };
        }
        if let Some(mut q) = qualify.take() {
            renumber(&mut q, base);
            plan = Plan::Filter { input: Box::new_in(plan, self.alloc), predicate: q };
        }
        for e in exprs.iter_mut() {
            renumber(e, base);
        }
        let hidden = exprs.len() > visible;
        plan = Plan::Project { input: Box::new_in(plan, self.alloc), exprs };
        if core.distinct {
            if hidden {
                return fail(self.loc(core.span), ErrorKind::Unsupported("`select distinct` ordered by a column it does not select"));
            }
            plan = Plan::Distinct { input: Box::new_in(plan, self.alloc) };
        }
        if !sort.is_empty() {
            plan = Plan::Sort { input: Box::new_in(plan, self.alloc), keys: sort };
        }
        if hidden {
            let mut keep = Vec::with_capacity_in(visible, self.alloc);
            let at = self.loc(core.span);
            for c in 0..visible {
                keep.push(Expr { kind: ExprKind::Col(u32::try_from(c).unwrap_or(u32::MAX)), at });
            }
            plan = Plan::Project { input: Box::new_in(plan, self.alloc), exprs: keep };
        }
        Ok((plan, names))
    }

    /// A `group by` item: a 1-based position or a select alias stands for that select item.
    fn group_target(&self, e: &'q ast::Expr<'s, A>, core: &'q Core<'s, A>, scope: &[Col<'n>]) -> &'q ast::Expr<'s, A> {
        let item = |at: usize| match core.columns.get(at) {
            Some(ResultColumn::Expr { expr, .. }) => Some(expr),
            _ => None,
        };
        match &e.kind {
            ast::ExprKind::Integer(n) => usize::try_from(*n).ok().and_then(|n| n.checked_sub(1)).and_then(item).unwrap_or(e),
            ast::ExprKind::Column { table: None, column } if !scope.iter().any(|c| eq(c.name, column.text)) => core
                .columns
                .iter()
                .find_map(|c| match c {
                    ResultColumn::Expr { expr, alias: Some(alias) } if eq(alias.text, column.text) => Some(expr),
                    _ => None,
                })
                .unwrap_or(e),
            _ => e,
        }
    }

    /// `predicate` over `plan`: into the scan itself where `plan` is one, with the conjuncts
    /// `column op number` its chunks' statistics are checked against.
    fn filtered(&self, plan: Plan<'n, A>, predicate: Expr<A>) -> Plan<'n, A> {
        match plan {
            Plan::Scan { table, width, filter: None, mut prune } => {
                let parts: &[Expr<A>] = match &predicate.kind {
                    ExprKind::And(items) => items,
                    _ => core::slice::from_ref(&predicate),
                };
                for part in parts {
                    if let ExprKind::Compare { op, left, right } = &part.kind {
                        let found = match (&left.kind, &right.kind) {
                            (ExprKind::Col(c), ExprKind::Lit(d)) => number(*d).map(|v| (*c, *op, v)),
                            (ExprKind::Lit(d), ExprKind::Col(c)) => number(*d).map(|v| (*c, flip(*op), v)),
                            _ => None,
                        };
                        if let Some((column, op, value)) = found {
                            prune.push(Prune { column, op, value });
                        }
                    }
                }
                Plan::Scan { table, width, filter: Some(predicate), prune }
            }
            plan => Plan::Filter { input: Box::new_in(plan, self.alloc), predicate },
        }
    }

    // ---- expressions ----

    /// Column `table.name` (or `name`) of `scope`.
    fn column(&self, scope: &[Col<'n>], table: Option<&ast::Name<'s>>, name: &ast::Name<'s>) -> Run<'s, u32, A> {
        let mut found = None;
        for (at, col) in scope.iter().enumerate() {
            if col.shadowed {
                continue;
            }
            let hit = match table {
                Some(t) => col.qual.is_some_and(|q| eq(q, t.text)) && eq(col.name, name.text),
                None => !col.hidden && eq(col.name, name.text),
            };
            if hit {
                if found.is_some() {
                    return fail(self.loc(name.span), ErrorKind::AmbiguousColumn(name.text));
                }
                found = Some(at);
            }
        }
        // A hidden `using` column is still found by its name alone if nothing else is.
        if found.is_none() && table.is_none() {
            found = scope.iter().position(|col| !col.shadowed && eq(col.name, name.text));
        }
        match found {
            Some(at) => Ok(u32::try_from(at).unwrap_or(u32::MAX)),
            None => fail(self.loc(name.span), ErrorKind::UnknownColumn { table: table.map(|it| it.text), column: name.text }),
        }
    }

    fn param(&mut self, name: &ast::Name<'s>) -> Run<'s, Datum, A> {
        let Some((_, arg)) = self.params.iter().find(|(it, _)| eq(it, name.text)) else {
            return fail(self.loc(name.span), ErrorKind::MissingParam(name.text));
        };
        Ok(match *arg {
            Arg::Null => Datum::Null,
            Arg::Bool(b) => Datum::Bool(b),
            Arg::Int(n) => Datum::Int(n),
            Arg::Float(f) => Datum::Float(f),
            Arg::Text(t) => self.intern(t)?,
        })
    }

    fn list(&mut self, items: &'q [ast::Expr<'s, A>], level: &mut Level<'_, 'n, A>) -> Run<'s, Vec<Expr<A>, A>, A> {
        let mut out = Vec::with_capacity_in(items.len(), self.alloc);
        for item in items {
            out.push(self.bind(item, level)?);
        }
        Ok(out)
    }

    fn bind_box(&mut self, e: &'q ast::Expr<'s, A>, level: &mut Level<'_, 'n, A>) -> Run<'s, Box<Expr<A>, A>, A> {
        let bound = self.bind(e, level)?;
        Ok(self.boxed(bound))
    }

    /// Binds `e` at `level`. In a grouped level an expression equal to a key is that key's
    /// column, an aggregate is its column after the keys, and any other column is refused.
    #[expect(clippy::too_many_lines, reason = "one arm a form of the grammar")]
    fn bind(&mut self, e: &'q ast::Expr<'s, A>, level: &mut Level<'_, 'n, A>) -> Run<'s, Expr<A>, A> {
        use ast::ExprKind as K;
        let at = self.loc(e.span);
        if level.group.is_some() && !has_aggregate(e) && !has_window(e) && !matches!(e.kind, K::Null | K::Boolean(_) | K::Integer(_) | K::Real(_) | K::Text(_) | K::Param(_)) {
            let scope = level.scope;
            if let Some(group) = level.group.as_deref()
                && let Some(k) = group.spans.iter().position(|span| *span == e.span)
            {
                return Ok(Expr { kind: ExprKind::Col(u32::try_from(k).unwrap_or(u32::MAX)), at });
            }
            if let Ok(plain) = self.bind(e, &mut Level::plain(scope))
                && let Some(group) = level.group.as_deref()
                && let Some(k) = group.keys.iter().position(|key| same_expr(key, &plain))
            {
                return Ok(Expr { kind: ExprKind::Col(u32::try_from(k).unwrap_or(u32::MAX)), at });
            }
            if let K::Column { column, .. } = &e.kind {
                return fail(at, ErrorKind::NotGrouped(column.text));
            }
        }
        let kind = match &e.kind {
            K::Null => ExprKind::Lit(Datum::Null),
            K::Boolean(b) => ExprKind::Lit(Datum::Bool(*b)),
            K::Integer(n) => ExprKind::Lit(Datum::from_u64(*n)),
            K::Real(f) => ExprKind::Lit(Datum::Float(*f)),
            K::Text(raw) => {
                let text = unquote(raw, self.alloc);
                ExprKind::Lit(self.intern(text.as_str())?)
            }
            K::Blob(_) => return fail(at, ErrorKind::Unsupported("a blob literal")),
            K::Column { table, column } => ExprKind::Col(self.column(level.scope, table.as_ref(), column)?),
            K::Param(name) => ExprKind::Lit(self.param(name)?),
            K::Unary { op: UnaryOp::Neg, operand } => match &operand.kind {
                // `-9223372036854775808` is a literal, not a negated overflow.
                K::Integer(n) => match Datum::int(-i128::from(*n)) {
                    Some(d) => ExprKind::Lit(d),
                    None => return fail(at, ErrorKind::Overflow("-")),
                },
                _ => ExprKind::Neg(self.bind_box(operand, level)?),
            },
            K::Unary { op: UnaryOp::Not, operand } => ExprKind::Not(self.bind_box(operand, level)?),
            K::Chain { first, rest } => {
                let mut acc = self.bind(first, level)?;
                for link in rest.iter() {
                    let right = self.bind(&link.operand, level)?;
                    let span_at = Loc { source: self.source, start: e.span.start, end: link.operand.span.end };
                    acc = match link.op {
                        ChainOp::And | ChainOp::Or => {
                            let is_and = link.op == ChainOp::And;
                            let mut items = match acc.kind {
                                ExprKind::And(items) if is_and => items,
                                ExprKind::Or(items) if !is_and => items,
                                other => {
                                    let mut items = Vec::with_capacity_in(2, self.alloc);
                                    items.push(Expr { kind: other, at: acc.at });
                                    items
                                }
                            };
                            items.push(right);
                            Expr { kind: if is_and { ExprKind::And(items) } else { ExprKind::Or(items) }, at: span_at }
                        }
                        ChainOp::Concat => Expr { kind: ExprKind::Concat { left: self.boxed(acc), right: self.boxed(right) }, at: span_at },
                        op => {
                            let op = match op {
                                ChainOp::Add => Arith::Add,
                                ChainOp::Sub => Arith::Sub,
                                ChainOp::Mul => Arith::Mul,
                                ChainOp::Div => Arith::Div,
                                _ => Arith::Rem,
                            };
                            Expr { kind: ExprKind::Arith { op, left: self.boxed(acc), right: self.boxed(right) }, at: span_at }
                        }
                    };
                }
                return Ok(acc);
            }
            K::Compare { op, left, right } => {
                let l = self.bind_box(left, level)?;
                let r = self.bind_box(right, level)?;
                match op {
                    CompareOp::Is => ExprKind::Distinct { left: l, right: r, negated: true },
                    CompareOp::IsNot => ExprKind::Distinct { left: l, right: r, negated: false },
                    op => ExprKind::Compare {
                        op: match op {
                            CompareOp::Eq => Cmp::Eq,
                            CompareOp::Ne => Cmp::Ne,
                            CompareOp::Lt => Cmp::Lt,
                            CompareOp::Le => Cmp::Le,
                            CompareOp::Gt => Cmp::Gt,
                            _ => Cmp::Ge,
                        },
                        left: l,
                        right: r,
                    },
                }
            }
            K::IsNull { operand, negated } => ExprKind::IsNull { operand: self.bind_box(operand, level)?, negated: *negated },
            K::InList { operand, list, negated } => {
                ExprKind::InList { operand: self.bind_box(operand, level)?, list: self.list(list, level)?, negated: *negated }
            }
            K::InParam { .. } => return fail(at, ErrorKind::Unsupported("`in $list`")),
            K::Between { operand, low, high, negated } => ExprKind::Between {
                operand: self.bind_box(operand, level)?,
                low: self.bind_box(low, level)?,
                high: self.bind_box(high, level)?,
                negated: *negated,
            },
            K::Like { operand, pattern, negated } => {
                ExprKind::Like { operand: self.bind_box(operand, level)?, pattern: self.bind_box(pattern, level)?, negated: *negated }
            }
            K::Case { operand, arms, otherwise } => {
                let operand = match operand {
                    Some(it) => Some(self.bind_box(it, level)?),
                    None => None,
                };
                let mut bound = Vec::with_capacity_in(arms.len(), self.alloc);
                for arm in arms.iter() {
                    let w = self.bind(&arm.when, level)?;
                    let t = self.bind(&arm.then, level)?;
                    bound.push((w, t));
                }
                let otherwise = match otherwise {
                    Some(it) => Some(self.bind_box(it, level)?),
                    None => None,
                };
                ExprKind::Case { operand, arms: bound, otherwise }
            }
            K::Cast { operand, to } => {
                let to = match to {
                    blaze_sql::desc::Storage::Boolean => q::TypeName::Boolean,
                    blaze_sql::desc::Storage::Integer => q::TypeName::Int { bits: 64, signed: true },
                    blaze_sql::desc::Storage::Real => q::TypeName::Float,
                    blaze_sql::desc::Storage::Text => q::TypeName::Text,
                    blaze_sql::desc::Storage::Blob => return fail(at, ErrorKind::Unsupported("a cast to blob")),
                };
                ExprKind::Cast { operand: self.bind_box(operand, level)?, to }
            }
            K::Call { function, args } => {
                if AggFunc::named(function.text).is_some() {
                    return self.aggregate(function, args, &[], None, e.span, level);
                }
                let Some(func) = Func::named(function.text) else {
                    return fail(at, ErrorKind::UnknownFunction(function.text));
                };
                let CallArgs::List { distinct: false, args } = args else {
                    return fail(at, ErrorKind::Unsupported("`*` or `distinct` in a scalar function's arguments"));
                };
                let (least, most) = func.arity();
                if args.len() < least || args.len() > most {
                    return fail(at, ErrorKind::Arity { function: function.text, found: args.len() });
                }
                ExprKind::Call { func, args: self.list(args, level)? }
            }
            K::Analytic(analytic) => match &**analytic {
                Analytic::Convert { operand, to } => ExprKind::Cast { operand: self.bind_box(operand, level)?, to: *to },
                Analytic::Files(_) => return fail(at, ErrorKind::Unsupported("`@table` outside `read_parquet`")),
                Analytic::Subquery(query) => {
                    let (plan, names) = self.subquery(query, level.scope, e.span)?;
                    if names.len() != 1 {
                        return fail(at, ErrorKind::NotScalar { rows: 0, columns: names.len() });
                    }
                    ExprKind::Scalar(self.sub(plan, SubKind::Scalar, at))
                }
                Analytic::InQuery { operand, query, negated } => {
                    let operand = self.bind_box(operand, level)?;
                    let (plan, names) = self.subquery(query, level.scope, e.span)?;
                    if names.len() != 1 {
                        return fail(at, ErrorKind::NotScalar { rows: 0, columns: names.len() });
                    }
                    ExprKind::InSet { operand, sub: self.sub(plan, SubKind::Set, at), negated: *negated }
                }
                Analytic::Exists(query) => return self.exists(query, level, e.span),
                Analytic::Aggregate(agg) => match &agg.over {
                    Some(window) => return self.window(agg, window, level, e.span),
                    None => return self.aggregate(&agg.function, &agg.args, &agg.order_by, agg.filter.as_ref(), e.span, level),
                },
            },
        };
        Ok(Expr { kind, at })
    }

    fn sub(&mut self, plan: Plan<'n, A>, kind: SubKind, at: Loc) -> u32 {
        let id = u32::try_from(self.subs.len()).unwrap_or(u32::MAX);
        self.subs.push(Sub { plan, kind, at });
        id
    }

    /// An uncorrelated subquery; one that names a column of `outer` is refused by name.
    fn subquery(&mut self, query: &'q Query<'s, A>, outer: &[Col<'n>], span: Span) -> Run<'s, (Plan<'n, A>, Vec<&'n str, A>), A> {
        match self.query(query) {
            Err(Fail { kind: ErrorKind::UnknownColumn { table, column }, at }) => {
                let correlated = outer.iter().any(|c| {
                    eq(c.name, column) && table.is_none_or(|t| c.qual.is_some_and(|q| eq(q, t)))
                });
                if correlated {
                    return fail(self.loc(span), ErrorKind::Unsupported("a correlated subquery but `exists` correlated by equalities"));
                }
                Err(Fail { kind: ErrorKind::UnknownColumn { table, column }, at })
            }
            other => other,
        }
    }

    /// `exists (query)`: uncorrelated, or correlated by equalities `inner = outer` in its
    /// `where`, planned as a set of the inner keys probed by the outer row's.
    fn exists(&mut self, query: &'q Query<'s, A>, level: &mut Level<'_, 'n, A>, span: Span) -> Run<'s, Expr<A>, A> {
        let at = self.loc(span);
        match self.query(query) {
            Ok((plan, _)) => {
                let sub = self.sub(plan, SubKind::Exists { keys: 0 }, at);
                return Ok(Expr { kind: ExprKind::Exists { sub, keys: Vec::new_in(self.alloc) }, at });
            }
            Err(Fail { kind: ErrorKind::UnknownColumn { .. }, .. }) => {}
            Err(other) => return Err(other),
        }
        let SetExpr::Core(core) = &query.body else {
            return fail(at, ErrorKind::Unsupported("a correlated `exists` over a set operation"));
        };
        if query.with.is_some() || !matches!(core.group_by, GroupBy::None) || core.having.is_some() || core.qualify.is_some() {
            return fail(at, ErrorKind::Unsupported("a correlated `exists` with `with`, `group by`, `having` or `qualify`"));
        }
        let mut filter = Vec::new_in(self.alloc);
        if let Some(e) = &core.filter {
            conjuncts(e, &mut filter);
        }
        let mut inner_only = Vec::new_in(self.alloc);
        let mut correlated = Vec::new_in(self.alloc);
        // The `from` first, with the conjuncts that bind inside it.
        let (mut plan, scope) = {
            let mut all = Vec::new_in(self.alloc);
            all.extend_from_slice(&filter);
            let mut keep = Vec::new_in(self.alloc);
            for part in all.iter() {
                keep.push(*part);
            }
            let depth = self.ctes.len();
            let out = self.from_list(&core.from, &mut keep);
            self.ctes.truncate(depth);
            let out = out?;
            filter = keep;
            out
        };
        for part in filter.iter() {
            if self.try_bind(part, &scope).is_some() {
                inner_only.push(*part);
            } else {
                correlated.push(*part);
            }
        }
        if !inner_only.is_empty() {
            let predicate = self.and_of(&inner_only, &scope)?;
            plan = self.filtered(plan, predicate);
        }
        let mut inner = Vec::new_in(self.alloc);
        let mut outer = Vec::new_in(self.alloc);
        for part in correlated.iter() {
            let ast::ExprKind::Compare { op: CompareOp::Eq, left, right } = &part.kind else {
                return fail(self.loc(part.span), ErrorKind::Unsupported("a correlation in `exists` that is not an equality"));
            };
            let (i, o) = match self.try_bind(left, &scope) {
                Some(i) => (i, self.bind(right, level)?),
                None => match self.try_bind(right, &scope) {
                    Some(i) => (i, self.bind(left, level)?),
                    None => return fail(self.loc(part.span), ErrorKind::Unsupported("a correlation in `exists` that is not `inner = outer`")),
                },
            };
            inner.push(i);
            outer.push(o);
        }
        let keys = u32::try_from(inner.len()).unwrap_or(u32::MAX);
        let plan = Plan::Project { input: Box::new_in(plan, self.alloc), exprs: inner };
        let sub = self.sub(plan, SubKind::Exists { keys }, at);
        Ok(Expr { kind: ExprKind::Exists { sub, keys: outer }, at })
    }

    #[expect(clippy::too_many_arguments, reason = "an aggregate's parts, as either form writes them")]
    fn aggregate(
        &mut self,
        function: &'q ast::Name<'s>,
        args: &'q CallArgs<'s, A>,
        order_by: &'q [q::Order<'s, A>],
        filter: Option<&'q ast::Expr<'s, A>>,
        span: Span,
        level: &mut Level<'_, 'n, A>,
    ) -> Run<'s, Expr<A>, A> {
        let at = self.loc(span);
        let agg = self.agg_parts(function, args, order_by, filter, span, level.scope)?;
        let Some(group) = level.group.as_deref_mut() else {
            return fail(at, ErrorKind::AggregateNotAllowed(function.text));
        };
        let column = group.keys.len() + group.aggs.len();
        group.aggs.push(agg);
        Ok(Expr { kind: ExprKind::Col(u32::try_from(column).unwrap_or(u32::MAX)), at })
    }

    #[expect(clippy::too_many_arguments, reason = "an aggregate's parts, as either form writes them")]
    fn agg_parts(
        &mut self,
        function: &'q ast::Name<'s>,
        args: &'q CallArgs<'s, A>,
        order_by: &'q [q::Order<'s, A>],
        filter: Option<&'q ast::Expr<'s, A>>,
        span: Span,
        scope: &[Col<'n>],
    ) -> Run<'s, Agg<A>, A> {
        let at = self.loc(span);
        let Some(mut func) = AggFunc::named(function.text) else {
            return fail(at, ErrorKind::UnknownFunction(function.text));
        };
        let (distinct, list): (bool, &'q [ast::Expr<'s, A>]) = match args {
            CallArgs::Star if func == AggFunc::Count => {
                func = AggFunc::CountRows;
                (false, &[])
            }
            CallArgs::Star => return fail(at, ErrorKind::Arity { function: function.text, found: 0 }),
            CallArgs::List { distinct, args } => (*distinct, args.as_slice()),
        };
        let (least, most) = func.arity();
        if list.len() < least || list.len() > most {
            return fail(at, ErrorKind::Arity { function: function.text, found: list.len() });
        }
        let mut plain = Level::plain(scope);
        let mut bound = Vec::with_capacity_in(list.len(), self.alloc);
        for arg in list {
            if has_aggregate(arg) || has_window(arg) {
                return fail(self.loc(arg.span), ErrorKind::AggregateNotAllowed(function.text));
            }
            bound.push(self.bind(arg, &mut plain)?);
        }
        let filter = match filter {
            Some(f) => Some(self.bind(f, &mut plain)?),
            None => None,
        };
        let mut order = Vec::with_capacity_in(order_by.len(), self.alloc);
        for o in order_by {
            order.push(SortKey { expr: self.bind(&o.expr, &mut plain)?, descending: o.descending, nulls_first: o.nulls_first.unwrap_or(false) });
        }
        Ok(Agg { func, args: bound, distinct, filter, order, at })
    }

    fn window(&mut self, agg: &'q q::Aggregate<'s, A>, window: &'q q::Window<'s, A>, level: &mut Level<'_, 'n, A>, span: Span) -> Run<'s, Expr<A>, A> {
        let at = self.loc(span);
        if level.windows.is_none() {
            return fail(at, ErrorKind::AggregateNotAllowed(agg.function.text));
        }
        // The window's own parts bind at the level, without windows.
        let mut inner = Level { scope: level.scope, group: level.group.as_deref_mut(), windows: None };
        let args: &'q [ast::Expr<'s, A>] = match &agg.args {
            CallArgs::List { args, .. } => args.as_slice(),
            CallArgs::Star => &[],
        };
        let name = agg.function.text;
        let func = if eq(name, "row_number") {
            WinFunc::RowNumber
        } else if eq(name, "rank") {
            WinFunc::Rank
        } else if eq(name, "dense_rank") {
            WinFunc::DenseRank
        } else if eq(name, "lead") || eq(name, "lag") {
            let Some(first) = args.first() else {
                return fail(at, ErrorKind::Arity { function: name, found: 0 });
            };
            if args.len() > 3 {
                return fail(at, ErrorKind::Arity { function: name, found: args.len() });
            }
            let expr = self.bind(first, &mut inner)?;
            let mut offset = match args.get(1) {
                Some(e) => {
                    let d = self.constant(e)?;
                    d.as_i128().and_then(|it| i64::try_from(it).ok()).unwrap_or(1)
                }
                None => 1,
            };
            if eq(name, "lag") {
                offset = -offset;
            }
            let default = match args.get(2) {
                Some(e) => Some(self.bind(e, &mut inner)?),
                None => None,
            };
            WinFunc::Shift { expr, offset, default }
        } else {
            // An aggregate over the frame: its arguments over the level's columns, which in a
            // grouped level are the keys and aggregates.
            let mut parts = Vec::with_capacity_in(args.len(), self.alloc);
            for arg in args {
                parts.push(self.bind(arg, &mut inner)?);
            }
            let filter = match &agg.filter {
                Some(f) => Some(self.bind(f, &mut inner)?),
                None => None,
            };
            let Some(mut func) = AggFunc::named(name) else {
                return fail(at, ErrorKind::UnknownFunction(name));
            };
            let distinct = match &agg.args {
                CallArgs::Star if func == AggFunc::Count => {
                    func = AggFunc::CountRows;
                    false
                }
                CallArgs::Star => return fail(at, ErrorKind::Arity { function: name, found: 0 }),
                CallArgs::List { distinct, .. } => *distinct,
            };
            let (least, most) = func.arity();
            if parts.len() < least || parts.len() > most {
                return fail(at, ErrorKind::Arity { function: name, found: parts.len() });
            }
            let mut order = Vec::new_in(self.alloc);
            for o in agg.order_by.iter() {
                order.push(SortKey { expr: self.bind(&o.expr, &mut inner)?, descending: o.descending, nulls_first: o.nulls_first.unwrap_or(false) });
            }
            WinFunc::Agg(Agg { func, args: parts, distinct, filter, order, at })
        };
        let mut partition = Vec::with_capacity_in(window.partition_by.len(), self.alloc);
        for p in window.partition_by.iter() {
            partition.push(self.bind(p, &mut inner)?);
        }
        let mut order = Vec::with_capacity_in(window.order_by.len(), self.alloc);
        for o in window.order_by.iter() {
            order.push(SortKey { expr: self.bind(&o.expr, &mut inner)?, descending: o.descending, nulls_first: o.nulls_first.unwrap_or(false) });
        }
        let frame = match &window.frame {
            None if order.is_empty() => Frame { rows: true, start: FrameBound::Unbounded, end: FrameBound::Unbounded },
            None => Frame { rows: false, start: FrameBound::Unbounded, end: FrameBound::Current },
            Some(frame) => {
                let rows = frame.units == FrameUnits::Rows;
                let start = self.frame_bound(&frame.start, rows, at)?;
                let end = self.frame_bound(&frame.end, rows, at)?;
                let start = match (&frame.start, start) {
                    (q::Bound::UnboundedFollowing, _) => return fail(at, ErrorKind::Unsupported("a frame starting unbounded following")),
                    (_, it) => it,
                };
                let end = match (&frame.end, end) {
                    (q::Bound::UnboundedPreceding, _) => return fail(at, ErrorKind::Unsupported("a frame ending unbounded preceding")),
                    (_, it) => it,
                };
                Frame { rows, start, end }
            }
        };
        let Some(windows) = level.windows.as_deref_mut() else {
            return fail(at, ErrorKind::AggregateNotAllowed(name));
        };
        let column = WINDOW_BASE + u32::try_from(windows.len()).unwrap_or(0);
        windows.push(Window { func, partition, order, frame, at });
        Ok(Expr { kind: ExprKind::Col(column), at })
    }

    fn frame_bound(&mut self, bound: &'q q::Bound<'s, A>, rows: bool, at: Loc) -> Run<'s, FrameBound, A> {
        let offset = |binder: &mut Self, e: &'q ast::Expr<'s, A>| -> Run<'s, i64, A> {
            if !rows {
                return fail(at, ErrorKind::Unsupported("a `range` frame of an offset"));
            }
            let d = binder.constant(e)?;
            match d.as_i128().and_then(|it| i64::try_from(it).ok()) {
                Some(n) => Ok(n),
                None => fail(at, ErrorKind::Type { op: "a frame offset", left: d.kind(), right: crate::datum::Kind::Int }),
            }
        };
        Ok(match bound {
            q::Bound::UnboundedPreceding | q::Bound::UnboundedFollowing => FrameBound::Unbounded,
            q::Bound::CurrentRow => FrameBound::Current,
            q::Bound::Preceding(e) => FrameBound::Rows(-offset(self, e)?),
            q::Bound::Following(e) => FrameBound::Rows(offset(self, e)?),
        })
    }
}

fn flip(op: Cmp) -> Cmp {
    match op {
        Cmp::Lt => Cmp::Gt,
        Cmp::Le => Cmp::Ge,
        Cmp::Gt => Cmp::Lt,
        Cmp::Ge => Cmp::Le,
        it => it,
    }
}

fn number(d: Datum) -> Option<Bound> {
    match d {
        Datum::Int(_) | Datum::UInt(_) => d.as_i128().map(Bound::Int),
        Datum::Float(f) if !f.is_nan() => Some(Bound::Float(f)),
        _ => None,
    }
}

/// Adds `by` to every column of `e` (an expression over the right side, to the joined row).
fn shift<A: Allocator>(e: &mut Expr<A>, by: u32) {
    map_cols(e, &mut |c| c + by);
}

/// Swaps the sides of an expression over `left` then `right` columns.
fn swap_sides<A: Allocator>(e: &mut Expr<A>, left: usize, right: usize) {
    let (lw, rw) = (u32::try_from(left).unwrap_or(0), u32::try_from(right).unwrap_or(0));
    map_cols(e, &mut |c| if c < lw { c + rw } else { c - lw });
}

fn map_cols<A: Allocator>(e: &mut Expr<A>, f: &mut dyn FnMut(u32) -> u32) {
    use ExprKind as K;
    match &mut e.kind {
        K::Col(c) => *c = f(*c),
        K::Lit(_) | K::Scalar(_) => {}
        K::Neg(x) | K::Not(x) => map_cols(x, f),
        K::And(xs) | K::Or(xs) | K::Call { args: xs, .. } | K::Exists { keys: xs, .. } => {
            for x in xs.iter_mut() {
                map_cols(x, f);
            }
        }
        K::Arith { left, right, .. } | K::Concat { left, right } | K::Compare { left, right, .. } | K::Distinct { left, right, .. } => {
            map_cols(left, f);
            map_cols(right, f);
        }
        K::IsNull { operand, .. } | K::Cast { operand, .. } | K::InSet { operand, .. } => map_cols(operand, f),
        K::InList { operand, list, .. } => {
            map_cols(operand, f);
            for x in list.iter_mut() {
                map_cols(x, f);
            }
        }
        K::Between { operand, low, high, .. } => {
            map_cols(operand, f);
            map_cols(low, f);
            map_cols(high, f);
        }
        K::Like { operand, pattern, .. } => {
            map_cols(operand, f);
            map_cols(pattern, f);
        }
        K::Case { operand, arms, otherwise } => {
            if let Some(x) = operand {
                map_cols(x, f);
            }
            for (w, t) in arms.iter_mut() {
                map_cols(w, f);
                map_cols(t, f);
            }
            if let Some(x) = otherwise {
                map_cols(x, f);
            }
        }
    }
}

fn renumber_window<A: Allocator>(w: &mut Window<A>, base: u32) {
    for p in w.partition.iter_mut() {
        renumber(p, base);
    }
    for o in w.order.iter_mut() {
        renumber(&mut o.expr, base);
    }
    match &mut w.func {
        WinFunc::RowNumber | WinFunc::Rank | WinFunc::DenseRank => {}
        WinFunc::Shift { expr, default, .. } => {
            renumber(expr, base);
            if let Some(d) = default {
                renumber(d, base);
            }
        }
        WinFunc::Agg(agg) => {
            for a in agg.args.iter_mut() {
                renumber(a, base);
            }
            if let Some(f) = &mut agg.filter {
                renumber(f, base);
            }
        }
    }
}
