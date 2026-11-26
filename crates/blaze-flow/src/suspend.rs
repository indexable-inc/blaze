//! Which locals a coroutine body saves across its suspensions (port27/tempo-run/ASYNC.md D-A1,
//! prompt P1): the flow-form half of the coroutine layout, known per BODY before any instance.
//!
//! The Reference, "Async blocks": "The future type that rustc generates is roughly equivalent to
//! an enum with one variant per await point, where each variant stores the data needed to resume
//! from its corresponding point." And "Await expressions": "If the call to poll returns
//! Poll::Pending, then the future returns Poll::Pending, suspending its state so that, when the
//! surrounding async context is re-polled, execution returns to step 3". `Future::poll` takes
//! `self: Pin<&mut Self>` (core/src/future/future.rs:113), so the state never moves once polled
//! and a loan of a saved local stays valid across the suspension: a local borrowed before a
//! suspension is saved even when no later statement names it, because the borrow may still be
//! read through.
//!
//! The rule (D-A1): a local is saved when, at some `Suspend`, it is maybe storage-live AND (live
//! by use on its resume or cancel path, OR maybe borrowed since its `StorageLive`). Local 0, the
//! return place, is never saved.
//!
//! Three bit problems over the flow, solved at block grain (one row per block entry, a block's
//! statements walked in order), each joined by union:
//! - maybe storage-live, forward: `StorageLive` gens, `StorageDead` kills; the arguments, and a
//!   local with no storage statement anywhere in the body, are live from entry;
//! - maybe borrowed, forward: a `Value::Borrow` of a place whose base is local `l` and whose
//!   projections hold no `Deref` gens `l` (a borrow through a pointer borrows memory outside the
//!   local's storage); `StorageDead(l)` kills;
//! - live by use, backward: a read gens (an operand, a borrowed place, a partly written
//!   destination, an activation, a switch, a return's place), a whole write and a storage
//!   statement kill; a `Drop(p)` reads `p` only where some part of `p` may be initialized (the
//!   Reference, "Destructors": "When an initialized variable or temporary goes out of scope, its
//!   destructor is run or it is dropped", so the flow's unconditional scope-end `Drop` of a
//!   moved-out local reads nothing). A `Suspend` joins its resume and cancel successors.
//!
//! Each fixed point counts its passes against `blocks * locals + 1` (rule 18): every pass but the
//! last adds at least one bit to some block's row, and there are `blocks * locals` bits.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::bits::{self, BlockBits};
use crate::flow::{Block, Bound, Flow, Local, Operand, Place, Stmt, StmtKind, Value};
use crate::moves::{Inits, for_each_operand, inits};
use crate::record::Plant;

/// The locals a coroutine body saves (coroutines.md D-A1 of port27/tempo-run/ASYNC.md):
/// at some `Suspend`, maybe storage-live and (live by use on its resume or cancel path, or
/// maybe borrowed since its `StorageLive`).
pub struct Saved<A: Allocator> {
    /// One bit per local of the flow (`flow.locals.div_ceil(64)` words): bit `l` is set when
    /// local `l` is saved at some `Suspend`. Bit 0 is never set.
    pub bits: Vec<u64, A>,
    /// The number of `Suspend` statements in the body.
    pub suspends: u32,
}

impl<A: Allocator> Saved<A> {
    /// Whether local `l` is saved at some suspension; a local the flow does not have is not.
    pub fn is_saved(&self, l: Local) -> bool {
        l.index() < self.bits.len() * 64 && bits::test(&self.bits, l.index())
    }
}

/// A planted bug for the tests' controls.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SavedPlant {
    None,
    /// The maybe-borrowed rule is left out: a borrowed local is saved only if used later.
    NoBorrows,
}

/// Maximum number of saved locals for which [`saved_plan`] builds a quadratic storage-conflict
/// bit matrix (4096 locals -> 2 MiB max); larger bodies fall back to empty conflicts (no overlap).
const MAX_CONFLICT_LOCALS: usize = 4096;

/// Saved locals together with their pairwise storage-conflict bit matrix (row `i`, bit `j` set
/// iff saved locals `locals[i]` and `locals[j]` may be storage-live simultaneously at some point
/// in the flow). An empty `conflicts` slice when `locals` is non-empty means the body exceeded
/// [`MAX_CONFLICT_LOCALS`] and callers must not overlap its saved locals.
pub struct SavedPlan<A: Allocator> {
    pub saved: Saved<A>,
    pub locals: Vec<Local, A>,
    pub conflicts: Vec<u64, A>,
}

/// The saved locals of `flow`, the union over its `Suspend` statements.
pub fn saved<A: Allocator + Copy>(flow: &Flow<A>, alloc: A) -> Result<Saved<A>, Bound> {
    saved_with(flow, true, false, alloc).map(|p| p.saved)
}

/// The saved locals of `flow` plus their pairwise storage-conflict matrix for overlapping
/// coroutine state layout.
pub fn saved_plan<A: Allocator + Copy>(flow: &Flow<A>, alloc: A) -> Result<SavedPlan<A>, Bound> {
    saved_with(flow, true, true, alloc)
}

#[cfg(test)]
pub(crate) fn saved_planted<A: Allocator + Copy>(
    flow: &Flow<A>,
    plant: SavedPlant,
    alloc: A,
) -> Result<Saved<A>, Bound> {
    saved_with(flow, plant != SavedPlant::NoBorrows, false, alloc).map(|p| p.saved)
}

fn zeroed<A: Allocator>(words: usize, alloc: A) -> Vec<u64, A> {
    let mut v = Vec::with_capacity_in(words, alloc);
    v.resize(words, 0u64);
    v
}

fn saved_with<A: Allocator + Copy>(flow: &Flow<A>, borrows: bool, want_conflicts: bool, alloc: A) -> Result<SavedPlan<A>, Bound> {
    let locals = usize::try_from(flow.locals).unwrap_or(usize::MAX);
    let words = locals.div_ceil(64);
    let mut out = zeroed(words, alloc);
    let suspends =
        u32::try_from(flow.stmts.iter().filter(|row| matches!(row.kind, StmtKind::Suspend { .. })).count()).unwrap_or(u32::MAX);
    if suspends == 0 {
        return Ok(SavedPlan {
            saved: Saved { bits: out, suspends },
            locals: Vec::new_in(alloc),
            conflicts: Vec::new_in(alloc),
        });
    }
    let blocks_u64 = u64::try_from(flow.blocks.len()).unwrap_or(u64::MAX);
    let limit = blocks_u64.saturating_mul(u64::from(flow.locals)).saturating_add(1);

    // Maybe storage-live on entry: the arguments, and every local no storage statement names.
    let mut named = zeroed(words, alloc);
    for row in flow.stmts.iter() {
        if let StmtKind::StorageLive(l) | StmtKind::StorageDead(l) = row.kind {
            bits::put(&mut named, l.index(), true);
        }
    }
    let mut entry = zeroed(words, alloc);
    let args = usize::try_from(flow.args).unwrap_or(usize::MAX);
    for l in 1..locals {
        if l <= args || !bits::test(&named, l) {
            bits::put(&mut entry, l, true);
        }
    }
    let storage = forward(flow, &entry, storage_step, limit, "maybe storage-live passes", alloc)?;
    let none = zeroed(words, alloc);
    let borrowed = forward(flow, &none, borrow_step, limit, "maybe borrowed passes", alloc)?;
    let init = inits(flow, Plant::None, alloc)?;
    let live = live_by_use(flow, &init, limit, alloc)?;

    let mut st = zeroed(words, alloc);
    let mut bo = zeroed(words, alloc);
    let mut keep = zeroed(words, alloc);
    for (s, row) in flow.stmts.iter().enumerate() {
        let StmtKind::Suspend { resume, cancel } = row.kind else {
            continue;
        };
        // A `Suspend` is its block's terminator: the statements before it in the block are
        // `range.start..s`.
        let b = flow.stmt_block[s];
        let range = flow.blocks[b.index()];
        st.copy_from_slice(storage.row(b));
        bo.copy_from_slice(borrowed.row(b));
        let s_u32 = u32::try_from(s).unwrap_or(u32::MAX);
        for t in range.start..s_u32 {
            storage_step(flow, Stmt(t), &mut st);
            borrow_step(flow, Stmt(t), &mut bo);
        }
        keep.copy_from_slice(live.row(resume));
        bits::union_into(&mut keep, live.row(cancel));
        if borrows {
            bits::union_into(&mut keep, &bo);
        }
        bits::intersect_into(&mut keep, &st);
        bits::union_into(&mut out, &keep);
    }
    if locals > 0 {
        bits::put(&mut out, 0, false);
    }
    let saved = Saved { bits: out, suspends };
    if !want_conflicts {
        return Ok(SavedPlan { saved, locals: Vec::new_in(alloc), conflicts: Vec::new_in(alloc) });
    }
    let mut saved_locals = Vec::new_in(alloc);
    for l in (flow.args + 1)..flow.locals {
        let local = Local(l);
        if saved.is_saved(local) {
            saved_locals.push(local);
        }
    }
    let conflicts = compute_conflicts(flow, &storage, &saved_locals, alloc);
    Ok(SavedPlan { saved, locals: saved_locals, conflicts })
}

fn each_bit(words: &[u64], mut f: impl FnMut(usize)) {
    for (w, &word) in words.iter().enumerate() {
        let mut rest = word;
        while rest != 0 {
            f(w * 64 + rest.trailing_zeros() as usize);
            rest &= rest - 1;
        }
    }
}

/// Pairwise storage-conflict bit matrix over `saved_locals`: row `i` holds bit `j` when some
/// point of `flow` has both `saved_locals[i]` and `saved_locals[j]` maybe storage-live (or
/// accesses one while the other is storage-live).
fn compute_conflicts<A: Allocator + Copy>(
    flow: &Flow<A>,
    storage: &BlockBits<A>,
    saved_locals: &[Local],
    alloc: A,
) -> Vec<u64, A> {
    let n = saved_locals.len();
    if n == 0 || n > MAX_CONFLICT_LOCALS {
        return Vec::new_in(alloc);
    }
    let locals = usize::try_from(flow.locals).unwrap_or(usize::MAX);
    let mut ordinal: Vec<u32, A> = Vec::with_capacity_in(locals, alloc);
    ordinal.resize(locals, u32::MAX);
    for (i, &local) in saved_locals.iter().enumerate() {
        if local.index() < locals {
            ordinal[local.index()] = i as u32;
        }
    }
    let rw = n.div_ceil(64);
    let mut matrix = zeroed(n * rw, alloc);
    let mut cur = zeroed(rw, alloc);
    let mark_live = |local: Local, ordinal: &[u32], cur: &mut [u64], matrix: &mut [u64]| {
        let ord = ordinal.get(local.index()).copied().unwrap_or(u32::MAX);
        if ord == u32::MAX {
            return;
        }
        let i = ord as usize;
        let (word, bit) = (i / 64, 1u64 << (i % 64));
        if cur[word] & bit != 0 {
            return;
        }
        cur[word] |= bit;
        let row = i * rw;
        for w in 0..rw {
            matrix[row + w] |= cur[w];
        }
        each_bit(cur, |j| matrix[j * rw + word] |= bit);
    };
    for b in 0..flow.blocks.len() {
        cur.fill(0);
        let blk = Block(b as u32);
        let st_row = storage.row(blk);
        for (i, &local) in saved_locals.iter().enumerate() {
            if bits::test(st_row, local.index()) {
                cur[i / 64] |= 1u64 << (i % 64);
            }
        }
        each_bit(&cur, |i| {
            let row = i * rw;
            for w in 0..rw {
                matrix[row + w] |= cur[w];
            }
        });
        let range = flow.blocks[b];
        for s in range.start..range.end {
            match flow.stmt(Stmt(s)).kind {
                StmtKind::StorageLive(l) => mark_live(l, &ordinal, &mut cur, &mut matrix),
                StmtKind::StorageDead(l) => {
                    let ord = ordinal.get(l.index()).copied().unwrap_or(u32::MAX);
                    if ord != u32::MAX {
                        let i = ord as usize;
                        cur[i / 64] &= !(1u64 << (i % 64));
                    }
                }
                StmtKind::Assign { dest, value } => {
                    mark_live(flow.place(dest).local, &ordinal, &mut cur, &mut matrix);
                    for_each_operand(flow, value, |o| {
                        if let Operand::Copy(p) | Operand::Move(p) = o {
                            mark_live(flow.place(p).local, &ordinal, &mut cur, &mut matrix);
                        }
                    });
                    if let Value::Borrow { place, .. } = value {
                        mark_live(flow.place(place).local, &ordinal, &mut cur, &mut matrix);
                    }
                }
                StmtKind::Call { args, dest } => {
                    mark_live(flow.place(dest).local, &ordinal, &mut cur, &mut matrix);
                    for o in flow.ops(args) {
                        if let Operand::Copy(p) | Operand::Move(p) = *o {
                            mark_live(flow.place(p).local, &ordinal, &mut cur, &mut matrix);
                        }
                    }
                }
                StmtKind::Drop(p) => mark_live(flow.place(p).local, &ordinal, &mut cur, &mut matrix),
                StmtKind::Activate(l) => mark_live(flow.place(flow.loan(l).place).local, &ordinal, &mut cur, &mut matrix),
                StmtKind::Switch { on, .. } => mark_live(flow.place(on).local, &ordinal, &mut cur, &mut matrix),
                _ => {}
            }
        }
    }
    matrix
}

fn storage_step<A: Allocator>(flow: &Flow<A>, s: Stmt, row: &mut [u64]) {
    match flow.stmt(s).kind {
        StmtKind::StorageLive(l) => bits::put(row, l.index(), true),
        StmtKind::StorageDead(l) => bits::put(row, l.index(), false),
        _ => {}
    }
}

fn borrow_step<A: Allocator>(flow: &Flow<A>, s: Stmt, row: &mut [u64]) {
    match flow.stmt(s).kind {
        StmtKind::Assign { value: Value::Borrow { place, .. }, .. } if !flow.deref_below(place, 0) => {
            bits::put(row, flow.place(place).local.index(), true);
        }
        StmtKind::StorageDead(l) => bits::put(row, l.index(), false),
        _ => {}
    }
}

/// A forward union problem at block grain: row `b` is the state on entry to block `b`.
fn forward<A: Allocator + Copy>(
    flow: &Flow<A>,
    entry: &[u64],
    step: fn(&Flow<A>, Stmt, &mut [u64]),
    limit: u64,
    what: &'static str,
    alloc: A,
) -> Result<BlockBits<A>, Bound> {
    let blocks = flow.blocks.len();
    let mut rows = BlockBits::new_in(flow, usize::try_from(flow.locals).unwrap_or(usize::MAX), alloc);
    if blocks == 0 {
        return Ok(rows);
    }
    rows.row_mut(Block(0)).copy_from_slice(entry);
    let mut cur = zeroed(rows.words(), alloc);
    let mut passes = 0u64;
    loop {
        passes += 1;
        if passes > limit {
            return Err(Bound::Passes { what, limit });
        }
        let mut changed = false;
        for b in 0..blocks {
            let blk = Block(b as u32);
            let range = flow.blocks[b];
            // `FlowBuilder::finish` refuses a block without its terminator: every block has a row.
            if range.end <= range.start {
                continue;
            }
            cur.copy_from_slice(rows.row(blk));
            for s in range.start..range.end {
                step(flow, Stmt(s), &mut cur);
            }
            flow.for_each_succ(Stmt(range.end - 1), |t| {
                let to = flow.stmt_block[t.index()];
                changed |= bits::union_into(rows.row_mut(to), &cur);
            });
        }
        if !changed {
            return Ok(rows);
        }
    }
}

/// Live by use, backward through statement `s`: `row` is live after `s` on entry, before on exit.
fn live_step<A: Allocator>(flow: &Flow<A>, init: &Inits<A>, s: Stmt, row: &mut [u64]) {
    let local = |p: Place| flow.place(p).local.index();
    let read = |o: Operand, row: &mut [u64]| {
        if let Operand::Copy(p) | Operand::Move(p) = o {
            bits::put(row, local(p), true);
        }
    };
    // A write to the whole local kills; a write to a part reads the rest of it.
    let write = |dest: Place, row: &mut [u64]| {
        if flow.place(dest).depth == 0 {
            bits::put(row, local(dest), false);
        }
    };
    let partly = |dest: Place, row: &mut [u64]| {
        if flow.place(dest).depth > 0 {
            bits::put(row, local(dest), true);
        }
    };
    match flow.stmt(s).kind {
        StmtKind::Assign { dest, value } => {
            write(dest, row);
            for_each_operand(flow, value, |o| read(o, row));
            if let Value::Borrow { place, .. } = value {
                bits::put(row, local(place), true);
            }
            partly(dest, row);
        }
        StmtKind::Call { args, dest } => {
            write(dest, row);
            for o in flow.ops(args) {
                read(*o, row);
            }
            partly(dest, row);
        }
        StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => bits::put(row, l.index(), false),
        StmtKind::Drop(p) => {
            if init.drop_may_run(flow, s, p) {
                bits::put(row, local(p), true);
            }
        }
        StmtKind::Activate(l) => bits::put(row, local(flow.loan(l).place), true),
        StmtKind::Switch { on, .. } => bits::put(row, local(on), true),
        // Live across a suspension where the mention dereferences it (loans.rs `uses_and_def`).
        StmtKind::Mention(p) => {
            if let Some(l) = flow.mention_use(p) {
                bits::put(row, l.index(), true);
            }
        }
        StmtKind::Return => bits::put(row, 0, true),
        StmtKind::Goto(_) | StmtKind::Suspend { .. } | StmtKind::Unreachable => {}
    }
}

/// Live by use on entry to each block, backward, passes in reverse block order.
fn live_by_use<A: Allocator + Copy>(
    flow: &Flow<A>,
    init: &Inits<A>,
    limit: u64,
    alloc: A,
) -> Result<BlockBits<A>, Bound> {
    let blocks = flow.blocks.len();
    let mut rows = BlockBits::new_in(flow, usize::try_from(flow.locals).unwrap_or(usize::MAX), alloc);
    let mut cur = zeroed(rows.words(), alloc);
    let mut passes = 0u64;
    loop {
        passes += 1;
        if passes > limit {
            return Err(Bound::Passes { what: "live-by-use passes", limit });
        }
        let mut changed = false;
        for b in (0..blocks).rev() {
            let blk = Block(b as u32);
            let range = flow.blocks[b];
            // `FlowBuilder::finish` refuses a block without its terminator: every block has a row.
            if range.end <= range.start {
                continue;
            }
            cur.fill(0);
            flow.for_each_succ(Stmt(range.end - 1), |t| {
                let from = flow.stmt_block[t.index()];
                bits::union_into(&mut cur, rows.row(from));
            });
            for s in (range.start..range.end).rev() {
                live_step(flow, init, Stmt(s), &mut cur);
            }
            changed |= bits::union_into(rows.row_mut(blk), &cur);
        }
        if !changed {
            return Ok(rows);
        }
    }
}
