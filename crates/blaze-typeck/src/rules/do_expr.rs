//! Postfix `.do` one-shot lexical algebraic effect suspension (`Expr::Do`,
//! `docs/v2/plan/design/sans-io-machines.md`):
//!
//! 1. **Typing (`expr_do`)**: mirrors `.await` (`rules/async_fmt.rs`). Evaluates `operand` to
//!    type `T`, proves `T: Request` (via `LangItem::Request`, or a unique `Request`-shaped trait
//!    in scope with `type Answer` when no `#[lang = "blaze_request"]` is registered), and
//!    projects `<T as Request>::Answer`. Unlike `.await`, `.do` does not require `async`.
//! 2. **No-local-borrow-across-`.do` (`local_borrow_across_do`)**: post-typeck pass after the
//!    end sequence that verifies no reference (`&local` / `&mut local` / `ref` binding / struct
//!    holding one) to a function's own local variable is held live across any `Expr::Do(_)`
//!    suspension point. This invariant keeps compiled state machines zero-`Pin` and freely
//!    movable across worker threads. Violations emit `TypedFinding::LocalBorrowAcrossDo`
//!    (lint `"local-borrow-across-do"`).

use blaze_own::{Alloc, Vec};

use crate::body::{Expr, Pat, Stmt, UnOp};
use crate::columns::AdjustKind;
use crate::diag::At;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, ItemKey, LangItem, Name, PatId, Range, ScopeId};
use crate::machine::{Expectation, Flow, Frame, Machine, Then};
use crate::portable::PNode;
use crate::rules::args_inline::TypedFinding;
use crate::ty::{Cause, Ty, TyView, TypeCtx};
use crate::world::World;

/// Most type nodes inspected when deciding whether a settled type can hold a borrow (rule 18).
const MOST_TY_NODES: usize = 64;

/// Most parentheses / projections peeled when finding the root local of a place (rule 18).
const MOST_PEELS: usize = 64;

/// Most loop re-walk passes per body for fixed-point loop-carried borrow checking (rule 18).
const MOST_LOOP_REWALKS: u32 = 32;

/// One live local-borrow edge tracked by [`Machine::local_borrow_across_do`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BorrowEdge {
    /// Binding holding a reference (or aggregate containing a reference).
    holder: PatId,
    /// Local variable of this body whose stack slot is borrowed.
    local: PatId,
    /// True once any `.do` suspension point has executed since `holder` acquired this borrow.
    crossed_do: bool,
}

/// Saved branch join accumulator for `if` / `match` / `let-else` in the iterative walk.
struct BranchJoinState<'a> {
    /// Snapshot of `state` before the branches ran.
    before: Vec<BorrowEdge, Alloc<'a>>,
    /// Union of exit states from all completed branches so far.
    joined: Vec<BorrowEdge, Alloc<'a>>,
    /// Union of borrowed locals returned as the value of completed branches so far.
    result_locals: Vec<PatId, Alloc<'a>>,
    /// Borrowed locals carried by a `match` scrutinee (empty for `if` / `let-else`).
    scrutinee_locals: Vec<PatId, Alloc<'a>>,
    /// Whether at least one branch has completed into `joined`.
    any_done: bool,
}

/// Iterative work-stack step for [`Machine::local_borrow_across_do`] (rule 18: no recursion).
enum Step {
    /// Evaluate expression `expr`, pushing one result slice onto `val_marks`/`val_pool`.
    Eval { expr: ExprId, as_place_write: bool },
    /// Push an empty result slice onto `val_marks`/`val_pool`.
    PushEmpty,
    /// Pop top result slice and discard it.
    Discard,
    /// Combine the top `count` result slices into one, filtering out locals if `check_ty` is
    /// true and `expr`'s settled type cannot hold a borrow.
    Combine { expr: ExprId, count: u32, check_ty: bool },
    /// Finish `&operand` / `&raw operand`: combine `operand`'s borrows with its root local.
    FinishRef { operand: ExprId },
    /// Finish `Expr::Do(operand)`: flag live temporary borrows on the evaluation stack and mark
    /// every active binding borrow in `state` as having crossed a `.do`.
    FinishDo { expr: ExprId },
    /// Finish `let pat = init`: pop `init`'s result slice (if `init.is_some()`) and bind `pat`.
    FinishLet { pat: PatId, init: Option<ExprId> },
    /// Execute a `let pat = init else { else_ };` statement lazily so `joins` stays strictly LIFO.
    LetElse { pat: PatId, init: Option<ExprId>, else_: ExprId },
    /// Finish `lhs = rhs`: pop `rhs` and `lhs`; if `lhs` is a plain local, overwrite its borrows.
    FinishAssign { lhs: ExprId },
    /// Snapshot `state` into `joins[join_idx].before` after `cond`/`scrutinee`/`init` has run;
    /// when `capture_scrutinee` is true, also pops the scrutinee's borrows into `scrutinee_locals`.
    BranchSnapshot { join_idx: usize, capture_scrutinee: bool },
    /// Bind a `match` arm's pattern against `joins[join_idx].scrutinee_locals`.
    BindArmPat { pat: PatId, scrutinee: ExprId, join_idx: usize },
    /// Restore `joins[join_idx].before` before evaluating a branch.
    BranchStart { join_idx: usize },
    /// Merge current `state` and top result slice into `joins[join_idx]`.
    BranchEnd { join_idx: usize },
    /// Restore `joins[join_idx].before` and pop `joins[join_idx]` (used after diverging `let-else`);
    /// when `restore_scrutinee` is true, pushes `scrutinee_locals` back for `FinishLet`.
    BranchRestore { join_idx: usize, restore_scrutinee: bool },
    /// Finish all branches for `join_idx`: install joined state and push merged branch result.
    BranchFinish { expr: ExprId, join_idx: usize },
    /// Finish one pass of `loop { body }`: if a `.do` ran and added new `crossed_do` edges,
    /// schedule one re-walk pass over `body` to catch loop-carried local borrows.
    LoopAfter { body: ExprId, saved_idx: usize, do_count_before: u32, pass: u8 },
    /// Run all `defer { ... }` cleanup blocks registered in this block scope in LIFO order
    /// at block exit, after every subsequent statement and tail expression has executed.
    BlockExitDefers { defer_idx: usize },
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// `operand.do`: one-shot lexical algebraic effect suspension.
    pub(crate) fn expr_do(&mut self, f: Frame, e: ExprId, operand: ExprId) -> Result<Flow, Stop> {
        if f.state == 0 {
            self.suspends = true;
            self.resume(f, 1)?;
            self.push_expr(operand, Expectation::None, Then::Nothing)?;
            return Ok(Flow::Wait);
        }
        let t = self.ret;
        if self.is_err(t) {
            return Ok(Flow::Done(t));
        }
        let tr = match self.lang(LangItem::Request)? {
            Some(tr) => tr,
            None => match self.request_trait_in_scope()? {
                Some(tr) => tr,
                None => {
                    self.missing_lang(LangItem::Request, At::Expr(e));
                    return Ok(Flow::Done(self.common.err));
                }
            },
        };
        let args = self.mk_list(&[t])?;
        self.register_trait(tr, args, Cause::Expr(e))?;
        Ok(Flow::Done(self.project(tr, args, Name::of("Answer"), Cause::Expr(e))?))
    }

    /// `defer { body }`: lexical scope-exit cleanup block.
    ///
    /// Typechecks `body` with `Expectation::None` (so `.do` and cleanup statements inside `body`
    /// are checked normally and set `self.suspends` via `expr_do`) and produces `()`.
    pub(crate) fn expr_defer(&mut self, mut f: Frame, _e: ExprId, body: ExprId) -> Result<Flow, Stop> {
        if f.state == 0 {
            f.saved = self.diverges;
            self.diverges = false;
            self.resume(f, 1)?;
            self.push_expr(body, Expectation::None, Then::Nothing)?;
            return Ok(Flow::Wait);
        }
        self.diverges = f.saved;
        Ok(Flow::Done(self.common.unit))
    }

    /// Fallback lookup when `LangItem::Request` is not registered: finds a unique 1-generic
    /// trait in scope declaring an `Answer` associated type (matching `blaze_machine::Request`).
    fn request_trait_in_scope(&mut self) -> Result<Option<ItemKey>, Stop> {
        let answer = Name::of("Answer");
        let mut found: Option<ItemKey> = None;
        for &tr in self.reads.traits_in_scope(ScopeId(0))? {
            let Some(def) = self.reads.trait_def(tr)? else { continue };
            if def.generics == 1 && def.assoc_types.iter().any(|a| a.name == answer) {
                if found.is_some() {
                    return Ok(None);
                }
                found = Some(tr);
            }
        }
        Ok(found)
    }

    /// Post-end-sequence check for the no-local-borrow-across-`.do` invariant:
    /// emits [`TypedFinding::LocalBorrowAcrossDo`] whenever a reference to a local variable of
    /// this body is live across an [`Expr::Do`] suspension point.
    pub(crate) fn local_borrow_across_do(&mut self) -> Result<(), Stop> {
        // Zero-cost fast path for bodies that contain no `.do` expressions.
        if !self.body.exprs.iter().any(|e| matches!(e, Expr::Do(_))) {
            return Ok(());
        }
        let alloc = self.alloc;
        let pat_count = self.body.pats.len();
        let mut local_names: Vec<Option<Name>, Alloc<'a>> = Vec::with_capacity_in(pat_count, alloc);
        local_names.resize(pat_count, None);
        for (i, pat) in self.body.pats.iter().enumerate() {
            if let Pat::Bind { name, .. } = *pat {
                local_names[i] = Some(name);
            }
        }

        // Check the root body and any nested closure/async bodies that contain `.do` independently
        // (each closure/async block is its own suspension scope).
        let mut roots: Vec<ExprId, Alloc<'a>> = Vec::new_in(alloc);
        roots.push(self.body.root);
        for expr in self.body.exprs.iter() {
            match *expr {
                Expr::Closure { body, .. } | Expr::AsyncBlock { body, .. } => roots.push(body),
                _ => {}
            }
        }

        let mut emitted: Vec<(Name, Range), Alloc<'a>> = Vec::new_in(alloc);
        for &root in roots.iter() {
            self.check_scope_borrows_across_do(root, &local_names, &mut emitted)?;
        }
        Ok(())
    }

    fn check_scope_borrows_across_do(
        &mut self,
        root: ExprId,
        local_names: &[Option<Name>],
        emitted: &mut Vec<(Name, Range), Alloc<'a>>,
    ) -> Result<(), Stop> {
        let alloc = self.alloc;
        let step_limit = 16usize
            .saturating_mul(self.body.exprs.len() + self.body.stmts.len() + self.body.pats.len())
            .saturating_add(128);

        let mut steps: Vec<Step, Alloc<'a>> = Vec::new_in(alloc);
        let mut state: Vec<BorrowEdge, Alloc<'a>> = Vec::new_in(alloc);
        let mut val_pool: Vec<PatId, Alloc<'a>> = Vec::new_in(alloc);
        let mut val_marks: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
        let mut joins: Vec<BranchJoinState<'a>, Alloc<'a>> = Vec::new_in(alloc);
        let mut loop_saves: Vec<Vec<BorrowEdge, Alloc<'a>>, Alloc<'a>> = Vec::new_in(alloc);
        let mut defer_scopes: Vec<Vec<ExprId, Alloc<'a>>, Alloc<'a>> = Vec::new_in(alloc);
        let mut do_count: u32 = 0;
        let mut loop_rewalks: u32 = 0;
        let mut ticks: usize = 0;

        steps.push(Step::Eval { expr: root, as_place_write: false });

        while let Some(step) = steps.pop() {
            ticks += 1;
            if ticks > step_limit {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            match step {
                Step::PushEmpty => {
                    val_marks.push(val_pool.len());
                }
                Step::Discard => {
                    if let Some(mark) = val_marks.pop() {
                        val_pool.truncate(mark);
                    }
                }
                Step::Combine { expr, count, check_ty } => {
                    let count = count as usize;
                    if count == 0 {
                        val_marks.push(val_pool.len());
                        continue;
                    }
                    let base_idx = val_marks.len().saturating_sub(count);
                    let base = val_marks[base_idx];
                    val_marks.truncate(base_idx + 1);
                    if check_ty {
                        let ty = self.expr_ty[expr.ix()];
                        if !self.ty_may_borrow(ty)? {
                            val_pool.truncate(base);
                            continue;
                        }
                    }
                    dedup_tail(&mut val_pool, base);
                }
                Step::FinishRef { operand } => {
                    let base = val_marks.last().copied().unwrap_or(val_pool.len());
                    if let Some(root_local) = self.place_root_local(operand)? {
                        if !val_pool[base..].contains(&root_local) {
                            val_pool.push(root_local);
                        }
                    }
                    dedup_tail(&mut val_pool, base);
                }
                Step::FinishDo { expr } => {
                    // Check every local borrow still live on the evaluation stack — BOTH sibling
                    // arguments already evaluated before `.do` (e.g. `f(&x, req.do)`) AND any
                    // local borrow stored inside `operand` itself (`BorrowReq { p: &x }.do`).
                    let at = self.range_of(At::Expr(expr));
                    for &local in val_pool.iter() {
                        self.emit_local_borrow(local, local_names, at, emitted);
                    }
                    // Pop `operand`'s result slice and push empty result for `.do`.
                    if let Some(mark) = val_marks.pop() {
                        val_pool.truncate(mark);
                    }
                    val_marks.push(val_pool.len());
                    // Every active binding borrow in `state` has now crossed a suspension point.
                    for edge in state.iter_mut() {
                        edge.crossed_do = true;
                    }
                    do_count = do_count.saturating_add(1);
                }
                Step::FinishLet { pat, init } => {
                    let mut init_locals: Vec<PatId, Alloc<'a>> = Vec::new_in(alloc);
                    if init.is_some() {
                        if let Some(mark) = val_marks.pop() {
                            init_locals.extend_from_slice(&val_pool[mark..]);
                            val_pool.truncate(mark);
                        }
                    }
                    let init_root = match init {
                        Some(e) => self.place_root_local(e)?,
                        None => None,
                    };
                    self.bind_pat_borrows(pat, &init_locals, init_root, &mut state)?;
                }
                Step::LetElse { pat, init, else_ } => {
                    let join_idx = joins.len();
                    joins.push(BranchJoinState {
                        before: Vec::new_in(alloc),
                        joined: Vec::new_in(alloc),
                        result_locals: Vec::new_in(alloc),
                        scrutinee_locals: Vec::new_in(alloc),
                        any_done: false,
                    });
                    steps.push(Step::FinishLet { pat, init });
                    steps.push(Step::BranchRestore { join_idx, restore_scrutinee: init.is_some() });
                    steps.push(Step::Discard);
                    steps.push(Step::Eval { expr: else_, as_place_write: false });
                    steps.push(Step::BranchSnapshot { join_idx, capture_scrutinee: init.is_some() });
                    if let Some(init_e) = init {
                        steps.push(Step::Eval { expr: init_e, as_place_write: false });
                    }
                }
                Step::FinishAssign { lhs } => {
                    // Stack has `[lhs_res, rhs_res]` (rhs on top).
                    let mut rhs_locals: Vec<PatId, Alloc<'a>> = Vec::new_in(alloc);
                    if let Some(mark) = val_marks.pop() {
                        rhs_locals.extend_from_slice(&val_pool[mark..]);
                        val_pool.truncate(mark);
                    }
                    if let Some(mark) = val_marks.pop() {
                        val_pool.truncate(mark);
                    }
                    if let Some(p) = self.plain_local(lhs)? {
                        state.retain(|edge| edge.holder != p);
                        if self.ty_may_borrow(self.local_ty[p.ix()])? {
                            for &local in rhs_locals.iter() {
                                push_edge(&mut state, BorrowEdge { holder: p, local, crossed_do: false });
                            }
                        }
                    }
                    val_marks.push(val_pool.len());
                }
                Step::BranchSnapshot { join_idx, capture_scrutinee } => {
                    let j = &mut joins[join_idx];
                    j.before.clear();
                    j.before.extend_from_slice(&state);
                    if capture_scrutinee {
                        j.scrutinee_locals.clear();
                        if let Some(mark) = val_marks.pop() {
                            j.scrutinee_locals.extend_from_slice(&val_pool[mark..]);
                            val_pool.truncate(mark);
                        }
                    }
                }
                Step::BindArmPat { pat, scrutinee, join_idx } => {
                    let scrutinee_root = self.place_root_local(scrutinee)?;
                    let mut locals: Vec<PatId, Alloc<'a>> = Vec::new_in(alloc);
                    locals.extend_from_slice(&joins[join_idx].scrutinee_locals);
                    self.bind_pat_borrows(pat, &locals, scrutinee_root, &mut state)?;
                }
                Step::BranchStart { join_idx } => {
                    state.clear();
                    state.extend_from_slice(&joins[join_idx].before);
                }
                Step::BranchEnd { join_idx } => {
                    let j = &mut joins[join_idx];
                    if let Some(mark) = val_marks.pop() {
                        for &local in &val_pool[mark..] {
                            if !j.result_locals.contains(&local) {
                                j.result_locals.push(local);
                            }
                        }
                        val_pool.truncate(mark);
                    }
                    if !j.any_done {
                        j.joined.clear();
                        j.joined.extend_from_slice(&state);
                        j.any_done = true;
                    } else {
                        merge_states(&mut j.joined, &state);
                    }
                }
                Step::BranchRestore { join_idx, restore_scrutinee } => {
                    let j = joins.pop().ok_or(CheckError::Malformed(crate::error::Malformed::Expr(root)))?;
                    debug_assert_eq!(joins.len(), join_idx);
                    state.clear();
                    state.extend_from_slice(&j.before);
                    if restore_scrutinee {
                        val_marks.push(val_pool.len());
                        val_pool.extend_from_slice(&j.scrutinee_locals);
                    }
                }
                Step::BranchFinish { expr, join_idx } => {
                    let j = joins.pop().ok_or(CheckError::Malformed(crate::error::Malformed::Expr(expr)))?;
                    debug_assert_eq!(joins.len(), join_idx);
                    state.clear();
                    if j.any_done {
                        state.extend_from_slice(&j.joined);
                    } else {
                        state.extend_from_slice(&j.before);
                    }
                    let base = val_pool.len();
                    val_marks.push(base);
                    if self.ty_may_borrow(self.expr_ty[expr.ix()])? {
                        val_pool.extend_from_slice(&j.result_locals);
                    }
                }
                Step::LoopAfter { body, saved_idx, do_count_before, pass } => {
                    // Discard body's value slice from this iteration.
                    if let Some(mark) = val_marks.pop() {
                        val_pool.truncate(mark);
                    }
                    let pre = &loop_saves[saved_idx];
                    let mut next: Vec<BorrowEdge, Alloc<'a>> = Vec::with_capacity_in(pre.len() + state.len(), alloc);
                    next.extend_from_slice(pre);
                    merge_states(&mut next, &state);
                    let changed = next != *pre;
                    if pass == 0 && do_count > do_count_before && changed && loop_rewalks < MOST_LOOP_REWALKS {
                        loop_rewalks += 1;
                        state.clear();
                        state.extend_from_slice(&next);
                        steps.push(Step::LoopAfter { body, saved_idx, do_count_before, pass: 1 });
                        steps.push(Step::Eval { expr: body, as_place_write: false });
                    } else {
                        let _ = loop_saves.pop();
                        state.clear();
                        state.extend_from_slice(&next);
                        val_marks.push(val_pool.len());
                    }
                }
                Step::BlockExitDefers { defer_idx } => {
                    let scope_defers =
                        defer_scopes.pop().ok_or(CheckError::Malformed(crate::error::Malformed::Expr(root)))?;
                    debug_assert_eq!(defer_scopes.len(), defer_idx);
                    // Push in registration order onto the LIFO work stack so later `defer` blocks
                    // execute first (LIFO scope-exit semantics).
                    for &body in scope_defers.iter() {
                        steps.push(Step::Discard);
                        steps.push(Step::Eval { expr: body, as_place_write: false });
                    }
                }
                Step::Eval { expr, as_place_write } => {
                    self.step_eval(
                        expr,
                        as_place_write,
                        local_names,
                        &state,
                        &mut steps,
                        &mut val_pool,
                        &mut val_marks,
                        &mut joins,
                        &mut loop_saves,
                        &mut defer_scopes,
                        do_count,
                        emitted,
                    )?;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn step_eval(
        &mut self,
        e: ExprId,
        as_place_write: bool,
        local_names: &[Option<Name>],
        state: &[BorrowEdge],
        steps: &mut Vec<Step, Alloc<'a>>,
        val_pool: &mut Vec<PatId, Alloc<'a>>,
        val_marks: &mut Vec<usize, Alloc<'a>>,
        joins: &mut Vec<BranchJoinState<'a>, Alloc<'a>>,
        loop_saves: &mut Vec<Vec<BorrowEdge, Alloc<'a>>, Alloc<'a>>,
        defer_scopes: &mut Vec<Vec<ExprId, Alloc<'a>>, Alloc<'a>>,
        do_count: u32,
        emitted: &mut Vec<(Name, Range), Alloc<'a>>,
    ) -> Result<(), Stop> {
        let alloc = self.alloc;
        let node = self.body.expr(e)?;

        // Check whether an implicit autoref adjustment borrows this expression's root local
        // (e.g. `local.as_ref()` where `local` is autoref'd as `&local`).
        let autoref_local = if !as_place_write && self.has_borrow_adjust(e) {
            self.place_root_local(e)?
        } else {
            None
        };

        match node {
            Expr::Missing
            | Expr::Lit(_)
            | Expr::Path(_)
            | Expr::Continue { .. }
            | Expr::Underscore
            | Expr::OffsetOf { .. }
            // Nested closures / async blocks are checked in their own scope pass; const blocks
            // cannot suspend.
            | Expr::Closure { .. }
            | Expr::AsyncBlock { .. }
            | Expr::ConstBlock { .. } => {
                val_marks.push(val_pool.len());
            }
            Expr::Local(p) => {
                let base = val_pool.len();
                val_marks.push(base);
                if !as_place_write {
                    let at = self.range_of(At::Expr(e));
                    for edge in state.iter() {
                        if edge.holder == p {
                            if edge.crossed_do {
                                self.emit_local_borrow(edge.local, local_names, at, emitted);
                            }
                            if !val_pool[base..].contains(&edge.local) {
                                val_pool.push(edge.local);
                            }
                        }
                    }
                    if let Some(rl) = autoref_local {
                        if !val_pool[base..].contains(&rl) {
                            val_pool.push(rl);
                        }
                    }
                }
            }
            Expr::Paren(inner) => {
                if autoref_local.is_some() {
                    steps.push(Step::FinishRef { operand: e });
                }
                steps.push(Step::Eval { expr: inner, as_place_write });
            }
            Expr::Block { stmts, tail, .. } => {
                let stmt_ids = self.body.stmt_list(stmts)?;
                let defer_idx = defer_scopes.len();
                defer_scopes.push(Vec::new_in(alloc));
                steps.push(Step::BlockExitDefers { defer_idx });
                match tail {
                    Some(t) => steps.push(Step::Eval { expr: t, as_place_write: false }),
                    None => steps.push(Step::PushEmpty),
                }
                for &s in stmt_ids.iter().rev() {
                    match self.body.stmt(s)? {
                        Stmt::Let { pat, init, else_, .. } => {
                            if let Some(el) = else_ {
                                steps.push(Step::LetElse { pat, init, else_: el });
                            } else {
                                steps.push(Step::FinishLet { pat, init });
                                if let Some(init_e) = init {
                                    steps.push(Step::Eval { expr: init_e, as_place_write: false });
                                }
                            }
                        }
                        Stmt::Expr { expr, .. } => {
                            steps.push(Step::Discard);
                            steps.push(Step::Eval { expr, as_place_write: false });
                        }
                        Stmt::Item => {}
                    }
                }
            }
            Expr::If { cond, then, else_ } => {
                let join_idx = joins.len();
                joins.push(BranchJoinState {
                    before: Vec::new_in(alloc),
                    joined: Vec::new_in(alloc),
                    result_locals: Vec::new_in(alloc),
                    scrutinee_locals: Vec::new_in(alloc),
                    any_done: false,
                });
                steps.push(Step::BranchFinish { expr: e, join_idx });
                if let Some(el) = else_ {
                    steps.push(Step::BranchEnd { join_idx });
                    steps.push(Step::Eval { expr: el, as_place_write: false });
                    steps.push(Step::BranchStart { join_idx });
                } else {
                    // Implicit empty `else` preserves the post-`cond` state as one exit path.
                    steps.push(Step::BranchEnd { join_idx });
                    steps.push(Step::PushEmpty);
                    steps.push(Step::BranchStart { join_idx });
                }
                steps.push(Step::BranchEnd { join_idx });
                steps.push(Step::Eval { expr: then, as_place_write: false });
                steps.push(Step::BranchStart { join_idx });
                steps.push(Step::BranchSnapshot { join_idx, capture_scrutinee: false });
                steps.push(Step::Discard);
                steps.push(Step::Eval { expr: cond, as_place_write: false });
            }
            Expr::Let { pat, init } => {
                steps.push(Step::PushEmpty);
                steps.push(Step::FinishLet { pat, init: Some(init) });
                steps.push(Step::Eval { expr: init, as_place_write: false });
            }
            Expr::Match { scrutinee, arms } => {
                let arm_list = self.body.arm_list(arms)?;
                let join_idx = joins.len();
                joins.push(BranchJoinState {
                    before: Vec::new_in(alloc),
                    joined: Vec::new_in(alloc),
                    result_locals: Vec::new_in(alloc),
                    scrutinee_locals: Vec::new_in(alloc),
                    any_done: false,
                });
                steps.push(Step::BranchFinish { expr: e, join_idx });
                for arm in arm_list.iter().rev() {
                    steps.push(Step::BranchEnd { join_idx });
                    steps.push(Step::Eval { expr: arm.body, as_place_write: false });
                    if let Some(g) = arm.guard {
                        steps.push(Step::Discard);
                        steps.push(Step::Eval { expr: g, as_place_write: false });
                    }
                    steps.push(Step::BindArmPat { pat: arm.pat, scrutinee, join_idx });
                    steps.push(Step::BranchStart { join_idx });
                }
                steps.push(Step::BranchSnapshot { join_idx, capture_scrutinee: true });
                steps.push(Step::Eval { expr: scrutinee, as_place_write: false });
            }
            Expr::Loop { body } => {
                let saved_idx = loop_saves.len();
                let mut saved = Vec::with_capacity_in(state.len(), alloc);
                saved.extend_from_slice(state);
                loop_saves.push(saved);
                steps.push(Step::LoopAfter { body, saved_idx, do_count_before: do_count, pass: 0 });
                steps.push(Step::Eval { expr: body, as_place_write: false });
            }
            Expr::Break { value, .. } | Expr::Return { value } => {
                if let Some(v) = value {
                    steps.push(Step::Combine { expr: e, count: 1, check_ty: true });
                    steps.push(Step::Eval { expr: v, as_place_write: false });
                } else {
                    val_marks.push(val_pool.len());
                }
            }
            Expr::Become { value } => {
                steps.push(Step::Combine { expr: e, count: 1, check_ty: true });
                steps.push(Step::Eval { expr: value, as_place_write: false });
            }
            Expr::Call { callee, args } => {
                let arg_list = self.body.expr_list(args)?;
                steps.push(Step::Combine { expr: e, count: (1 + arg_list.len()) as u32, check_ty: true });
                for &a in arg_list.iter().rev() {
                    steps.push(Step::Eval { expr: a, as_place_write: false });
                }
                steps.push(Step::Eval { expr: callee, as_place_write: false });
            }
            Expr::MethodCall { receiver, args, .. } => {
                let arg_list = self.body.expr_list(args)?;
                steps.push(Step::Combine { expr: e, count: (1 + arg_list.len()) as u32, check_ty: true });
                for &a in arg_list.iter().rev() {
                    steps.push(Step::Eval { expr: a, as_place_write: false });
                }
                steps.push(Step::Eval { expr: receiver, as_place_write: false });
            }
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                if autoref_local.is_some() {
                    steps.push(Step::FinishRef { operand: e });
                }
                steps.push(Step::Combine { expr: e, count: 1, check_ty: !as_place_write });
                steps.push(Step::Eval { expr: base, as_place_write });
            }
            Expr::Unary { op, operand } => {
                // `*r = val` reads `r` even when `*r` is the LHS of an assignment.
                let inner_write = as_place_write && op != UnOp::Deref;
                steps.push(Step::Combine { expr: e, count: 1, check_ty: true });
                steps.push(Step::Eval { expr: operand, as_place_write: inner_write });
            }
            Expr::Binary { lhs, rhs, .. } | Expr::AssignOp { lhs, rhs, .. } => {
                steps.push(Step::Combine { expr: e, count: 2, check_ty: true });
                steps.push(Step::Eval { expr: rhs, as_place_write: false });
                steps.push(Step::Eval { expr: lhs, as_place_write: false });
            }
            Expr::Assign { lhs, rhs } => {
                steps.push(Step::FinishAssign { lhs });
                steps.push(Step::Eval { expr: rhs, as_place_write: false });
                steps.push(Step::Eval { expr: lhs, as_place_write: true });
            }
            Expr::Ref { operand, .. } | Expr::RawRef { operand, .. } => {
                steps.push(Step::FinishRef { operand });
                steps.push(Step::Eval { expr: operand, as_place_write: false });
            }
            Expr::Cast { operand, .. } => {
                steps.push(Step::Combine { expr: e, count: 1, check_ty: true });
                steps.push(Step::Eval { expr: operand, as_place_write: false });
            }
            Expr::Tuple { elems } | Expr::Array { elems } | Expr::FormatArgs { args: elems, .. } => {
                let list = self.body.expr_list(elems)?;
                steps.push(Step::Combine { expr: e, count: list.len() as u32, check_ty: true });
                for &x in list.iter().rev() {
                    steps.push(Step::Eval { expr: x, as_place_write: false });
                }
            }
            Expr::Repeat { elem, .. } => {
                steps.push(Step::Combine { expr: e, count: 1, check_ty: true });
                steps.push(Step::Eval { expr: elem, as_place_write: false });
            }
            Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                let inits = self.body.field_init_list(fields)?;
                let base_expr = base.expr();
                let count = inits.len() + usize::from(base_expr.is_some());
                steps.push(Step::Combine { expr: e, count: count as u32, check_ty: true });
                if let Some(b) = base_expr {
                    steps.push(Step::Eval { expr: b, as_place_write: false });
                }
                for f in inits.iter().rev() {
                    steps.push(Step::Eval { expr: f.expr, as_place_write: false });
                }
            }
            Expr::Index { base, index } => {
                if autoref_local.is_some() {
                    steps.push(Step::FinishRef { operand: e });
                }
                steps.push(Step::Combine { expr: e, count: 2, check_ty: !as_place_write });
                steps.push(Step::Eval { expr: index, as_place_write: false });
                steps.push(Step::Eval { expr: base, as_place_write });
            }
            Expr::Await(operand) => {
                steps.push(Step::Combine { expr: e, count: 1, check_ty: true });
                steps.push(Step::Eval { expr: operand, as_place_write: false });
            }
            Expr::Do(operand) => {
                steps.push(Step::FinishDo { expr: e });
                steps.push(Step::Eval { expr: operand, as_place_write: false });
            }
            Expr::Defer(body) => {
                if let Some(scope_defers) = defer_scopes.last_mut() {
                    // `defer { body }` evaluates to `()` immediately and executes `body` at its
                    // enclosing block's exit (LIFO), catching both borrows held across `.do`
                    // inside the enclosing scope and borrows crossing `.do` inside `body`.
                    val_marks.push(val_pool.len());
                    scope_defers.push(body);
                } else {
                    steps.push(Step::PushEmpty);
                    steps.push(Step::Discard);
                    steps.push(Step::Eval { expr: body, as_place_write: false });
                }
            }
            Expr::Asm { operands, .. } => {
                let ops = self.body.asm_operand_list(operands)?;
                let mut count = 0u32;
                for o in ops.iter() {
                    count += 1;
                    if o.out_expr.is_some() {
                        count += 1;
                    }
                }
                steps.push(Step::Combine { expr: e, count, check_ty: true });
                for o in ops.iter().rev() {
                    if let Some(out) = o.out_expr {
                        steps.push(Step::Eval { expr: out, as_place_write: true });
                    }
                    steps.push(Step::Eval { expr: o.expr, as_place_write: false });
                }
            }
        }
        Ok(())
    }

    /// Binds every `Pat::Bind` inside `pat` to its borrowed local(s) in `state`.
    fn bind_pat_borrows(
        &mut self,
        root_pat: PatId,
        init_locals: &[PatId],
        init_root: Option<PatId>,
        state: &mut Vec<BorrowEdge, Alloc<'a>>,
    ) -> Result<(), Stop> {
        let alloc = self.alloc;
        let mut work: Vec<PatId, Alloc<'a>> = Vec::new_in(alloc);
        work.push(root_pat);
        let mut seen = 0usize;
        while let Some(p) = work.pop() {
            seen += 1;
            if seen > self.body.pats.len() + 1 {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            match self.body.pat(p)? {
                Pat::Missing | Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) => {}
                Pat::Bind { ann, sub, .. } => {
                    // Shadowing / re-binding clears any prior edges on this `PatId`.
                    state.retain(|edge| edge.holder != p);
                    if ann.by_ref.is_some() {
                        if let Some(local) = init_root {
                            push_edge(state, BorrowEdge { holder: p, local, crossed_do: false });
                        }
                    }
                    if self.ty_may_borrow(self.local_ty[p.ix()])? {
                        for &local in init_locals {
                            push_edge(state, BorrowEdge { holder: p, local, crossed_do: false });
                        }
                    }
                    if let Some(s) = sub {
                        work.push(s);
                    }
                }
                Pat::Ref { inner, .. } => work.push(inner),
                Pat::Tuple { elems, .. }
                | Pat::TupleStruct { elems, .. }
                | Pat::Slice { elems, .. }
                | Pat::Or { alts: elems } => {
                    for &child in self.body.pat_list(elems)? {
                        work.push(child);
                    }
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    for f in self.body.field_pat_list(fields)? {
                        work.push(f.pat);
                    }
                }
                Pat::Range { lo, hi, .. } => {
                    if let Some(l) = lo {
                        work.push(l);
                    }
                    if let Some(h) = hi {
                        work.push(h);
                    }
                }
            }
        }
        Ok(())
    }

    /// If `e` (peeling parentheses) is a plain local variable `Expr::Local(p)`, returns `Some(p)`.
    fn plain_local(&self, mut e: ExprId) -> Result<Option<PatId>, Stop> {
        for _ in 0..MOST_PEELS {
            match self.body.expr(e)? {
                Expr::Paren(inner) => e = inner,
                Expr::Local(p) => return Ok(Some(p)),
                _ => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Root local variable of a stack place expression (`x`, `x.f`, `x.0`, `x[i]`), stopping at
    /// any explicit or implicit dereference (since dereferencing leaves the local's stack slot).
    fn place_root_local(&self, mut e: ExprId) -> Result<Option<PatId>, Stop> {
        for _ in 0..MOST_PEELS {
            match self.body.expr(e)? {
                Expr::Paren(inner) => e = inner,
                Expr::Local(p) => return Ok(Some(p)),
                Expr::Field { base, .. }
                | Expr::TupleField { base, .. }
                | Expr::Index { base, .. } => {
                    if self.has_deref_adjust(base) {
                        return Ok(None);
                    }
                    e = base;
                }
                _ => return Ok(None),
            }
        }
        Ok(None)
    }

    fn has_deref_adjust(&self, e: ExprId) -> bool {
        let kinds = self.adjust.column::<1>();
        self.adjust.rows_rev(e).any(|i| matches!(kinds[i], AdjustKind::Deref { .. }))
    }

    fn has_borrow_adjust(&self, e: ExprId) -> bool {
        let kinds = self.adjust.column::<1>();
        self.adjust.rows_rev(e).any(|i| matches!(kinds[i], AdjustKind::Borrow { .. }))
    }

    /// Whether a settled type `ty` can hold a reference/pointer (i.e. a borrow of a local).
    /// Returns `false` for pure scalars (`u32`, `bool`, `()`, fn items/ptrs) and ADTs whose fields
    /// are all pure scalars, preventing false positives on `let y = *r;` or `let n = x.len();`.
    fn ty_may_borrow(&mut self, ty: Ty) -> Result<bool, Stop> {
        let alloc = self.alloc;
        let mut work: Vec<Ty, Alloc<'a>> = Vec::new_in(alloc);
        let mut seen_adts: Vec<ItemKey, Alloc<'a>> = Vec::new_in(alloc);
        work.push(ty);
        let mut steps = 0usize;
        while let Some(cur) = work.pop() {
            steps += 1;
            if steps > MOST_TY_NODES {
                return Ok(true);
            }
            let cur = self.ty.shallow_resolve(cur);
            match self.view(cur) {
                TyView::Bool
                | TyView::Char
                | TyView::Int(_)
                | TyView::Uint(_)
                | TyView::Float(_)
                | TyView::Never
                | TyView::FnDef(..)
                | TyView::FnPtr(..)
                | TyView::ConstValue { .. }
                | TyView::ConstParam(_)
                | TyView::ConstInfer(_)
                | TyView::ExTrait(..)
                | TyView::ExProj { .. }
                | TyView::ExAuto(_) => {}
                TyView::Ref(..)
                | TyView::Ptr(..)
                | TyView::Str
                | TyView::Param(_)
                | TyView::Alias { .. }
                | TyView::Opaque(..)
                | TyView::Dynamic(_)
                | TyView::Closure(..)
                | TyView::ClosureOf { .. }
                | TyView::Future(..)
                | TyView::CoroutineOf { .. }
                | TyView::Infer(_)
                | TyView::Error
                | TyView::Foreign(_) => return Ok(true),
                TyView::Slice(elem) | TyView::Array(elem, _) | TyView::Pat(elem, _) => {
                    work.push(elem);
                }
                TyView::Tuple(list) | TyView::Record(_, list) | TyView::Union(list) => {
                    for i in 0..self.ty.list_len(list) {
                        work.push(self.ty.list_get(list, i));
                    }
                }
                TyView::Adt(adt, args) => {
                    for i in 0..self.ty.list_len(args) {
                        work.push(self.ty.list_get(args, i));
                    }
                    if seen_adts.contains(&adt) {
                        continue;
                    }
                    seen_adts.push(adt);
                    if let Some(def) = self.reads.adt(adt)? {
                        if adt_fields_may_borrow(def.types, def.fields, alloc) {
                            return Ok(true);
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    fn emit_local_borrow(
        &mut self,
        local: PatId,
        local_names: &[Option<Name>],
        at: Range,
        emitted: &mut Vec<(Name, Range), Alloc<'a>>,
    ) {
        let Some(name) = local_names.get(local.ix()).copied().flatten() else {
            return;
        };
        if emitted.contains(&(name, at)) {
            return;
        }
        emitted.push((name, at));
        self.typed_findings.push(TypedFinding::LocalBorrowAcrossDo { local: name, at });
    }
}

/// Inspects an ADT's field `PNode`s to check if any field directly carries a reference/pointer
/// or nested aggregate that may hold a borrow.
fn adt_fields_may_borrow(
    types: crate::portable::PTys<'_>,
    fields: &[crate::world::Field],
    alloc: Alloc<'_>,
) -> bool {
    let mut work: Vec<crate::portable::PTy, Alloc<'_>> = Vec::new_in(alloc);
    for f in fields {
        work.push(f.ty);
    }
    let mut steps = 0usize;
    while let Some(t) = work.pop() {
        steps += 1;
        if steps > MOST_TY_NODES {
            return true;
        }
        let Ok(node) = types.node(t) else { return true };
        match node {
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Never
            | PNode::FnDef(..)
            | PNode::FnPtr(..)
            | PNode::ConstValue { .. }
            | PNode::ConstParam(_) => {}
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                if let Ok(elems) = types.list(l) {
                    work.extend_from_slice(elems);
                }
            }
            PNode::Array(elem, _) | PNode::Slice(elem) | PNode::Pat(elem, _) => {
                work.push(elem);
            }
            // Note: `PNode::Param` is already checked via the ADT's instantiated `args` in
            // `ty_may_borrow`, so a generic `struct Boxed<T>(T)` instantiated at `u32` is pure!
            PNode::Param(_) => {}
            _ => return true,
        }
    }
    false
}

fn push_edge(state: &mut Vec<BorrowEdge, Alloc<'_>>, edge: BorrowEdge) {
    for cur in state.iter_mut() {
        if cur.holder == edge.holder && cur.local == edge.local {
            cur.crossed_do |= edge.crossed_do;
            return;
        }
    }
    state.push(edge);
}

fn merge_states(dst: &mut Vec<BorrowEdge, Alloc<'_>>, src: &[BorrowEdge]) {
    for &edge in src {
        push_edge(dst, edge);
    }
}

fn dedup_tail(pool: &mut Vec<PatId, Alloc<'_>>, base: usize) {
    let mut write = base;
    for read in base..pool.len() {
        let p = pool[read];
        if !pool[base..write].contains(&p) {
            pool[write] = p;
            write += 1;
        }
    }
    pool.truncate(write);
}
