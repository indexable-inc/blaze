//! The borrow check (flow.md sections 3 to 5): accesses and conflicts, liveness, two-phase
//! activation, tier one (location-insensitive) and tier two (the Polonius rules as stated).
//!
//! Tier two implements the point-sensitive rules of the Polonius book (rules/loans).
//! Tier one drops the point and `loan_killed_at`, so every
//! fact of tier two is a fact of tier one, and tier one accepting a body means tier two does.
#![blaze::sans_io]

mod components;
mod filter;
mod points;
mod solver;
#[cfg(test)]
mod solver_tests;

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::bits::{self, BitWorklist, Bits};
use crate::flow::{
    Block, BorrowKind, Bound, Flow, Loan, Local, Operand, Origin, Place, Point, Proj, Span, Stmt,
    StmtKind, Value,
};
use crate::moves::{Inits, for_each_operand};
use crate::record::Plant;

/// An access: a place, read or write, deep or shallow (RFC 2094).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Access {
    pub place: Place,
    pub write: bool,
    pub deep: bool,
    /// `StmtKind::Drop`: does not reach behind a reference `Proj::Deref` unless an enclosing
    /// ancestor implements `Drop` (`of_drop_type`).
    pub drop: bool,
    /// An activation does not conflict with its own loan.
    pub except: Option<Loan>,
}

fn access(place: Place, write: bool, deep: bool) -> Access {
    Access { place, write, deep, drop: false, except: None }
}

/// Calls `f` on every access statement `s` makes, at its Start.
pub fn for_each_access<A: Allocator>(flow: &Flow<A>, s: Stmt, mut f: impl FnMut(Access)) {
    let operand = |o: Operand, f: &mut dyn FnMut(Access)| match o {
        Operand::Copy(p) => f(access(p, false, true)),
        Operand::Move(p) => f(access(p, true, true)),
        Operand::Const => {}
    };
    match flow.stmt(s).kind {
        StmtKind::Assign { dest, value } => {
            for_each_operand(flow, value, |o| operand(o, &mut f));
            if let Value::Borrow { kind, place, .. } = value {
                f(match kind {
                    BorrowKind::Shared | BorrowKind::TwoPhase => access(place, false, true),
                    BorrowKind::Mut => access(place, true, true),
                    BorrowKind::Shallow => access(place, false, false),
                });
            }
            f(access(dest, true, false));
        }
        StmtKind::Call { args, dest } => {
            for o in flow.ops(args) {
                operand(*o, &mut f);
            }
            f(access(dest, true, false));
        }
        StmtKind::StorageDead(l) => f(access(flow.local_place(l), true, false)),
        StmtKind::Drop(p) => f(Access { place: p, write: true, deep: true, drop: true, except: None }),
        StmtKind::Activate(l) => {
            f(Access { place: flow.loan(l).place, write: true, deep: true, drop: false, except: Some(l) })
        }
        StmtKind::Switch { on, .. } => f(access(on, false, false)),
        // A `Mention` reads nothing (the Reference, "Wildcard pattern"): no access, so no loan
        // is invalidated by it. Its use of the place's local is liveness's (`uses_and_def`).
        _ => {}
    }
}

/// rustc's `place_projection_conflict` for two constant projections of one array or slice:
/// whether they can never name the same element. Two indices from the same end are disjoint
/// when they differ; one from each end, when the shortest value both match keeps them apart;
/// an index and a subslice, when the index lies outside it. Anything else may overlap.
fn constant_disjoint(x: Proj, y: Proj) -> bool {
    use Proj::{ConstantIndex as Ci, Subslice as Sub};
    match (x, y) {
        (Ci { offset: a, from_end: fa, .. }, Ci { offset: b, from_end: fb, .. }) if fa == fb => a != b,
        (Ci { offset: begin, min_length: m1, from_end: false }, Ci { offset: end, min_length: m2, from_end: true })
        | (Ci { offset: end, min_length: m2, from_end: true }, Ci { offset: begin, min_length: m1, from_end: false }) => begin < m1.max(m2).saturating_sub(end),
        (Ci { offset, from_end: false, .. }, Sub { from, to, from_end: false }) | (Sub { from, to, from_end: false }, Ci { offset, from_end: false, .. }) => offset < from || offset >= to,
        (Ci { offset, from_end: false, .. }, Sub { from, from_end: true, .. }) | (Sub { from, from_end: true, .. }, Ci { offset, from_end: false, .. }) => offset < from,
        (Ci { offset, from_end: true, .. }, Sub { to, from_end: true, .. }) | (Sub { to, from_end: true, .. }, Ci { offset, from_end: true, .. }) => offset <= to,
        _ => false,
    }
}

/// Places `a` (shallow or not) and `b` (shallow or not) overlap: one is a prefix of the other,
/// distinct fields and variants are disjoint, a shallow side does not reach through a `Deref`.
pub fn overlap<A: Allocator>(flow: &Flow<A>, a: Place, a_shallow: bool, b: Place, b_shallow: bool) -> bool {
    let (ra, rb) = (flow.place(a), flow.place(b));
    if ra.local != rb.local {
        return false;
    }
    for d in 1..=ra.depth.min(rb.depth) {
        let (pa, pb) = (flow.ancestor(a, d), flow.ancestor(b, d));
        if pa == pb {
            continue;
        }
        return match (flow.place(pa).proj, flow.place(pb).proj) {
            (Some(Proj::Field(x)), Some(Proj::Field(y))) if x != y => false,
            (Some(Proj::Downcast(x)), Some(Proj::Downcast(y))) if x != y => false,
            (Some(x), Some(y)) if constant_disjoint(x, y) => false,
            _ => true,
        };
    }
    if rb.depth <= ra.depth {
        !b_shallow || !flow.deref_below(a, rb.depth)
    } else {
        !a_shallow || !flow.deref_below(b, ra.depth)
    }
}

/// Access `a` invalidates loan `l` (`loan_invalidated_at`): it writes, or it reads and the loan
/// is mutable or an activated two-phase loan (RFC 2025: reserved "acts exactly like a shared
/// borrow").
pub fn conflicts<A: Allocator>(flow: &Flow<A>, l: Loan, a: Access, activated: bool) -> bool {
    if a.except == Some(l) {
        return false;
    }
    let row = flow.loan(l);
    if !a.write && !(row.kind == BorrowKind::Mut || row.kind == BorrowKind::TwoPhase && activated) {
        return false;
    }
    // Prefix checks when the access is a strict prefix of the borrowed place (`a.place` above
    // `row.place`):
    // 1. A loan behind a `Proj::Deref` of a shared reference (`&T`) or raw pointer
    //    (`of_shared_ref`) does not lock prefixes above that deref: `&T` is `Copy` and mutating,
    //    borrowing or dropping an ancestor holding `&T` cannot invalidate its referent (rustc
    //    `places_conflict`: supporting prefixes stop at a shared-ref / raw-ptr deref).
    // 2. Dropping a value without a `Drop` impl (and whose intermediate fields up to the deref
    //    have no `Drop` impl) never accesses behind a `&`/`&mut` `Proj::Deref` (`Proj::DerefBox`
    //    still conflicts because dropping `Box` frees its referent).
    {
        let (ra, rb) = (flow.place(row.place), flow.place(a.place));
        if ra.local == rb.local && rb.depth < ra.depth {
            let mut prefix = true;
            for d in 1..=rb.depth {
                if flow.ancestor(row.place, d) != flow.ancestor(a.place, d) {
                    prefix = false;
                    break;
                }
            }
            if prefix {
                for d in (rb.depth + 1)..=ra.depth {
                    let step = flow.place(flow.ancestor(row.place, d));
                    if step.of_shared_ref {
                        return false;
                    }
                }
                if a.drop {
                    for d in (rb.depth + 1)..=ra.depth {
                        let step = flow.place(flow.ancestor(row.place, d));
                        if step.of_drop_type {
                            break;
                        }
                        if step.proj == Some(Proj::Deref) {
                            return false;
                        }
                    }
                }
            }
        }
    }
    overlap(flow, row.place, row.kind == BorrowKind::Shallow, a.place, !a.deep)
}

/// `loan_killed_at(l, Start(s))`: "a prefix of the path borrowed in loan `loan` is
/// assigned/overwritten at the point".
pub fn kills<A: Allocator>(flow: &Flow<A>, s: Stmt, l: Loan) -> bool {
    match flow.stmt(s).kind {
        StmtKind::Assign { dest, .. } | StmtKind::Call { dest, .. } => {
            flow.is_prefix(dest, flow.loan(l).place)
        }
        _ => false,
    }
}

#[inline]
fn stmt_def<A: Allocator>(flow: &Flow<A>, s: Stmt) -> Option<Local> {
    match flow.stmt(s).kind {
        StmtKind::Assign { dest, .. } | StmtKind::Call { dest, .. } => {
            (flow.place(dest).depth == 0).then(|| flow.place(dest).local)
        }
        StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => Some(l),
        _ => None,
    }
}

#[inline]
fn for_each_stmt_use_no_drop<A: Allocator>(flow: &Flow<A>, s: Stmt, mut used: impl FnMut(Local)) {
    let local = |p: Place| flow.place(p).local;
    match flow.stmt(s).kind {
        StmtKind::Assign { dest, value } => {
            for_each_operand(flow, value, |o| {
                if let Operand::Copy(p) | Operand::Move(p) = o {
                    used(local(p));
                }
            });
            if let Value::Borrow { place, .. } = value {
                used(local(place));
            }
            if flow.place(dest).depth > 0 {
                used(local(dest));
            }
        }
        StmtKind::Call { args, dest } => {
            for o in flow.ops(args) {
                if let Operand::Copy(p) | Operand::Move(p) = o {
                    used(local(*p));
                }
            }
            if flow.place(dest).depth > 0 {
                used(local(dest));
            }
        }
        StmtKind::Activate(l) => used(local(flow.loan(l).place)),
        StmtKind::Switch { on, .. } => used(local(on)),
        // A mention defines nothing and is an access of nothing (`for_each_access` gives it
        // none); it uses its local only through a `Deref` (`Flow::mention_use`).
        StmtKind::Mention(p) => {
            if let Some(l) = flow.mention_use(p) {
                used(l);
            }
        }
        StmtKind::Return => used(Local(0)),
        _ => {}
    }
}

fn uses_and_def<A: Allocator>(flow: &Flow<A>, init: &Inits<A>, s: Stmt, include_drops: bool, mut used: impl FnMut(Local)) -> Option<Local> {
    // Drop-liveness: origins in dropping positions of a dropped local are drop-live where
    // the drop may run (`Inits::drop_may_run`, as suspend.rs's liveness has it). Non-dropping
    // positions (`flow.local_no_drop_origins`, e.g. `&mut U` in `Result<&mut U, E>`) are
    // excluded when `include_drops` is false.
    if let StmtKind::Drop(p) = flow.stmt(s).kind {
        if include_drops && init.drop_may_run(flow, s, p) {
            used(flow.place(p).local);
        }
        return None;
    }
    for_each_stmt_use_no_drop(flow, s, used);
    stmt_def(flow, s)
}

/// What both tiers read: origins live at each point, and two-phase loans activated on entry.
pub struct Facts<A: Allocator> {
    /// Rows are points, columns origins: `origin_live_on_entry` or a placeholder.
    pub live: Bits<A>,
    /// Rows are statements, columns loans.
    pub active: Bits<A>,
    /// Where each drop may run (the move check's dataflow, unplanted): drop-liveness read it,
    /// and [`loan_parts`] reads it again to find the use that kept a loan live.
    pub init: Inits<A>,
}

/// Compact CSR grouping of `(row_key, value)` pairs, built in two linear passes.
struct Csr<T, A: Allocator> {
    offsets: Vec<u32, A>,
    entries: Vec<T, A>,
}

impl<T: Copy, A: Allocator + Copy> Csr<T, A> {
    fn from_iter(
        rows: usize,
        iter: impl ExactSizeIterator<Item = (usize, T)> + Clone,
        alloc: A,
    ) -> Self {
        let mut offsets = Vec::with_capacity_in(rows + 1, alloc);
        offsets.resize(rows + 1, 0u32);
        let mut entries = Vec::with_capacity_in(iter.len(), alloc);
        for (k, v) in iter.clone() {
            offsets[k] += 1;
            entries.push(v);
        }
        let mut end = 0u32;
        for c in offsets[..rows].iter_mut() {
            end += *c;
            *c = end;
        }
        offsets[rows] = end;
        for (k, v) in iter {
            offsets[k] -= 1;
            entries[offsets[k] as usize] = v;
        }
        Self { offsets, entries }
    }
}

impl<T, A: Allocator> Csr<T, A> {
    #[inline]
    fn row(&self, key: usize) -> &[T] {
        &self.entries[self.offsets[key] as usize..self.offsets[key + 1] as usize]
    }
}

#[inline]
fn fill_all_bits(row: &mut [u64], count: usize) {
    if count == 0 {
        return;
    }
    row.fill(u64::MAX);
    let rem = count % 64;
    if rem != 0
        && let Some(last) = row.last_mut()
    {
        *last = (1u64 << rem) - 1;
    }
}

/// `dst |= src & mask`; says whether `dst` grew.
#[inline]
fn union_masked_into(dst: &mut [u64], src: &[u64], mask: &[u64]) -> bool {
    let mut grew = false;
    for ((d, &s), &m) in dst.iter_mut().zip(src.iter()).zip(mask.iter()) {
        let next = *d | (s & m);
        if next != *d {
            *d = next;
            grew = true;
        }
    }
    grew
}

/// Initializes `(cur_init, init_count, mi_row)` from block `b`'s entry `init` row for the
/// tracked drop-origin locals (`local_to_dl`).
fn init_tracked_block<A: Allocator>(
    flow: &Flow<A>,
    init: &Inits<A>,
    b: usize,
    local_to_dl: &[u32],
    cur_init: &mut [u64],
    init_count: &mut [u32],
    mi_row: &mut [u64],
) {
    cur_init.copy_from_slice(init.init.row(b));
    init_count.fill(0);
    mi_row.fill(0);
    for (w, &word) in cur_init.iter().enumerate() {
        let mut bits_w = word;
        let base = w * 64;
        while bits_w != 0 {
            let bit = usize::try_from(bits_w.trailing_zeros()).unwrap_or(0);
            bits_w &= bits_w - 1;
            if let Some(pr) = flow.places.get(base + bit) {
                let dli = local_to_dl[pr.local.index()];
                if dli != u32::MAX {
                    let dli = dli as usize;
                    init_count[dli] += 1;
                    if init_count[dli] == 1 {
                        bits::put(mi_row, dli, true);
                    }
                }
            }
        }
    }
}

/// Advances `(cur_init, init_count, mi_row)` across statement `s` for tracked drop-origin locals.
fn step_tracked_init<A: Allocator>(
    flow: &Flow<A>,
    init: &Inits<A>,
    s: Stmt,
    local_to_dl: &[u32],
    cur_init: &mut [u64],
    init_count: &mut [u32],
    mi_row: &mut [u64],
) {
    let mut set_sub = |p: Place, on: bool| {
        let dli = local_to_dl[flow.place(p).local.index()];
        if dli == u32::MAX {
            return;
        }
        let dli = dli as usize;
        for &q in init.subtrees.subtree(p) {
            let qi = q as usize;
            let was = bits::test(cur_init, qi);
            if was != on {
                bits::put(cur_init, qi, on);
                if on {
                    init_count[dli] += 1;
                    if init_count[dli] == 1 {
                        bits::put(mi_row, dli, true);
                    }
                } else {
                    init_count[dli] -= 1;
                    if init_count[dli] == 0 {
                        bits::put(mi_row, dli, false);
                    }
                }
            }
        }
    };
    match flow.stmt(s).kind {
        StmtKind::Assign { dest, value } => {
            for_each_operand(flow, value, |o| {
                if let Operand::Move(p) = o {
                    set_sub(p, false);
                }
            });
            set_sub(dest, true);
        }
        StmtKind::Call { args, dest } => {
            for &o in flow.ops(args) {
                if let Operand::Move(p) = o {
                    set_sub(p, false);
                }
            }
            set_sub(dest, true);
        }
        StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => {
            set_sub(flow.local_place(l), false);
        }
        StmtKind::Drop(p) => set_sub(p, false),
        _ => {}
    }
}

fn mark_origins<A: Allocator>(
    in_row: &[u64],
    out_row: &[u64],
    idx_to_local: &[Local],
    origins_by_local: &Csr<Origin, A>,
    live: &mut Bits<A>,
    start_p: usize,
    mid_p: usize,
) {
    for w in 0..in_row.len() {
        let in_w = in_row[w];
        let base = w * 64;
        // Locals in `in_row`: origins live at both Start and Mid.
        let mut b = in_w;
        while b != 0 {
            let idx = base + usize::try_from(b.trailing_zeros()).unwrap_or(0);
            b &= b - 1;
            let local = idx_to_local[idx].index();
            for &origin in origins_by_local.row(local) {
                let oi = origin.index();
                live.set(start_p, oi);
                live.set(mid_p, oi);
            }
        }
        // Locals only in `out_row`: origins live at Mid only.
        let mut b = out_row[w] & !in_w;
        while b != 0 {
            let idx = base + usize::try_from(b.trailing_zeros()).unwrap_or(0);
            b &= b - 1;
            let local = idx_to_local[idx].index();
            for &origin in origins_by_local.row(local) {
                live.set(mid_p, origin.index());
            }
        }
    }
}

/// Local liveness (backward, block-grain postorder worklist over origin-bearing locals; bound
/// `2 * blocks * origin_locals + 1`), origin liveness from it (RFC 2094: "a lifetime L is live at
/// a point P if there is some variable `p` which is live at P, and L appears in the type of `p`"),
/// and activation (forward, block-grain RPO worklist; bound `blocks * loans + blocks + 1`).
pub fn facts<A: Allocator + Copy>(flow: &Flow<A>, alloc: A) -> Result<Facts<A>, Bound> {
    // Where each drop may run: the move check's dataflow, unplanted (a planted move bug is
    // the move check's control, not liveness's).
    let init = crate::moves::inits(flow, Plant::None, alloc)?;
    facts_with_inits(flow, init, alloc)
}

pub(crate) fn facts_with_inits<A: Allocator + Copy>(
    flow: &Flow<A>,
    init: Inits<A>,
    alloc: A,
) -> Result<Facts<A>, Bound> {
    let (nb, n, nl, o) = (flow.blocks.len(), flow.stmts.len(), flow.loans.len(), flow.origins as usize);
    let locals = flow.locals as usize;

    if n == 0 || nb == 0 {
        let live = Bits::new_in(0, o, alloc);
        let active = Bits::new_in(0, nl, alloc);
        return Ok(Facts { live, active, init });
    }

    let has_activate = nl > 0
        && flow.stmts.iter().any(|row| matches!(row.kind, StmtKind::Activate(_)));

    // Fast exit when there are neither origins nor two-phase activations to solve.
    if o == 0 && !has_activate {
        let live = Bits::new_in(2 * n, 0, alloc);
        let active = Bits::new_in(n, nl, alloc);
        return Ok(Facts { live, active, init });
    }

    // Full-CFG block DFS forest postorder (entry block first, then any disconnected blocks).
    // Smallest `post_rank` is backward topological order; `(nb - 1) - post_rank` is forward RPO.
    let eb = flow.stmt_block[flow.entry().index()].index();
    let mut visited = Vec::with_capacity_in(nb, alloc);
    visited.resize(nb, false);
    let mut postorder = Vec::with_capacity_in(nb, alloc);
    let mut dfs = Vec::with_capacity_in(nb, alloc);
    let visit_root = |root: usize,
                      visited: &mut Vec<bool, A>,
                      postorder: &mut Vec<u32, A>,
                      dfs: &mut Vec<(u32, u32), A>| {
        if visited[root] {
            return;
        }
        visited[root] = true;
        dfs.push((root as u32, 0u32));
        while let Some((node, next)) = dfs.last_mut() {
            let b = Block(*node);
            if let Some(t) = flow.nth_block_succ(b, *next) {
                *next += 1;
                let ti = t.index();
                if !visited[ti] {
                    visited[ti] = true;
                    dfs.push((t.0, 0u32));
                }
            } else {
                postorder.push(*node);
                dfs.pop();
            }
        }
    };
    visit_root(eb, &mut visited, &mut postorder, &mut dfs);
    for b in 0..nb {
        visit_root(b, &mut visited, &mut postorder, &mut dfs);
    }
    drop(dfs);
    drop(visited);
    let mut post_rank = Vec::with_capacity_in(nb, alloc);
    post_rank.resize(nb, 0u32);
    for (r, &b) in postorder.iter().enumerate() {
        post_rank[b as usize] = r as u32;
    }

    let mut worklist = BitWorklist::new_in(nb, alloc);
    let mut live = Bits::new_in(2 * n, o, alloc);

    if o > 0 {
        let ow = live.words();
        let mut ph_mask = Vec::with_capacity_in(ow, alloc);
        ph_mask.resize(ow, 0u64);
        let mut has_ph = false;
        for origin in 0..o {
            if flow.placeholder[origin] {
                bits::put(&mut ph_mask, origin, true);
                has_ph = true;
            }
        }
        if has_ph {
            for p in 0..(2 * n) {
                live.row_mut(p).copy_from_slice(&ph_mask);
            }
        }
        drop(ph_mask);

        let origins_by_local = Csr::from_iter(
            locals,
            flow.local_origins.iter().map(|&(l, origin)| (l.index(), origin)),
            alloc,
        );
        // Project liveness to only locals whose types mention at least one origin.
        let mut local_to_ol = Vec::with_capacity_in(locals, alloc);
        local_to_ol.resize(locals, u32::MAX);
        let mut ol_to_local = Vec::new_in(alloc);
        for l in 0..locals {
            if !origins_by_local.row(l).is_empty() {
                local_to_ol[l] = ol_to_local.len() as u32;
                ol_to_local.push(Local(l as u32));
            }
        }
        let ol = ol_to_local.len();

        if ol > 0 {
            // Build block predecessor CSR in two linear passes.
            let mut pred_off = Vec::with_capacity_in(nb + 1, alloc);
            pred_off.resize(nb + 1, 0u32);
            for b in 0..nb {
                flow.for_each_block_succ(Block(b as u32), |tb| {
                    pred_off[tb.index()] += 1;
                });
            }
            let mut total_edges = 0u32;
            for c in pred_off[..nb].iter_mut() {
                total_edges += *c;
                *c = total_edges;
            }
            pred_off[nb] = total_edges;
            let mut pred_entries = Vec::with_capacity_in(total_edges as usize, alloc);
            pred_entries.resize(total_edges as usize, 0u32);
            for b in 0..nb {
                flow.for_each_block_succ(Block(b as u32), |tb| {
                    let ti = tb.index();
                    pred_off[ti] -= 1;
                    pred_entries[pred_off[ti] as usize] = b as u32;
                });
            }
            let block_preds = Csr { offsets: pred_off, entries: pred_entries };

            // Block-grain backward liveness over origin-bearing locals:
            // `live_in(b) = block_use(b) | (live_out(b) & block_trans(b))`.
            let mut block_live_in = Bits::new_in(nb, ol, alloc);
            let mut block_live_out = Bits::new_in(nb, ol, alloc);
            let mut block_trans = Bits::new_in(nb, ol, alloc);
            for b in 0..nb {
                let range = flow.blocks[b];
                fill_all_bits(block_trans.row_mut(b), ol);
                let mut any_use = false;
                for s in range.start..range.end {
                    let stmt = Stmt(s);
                    for_each_stmt_use_no_drop(flow, stmt, |l| {
                        let oi = local_to_ol[l.index()];
                        if oi != u32::MAX && block_trans.get(b, oi as usize) {
                            block_live_in.set(b, oi as usize);
                            any_use = true;
                        }
                    });
                    if let Some(d) = stmt_def(flow, stmt) {
                        let oi = local_to_ol[d.index()];
                        if oi != u32::MAX {
                            block_trans.clear(b, oi as usize);
                        }
                    }
                }
                if any_use {
                    for &pred in block_preds.row(b) {
                        let p = pred as usize;
                        if bits::union_into(block_live_out.row_mut(p), block_live_in.row(b)) {
                            worklist.push(post_rank[p] as usize);
                        }
                    }
                }
            }

            let olw = block_live_in.words();
            let mut scratch_l = Vec::with_capacity_in(olw, alloc);
            scratch_l.resize(olw, 0u64);
            let limit = (nb as u64)
                .saturating_mul(ol as u64)
                .saturating_mul(2)
                .saturating_add(1);
            let mut pops = 0u64;
            while let Some(rank) = worklist.pop_min() {
                pops += 1;
                if pops > limit {
                    return Err(Bound::Passes { what: "liveness passes", limit });
                }
                let b = postorder[rank] as usize;
                for ((dst, &out_w), &tr_w) in scratch_l
                    .iter_mut()
                    .zip(block_live_out.row(b).iter())
                    .zip(block_trans.row(b).iter())
                {
                    *dst = out_w & tr_w;
                }
                if bits::union_into(block_live_in.row_mut(b), &scratch_l) {
                    for &pred in block_preds.row(b) {
                        let p = pred as usize;
                        if bits::union_into(block_live_out.row_mut(p), block_live_in.row(b)) {
                            worklist.push(post_rank[p] as usize);
                        }
                    }
                }
            }
            drop(block_trans);
            drop(block_live_in);

            // Expand block-exit liveness backward through each block to populate `facts.live`.
            let mut cur_out = Vec::with_capacity_in(olw, alloc);
            cur_out.resize(olw, 0u64);
            let mut cur_in = scratch_l;
            for b in 0..nb {
                let range = flow.blocks[b];
                if range.end <= range.start {
                    continue;
                }
                cur_out.copy_from_slice(block_live_out.row(b));
                for s in (range.start..range.end).rev() {
                    let stmt = Stmt(s);
                    cur_in.copy_from_slice(&cur_out);
                    if let Some(d) = stmt_def(flow, stmt) {
                        let oi = local_to_ol[d.index()];
                        if oi != u32::MAX {
                            bits::put(&mut cur_in, oi as usize, false);
                        }
                    }
                    for_each_stmt_use_no_drop(flow, stmt, |l| {
                        let oi = local_to_ol[l.index()];
                        if oi != u32::MAX {
                            bits::put(&mut cur_in, oi as usize, true);
                        }
                    });
                    let su = s as usize;
                    mark_origins(
                        &cur_in,
                        &cur_out,
                        &ol_to_local,
                        &origins_by_local,
                        &mut live,
                        2 * su,
                        2 * su + 1,
                    );
                    cur_out.copy_from_slice(&cur_in);
                }
            }
            drop(cur_out);
            drop(cur_in);
            drop(block_live_out);

            // Second backward liveness pass for drop-only uses: a local is drop-live along paths
            // reaching `StmtKind::Drop(p)` ONLY where the local may be initialized (RFC 2094,
            // "liveness"), so a conditional move in one branch of an `if`/`else` does not leak
            // drop-liveness back from the join block's `Drop(p)`. Solved at block grain over only
            // the locals that both appear in a reachable `Drop` and carry a drop-live origin.
            let split_drop = !flow.local_no_drop_origins.is_empty();
            let drop_origins_owned = split_drop.then(|| {
                let mut drop_pairs: Vec<(usize, Origin), A> = Vec::new_in(alloc);
                for &(l, origin) in &flow.local_origins {
                    if flow.local_no_drop_origins.binary_search(&(l, origin)).is_err() {
                        drop_pairs.push((l.index(), origin));
                    }
                }
                Csr::from_iter(locals, drop_pairs.iter().copied(), alloc)
            });
            let drop_origins_by_local = drop_origins_owned.as_ref().unwrap_or(&origins_by_local);

            let mut local_to_dl = Vec::with_capacity_in(locals, alloc);
            local_to_dl.resize(locals, u32::MAX);
            let mut dl_to_local = Vec::new_in(alloc);
            let mut drop_uses: Vec<(Stmt, u32), A> = Vec::new_in(alloc);
            let mut block_has_drop = Vec::with_capacity_in(nb, alloc);
            block_has_drop.resize(nb, false);
            for s in 0..n {
                let stmt = Stmt(s as u32);
                if let StmtKind::Drop(p) = flow.stmt(stmt).kind {
                    let l = flow.place(p).local;
                    if !drop_origins_by_local.row(l.index()).is_empty()
                        && init.drop_may_run(flow, stmt, p)
                    {
                        let dli = if local_to_dl[l.index()] == u32::MAX {
                            let idx = dl_to_local.len() as u32;
                            local_to_dl[l.index()] = idx;
                            dl_to_local.push(l);
                            idx
                        } else {
                            local_to_dl[l.index()]
                        };
                        drop_uses.push((stmt, dli));
                        block_has_drop[flow.stmt_block[s].index()] = true;
                    }
                }
            }
            let dl = dl_to_local.len();
            if dl > 0 {
                let mut block_d_in = Bits::new_in(nb, dl, alloc);
                let mut block_d_out = Bits::new_in(nb, dl, alloc);
                let mut block_drop_trans = Bits::new_in(nb, dl, alloc);
                let mut block_mi_out = Bits::new_in(nb, dl, alloc);

                let pw = init.init.words();
                let mut cur_init = Vec::with_capacity_in(pw, alloc);
                cur_init.resize(pw, 0u64);
                let mut init_count = Vec::with_capacity_in(dl, alloc);
                init_count.resize(dl, 0u32);
                let dlw = block_d_in.words();
                let mut mi_row = Vec::with_capacity_in(dlw, alloc);
                mi_row.resize(dlw, 0u64);

                // Summarize each reached block's drop-liveness transfer in one forward walk:
                // `d_in(b) = block_drop_gen(b) | (d_out(b) & block_drop_trans(b))`.
                for b in 0..nb {
                    if !init.reached[b] {
                        continue;
                    }
                    let range = flow.blocks[b];
                    if range.end <= range.start {
                        continue;
                    }
                    init_tracked_block(
                        flow,
                        &init,
                        b,
                        &local_to_dl,
                        &mut cur_init,
                        &mut init_count,
                        &mut mi_row,
                    );
                    fill_all_bits(block_drop_trans.row_mut(b), dl);
                    let mut du = drop_uses.partition_point(|&(ds, _)| ds.0 < range.start);
                    for s in range.start..range.end {
                        let stmt = Stmt(s);
                        while du < drop_uses.len() && drop_uses[du].0 == stmt {
                            let dli = drop_uses[du].1 as usize;
                            du += 1;
                            if block_drop_trans.get(b, dli) {
                                block_d_in.set(b, dli);
                            }
                        }
                        if let Some(d) = stmt_def(flow, stmt) {
                            let dli = local_to_dl[d.index()];
                            if dli != u32::MAX {
                                block_drop_trans.clear(b, dli as usize);
                            }
                        }
                        bits::intersect_into(block_drop_trans.row_mut(b), &mi_row);
                        step_tracked_init(
                            flow,
                            &init,
                            stmt,
                            &local_to_dl,
                            &mut cur_init,
                            &mut init_count,
                            &mut mi_row,
                        );
                        if s + 1 < range.end {
                            bits::intersect_into(block_drop_trans.row_mut(b), &mi_row);
                        }
                    }
                    block_mi_out.row_mut(b).copy_from_slice(&mi_row);
                }
                // Seed backward worklist from blocks whose `block_d_in` has any upward-exposed drop.
                for b in 0..nb {
                    if !init.reached[b] || !block_d_in.row(b).iter().any(|&w| w != 0) {
                        continue;
                    }
                    for &pred in block_preds.row(b) {
                        let p = pred as usize;
                        if union_masked_into(
                            block_d_out.row_mut(p),
                            block_d_in.row(b),
                            block_mi_out.row(p),
                        ) {
                            worklist.push(post_rank[p] as usize);
                        }
                    }
                }
                let dlimit = (nb as u64)
                    .saturating_mul(dl as u64)
                    .saturating_mul(2)
                    .saturating_add(1);
                let mut scratch_d = Vec::with_capacity_in(dlw, alloc);
                scratch_d.resize(dlw, 0u64);
                let mut dpops = 0u64;
                while let Some(rank) = worklist.pop_min() {
                    dpops += 1;
                    if dpops > dlimit {
                        return Err(Bound::Passes { what: "drop liveness passes", limit: dlimit });
                    }
                    let b = postorder[rank] as usize;
                    for ((dst, &out_w), &tr_w) in scratch_d
                        .iter_mut()
                        .zip(block_d_out.row(b).iter())
                        .zip(block_drop_trans.row(b).iter())
                    {
                        *dst = out_w & tr_w;
                    }
                    if bits::union_into(block_d_in.row_mut(b), &scratch_d) {
                        for &pred in block_preds.row(b) {
                            let p = pred as usize;
                            if union_masked_into(
                                block_d_out.row_mut(p),
                                block_d_in.row(b),
                                block_mi_out.row(p),
                            ) {
                                worklist.push(post_rank[p] as usize);
                            }
                        }
                    }
                }
                drop(block_drop_trans);
                drop(block_mi_out);
                drop(block_d_in);

                // Expand block-exit drop-liveness backward through each active block to `facts.live`.
                let mut block_mi: Vec<u64, A> = Vec::new_in(alloc);
                let mut cur_out = Vec::with_capacity_in(dlw, alloc);
                cur_out.resize(dlw, 0u64);
                let mut cur_in = scratch_d;
                for b in 0..nb {
                    if !init.reached[b] {
                        continue;
                    }
                    let range = flow.blocks[b];
                    if range.end <= range.start {
                        continue;
                    }
                    if !block_has_drop[b] && !block_d_out.row(b).iter().any(|&w| w != 0) {
                        continue;
                    }
                    let len = (range.end - range.start) as usize;
                    let needed = (len + 1) * dlw;
                    if block_mi.len() < needed {
                        block_mi.resize(needed, 0u64);
                    }
                    init_tracked_block(
                        flow,
                        &init,
                        b,
                        &local_to_dl,
                        &mut cur_init,
                        &mut init_count,
                        &mut mi_row,
                    );
                    block_mi[0..dlw].copy_from_slice(&mi_row);
                    for i in 0..len {
                        let stmt = Stmt(range.start + i as u32);
                        step_tracked_init(
                            flow,
                            &init,
                            stmt,
                            &local_to_dl,
                            &mut cur_init,
                            &mut init_count,
                            &mut mi_row,
                        );
                        let off = (i + 1) * dlw;
                        block_mi[off..off + dlw].copy_from_slice(&mi_row);
                    }
                    let mut du_end = drop_uses.partition_point(|&(ds, _)| ds.0 < range.end);
                    cur_out.copy_from_slice(block_d_out.row(b));
                    for i in (0..len).rev() {
                        let s = range.start + i as u32;
                        let stmt = Stmt(s);
                        cur_in.copy_from_slice(&cur_out);
                        if let Some(d) = stmt_def(flow, stmt) {
                            let dli = local_to_dl[d.index()];
                            if dli != u32::MAX {
                                bits::put(&mut cur_in, dli as usize, false);
                            }
                        }
                        let mi_in_slice = &block_mi[i * dlw..(i + 1) * dlw];
                        bits::intersect_into(&mut cur_in, mi_in_slice);
                        while du_end > 0 && drop_uses[du_end - 1].0 == stmt {
                            du_end -= 1;
                            bits::put(&mut cur_in, drop_uses[du_end].1 as usize, true);
                        }
                        let su = s as usize;
                        mark_origins(
                            &cur_in,
                            &cur_out,
                            &dl_to_local,
                            drop_origins_by_local,
                            &mut live,
                            2 * su,
                            2 * su + 1,
                        );
                        // For the previous statement `i - 1`, `d_out[i - 1] = d_in[i] & maybe_init_out[i - 1]`,
                        // and `maybe_init_out[i - 1] == maybe_init_in[i] == mi_in_slice`.
                        for ((dst, &in_w), &m) in
                            cur_out.iter_mut().zip(cur_in.iter()).zip(mi_in_slice.iter())
                        {
                            *dst = in_w & m;
                        }
                    }
                }
            }
        }
    }

    let mut active = Bits::new_in(n, nl, alloc);
    if has_activate && nl > 0 {
        let mut block_active = Bits::new_in(nb, nl, alloc);
        let mut reached = Vec::with_capacity_in(nb, alloc);
        reached.resize(nb, false);
        reached[eb] = true;
        let mut scratch = Vec::with_capacity_in(active.words(), alloc);
        scratch.resize(active.words(), 0u64);
        // Reuse the now-empty `worklist` in forward block RPO (`(nb - 1) - post_rank`).
        worklist.push(nb - 1 - post_rank[eb] as usize);
        let limit = (nb as u64)
            .saturating_mul(nl as u64)
            .saturating_add(nb as u64)
            .saturating_add(1);
        let mut pops = 0u64;
        while let Some(rpo_r) = worklist.pop_min() {
            pops += 1;
            if pops > limit {
                return Err(Bound::Passes { what: "activation passes", limit });
            }
            let b = postorder[nb - 1 - rpo_r] as usize;
            let range = flow.blocks[b];
            scratch.copy_from_slice(block_active.row(b));
            for s in range.start..range.end {
                match flow.stmt(Stmt(s)).kind {
                    StmtKind::Activate(l) => bits::put(&mut scratch, l.index(), true),
                    StmtKind::Assign { value: Value::Borrow { loan, .. }, .. } => {
                        bits::put(&mut scratch, loan.index(), false)
                    }
                    _ => {}
                }
            }
            flow.for_each_block_succ(Block(b as u32), |tb| {
                let t = tb.index();
                let first = !reached[t];
                if first {
                    reached[t] = true;
                }
                let grew = bits::union_into(block_active.row_mut(t), &scratch);
                if first || grew {
                    worklist.push(nb - 1 - post_rank[t] as usize);
                }
            });
        }
        // Expand block-entry activation forward into per-statement `active`.
        for b in 0..nb {
            if !reached[b] {
                continue;
            }
            let range = flow.blocks[b];
            scratch.copy_from_slice(block_active.row(b));
            for s in range.start..range.end {
                active.row_mut(s as usize).copy_from_slice(&scratch);
                match flow.stmt(Stmt(s)).kind {
                    StmtKind::Activate(l) => bits::put(&mut scratch, l.index(), true),
                    StmtKind::Assign { value: Value::Borrow { loan, .. }, .. } => {
                        bits::put(&mut scratch, loan.index(), false)
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(Facts { live, active, init })
}

/// Statement `s` invalidates loan `l` at its Start.
pub fn invalidated<A: Allocator>(flow: &Flow<A>, facts: &Facts<A>, s: Stmt, l: Loan) -> bool {
    let activated = facts.active.get(s.index(), l.index());
    let mut hit = false;
    for_each_access(flow, s, |a| hit |= conflicts(flow, l, a, activated));
    hit
}

/// A borrow finding: what, where, and the statement's item-relative range.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BorrowFinding {
    /// `errors(L, P)`: a live loan is invalidated. `returned`: an origin of the return place
    /// (local 0) holds the loan at some point, so the value returned borrows what ends (a
    /// returned reference must outlive the call; rustc words it E0515, "cannot return
    /// reference to local variable").
    Invalidated { at: Point, loan: Loan, span: Span, returned: bool },
    /// `subset_errors`: two placeholders related without the signature saying so.
    UnprovenOutlives { at: Point, from: Origin, into: Origin, span: Span },
}

impl BorrowFinding {
    pub fn at(&self) -> Point {
        match *self {
            BorrowFinding::Invalidated { at, .. } | BorrowFinding::UnprovenOutlives { at, .. } => at,
        }
    }
}

/// The three parts of an `Invalidated` finding a reader needs (rule 19; rustc's E0502/E0505/
/// E0597 say the same three): the borrow (its place and the statement that took it), the
/// conflicting action (the finding's own statement, `at`), and the later use that keeps the
/// loan live there. One row per `Invalidated` finding, sorted by `(at, loan)` as the findings
/// are: a column of the body's record beside `borrows` (rule 81).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LoanParts {
    pub at: Point,
    pub loan: Loan,
    /// The borrowed place (`LoanRow::place`).
    pub place: Place,
    /// The item-relative range of the statement that took the borrow (`LoanRow::at`).
    pub issued: Span,
    pub kept: Kept,
}

/// What keeps a loan live where it is invalidated.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Kept {
    /// A statement at or after the conflict uses a local whose type mentions an origin that
    /// holds the loan and is live there, reached with no redefinition of that local between:
    /// the nearest such statement (fewest edges, then lowest index). rustc: "borrow later used
    /// here".
    Use { stmt: Stmt, local: Local, span: Span },
    /// No local's use keeps it: a lifetime of the signature holds it, live at every point (the
    /// borrow must outlive the call).
    Signature { origin: Origin },
    /// Neither was found: liveness said an origin holding the loan is live at the conflict, so
    /// one of the two must exist. Named, never dropped: the note says the invariant broke.
    Unfound,
}

/// The parts of each `Invalidated` finding in `borrows`, in their order ([`LoanParts`]).
///
/// Which origins hold the loan at the conflict is the solver's point-sensitive answer, which
/// the two tiers compute in different forms (dense rows, closed components); the holders here
/// are taken location-insensitively instead (every origin the loan's own reaches over the
/// subset relation at any point, intersected with the origins live at the conflict), from the
/// flow and facts alone, so both tiers' findings get equal parts. The superset can name the
/// nearer use of a second holder; it never misses the true one, which is among them. Work: per
/// finding, one sort of the subset edges' endpoints and one forward walk of the statements per
/// holding local: findings are few, and nothing is computed for a body without one.
pub fn loan_parts<A: Allocator + Copy>(
    flow: &Flow<A>,
    facts: &Facts<A>,
    borrows: &[BorrowFinding],
    alloc: A,
) -> Vec<LoanParts, A> {
    let mut out = Vec::new_in(alloc);
    if !borrows.iter().any(|f| matches!(f, BorrowFinding::Invalidated { .. })) {
        return out;
    }
    let o = flow.origins as usize;
    let n = flow.stmts.len();
    // The subset relation as rows by `from`, points dropped.
    let mut edges = Vec::with_capacity_in(flow.subsets.len(), alloc);
    edges.extend(flow.subsets.iter().map(|&(_, a, b)| (a, b)));
    edges.sort_unstable();
    edges.dedup();
    let mut holds = Vec::with_capacity_in(o, alloc);
    let mut stack = Vec::new_in(alloc);
    let mut seen = Vec::with_capacity_in(flow.locals as usize, alloc);
    let mut dist = Vec::with_capacity_in(n, alloc);
    let mut queue = Vec::new_in(alloc);
    for f in borrows {
        let BorrowFinding::Invalidated { at, loan, .. } = *f else { continue };
        let row = flow.loan(loan);
        holds.clear();
        holds.resize(o, false);
        holds[row.origin.index()] = true;
        stack.clear();
        stack.push(row.origin);
        while let Some(x) = stack.pop() {
            let from = edges.partition_point(|&(a, _)| a < x);
            for &(a, b) in &edges[from..] {
                if a != x {
                    break;
                }
                if !holds[b.index()] {
                    holds[b.index()] = true;
                    stack.push(b);
                }
            }
        }
        let p = at.index();
        let s = at.stmt();
        let mut best: Option<(u32, Stmt, Local)> = None;
        seen.clear();
        seen.resize(flow.locals as usize, false);
        for &(local, origin) in flow.local_origins.iter() {
            if !holds[origin.index()] || !facts.live.get(p, origin.index()) || seen[local.index()] {
                continue;
            }
            seen[local.index()] = true;
            // The nearest statement from `s` on that uses `local`, along paths that do not
            // define it first (a use and a def in one statement is a use: liveness's order).
            dist.clear();
            dist.resize(n, u32::MAX);
            queue.clear();
            dist[s.index()] = 0;
            queue.push(s);
            let mut head = 0;
            while head < queue.len() {
                let t = queue[head];
                head += 1;
                let d = dist[t.index()];
                if best.is_some_and(|(bd, _, _)| d > bd) {
                    break;
                }
                let mut used = false;
                let drop_live = flow.local_no_drop_origins.binary_search(&(local, origin)).is_err()
                    && facts.init.drop_may_run(flow, s, flow.local_place(local));
                let def = uses_and_def(flow, &facts.init, t, drop_live, |l| used |= l == local);
                if used {
                    if best.is_none_or(|(bd, bs, _)| (d, t) < (bd, bs)) {
                        best = Some((d, t, local));
                    }
                    continue;
                }
                if def == Some(local) {
                    continue;
                }
                flow.for_each_succ(t, |u| {
                    if dist[u.index()] == u32::MAX {
                        dist[u.index()] = d + 1;
                        queue.push(u);
                    }
                });
            }
        }
        let kept = match best {
            Some((_, stmt, local)) => Kept::Use { stmt, local, span: flow.stmt(stmt).span },
            None => match (0..o).find(|&x| holds[x] && flow.placeholder[x]) {
                Some(x) => Kept::Signature { origin: Origin(x as u32) },
                None => Kept::Unfound,
            },
        };
        out.push(LoanParts { at, loan, place: row.place, issued: flow.stmt(row.at).span, kept });
    }
    out
}

/// Tier one's answer: it accepts, or it cannot and tier two decides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TierOne {
    Accepted,
    Refused,
}

/// Closes `rows` over CSR outgoing edges (`from -> into`: row `into` |= row `from`) with a
/// bounded FIFO ring buffer. Bound: pops are at most `origins + origins * columns`.
fn close_csr<A: Allocator + Copy>(
    outgoing: &Csr<Origin, A>,
    rows: &mut Bits<A>,
    origins: usize,
    cols: usize,
    alloc: A,
) -> Result<(), Bound> {
    if origins == 0 || cols == 0 || outgoing.entries.is_empty() {
        return Ok(());
    }
    let mut queue = Vec::with_capacity_in(origins, alloc);
    queue.resize(origins, Origin(0));
    let mut queued = Vec::with_capacity_in(origins, alloc);
    queued.resize(origins, false);
    let (mut head, mut tail, mut len) = (0usize, 0usize, 0usize);
    for x in 0..origins {
        if !outgoing.row(x).is_empty() && rows.row(x).iter().any(|&w| w != 0) {
            queue[tail] = Origin(x as u32);
            tail += 1;
            if tail == origins {
                tail = 0;
            }
            len += 1;
            queued[x] = true;
        }
    }
    let limit = (origins + origins * cols) as u64;
    let mut pops = 0u64;
    while len > 0 {
        if pops >= limit {
            return Err(Bound::Passes { what: "tier one work list pops", limit });
        }
        let from = queue[head];
        head += 1;
        if head == origins {
            head = 0;
        }
        len -= 1;
        pops += 1;
        queued[from.index()] = false;
        for &into in outgoing.row(from.index()) {
            if rows.union_rows(into.index(), from.index()) && !queued[into.index()] {
                queued[into.index()] = true;
                queue[tail] = into;
                tail += 1;
                if tail == origins {
                    tail = 0;
                }
                len += 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn close<A: Allocator + Copy>(
    edges: &[(Origin, Origin)],
    rows: &mut Bits<A>,
    origins: usize,
    cols: usize,
    alloc: A,
) -> Result<(), Bound> {
    if origins == 0 || cols == 0 || edges.is_empty() {
        return Ok(());
    }
    let outgoing = Csr::from_iter(origins, edges.iter().map(|&(a, b)| (a.index(), b)), alloc);
    close_csr(&outgoing, rows, origins, cols, alloc)
}

fn subset_edges<A: Allocator + Copy>(flow: &Flow<A>, alloc: A) -> Vec<(Origin, Origin), A> {
    let mut edges = Vec::with_capacity_in(flow.subsets.len(), alloc);
    edges.extend(flow.subsets.iter().map(|&(_, from, into)| (from, into)));
    edges.sort_unstable();
    edges.dedup();
    edges
}

/// The signature's known relations among placeholders, closed: row `into` holds every `from`.
///
/// The origins are re-related (flow.md section 3): what the signature states is the only thing a
/// body may assume among its placeholders, so a `subset` between two of them that this closure
/// does not hold is `UnprovenOutlives`. The two planted controls bend exactly this table.
#[cfg(test)]
fn known_closure<A: Allocator + Copy>(flow: &Flow<A>, plant: Plant, alloc: A) -> Result<Bits<A>, Bound> {
    let o = flow.origins as usize;
    let mut known = Bits::new_in(o, o, alloc);
    for x in 0..o {
        known.set(x, x);
    }
    if plant == Plant::AllKnown {
        for into in 0..o {
            for from in 0..o {
                known.set(into, from);
            }
        }
    }
    if plant != Plant::KnownIgnored {
        close(&flow.known, &mut known, o, o, alloc)?;
    }
    Ok(known)
}

/// Tier one: one global subset graph, contained loans per origin closed over it, kills ignored,
/// liveness kept. Accepts only when no invalidated loan may be in an origin live there and no
/// two placeholders are related beyond the signature.
pub fn tier_one<A: Allocator + Copy>(
    flow: &Flow<A>,
    facts: &Facts<A>,
    plant: Plant,
    alloc: A,
) -> Result<TierOne, Bound> {
    let (o, nl) = (flow.origins as usize, flow.loans.len());
    if o == 0 {
        return Ok(TierOne::Accepted);
    }
    let mut outgoing: Option<Csr<Origin, A>> = None;

    if nl > 0 {
        let loans_by_local = Csr::from_iter(
            flow.locals as usize,
            flow.loans.iter().enumerate().map(|(i, row)| (flow.place(row.place).local.index(), Loan(i as u32))),
            alloc,
        );
        // The planted control keeps each loan in the origin it was issued into only (as does an
        // empty subset table).
        if plant == Plant::TierOneUnclosed || flow.subsets.is_empty() {
            for s in 0..flow.stmts.len() {
                let stmt = Stmt(s as u32);
                let p = Point::start(stmt).index();
                let mut refused = false;
                for_each_access(flow, stmt, |a| {
                    if refused {
                        return;
                    }
                    let local = flow.place(a.place).local.index();
                    for &loan in loans_by_local.row(local) {
                        let l = loan.index();
                        if conflicts(flow, loan, a, facts.active.get(s, l))
                            && facts.live.get(p, flow.loan(loan).origin.index())
                        {
                            refused = true;
                            return;
                        }
                    }
                });
                if refused {
                    return Ok(TierOne::Refused);
                }
            }
        } else {
            let edges = subset_edges(flow, alloc);
            let out = Csr::from_iter(o, edges.iter().map(|&(a, b)| (a.index(), b)), alloc);
            drop(edges);
            let mut contains = Bits::new_in(o, nl, alloc);
            for (l, row) in flow.loans.iter().enumerate() {
                contains.set(row.origin.index(), l);
            }
            close_csr(&out, &mut contains, o, nl, alloc)?;
            // Transpose `contains[origin][loan]` into `contains_t[loan][origin]` for word-wise live checks.
            let mut contains_t = Bits::new_in(nl, o, alloc);
            let lw = contains.words();
            for x in 0..o {
                let row = contains.row(x);
                for w in 0..lw {
                    let mut b = row[w];
                    let base = w * 64;
                    while b != 0 {
                        let l = base + b.trailing_zeros() as usize;
                        b &= b - 1;
                        contains_t.set(l, x);
                    }
                }
            }
            drop(contains);
            outgoing = Some(out);
            for s in 0..flow.stmts.len() {
                let stmt = Stmt(s as u32);
                let live_row = facts.live.row(Point::start(stmt).index());
                let mut refused = false;
                for_each_access(flow, stmt, |a| {
                    if refused {
                        return;
                    }
                    let local = flow.place(a.place).local.index();
                    for &loan in loans_by_local.row(local) {
                        let l = loan.index();
                        if conflicts(flow, loan, a, facts.active.get(s, l))
                            && bits::intersects(contains_t.row(l), live_row)
                        {
                            refused = true;
                            return;
                        }
                    }
                });
                if refused {
                    return Ok(TierOne::Refused);
                }
            }
        }
    }

    // Placeholder outlives check: only possible when at least two distinct placeholders exist,
    // `AllKnown` is not planted, and there is at least one subset edge. Runs `O(P * (O + E + K))`
    // DFS from the `P` placeholder origins without allocating `O × O` matrices.
    if plant != Plant::AllKnown && !flow.subsets.is_empty() {
        let mut placeholders = Vec::new_in(alloc);
        for (i, &is_ph) in flow.placeholder.iter().enumerate() {
            if is_ph {
                placeholders.push(Origin(i as u32));
            }
        }
        if placeholders.len() >= 2 {
            let out = match outgoing {
                Some(out) => out,
                None => {
                    let edges = subset_edges(flow, alloc);
                    Csr::from_iter(o, edges.iter().map(|&(a, b)| (a.index(), b)), alloc)
                }
            };
            let known_out = (plant != Plant::KnownIgnored && !flow.known.is_empty()).then(|| {
                Csr::from_iter(o, flow.known.iter().map(|&(a, b)| (a.index(), b)), alloc)
            });
            let mut seen_reach = Vec::with_capacity_in(o, alloc);
            seen_reach.resize(o, u32::MAX);
            let mut seen_known = Vec::with_capacity_in(o, alloc);
            if known_out.is_some() {
                seen_known.resize(o, u32::MAX);
            }
            let mut stack = Vec::with_capacity_in(o, alloc);
            let mut reached_ph = Vec::with_capacity_in(placeholders.len(), alloc);
            for (epoch, &from) in placeholders.iter().enumerate() {
                let epoch = epoch as u32;
                reached_ph.clear();
                stack.clear();
                seen_reach[from.index()] = epoch;
                stack.push(from);
                while let Some(u) = stack.pop() {
                    for &v in out.row(u.index()) {
                        let vi = v.index();
                        if seen_reach[vi] != epoch {
                            seen_reach[vi] = epoch;
                            if flow.placeholder[vi] {
                                reached_ph.push(v);
                            }
                            stack.push(v);
                        }
                    }
                }
                if reached_ph.is_empty() {
                    continue;
                }
                let Some(ref known_out) = known_out else {
                    return Ok(TierOne::Refused);
                };
                stack.clear();
                seen_known[from.index()] = epoch;
                stack.push(from);
                while let Some(u) = stack.pop() {
                    for &v in known_out.row(u.index()) {
                        let vi = v.index();
                        if seen_known[vi] != epoch {
                            seen_known[vi] = epoch;
                            stack.push(v);
                        }
                    }
                }
                if reached_ph.iter().any(|&into| seen_known[into.index()] != epoch) {
                    return Ok(TierOne::Refused);
                }
            }
        }
    }
    Ok(TierOne::Accepted)
}

/// Location-sensitive loan and placeholder findings, in diagnostic order.
pub fn tier_two<A: Allocator + Copy>(
    flow: &Flow<A>,
    facts: &Facts<A>,
    plant: Plant,
    alloc: A,
) -> Result<Vec<BorrowFinding, A>, Bound> {
    let result = solver::solve((), flow, facts, plant, alloc, &mut solver::Meter::new(solver::LIMIT));
    #[cfg(test)]
    if let Ok(found) = &result {
        let expected = tier_two_oracle(flow, facts, plant, alloc)?;
        assert_eq!(found.as_slice(), expected.as_slice(), "tier two disagrees with the pointwise rules");
    }
    result
}

#[cfg(test)]
fn tier_two_oracle<A: Allocator + Copy>(
    flow: &Flow<A>, facts: &Facts<A>, plant: Plant, alloc: A,
) -> Result<Vec<BorrowFinding, A>, Bound> {
    tier_two_oracle_counted(flow, facts, plant, alloc, &mut 0)
}

#[cfg(test)]
pub(crate) fn assert_tiers_equal_for_fixture<A: Allocator + Copy>(flow: &Flow<A>) {
    let alloc = *flow.stmts.allocator();
    let facts = facts(flow, alloc).expect("fixture liveness converges");
    tier_two(flow, &facts, Plant::None, alloc).expect("fixture loan analysis converges");
}

/// The original pointwise rules, kept as a test oracle.
/// `subset` is one bit per (point, origin, origin), `contains` one per (point, origin, loan).
/// Bound: passes in point order, each but the last adding a fact, so at most
/// `points * (origins^2 + origins * loans) + 1`.
#[cfg(test)]
fn tier_two_oracle_counted<A: Allocator + Copy>(
    flow: &Flow<A>,
    facts: &Facts<A>,
    plant: Plant,
    alloc: A,
    cells: &mut u64,
) -> Result<Vec<BorrowFinding, A>, Bound> {
    let (n, o, nl) = (flow.stmts.len(), flow.origins as usize, flow.loans.len());
    let points = 2 * n;
    let mut subset = Bits::new_in(points, o * o, alloc);
    let mut contains = Bits::new_in(points, o * nl, alloc);
    let live = |p: usize, x: usize| facts.live.get(p, x);
    // subset(O1, O2, P) :- subset_base(O1, O2, P).
    for &(s, from, into) in flow.subsets.iter() {
        subset.set(Point::mid(s).index(), from.index() * o + into.index());
    }
    // origin_contains_loan_on_entry(O, L, P) :- loan_issued_at(O, L, P).
    for (l, row) in flow.loans.iter().enumerate() {
        contains.set(Point::mid(row.at).index(), row.origin.index() * nl + l);
    }
    let limit = (points * (o * o + o * nl) + 1) as u64;
    let mut passes = 0u64;
    loop {
        passes += 1;
        if passes > limit {
            return Err(Bound::Passes { what: "tier two passes", limit });
        }
        let mut changed = false;
        for p in 0..points {
            // subset(O1, O3, P) :- subset(O1, O2, P), subset(O2, O3, P).
            // Each round adds a fact or ends: at most o^2 + 1 rounds.
            loop {
                let mut grew = false;
                for a in 0..o {
                    *cells += o as u64;
                    for b in 0..o {
                        if subset.get(p, a * o + b) {
                            *cells += o as u64;
                            for c in 0..o {
                                if subset.get(p, b * o + c) {
                                    grew |= subset.set(p, a * o + c);
                                }
                            }
                        }
                    }
                }
                changed |= grew;
                if !grew {
                    break;
                }
            }
            // origin_contains_loan_on_entry(O2, L, P) :- ..(O1, L, P), subset(O1, O2, P).
            // subset is closed at P, so one round suffices.
            for a in 0..o {
                *cells += o as u64;
                for b in 0..o {
                    if subset.get(p, a * o + b) {
                        let (from, into) = if plant == Plant::TierTwoReversed { (b, a) } else { (a, b) };
                        *cells += nl as u64;
                        for l in 0..nl {
                            if contains.get(p, from * nl + l) {
                                changed |= contains.set(p, into * nl + l);
                            }
                        }
                    }
                }
            }
            // The cfg_edge rules: Start(s) -> Mid(s); Mid(s) -> Start of each successor.
            let s = Stmt((p / 2) as u32);
            let is_start = p % 2 == 0;
            let mut edge = |q: usize| {
                // subset(O1, O2, Q) :- subset(O1, O2, P), cfg_edge(P, Q), live(O1, Q), live(O2, Q).
                for a in 0..o {
                    *cells += o as u64;
                    for b in 0..o {
                        if subset.get(p, a * o + b) && live(q, a) && live(q, b) {
                            changed |= subset.set(q, a * o + b);
                        }
                    }
                }
                // ..(O, L, Q) :- ..(O, L, P), !loan_killed_at(L, P), cfg_edge(P, Q), live(O, Q).
                for a in 0..o {
                    *cells += nl as u64;
                    for l in 0..nl {
                        if contains.get(p, a * nl + l)
                            && !(is_start && kills(flow, s, Loan(l as u32)))
                            && live(q, a)
                        {
                            changed |= contains.set(q, a * nl + l);
                        }
                    }
                }
            };
            if is_start {
                edge(p + 1);
            } else {
                flow.for_each_succ(s, |t| edge(Point::start(t).index()));
            }
        }
        if !changed {
            break;
        }
    }
    let mut out = Vec::new_in(alloc);
    for s in 0..n {
        let stmt = Stmt(s as u32);
        let p = Point::start(stmt);
        for l in 0..nl {
            // loan_live_at(L, P) :- origin_contains_loan_on_entry(O, L, P), live(O, P).
            // errors(L, P) :- loan_invalidated_at(L, P), loan_live_at(L, P).
            let live_at = (0..o).any(|x| {
                contains.get(p.index(), x * nl + l) && live(p.index(), x)
            });
            if live_at && invalidated(flow, facts, stmt, Loan(l as u32)) {
                // The loan reaches the return place: at some point an origin of local 0 holds
                // it (the value returned borrows it, wherever the loan's end falls; a block's
                // value is often moved into local 0 after its locals' storage ended).
                let returned = flow.local_origins.iter().any(|&(local, x)| {
                    local == Local(0) && (0..points).any(|q| contains.get(q, x.index() * nl + l))
                });
                out.push(BorrowFinding::Invalidated {
                    at: p,
                    loan: Loan(l as u32),
                    span: flow.stmts[s].span,
                    returned,
                });
            }
        }
    }
    // subset_errors(O1, O2, P) :- subset(O1, O2, P), placeholder(O1), placeholder(O2),
    //   !known_placeholder_subset(O1, O2). Reported once per pair, at its first point.
    let known = known_closure(flow, plant, alloc)?;
    let mut reported = Bits::new_in(o, o, alloc);
    for p in 0..points {
        for a in 0..o {
            for b in 0..o {
                if a != b
                    && flow.placeholder[a]
                    && flow.placeholder[b]
                    && subset.get(p, a * o + b)
                    && !known.get(b, a)
                    && reported.set(a, b)
                {
                    out.push(BorrowFinding::UnprovenOutlives {
                        at: Point(p as u32),
                        from: Origin(a as u32),
                        into: Origin(b as u32),
                        span: flow.stmts[p / 2].span,
                    });
                }
            }
        }
    }
    // `core`'s sort (the stable one is `alloc`'s): the whole finding breaks ties, so the order
    // is total and the same on every run.
    out.sort_unstable_by_key(|f| (f.at(), *f));
    Ok(out)
}
