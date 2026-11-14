//! `loop_match_candidate`: a hand-written state machine, a `loop` that branches on a local
//! `state` and sets the next one, that nightly Rust's `#[loop_match]` could compile into
//! direct jumps. As written, every transition goes back to the top of the loop and through the
//! `match` again, one indirect or chained branch per step. With `#[loop_match]` on the loop and
//! `#[const_continue]` on a transition whose next state is a constant, rustc jumps straight to
//! that state's arm.
//!
//! Found, in a `loop` (not a `while` or `for`) written in the file and not in a closure or an
//! async block, whose body is one statement or tail and nothing else:
//!
//! - `state = match state { .. };`, each arm's value a transition;
//! - `match state { A => { ..; state = B; } .. }`, each arm's trailing assignment to `state` a
//!   transition, and no other write to `state` in the `match`.
//!
//! `state` is a local whose type rustc's `loop_match` takes: an integer, a float, `bool`,
//! `char`, or an enum whose variants have no fields. No arm has a guard, and at least one
//! transition is a constant rustc accepts after `#[const_continue]`: a literal (negated or not,
//! not a string), a fieldless variant, or a `const` of a module. A block's tail and both
//! branches of an `if` are followed to their values; a `break`, `continue` or `return` leaves
//! the machine and is no transition.
//!
//! Left alone: a state that is a field (`self.stage`), a type with data, a guard, a second
//! statement in the loop body, a write to `state` other than a transition (`state += 1`,
//! `&mut state`, one nested deeper), no constant transition, a loop that has `#[loop_match]`
//! already, and a loop whose syntax node is not found.
//!
//! Off by default: `loop_match` is an incomplete nightly feature (rustc warns
//! `incomplete_features`, stable rejects it), and the lint cannot see whether the loop is hot.
//! No fix: the rewrite into the labeled-block form is the author's to make.

use hir::{Adt, EnumVariant};
use hir_def::{
    DefWithBodyId, ItemContainerId, Lookup as _,
    expr_store::Body,
    hir::{
        BinaryOp, BindingId, Expr, ExprId, Literal, LoopSource, MatchArm, Pat, PatId, Statement,
        UnaryOp, Unsafe, type_ref::Mutability,
    },
    resolver::ValueNs,
};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

use super::{
    Finding, Lint, exempt, facts, loops,
    per_item::{local_of, value_of},
};
use crate::check::Cx;

const LINT: Lint = Lint::LoopMatchCandidate;

/// How many expressions up a loop is searched for a closure it is in.
const UP: usize = 64;

/// How many blocks and `if`s deep an arm is followed to its transitions.
const DEPTH: u32 = 8;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let body = analysis.body();
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let Expr::Loop { body: inner, source: LoopSource::Loop, .. } = expr else { continue };
        if !loops::written(analysis, id) {
            continue;
        }
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        // rustc takes only a local that is not a closure's capture (`thir/cx/expr.rs`), and
        // which locals a closure or async block captures is not read here.
        if parents.up(id, UP).any(|it| matches!(body.expr(it), Expr::Closure { .. })) {
            continue;
        }
        let Some(machine) = machine(db, def, body, inner) else { continue };
        if machine.constant == 0
            || !supported(db, &analysis.ty(analysis.infer().binding_ty(machine.state)))
        {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        if annotated(cx, range) || exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let name = body[machine.state].name.as_str();
        let (constant, all) = (machine.constant, machine.all);
        let message = format!(
            "this `loop` is a state machine over `{name}`: {constant} of its {all} transitions go to a constant \
             state, and each still goes back through the `match` on `{name}`"
        );
        let note = format!(
            "nightly's `#[loop_match]` makes a constant transition a direct jump to its arm: put `#[loop_match]` on \
             the loop, write its body as `{name} = 'blk: {{ match {name} {{ .. }} }}`, write each constant \
             transition as `#[const_continue] break 'blk NEXT`, and add `#![feature(loop_match)]` to the crate. The \
             feature is incomplete in rustc (it warns `incomplete_features`) and stable Rust rejects it"
        );
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

/// A loop body the module docs find: its state and how many of its transitions are constant.
struct Machine {
    state: BindingId,
    constant: u32,
    all: u32,
}

/// The state machine `inner`, a loop's body, is in one of the two shapes the module docs name.
fn machine(db: &base_db::Db, def: DefWithBodyId, body: &Body, inner: ExprId) -> Option<Machine> {
    let Expr::Block { statements, tail, label: None, unsafe_: Unsafe::No, .. } = body.expr(inner)
    else {
        return None;
    };
    let only = match (statements.len(), tail) {
        (0, Some(tail)) => tail,
        (1, None) => match statements.first()? {
            Statement::Expr { expr, .. } => expr,
            Statement::Let { .. } | Statement::Item(_) => return None,
        },
        _ => return None,
    };
    let (scrutinee, arms, assigned) = match body.expr(only) {
        Expr::Assignment { target, value } => {
            let Expr::Match { expr, arms } = body.expr(value) else { return None };
            (expr, arms, Some(target))
        }
        Expr::Match { expr, arms } => (expr, arms, None),
        _ => return None,
    };
    let state = local_of(db, def, body, scrutinee)?;
    if arms.iter().any(|arm| arm.guard.is_some()) {
        return None;
    }
    let walk = Walk { db, def, body, state };
    let mut count = Count { constant: 0, all: 0 };
    // alloc: the transitions of one loop, a few
    let mut sites = Vec::new();
    match assigned {
        Some(target) => {
            let Pat::Expr(place) = body.pat(target) else { return None };
            if local_of(db, def, body, place) != Some(state) {
                return None;
            }
            sites.push(only);
            for arm in arms {
                walk.values(arm.expr, DEPTH, &mut count);
            }
        }
        None => {
            for &MatchArm { expr, .. } in arms {
                walk.transitions(expr, DEPTH, &mut count, &mut sites);
            }
        }
    }
    // A write the shapes don't account for changes the state between transitions, and
    // `loop_match` has nowhere to put it.
    let writes = walk.writes(only);
    if writes.len() != sites.len() || !writes.iter().all(|it| sites.contains(it)) {
        return None;
    }
    Some(Machine { state, constant: count.constant, all: count.all })
}

/// How many transitions there are, and how many of them go to a constant state.
struct Count {
    constant: u32,
    all: u32,
}

/// One body, and the state of the loop it is asked about.
struct Walk<'a> {
    db: &'a base_db::Db,
    def: DefWithBodyId,
    body: &'a Body,
    state: BindingId,
}

impl Walk<'_> {
    /// Counts the next states the value `expr` can be: a block's tail and each branch of an `if`
    /// followed, an exit not counted.
    fn values(&self, expr: ExprId, depth: u32, count: &mut Count) {
        if self.constant(expr) {
            count.constant += 1;
            count.all += 1;
            return;
        }
        if self.exits(expr) {
            return;
        }
        match self.body.expr(expr) {
            Expr::Block { tail: Some(tail), label: None, .. } if depth > 0 => {
                self.values(tail, depth - 1, count);
            }
            Expr::If { then_branch, else_branch: Some(else_branch), .. } if depth > 0 => {
                self.values(then_branch, depth - 1, count);
                self.values(else_branch, depth - 1, count);
            }
            _ => count.all += 1,
        }
    }

    /// Counts the transitions of the arm body `expr` of the second shape, each assignment to the
    /// state in a trailing place added to `sites`. A path that ends without one keeps the state,
    /// a transition to whatever it was.
    fn transitions(&self, expr: ExprId, depth: u32, count: &mut Count, sites: &mut Vec<ExprId>) {
        if self.exits(expr) {
            return;
        }
        match self.body.expr(expr) {
            // `state = next` alone; a destructuring assignment is a write the shape doesn't
            // take, left to `writes` to refuse.
            Expr::Assignment { target, value }
                if matches!(self.body.pat(target), Pat::Expr(place) if self.is_state(place)) =>
            {
                sites.push(expr); // alloc: as above
                self.values(value, depth, count);
            }
            Expr::Block { statements, tail, label: None, .. } if depth > 0 => {
                let last = match (tail, statements.last()) {
                    (Some(tail), _) => Some(tail),
                    (None, Some(Statement::Expr { expr, .. })) => Some(expr),
                    (None, _) => None,
                };
                match last {
                    Some(last) => self.transitions(last, depth - 1, count, sites),
                    None => count.all += 1,
                }
            }
            Expr::If { then_branch, else_branch, .. } if depth > 0 => {
                self.transitions(then_branch, depth - 1, count, sites);
                match else_branch {
                    Some(else_branch) => self.transitions(else_branch, depth - 1, count, sites),
                    None => count.all += 1,
                }
            }
            _ => count.all += 1,
        }
    }

    /// Every expression under `root` that writes the state: an assignment to it (destructuring
    /// included), a compound assignment, or a `&mut` of it.
    fn writes(&self, root: ExprId) -> Vec<ExprId> {
        // alloc: the writes of one loop body, a few, and the walk's stack
        let mut writes = Vec::new();
        let mut stack = vec![root]; // alloc: as above
        while let Some(at) = stack.pop() {
            self.body.walk_child_exprs(at, |child| stack.push(child));
            let write = match self.body.expr(at) {
                Expr::Assignment { target, .. } => self.is_state_pat(target),
                Expr::BinaryOp { lhs, op: Some(BinaryOp::Assignment { .. }), .. } => {
                    self.is_state(lhs)
                }
                Expr::Ref { expr, mutability: Mutability::Mut, .. } => self.is_state(expr),
                _ => false,
            };
            if write && !writes.contains(&at) {
                writes.push(at); // alloc: as above
            }
        }
        writes
    }

    /// Whether the assignment target `pat` names the state anywhere in it.
    fn is_state_pat(&self, pat: PatId) -> bool {
        let mut found = false;
        self.body.walk_pats(pat, &mut |it| {
            if let Pat::Expr(expr) = self.body.pat(it) {
                found |= self.is_state(expr);
            }
        });
        found
    }

    fn is_state(&self, expr: ExprId) -> bool {
        local_of(self.db, self.def, self.body, expr) == Some(self.state)
    }

    /// Whether `expr` leaves the machine: a `break`, `continue`, `return` or `become`, or a
    /// block that ends in one.
    fn exits(&self, expr: ExprId) -> bool {
        match self.body.expr(expr) {
            Expr::Break { .. }
            | Expr::Continue { .. }
            | Expr::Return { .. }
            | Expr::Become { .. } => true,
            Expr::Block { statements, tail: None, label: None, .. } => {
                matches!(statements.last(), Some(Statement::Expr { expr, .. }) if self.exits(expr))
            }
            _ => false,
        }
    }

    /// Whether `expr` is a value rustc's `#[const_continue]` takes (`builder/scope.rs`): a
    /// literal, a negated number, a fieldless variant, or a `const` of a module. An associated
    /// `const` may be generic, which rustc refuses, so it is not counted.
    fn constant(&self, expr: ExprId) -> bool {
        match self.body.expr(expr) {
            Expr::Literal(it) => !matches!(
                it,
                Literal::String(_) | Literal::ByteString(_) | Literal::CString(_)
            ),
            Expr::UnaryOp { expr, op: UnaryOp::Neg } => matches!(
                self.body.expr(expr),
                Expr::Literal(Literal::Int(..) | Literal::Uint(..) | Literal::Float(..))
            ),
            Expr::Path(_) => match value_of(self.db, self.def, self.body, expr) {
                Some(ValueNs::EnumVariantId(it)) => {
                    EnumVariant::from(it).fields_iter(self.db).len() == 0
                }
                Some(ValueNs::ConstId(it)) => {
                    matches!(it.lookup(self.db).container, ItemContainerId::ModuleId(_))
                }
                _ => false,
            },
            _ => false,
        }
    }
}

/// Whether rustc's `loop_match` takes a state of type `ty` (`builder/expr/into.rs`).
fn supported(db: &base_db::Db, ty: &hir::Type<'_>) -> bool {
    if ty.is_int_or_uint() || ty.is_float() || ty.is_bool() || ty.is_char() {
        return true;
    }
    match ty.as_adt() {
        Some(Adt::Enum(it)) => it.variants_iter(db).all(|it| it.fields_iter(db).len() == 0),
        _ => false,
    }
}

/// Whether the loop at `range` has `#[loop_match]` already. A loop whose node is not found
/// counts as having it, so the lint stays quiet when it can't tell.
fn annotated(cx: &Cx<'_, '_>, range: TextRange) -> bool {
    let root = cx.root();
    if !root.text_range().contains_range(range) {
        return true;
    }
    let found = root
        .covering_element(range)
        .ancestors()
        .filter_map(ast::LoopExpr::cast)
        .find(|it| it.syntax().text_range() == range);
    let Some(loop_) = found else { return true };
    loop_.attrs().any(|attr| attr.path().is_some_and(|path| path.syntax().text() == "loop_match"))
}
