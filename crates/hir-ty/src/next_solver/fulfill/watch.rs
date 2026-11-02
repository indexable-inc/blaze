//! Watch lists for stalled obligations: a round visits only the obligations whose variables
//! were written since they were found stalled, in registration order.
//!
//! `goal_remains_stalled` reads, for a goal, the rows of its `stalled_vars` in their tables,
//! the rows of its `sub_roots` in the sub-unification table, and, when it is stalled on opaque
//! types, the opaque type storage. A row changes only by a write the table logs
//! (`infer::touched`) or by a rollback, which puts back an earlier state and truncates the log.
//! So a pending obligation is either ready, or watched on each row it read when it was last
//! found stalled; draining the logs makes a written row's watchers ready, and a log that lost
//! rows a cursor read makes every watcher of that table ready.
//!
//! The invariant: at every visit, each obligation for which `goal_remains_stalled` is false is
//! ready. Each visit still runs `goal_remains_stalled` first, so the obligations evaluated,
//! their order and every effect are rustc's loop's.

use rustc_next_trait_solver::solve::{GoalEvaluation, GoalStalledOn, HasChanged, SolverDelegateEvalExt};
use rustc_type_ir::{
    Interner,
    solve::{Certainty, GoalStalledOnOpaques, NoSolution, TyOrConstInferVar},
};

use super::{FulfillmentCtxt, NextSolverError, Pending};
use crate::next_solver::{
    DbInterner, SolverContext, global_goals,
    infer::{
        InferCtxt,
        touched::{TouchedAt, VarTable},
    },
};

const NONE: u32 = u32::MAX;
const TABLES: usize = VarTable::ALL.len();

#[derive(Clone, Copy, Debug)]
struct Node {
    row: u32,
    next: u32,
}

/// The watch state of the rows of a [`Pending`]. Rows `[0, known)` are ready (a bit in
/// `ready`), watched (a bit in `watched`), or finished in the current call (a bit in `dead`);
/// later rows are new and ready. A node left in a list by a row woken through another key may
/// wake it again later, which only costs a stall check.
#[derive(Debug)]
pub(super) struct Watches {
    known: u32,
    ready: Vec<u64>,
    watched: Vec<u64>,
    dead: Vec<u64>,
    any_dead: bool,
    /// Per table and variable index, the first node of the variable's list of watchers.
    heads: [Vec<u32>; TABLES],
    /// The rows stalled on the opaque type storage.
    opaque_rows: Vec<u32>,
    nodes: Vec<Node>,
    free: u32,
    /// Nodes in some list, and keys of the rows watched now: a list keeps the nodes of rows
    /// woken through another key until it is drained, so they are rebuilt past twice the keys.
    linked: u32,
    keys: u32,
    cursors: [Option<TouchedAt>; TABLES],
    opaque_seen: Option<u64>,
}

impl Default for Watches {
    fn default() -> Self {
        Watches {
            known: 0,
            ready: Vec::new(),
            watched: Vec::new(),
            dead: Vec::new(),
            any_dead: false,
            heads: Default::default(),
            opaque_rows: Vec::new(),
            nodes: Vec::new(),
            free: NONE,
            linked: 0,
            keys: 0,
            cursors: [None; TABLES],
            opaque_seen: None,
        }
    }
}

impl Watches {
    /// Forgets every watch and makes every row ready, keeping the buffers. The cursors go too,
    /// as the rows may next be evaluated in another inference context.
    pub(super) fn forget(&mut self) {
        self.known = 0;
        self.ready.clear();
        self.watched.clear();
        self.dead.clear();
        self.any_dead = false;
        self.unlink_all();
        self.cursors = [None; TABLES];
        self.opaque_seen = None;
    }

    /// Bytes its buffers hold room for.
    pub(super) fn held(&self) -> usize {
        (self.ready.capacity() + self.watched.capacity() + self.dead.capacity())
            * size_of::<u64>()
            + self.heads.iter().map(|heads| heads.capacity() * size_of::<u32>()).sum::<usize>()
            + self.opaque_rows.capacity() * size_of::<u32>()
            + self.nodes.capacity() * size_of::<Node>()
    }

    fn unlink_all(&mut self) {
        self.watched.fill(0);
        for heads in &mut self.heads {
            heads.clear();
        }
        self.opaque_rows.clear();
        self.nodes.clear();
        self.free = NONE;
        self.linked = 0;
        self.keys = 0;
    }

    /// Makes the rows pushed since the last call ready.
    fn admit(&mut self, len: usize) {
        let words = len.div_ceil(64);
        if self.ready.len() < words {
            // alloc: one bit a pending obligation, grown with them.
            self.ready.resize(words, 0);
            // alloc: as above.
            self.watched.resize(words, 0);
            // alloc: as above.
            self.dead.resize(words, 0);
        }
        for row in self.known as usize..len {
            set(&mut self.ready, row);
        }
        self.known = len as u32;
    }

    /// The first ready row at or after `from`.
    #[inline]
    fn next_ready(&self, from: usize) -> Option<usize> {
        let mut word = from / 64;
        let mut bits = *self.ready.get(word)? & (!0u64 << (from % 64));
        loop {
            if bits != 0 {
                let row = word * 64 + bits.trailing_zeros() as usize;
                return (row < self.known as usize).then_some(row);
            }
            word += 1;
            bits = *self.ready.get(word)?;
        }
    }

    fn finish(&mut self, row: usize) {
        clear(&mut self.ready, row);
        set(&mut self.dead, row);
        self.any_dead = true;
    }

    /// Takes `row` out of the ready rows for a visit; `false` when it finished in this call.
    fn visit(&mut self, row: usize, stalled_on: &Option<GoalStalledOn<DbInterner<'_>>>) -> bool {
        clear(&mut self.ready, row);
        if get(&self.watched, row) {
            clear(&mut self.watched, row);
            self.keys -= stalled_on.as_ref().map_or(0, keys);
        }
        !get(&self.dead, row)
    }

    /// Watches `row` on every row of a table and on the storage `stalled_on` was read from.
    fn watch(&mut self, row: usize, stalled_on: &GoalStalledOn<DbInterner<'_>>) {
        set(&mut self.watched, row);
        let row = row as u32;
        for &var in &stalled_on.stalled_vars {
            let (table, index) = match var {
                TyOrConstInferVar::Ty(vid) => (VarTable::Ty, vid.as_u32()),
                TyOrConstInferVar::TyInt(vid) => (VarTable::Int, vid.as_u32()),
                TyOrConstInferVar::TyFloat(vid) => (VarTable::Float, vid.as_u32()),
                TyOrConstInferVar::Const(vid) => (VarTable::Const, vid.as_u32()),
            };
            self.link(table, index, row);
        }
        for &vid in &stalled_on.sub_roots {
            self.link(VarTable::Sub, vid.as_u32(), row);
        }
        if let GoalStalledOnOpaques::Yes { .. } = stalled_on.opaques {
            // alloc: grown to the most rows stalled on opaque types at once.
            self.opaque_rows.push(row);
        }
        self.keys += keys(stalled_on);
    }

    fn link(&mut self, table: VarTable, index: u32, row: u32) {
        let heads = &mut self.heads[table as usize];
        if heads.len() <= index as usize {
            // alloc: one slot a variable of the table, grown to the most watched.
            heads.resize(index as usize + 1, NONE);
        }
        let next = heads[index as usize];
        let node = if self.free != NONE {
            let node = self.free;
            self.free = self.nodes[node as usize].next;
            self.nodes[node as usize] = Node { row, next };
            node
        } else {
            // alloc: grown to the most watches at once, then reused from `free`.
            self.nodes.push(Node { row, next });
            self.nodes.len() as u32 - 1
        };
        heads[index as usize] = node;
        self.linked += 1;
    }

    fn wake_list(&mut self, mut node: u32) {
        while node != NONE {
            let Node { row, next } = self.nodes[node as usize];
            set(&mut self.ready, row as usize);
            self.nodes[node as usize].next = self.free;
            self.free = node;
            self.linked -= 1;
            node = next;
        }
    }

    /// Makes ready every row watching a row written since the last drain.
    fn drain(&mut self, infcx: &InferCtxt<'_>) {
        let inner = infcx.inner.borrow();
        for table in VarTable::ALL {
            let t = table as usize;
            let log = inner.touched(table);
            match log.since(self.cursors[t]) {
                Some(rows) => {
                    for written in rows {
                        let Some(head) = self.heads[t].get_mut(written.index as usize) else {
                            continue;
                        };
                        let node = std::mem::replace(head, NONE);
                        self.wake_list(node);
                    }
                }
                None => {
                    for at in 0..self.heads[t].len() {
                        let node = std::mem::replace(&mut self.heads[t][at], NONE);
                        self.wake_list(node);
                    }
                }
            }
            self.cursors[t] = Some(log.now());
        }
        let opaques = inner.opaque_type_storage.changes();
        if self.opaque_seen != Some(opaques) {
            for &row in &self.opaque_rows {
                set(&mut self.ready, row as usize);
            }
            self.opaque_rows.clear();
            self.opaque_seen = Some(opaques);
        }
    }
}

/// The keys a watch on `stalled_on` links.
fn keys(stalled_on: &GoalStalledOn<DbInterner<'_>>) -> u32 {
    (stalled_on.stalled_vars.len() + stalled_on.sub_roots.len()) as u32
}

#[inline]
fn set(bits: &mut [u64], at: usize) {
    bits[at / 64] |= 1 << (at % 64);
}

#[inline]
fn clear(bits: &mut [u64], at: usize) {
    bits[at / 64] &= !(1 << (at % 64));
}

#[inline]
fn get(bits: &[u64], at: usize) -> bool {
    bits[at / 64] & (1 << (at % 64)) != 0
}

impl<'db> Pending<'db> {
    /// Drops the rows finished in this call, keeping the others in order, and watches the kept
    /// rows that are not ready again under their new indices.
    fn compact(&mut self) {
        let watches = &mut self.watches;
        let mut kept = 0;
        for at in 0..self.predicate.len() {
            if get(&watches.dead, at) {
                continue;
            }
            self.cause[kept] = self.cause[at];
            self.param_env[kept] = self.param_env[at];
            self.predicate[kept] = self.predicate[at];
            self.recursion_depth[kept] = self.recursion_depth[at];
            // `kept <= at`, and the rows before `at` are all decided.
            self.stalled_on.swap(kept, at);
            let ready = get(&watches.ready, at);
            clear(&mut watches.ready, at);
            if ready {
                set(&mut watches.ready, kept);
            }
            kept += 1;
        }
        self.cause.truncate(kept);
        self.param_env.truncate(kept);
        self.predicate.truncate(kept);
        self.recursion_depth.truncate(kept);
        self.stalled_on.truncate(kept);
        self.rewatch();
    }

    /// Rebuilds every list from the rows that are not ready, which are watched on what they
    /// are stalled on. The cursors stay: nothing was written since the last drain.
    fn rewatch(&mut self) {
        let watches = &mut self.watches;
        watches.unlink_all();
        watches.dead.fill(0);
        watches.any_dead = false;
        watches.known = self.predicate.len() as u32;
        for at in 0..self.predicate.len() {
            if get(&watches.ready, at) {
                continue;
            }
            let stalled_on = self.stalled_on[at]
                .as_ref()
                .expect("a pending obligation that is not ready was found stalled");
            watches.watch(at, stalled_on);
        }
    }
}

impl<'db> FulfillmentCtxt<'db> {
    /// rustc's loop: rounds over the pending obligations in order until one changes nothing.
    /// An obligation that made inference progress more often than the recursion limit is
    /// rustc's fatal fulfillment overflow; here it ends the loop with an
    /// [`NextSolverError::Overflow`].
    pub(crate) fn try_evaluate_obligations(
        &mut self,
        infcx: &InferCtxt<'db>,
    ) -> Vec<NextSolverError<'db>> {
        let mut errors = Vec::new(); // alloc: empty unless a goal fails
        self.try_evaluate_obligations_with(infcx, &mut |error| errors.push(error));
        errors
    }

    /// [`Self::try_evaluate_obligations`] handing each error to `on_error` as it is found, for
    /// a caller that needs only a verdict or keeps the errors elsewhere; whether there was one.
    pub(crate) fn try_evaluate_obligations_with(
        &mut self,
        infcx: &InferCtxt<'db>,
        on_error: &mut impl FnMut(NextSolverError<'db>),
    ) -> bool {
        assert_eq!(self.usable_in_snapshot, infcx.num_open_snapshots());
        if self.fixpoint == Some(infcx.state_changes()) {
            return false;
        }
        self.fixpoint = None;
        #[cfg(test)]
        if tests::old_rounds() {
            let errors = self.try_evaluate_obligations_old(infcx);
            let any = !errors.is_empty();
            errors.into_iter().for_each(&mut *on_error);
            return any;
        }
        let delegate = <&SolverContext<'db>>::from(infcx);
        let recursion_limit = infcx.interner.recursion_limit();
        let mut any = false;
        let mut overflowed = false;
        let pending = &mut self.pending;
        pending.watches.admit(pending.len());
        pending.watches.drain(infcx);
        loop {
            let mut any_changed = false;
            let round_start = infcx.state_changes();

            let mut from = 0;
            while let Some(at) = pending.watches.next_ready(from) {
                from = at + 1;
                if !pending.watches.visit(at, &pending.stalled_on[at]) {
                    continue;
                }
                // Common case: still stalled; watch the obligation.
                if let Some(stalled_on) = &pending.stalled_on[at]
                    && delegate.goal_remains_stalled(stalled_on)
                {
                    pending.watches.watch(at, stalled_on);
                    continue;
                }

                let mut obligation = pending.obligation(at);
                let goal = obligation.as_goal();
                let span = obligation.cause.span();
                let result = global_goals::evaluate_root_goal(
                    delegate,
                    goal,
                    span,
                    pending.stalled_on[at].take(),
                );
                infcx.inspect_evaluated_obligation(&obligation, &result, || {
                    Some(delegate.evaluate_root_goal_for_proof_tree(goal, span).1)
                });
                pending.watches.drain(infcx);
                let GoalEvaluation { goal, certainty, has_changed, stalled_on } = match result {
                    Ok(result) => result,
                    Err(NoSolution) => {
                        any = true;
                        on_error(NextSolverError::TrueError(obligation));
                        pending.watches.finish(at);
                        continue;
                    }
                };

                // We've resolved the goal in `evaluate_root_goal`, avoid redoing this work
                // in the next iteration. This does not resolve the inference variables
                // constrained by evaluating the goal.
                obligation.predicate = goal.predicate;
                pending.predicate[at] = goal.predicate;
                if has_changed == HasChanged::Yes {
                    if obligation.recursion_depth as usize > recursion_limit {
                        // We limit the total count of inference progress to avoid hangs, and
                        // don't try to recover from this, as rustc doesn't.
                        any = true;
                        on_error(NextSolverError::Overflow(obligation));
                        pending.watches.finish(at);
                        overflowed = true;
                        break;
                    }
                    // A recursion limit of `u32::MAX` must not overflow the count; see rustc's
                    // comment on the old solver's recursion depth.
                    pending.recursion_depth[at] = obligation.recursion_depth.saturating_add(1);
                    any_changed = true;
                }

                match certainty {
                    Certainty::Yes => pending.watches.finish(at),
                    Certainty::Maybe(_) => {
                        // Checked again next round, as rustc's loop runs to a fixpoint.
                        pending.stalled_on[at] = stalled_on;
                        set(&mut pending.watches.ready, at);
                    }
                }
            }

            if overflowed || !any_changed {
                if !overflowed && infcx.state_changes() == round_start {
                    self.fixpoint = Some(round_start);
                }
                break;
            }
        }

        let watches = &pending.watches;
        if watches.any_dead {
            pending.compact();
        } else if watches.linked > 2 * watches.keys + 64 {
            pending.rewatch();
        }
        any
    }
}

#[cfg(test)]
pub(crate) mod tests;
