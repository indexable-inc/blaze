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

#[cfg(test)]
use crate::bits::Bits;
use crate::bits::{self, BitWorklist, BlockBits};
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

const EVENT_FLAG: u32 = 1u32 << 31;
const EVENT_STMT_MASK: u32 = !EVENT_FLAG;

/// Block-grain origin liveness: stores only cross-block local liveness bitsets (`nb × cross_ol`,
/// `nb × cross_dl`) plus compact intra-block event/interval CSRs. Eliminates the dense
/// `2 * stmts × origins` matrix while answering exact `Start(s)` / `Mid(s)` queries in `O(log k)`.
struct LiveFacts<A: Allocator> {
    /// For each `origin` (`0..o`), the `ol_idx`s whose local mentions `origin`.
    ol_by_origin: Csr<u32, A>,
    /// Maps `ol_idx -> cross_ol` column, or `u32::MAX` if never upward-exposed in any block.
    ol_to_cross: Vec<u32, A>,
    /// Block-exit regular liveness (`nb × cross_ol`).
    block_live_out: BlockBits<A>,
    /// Per `ol_idx`, sorted ascending events (at most one per stmt):
    /// `EVENT_FLAG` = 1 for Use / UseAndDef, 0 for pure Def; low 31 bits = `stmt.0`.
    ol_events: Csr<u32, A>,

    /// For each `origin` (`0..o`), the `dl_idx`s whose drop-live local mentions `origin`.
    dl_by_origin: Csr<u32, A>,
    /// Maps `dl_idx -> cross_dl` column, or `u32::MAX` if never constant-true across any block.
    dl_to_cross: Vec<u32, A>,
    /// `block_d_out & block_drop_trans` (`nb × cross_dl`): when set on a reached block, `dl` is
    /// drop-live at every point in that block.
    block_d_const: BlockBits<A>,
    /// Per `dl_idx`, sorted ascending disjoint half-open `Point` intervals `(p_lo, p_hi)` for
    /// blocks where drop-liveness varies within the block.
    dl_intervals: Csr<(u32, u32), A>,
}

impl<A: Allocator + Copy> LiveFacts<A> {
    fn empty_in(alloc: A) -> Self {
        Self {
            ol_by_origin: Csr::empty_in(alloc),
            ol_to_cross: Vec::new_in(alloc),
            block_live_out: BlockBits::empty_in(alloc),
            ol_events: Csr::empty_in(alloc),
            dl_by_origin: Csr::empty_in(alloc),
            dl_to_cross: Vec::new_in(alloc),
            block_d_const: BlockBits::empty_in(alloc),
            dl_intervals: Csr::empty_in(alloc),
        }
    }
}

impl<A: Allocator> LiveFacts<A> {
    #[inline]
    fn is_ol_live<B: Allocator>(&self, flow: &Flow<B>, p: Point, oi: usize) -> bool {
        let s = p.stmt();
        let b = flow.stmt_block[s.index()];
        let block_end = flow.blocks[b.index()].end;
        let evs = self.ol_events.row(oi);
        let idx = evs.partition_point(|&e| (e & EVENT_STMT_MASK) < s.0);
        let cross_out = || {
            let ci = self.ol_to_cross[oi];
            ci != u32::MAX && self.block_live_out.get(b, ci as usize)
        };
        let in_live = match evs.get(idx) {
            Some(&e) if (e & EVENT_STMT_MASK) < block_end => (e & EVENT_FLAG) != 0,
            _ => cross_out(),
        };
        if !p.is_mid() || in_live {
            return in_live;
        }
        // `in(s)` is false and we want `Mid(s)` (`in(s) || out(s)`): `out(s)` can only differ
        // when statement `s` itself had a pure Def event (`evs[idx]` at `s.0`).
        if evs.get(idx).is_some_and(|&e| (e & EVENT_STMT_MASK) == s.0) {
            match evs.get(idx + 1) {
                Some(&e2) if (e2 & EVENT_STMT_MASK) < block_end => (e2 & EVENT_FLAG) != 0,
                _ => cross_out(),
            }
        } else {
            false
        }
    }

    #[inline]
    fn is_dl_live<B: Allocator>(&self, flow: &Flow<B>, init: &Inits<A>, p: Point, di: usize) -> bool {
        let b = flow.stmt_block[p.stmt().index()];
        if !init.reached[b.index()] {
            return false;
        }
        let ci = self.dl_to_cross[di];
        if ci != u32::MAX && self.block_d_const.get(b, ci as usize) {
            return true;
        }
        let ivs = self.dl_intervals.row(di);
        let pt = p.0;
        let idx = ivs.partition_point(|&(lo, _)| lo <= pt);
        idx > 0 && pt < ivs[idx - 1].1
    }
}

/// Block-grain two-phase activation: tracks only loans that appear in at least one
/// `StmtKind::Activate`, eliminating the dense `stmts × loans` matrix.
struct ActiveFacts<A: Allocator> {
    /// Maps `loan.index() -> al_idx`, or `u32::MAX` if never activated. Empty when `al == 0`.
    loan_to_al: Vec<u32, A>,
    /// Reachability from entry block (empty when `al == 0`).
    reached: Vec<bool, A>,
    /// Block-entry activation bits (`nb × al`).
    block_active: BlockBits<A>,
    /// Per `al_idx`, sorted ascending events: `EVENT_FLAG` = 1 for Activate, 0 for Borrow;
    /// low 31 bits = `stmt.0`.
    events: Csr<u32, A>,
}

impl<A: Allocator + Copy> ActiveFacts<A> {
    fn empty_in(alloc: A) -> Self {
        Self {
            loan_to_al: Vec::new_in(alloc),
            reached: Vec::new_in(alloc),
            block_active: BlockBits::empty_in(alloc),
            events: Csr::empty_in(alloc),
        }
    }
}

impl<A: Allocator> ActiveFacts<A> {
    #[inline]
    fn is_active<B: Allocator>(&self, flow: &Flow<B>, s: Stmt, loan: Loan) -> bool {
        let Some(&al) = self.loan_to_al.get(loan.index()) else {
            return false;
        };
        if al == u32::MAX {
            return false;
        }
        let b = flow.stmt_block[s.index()];
        if !self.reached[b.index()] {
            return false;
        }
        let start = flow.blocks[b.index()].start;
        if s.0 > start {
            let evs = self.events.row(al as usize);
            let end = evs.partition_point(|&e| (e & EVENT_STMT_MASK) < s.0);
            if end > 0 {
                let last = evs[end - 1];
                if (last & EVENT_STMT_MASK) >= start {
                    return (last & EVENT_FLAG) != 0;
                }
            }
        }
        self.block_active.get(b, al as usize)
    }
}

/// What both tiers read: origins live at each point, and two-phase loans activated on entry.
pub struct Facts<A: Allocator> {
    live: LiveFacts<A>,
    active: ActiveFacts<A>,
    /// Where each drop may run (the move check's dataflow, unplanted): drop-liveness read it,
    /// and [`loan_parts`] reads it again to find the use that kept a loan live.
    pub init: Inits<A>,
}

impl<A: Allocator> Facts<A> {
    /// Whether `origin` is live at `p`: either a placeholder, or mentioned by a local that is
    /// regular-live or drop-live at `p`.
    #[inline]
    pub fn is_live<B: Allocator>(&self, flow: &Flow<B>, p: Point, origin: Origin) -> bool {
        let oi = origin.index();
        if oi >= flow.placeholder.len() || p.stmt().index() >= flow.stmts.len() {
            return false;
        }
        if flow.placeholder[oi] {
            return true;
        }
        for &ol_idx in self.live.ol_by_origin.row(oi) {
            if self.live.is_ol_live(flow, p, ol_idx as usize) {
                return true;
            }
        }
        for &dl_idx in self.live.dl_by_origin.row(oi) {
            if self.live.is_dl_live(flow, &self.init, p, dl_idx as usize) {
                return true;
            }
        }
        false
    }

    /// Whether two-phase loan `loan` is activated on entry to statement `s`.
    #[inline]
    pub fn is_active<B: Allocator>(&self, flow: &Flow<B>, s: Stmt, loan: Loan) -> bool {
        if s.index() >= flow.stmts.len() {
            return false;
        }
        self.active.is_active(flow, s, loan)
    }

    /// Sound inclusive statement range `(lo, hi)` bounding every `Start(s)` where `origin` can be
    /// live, when `origin` is purely intra-block (not a placeholder, no drop-live locals, and no
    /// cross-block origin-bearing locals). Returns `Some((1, 0))` if `origin` is never live at any
    /// `Start(s)`, or `None` if it may be live across block boundaries.
    pub(crate) fn origin_start_bounds<B: Allocator>(&self, flow: &Flow<B>, origin: Origin) -> Option<(u32, u32)> {
        let oi = origin.index();
        if oi >= flow.placeholder.len() || flow.placeholder[oi] {
            return None;
        }
        if !self.live.dl_by_origin.row(oi).is_empty() {
            return None;
        }
        let ols = self.live.ol_by_origin.row(oi);
        if ols.is_empty() {
            return Some((1, 0));
        }
        let (mut lo, mut hi) = (u32::MAX, 0u32);
        for &ol_idx in ols {
            let idx = ol_idx as usize;
            if self.live.ol_to_cross[idx] != u32::MAX {
                return None;
            }
            let evs = self.live.ol_events.row(idx);
            let Some(&first) = evs.first() else {
                continue;
            };
            // Because `ol_to_cross[idx] == u32::MAX`, the earliest event in every block is a pure
            // Def, so `Start(s)` cannot be live for `s <= first_stmt` or after the last Use event.
            let Some(&last_use) = evs.iter().rev().find(|&&e| (e & EVENT_FLAG) != 0) else {
                continue;
            };
            let first_stmt = first & EVENT_STMT_MASK;
            let last_stmt = last_use & EVENT_STMT_MASK;
            if first_stmt < last_stmt {
                lo = lo.min(first_stmt + 1);
                hi = hi.max(last_stmt);
            }
        }
        if lo <= hi { Some((lo, hi)) } else { Some((1, 0)) }
    }
}

/// Compact CSR grouping of `(row_key, value)` pairs, built in two linear passes.
struct Csr<T, A: Allocator> {
    offsets: Vec<u32, A>,
    entries: Vec<T, A>,
}

impl<T: Copy, A: Allocator + Copy> Csr<T, A> {
    fn empty_in(alloc: A) -> Self {
        Self { offsets: Vec::new_in(alloc), entries: Vec::new_in(alloc) }
    }

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
        if key + 1 >= self.offsets.len() {
            return &[];
        }
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
    b: Block,
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

/// Advances `(cur_init, init_count, mi_row)` across statement `s` for tracked drop-origin locals,
/// notifying `on_flip(dli)` whenever `mi_row[dli]` toggles between `false` and `true`.
#[allow(clippy::too_many_arguments)]
fn step_tracked_init<A: Allocator>(
    flow: &Flow<A>,
    init: &Inits<A>,
    s: Stmt,
    local_to_dl: &[u32],
    cur_init: &mut [u64],
    init_count: &mut [u32],
    mi_row: &mut [u64],
    mut on_flip: impl FnMut(u32),
) {
    let mut set_sub = |p: Place, on: bool| {
        let dli = local_to_dl[flow.place(p).local.index()];
        if dli == u32::MAX {
            return;
        }
        let dli_u32 = dli;
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
                        on_flip(dli_u32);
                    }
                } else {
                    init_count[dli] -= 1;
                    if init_count[dli] == 0 {
                        bits::put(mi_row, dli, false);
                        on_flip(dli_u32);
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

fn build_block_preds<A: Allocator + Copy>(flow: &Flow<A>, nb: usize, alloc: A) -> Csr<u32, A> {
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
    Csr { offsets: pred_off, entries: pred_entries }
}

/// Local liveness (backward, block-grain postorder worklist over cross-block origin-bearing
/// locals; bound `2 * blocks * cross_origin_locals + 1`), origin liveness from it (RFC 2094:
/// "a lifetime L is live at a point P if there is some variable `p` which is live at P, and L
/// appears in the type of `p`"), and activation (forward, block-grain RPO worklist over activated
/// two-phase loans; bound `blocks * activated_loans + blocks + 1`).
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
        return Ok(Facts {
            live: LiveFacts::empty_in(alloc),
            active: ActiveFacts::empty_in(alloc),
            init,
        });
    }

    let has_activate = nl > 0
        && flow.stmts.iter().any(|row| matches!(row.kind, StmtKind::Activate(_)));

    // Fast exit when there are neither origins nor two-phase activations to solve.
    if o == 0 && !has_activate {
        return Ok(Facts {
            live: LiveFacts::empty_in(alloc),
            active: ActiveFacts::empty_in(alloc),
            init,
        });
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
    let mut live = LiveFacts::empty_in(alloc);

    if o > 0 {
        // Project liveness to only locals whose types mention at least one origin.
        let mut local_to_ol = Vec::with_capacity_in(locals, alloc);
        local_to_ol.resize(locals, u32::MAX);
        let mut ol_count = 0u32;
        for &(l, _) in &flow.local_origins {
            let li = l.index();
            if local_to_ol[li] == u32::MAX {
                local_to_ol[li] = ol_count;
                ol_count += 1;
            }
        }
        let ol = ol_count as usize;

        if ol > 0 {
            live.ol_by_origin = Csr::from_iter(
                o,
                flow.local_origins.iter().map(|&(l, origin)| (origin.index(), local_to_ol[l.index()])),
                alloc,
            );

            // Build per-`ol` sorted statement event CSR (at most one entry per `(oi, stmt)`,
            // with `EVENT_FLAG` set for Use/UseAndDef and clear for pure Def).
            let mut ev_off = Vec::with_capacity_in(ol + 1, alloc);
            ev_off.resize(ol + 1, 0u32);
            let mut seen_stmt = Vec::with_capacity_in(ol, alloc);
            seen_stmt.resize(ol, u32::MAX);
            for s in 0..n {
                let su = s as u32;
                let stmt = Stmt(su);
                for_each_stmt_use_no_drop(flow, stmt, |l| {
                    let oi = local_to_ol[l.index()];
                    if oi != u32::MAX && seen_stmt[oi as usize] != su {
                        seen_stmt[oi as usize] = su;
                        ev_off[oi as usize] += 1;
                    }
                });
                if let Some(d) = stmt_def(flow, stmt) {
                    let oi = local_to_ol[d.index()];
                    if oi != u32::MAX && seen_stmt[oi as usize] != su {
                        seen_stmt[oi as usize] = su;
                        ev_off[oi as usize] += 1;
                    }
                }
            }
            let mut total_ev = 0u32;
            for c in ev_off[..ol].iter_mut() {
                total_ev += *c;
                *c = total_ev;
            }
            ev_off[ol] = total_ev;
            let mut ev_entries = Vec::with_capacity_in(total_ev as usize, alloc);
            ev_entries.resize(total_ev as usize, 0u32);
            seen_stmt.fill(u32::MAX);
            // Reverse iteration with decrementing offsets keeps each row strictly ascending by `stmt`.
            for s in (0..n).rev() {
                let su = s as u32;
                let stmt = Stmt(su);
                for_each_stmt_use_no_drop(flow, stmt, |l| {
                    let oi = local_to_ol[l.index()];
                    if oi != u32::MAX && seen_stmt[oi as usize] != su {
                        let oiu = oi as usize;
                        seen_stmt[oiu] = su;
                        ev_off[oiu] -= 1;
                        ev_entries[ev_off[oiu] as usize] = su | EVENT_FLAG;
                    }
                });
                if let Some(d) = stmt_def(flow, stmt) {
                    let oi = local_to_ol[d.index()];
                    if oi != u32::MAX && seen_stmt[oi as usize] != su {
                        let oiu = oi as usize;
                        seen_stmt[oiu] = su;
                        ev_off[oiu] -= 1;
                        ev_entries[ev_off[oiu] as usize] = su;
                    }
                }
            }
            drop(seen_stmt);
            live.ol_events = Csr { offsets: ev_off, entries: ev_entries };

            // Only locals upward-exposed in at least one basic block can ever be live across a
            // block boundary; purely block-local temporaries need zero columns in `block_live_out`.
            let mut ol_to_cross = Vec::with_capacity_in(ol, alloc);
            ol_to_cross.resize(ol, u32::MAX);
            let mut cross_to_ol = Vec::new_in(alloc);
            for oi in 0..ol {
                let mut last_b = u32::MAX;
                for &e in live.ol_events.row(oi) {
                    let b = flow.stmt_block[(e & EVENT_STMT_MASK) as usize].0;
                    if b != last_b {
                        last_b = b;
                        if (e & EVENT_FLAG) != 0 {
                            ol_to_cross[oi] = cross_to_ol.len() as u32;
                            cross_to_ol.push(oi as u32);
                            break;
                        }
                    }
                }
            }
            let cross_ol = cross_to_ol.len();
            live.ol_to_cross = ol_to_cross;

            let mut block_preds: Option<Csr<u32, A>> = None;
            if cross_ol > 0 {
                let preds = build_block_preds(flow, nb, alloc);
                // Block-grain backward liveness over cross-block origin-bearing locals:
                // `live_in(b) = block_use(b) | (live_out(b) & block_trans(b))`.
                let mut block_live_in = BlockBits::new_in(flow, cross_ol, alloc);
                let mut block_live_out = BlockBits::new_in(flow, cross_ol, alloc);
                let mut block_trans = BlockBits::new_in(flow, cross_ol, alloc);
                for b in 0..nb {
                    fill_all_bits(block_trans.row_mut(Block(b as u32)), cross_ol);
                }
                for (ci, &oi) in cross_to_ol.iter().enumerate() {
                    let mut last_b = u32::MAX;
                    for &e in live.ol_events.row(oi as usize) {
                        let blk = flow.stmt_block[(e & EVENT_STMT_MASK) as usize];
                        if blk.0 != last_b {
                            last_b = blk.0;
                            if (e & EVENT_FLAG) != 0 {
                                block_live_in.set(blk, ci);
                            } else {
                                block_trans.clear(blk, ci);
                            }
                        }
                    }
                }
                for b in 0..nb {
                    let blk = Block(b as u32);
                    if block_live_in.row(blk).iter().any(|&w| w != 0) {
                        for &pred in preds.row(b) {
                            let pb = Block(pred);
                            if bits::union_into(block_live_out.row_mut(pb), block_live_in.row(blk)) {
                                worklist.push(post_rank[pb.index()] as usize);
                            }
                        }
                    }
                }

                let olw = block_live_in.words();
                let mut scratch_l = Vec::with_capacity_in(olw, alloc);
                scratch_l.resize(olw, 0u64);
                let limit = (nb as u64)
                    .saturating_mul(cross_ol as u64)
                    .saturating_mul(2)
                    .saturating_add(1);
                let mut pops = 0u64;
                while let Some(rank) = worklist.pop_min() {
                    pops += 1;
                    if pops > limit {
                        return Err(Bound::Passes { what: "liveness passes", limit });
                    }
                    let blk = Block(postorder[rank]);
                    for ((dst, &out_w), &tr_w) in scratch_l
                        .iter_mut()
                        .zip(block_live_out.row(blk).iter())
                        .zip(block_trans.row(blk).iter())
                    {
                        *dst = out_w & tr_w;
                    }
                    if bits::union_into(block_live_in.row_mut(blk), &scratch_l) {
                        for &pred in preds.row(blk.index()) {
                            let pb = Block(pred);
                            if bits::union_into(block_live_out.row_mut(pb), block_live_in.row(blk)) {
                                worklist.push(post_rank[pb.index()] as usize);
                            }
                        }
                    }
                }
                drop(scratch_l);
                drop(block_trans);
                drop(block_live_in);
                live.block_live_out = block_live_out;
                block_preds = Some(preds);
            }
            drop(cross_to_ol);

            // Second backward liveness pass for drop-only uses: a local is drop-live along paths
            // reaching `StmtKind::Drop(p)` ONLY where the local may be initialized (RFC 2094,
            // "liveness"), so a conditional move in one branch of an `if`/`else` does not leak
            // drop-liveness back from the join block's `Drop(p)`. Solved at block grain over only
            // the locals that both appear in a reachable `Drop` and carry a drop-live origin.
            let split_drop = !flow.local_no_drop_origins.is_empty();
            let mut has_drop_origin = Vec::with_capacity_in(locals, alloc);
            has_drop_origin.resize(locals, false);
            for &(l, origin) in &flow.local_origins {
                if !split_drop || flow.local_no_drop_origins.binary_search(&(l, origin)).is_err() {
                    has_drop_origin[l.index()] = true;
                }
            }

            let mut local_to_dl = Vec::with_capacity_in(locals, alloc);
            local_to_dl.resize(locals, u32::MAX);
            let mut dl_count = 0u32;
            let mut drop_uses: Vec<(Stmt, u32), A> = Vec::new_in(alloc);
            for s in 0..n {
                let stmt = Stmt(s as u32);
                if let StmtKind::Drop(p) = flow.stmt(stmt).kind {
                    let l = flow.place(p).local;
                    if has_drop_origin[l.index()] && init.drop_may_run(flow, stmt, p) {
                        let dli = if local_to_dl[l.index()] == u32::MAX {
                            let idx = dl_count;
                            local_to_dl[l.index()] = idx;
                            dl_count += 1;
                            idx
                        } else {
                            local_to_dl[l.index()]
                        };
                        drop_uses.push((stmt, dli));
                    }
                }
            }
            drop(has_drop_origin);
            let dl = dl_count as usize;

            if dl > 0 {
                let mut dl_orig_pairs: Vec<(usize, u32), A> = Vec::new_in(alloc);
                for &(l, origin) in &flow.local_origins {
                    let dli = local_to_dl[l.index()];
                    if dli != u32::MAX
                        && (!split_drop
                            || flow.local_no_drop_origins.binary_search(&(l, origin)).is_err())
                    {
                        dl_orig_pairs.push((origin.index(), dli));
                    }
                }
                live.dl_by_origin = Csr::from_iter(o, dl_orig_pairs.iter().copied(), alloc);
                drop(dl_orig_pairs);

                let preds = match block_preds {
                    Some(preds) => preds,
                    None => build_block_preds(flow, nb, alloc),
                };

                let mut block_d_in = BlockBits::new_in(flow, dl, alloc);
                let mut block_d_out = BlockBits::new_in(flow, dl, alloc);
                let mut block_drop_trans = BlockBits::new_in(flow, dl, alloc);
                let mut block_mi_out = BlockBits::new_in(flow, dl, alloc);

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
                    let blk = Block(b as u32);
                    init_tracked_block(
                        flow,
                        &init,
                        blk,
                        &local_to_dl,
                        &mut cur_init,
                        &mut init_count,
                        &mut mi_row,
                    );
                    fill_all_bits(block_drop_trans.row_mut(blk), dl);
                    let mut du = drop_uses.partition_point(|&(ds, _)| ds.0 < range.start);
                    for s in range.start..range.end {
                        let stmt = Stmt(s);
                        while du < drop_uses.len() && drop_uses[du].0 == stmt {
                            let dli = drop_uses[du].1 as usize;
                            du += 1;
                            if block_drop_trans.get(blk, dli) {
                                block_d_in.set(blk, dli);
                            }
                        }
                        if let Some(d) = stmt_def(flow, stmt) {
                            let dli = local_to_dl[d.index()];
                            if dli != u32::MAX {
                                block_drop_trans.clear(blk, dli as usize);
                            }
                        }
                        bits::intersect_into(block_drop_trans.row_mut(blk), &mi_row);
                        step_tracked_init(
                            flow,
                            &init,
                            stmt,
                            &local_to_dl,
                            &mut cur_init,
                            &mut init_count,
                            &mut mi_row,
                            |_| {},
                        );
                        if s + 1 < range.end {
                            bits::intersect_into(block_drop_trans.row_mut(blk), &mi_row);
                        }
                    }
                    block_mi_out.row_mut(blk).copy_from_slice(&mi_row);
                }

                // Seed backward worklist from blocks whose `block_d_in` has any upward-exposed drop.
                for b in 0..nb {
                    let blk = Block(b as u32);
                    if !init.reached[b] || !block_d_in.row(blk).iter().any(|&w| w != 0) {
                        continue;
                    }
                    for &pred in preds.row(b) {
                        let pb = Block(pred);
                        if union_masked_into(
                            block_d_out.row_mut(pb),
                            block_d_in.row(blk),
                            block_mi_out.row(pb),
                        ) {
                            worklist.push(post_rank[pb.index()] as usize);
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
                    let blk = Block(postorder[rank]);
                    for ((dst, &out_w), &tr_w) in scratch_d
                        .iter_mut()
                        .zip(block_d_out.row(blk).iter())
                        .zip(block_drop_trans.row(blk).iter())
                    {
                        *dst = out_w & tr_w;
                    }
                    if bits::union_into(block_d_in.row_mut(blk), &scratch_d) {
                        for &pred in preds.row(blk.index()) {
                            let pb = Block(pred);
                            if union_masked_into(
                                block_d_out.row_mut(pb),
                                block_d_in.row(blk),
                                block_mi_out.row(pb),
                            ) {
                                worklist.push(post_rank[pb.index()] as usize);
                            }
                        }
                    }
                }
                drop(block_mi_out);
                drop(block_d_in);

                // Compress constant-true blocks (`block_d_out & block_drop_trans`) into `block_d_const`.
                let mut dl_to_cross = Vec::with_capacity_in(dl, alloc);
                dl_to_cross.resize(dl, u32::MAX);
                let mut cross_to_dl = Vec::new_in(alloc);
                for b in 0..nb {
                    if !init.reached[b] {
                        continue;
                    }
                    let blk = Block(b as u32);
                    let out_r = block_d_out.row(blk);
                    let tr_r = block_drop_trans.row(blk);
                    for w in 0..dlw {
                        let mut c = out_r[w] & tr_r[w];
                        let base = w * 64;
                        while c != 0 {
                            let dli = base + usize::try_from(c.trailing_zeros()).unwrap_or(0);
                            c &= c - 1;
                            if dl_to_cross[dli] == u32::MAX {
                                dl_to_cross[dli] = cross_to_dl.len() as u32;
                                cross_to_dl.push(dli as u32);
                            }
                        }
                    }
                }
                let cross_dl = cross_to_dl.len();
                if cross_dl > 0 {
                    let mut block_d_const = BlockBits::new_in(flow, cross_dl, alloc);
                    for b in 0..nb {
                        if !init.reached[b] {
                            continue;
                        }
                        let blk = Block(b as u32);
                        let out_r = block_d_out.row(blk);
                        let tr_r = block_drop_trans.row(blk);
                        for w in 0..dlw {
                            let mut c = out_r[w] & tr_r[w];
                            let base = w * 64;
                            while c != 0 {
                                let dli = base + usize::try_from(c.trailing_zeros()).unwrap_or(0);
                                c &= c - 1;
                                block_d_const.set(blk, dl_to_cross[dli] as usize);
                            }
                        }
                    }
                    live.block_d_const = block_d_const;
                }
                live.dl_to_cross = dl_to_cross;
                drop(cross_to_dl);

                // Record compact half-open `Point` intervals only for `(block, dl)` pairs whose
                // drop-liveness varies within the block (either `d_out && !drop_trans`, or a
                // reachable `Drop` in the block while not `block_d_const`).
                let mut raw_intervals: Vec<(u32, u32, u32), A> = Vec::new_in(alloc);
                let mut case3_mask = Vec::with_capacity_in(dlw, alloc);
                case3_mask.resize(dlw, 0u64);
                let mut case3_dls: Vec<u32, A> = Vec::new_in(alloc);
                let mut open_hi = Vec::with_capacity_in(dl, alloc);
                open_hi.resize(dl, u32::MAX);
                let mut mi_flips: Vec<(u32, u32), A> = Vec::new_in(alloc);
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
                    let blk = Block(b as u32);
                    let out_r = block_d_out.row(blk);
                    let tr_r = block_drop_trans.row(blk);
                    let mut any_case3 = false;
                    for w in 0..dlw {
                        let m = out_r[w] & !tr_r[w];
                        case3_mask[w] = m;
                        any_case3 |= m != 0;
                    }
                    let du_start = drop_uses.partition_point(|&(ds, _)| ds.0 < range.start);
                    let mut du_end = du_start;
                    while du_end < drop_uses.len() && drop_uses[du_end].0.0 < range.end {
                        let dli = drop_uses[du_end].1 as usize;
                        if !(bits::test(out_r, dli) && bits::test(tr_r, dli)) {
                            bits::put(&mut case3_mask, dli, true);
                            any_case3 = true;
                        }
                        du_end += 1;
                    }
                    if !any_case3 {
                        continue;
                    }
                    case3_dls.clear();
                    for w in 0..dlw {
                        let mut m = case3_mask[w];
                        let base = w * 64;
                        while m != 0 {
                            let dli = base + usize::try_from(m.trailing_zeros()).unwrap_or(0);
                            m &= m - 1;
                            case3_dls.push(dli as u32);
                        }
                    }

                    let len = (range.end - range.start) as usize;
                    mi_flips.clear();
                    init_tracked_block(
                        flow,
                        &init,
                        blk,
                        &local_to_dl,
                        &mut cur_init,
                        &mut init_count,
                        &mut mi_row,
                    );
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
                            |dli| mi_flips.push((i as u32, dli)),
                        );
                    }

                    cur_out.copy_from_slice(block_d_out.row(blk));
                    let mut du_cur = du_end;
                    for i in (0..len).rev() {
                        // Undo statement `i`'s flips so `mi_row` holds `mi_in[i]`.
                        while let Some(&(si, dli)) = mi_flips.last() {
                            if si != i as u32 {
                                break;
                            }
                            mi_flips.pop();
                            let d = dli as usize;
                            let was = bits::test(&mi_row, d);
                            bits::put(&mut mi_row, d, !was);
                        }
                        let s = range.start + i as u32;
                        let stmt = Stmt(s);
                        cur_in.copy_from_slice(&cur_out);
                        if let Some(d) = stmt_def(flow, stmt) {
                            let dli = local_to_dl[d.index()];
                            if dli != u32::MAX {
                                bits::put(&mut cur_in, dli as usize, false);
                            }
                        }
                        bits::intersect_into(&mut cur_in, &mi_row);
                        while du_cur > du_start && drop_uses[du_cur - 1].0 == stmt {
                            du_cur -= 1;
                            bits::put(&mut cur_in, drop_uses[du_cur].1 as usize, true);
                        }

                        let p_start = s * 2;
                        let p_mid = p_start + 1;
                        for &dli in &case3_dls {
                            let d = dli as usize;
                            let in_bit = bits::test(&cur_in, d);
                            let mid_bit = in_bit || bits::test(&cur_out, d);
                            if mid_bit {
                                if open_hi[d] == u32::MAX {
                                    open_hi[d] = p_mid + 1;
                                }
                            } else if open_hi[d] != u32::MAX {
                                raw_intervals.push((dli, p_mid + 1, open_hi[d]));
                                open_hi[d] = u32::MAX;
                            }
                            if in_bit {
                                if open_hi[d] == u32::MAX {
                                    open_hi[d] = p_start + 1;
                                }
                            } else if open_hi[d] != u32::MAX {
                                raw_intervals.push((dli, p_start + 1, open_hi[d]));
                                open_hi[d] = u32::MAX;
                            }
                        }

                        for ((dst, &in_w), &m) in
                            cur_out.iter_mut().zip(cur_in.iter()).zip(mi_row.iter())
                        {
                            *dst = in_w & m;
                        }
                    }
                    let b_start_pt = range.start * 2;
                    for &dli in &case3_dls {
                        let d = dli as usize;
                        if open_hi[d] != u32::MAX {
                            raw_intervals.push((dli, b_start_pt, open_hi[d]));
                            open_hi[d] = u32::MAX;
                        }
                    }
                }
                if !raw_intervals.is_empty() {
                    raw_intervals.sort_unstable();
                    // `Csr::from_iter` fills each row backwards, so iterate in reverse to keep
                    // every `dl`'s disjoint intervals strictly ascending by `p_lo`.
                    live.dl_intervals = Csr::from_iter(
                        dl,
                        raw_intervals.iter().rev().map(|&(d, lo, hi)| (d as usize, (lo, hi))),
                        alloc,
                    );
                }
            }
        }
    }

    let mut active = ActiveFacts::empty_in(alloc);
    if has_activate && nl > 0 {
        // Compress activation tracking to only loans that actually appear in `StmtKind::Activate`.
        let mut loan_to_al = Vec::with_capacity_in(nl, alloc);
        loan_to_al.resize(nl, u32::MAX);
        let mut al_count = 0u32;
        for s in 0..n {
            if let StmtKind::Activate(l) = flow.stmt(Stmt(s as u32)).kind {
                let li = l.index();
                if loan_to_al[li] == u32::MAX {
                    loan_to_al[li] = al_count;
                    al_count += 1;
                }
            }
        }
        let al = al_count as usize;
        if al > 0 {
            let mut ev_off = Vec::with_capacity_in(al + 1, alloc);
            ev_off.resize(al + 1, 0u32);
            for s in 0..n {
                match flow.stmt(Stmt(s as u32)).kind {
                    StmtKind::Activate(l)
                    | StmtKind::Assign { value: Value::Borrow { loan: l, .. }, .. } => {
                        let ai = loan_to_al[l.index()];
                        if ai != u32::MAX {
                            ev_off[ai as usize] += 1;
                        }
                    }
                    _ => {}
                }
            }
            let mut total_ev = 0u32;
            for c in ev_off[..al].iter_mut() {
                total_ev += *c;
                *c = total_ev;
            }
            ev_off[al] = total_ev;
            let mut ev_entries = Vec::with_capacity_in(total_ev as usize, alloc);
            ev_entries.resize(total_ev as usize, 0u32);
            for s in (0..n).rev() {
                let su = s as u32;
                match flow.stmt(Stmt(su)).kind {
                    StmtKind::Activate(l) => {
                        let ai = loan_to_al[l.index()];
                        if ai != u32::MAX {
                            ev_off[ai as usize] -= 1;
                            ev_entries[ev_off[ai as usize] as usize] = su | EVENT_FLAG;
                        }
                    }
                    StmtKind::Assign { value: Value::Borrow { loan: l, .. }, .. } => {
                        let ai = loan_to_al[l.index()];
                        if ai != u32::MAX {
                            ev_off[ai as usize] -= 1;
                            ev_entries[ev_off[ai as usize] as usize] = su;
                        }
                    }
                    _ => {}
                }
            }
            let events = Csr { offsets: ev_off, entries: ev_entries };

            let mut block_active = BlockBits::new_in(flow, al, alloc);
            let mut reached = Vec::with_capacity_in(nb, alloc);
            reached.resize(nb, false);
            reached[eb] = true;
            let mut scratch = Vec::with_capacity_in(block_active.words(), alloc);
            scratch.resize(block_active.words(), 0u64);
            // Reuse the now-empty `worklist` in forward block RPO (`(nb - 1) - post_rank`).
            worklist.push(nb - 1 - post_rank[eb] as usize);
            let limit = (nb as u64)
                .saturating_mul(al as u64)
                .saturating_add(nb as u64)
                .saturating_add(1);
            let mut pops = 0u64;
            while let Some(rpo_r) = worklist.pop_min() {
                pops += 1;
                if pops > limit {
                    return Err(Bound::Passes { what: "activation passes", limit });
                }
                let blk = Block(postorder[nb - 1 - rpo_r]);
                let range = flow.blocks[blk.index()];
                scratch.copy_from_slice(block_active.row(blk));
                for s in range.start..range.end {
                    match flow.stmt(Stmt(s)).kind {
                        StmtKind::Activate(l) => {
                            let ai = loan_to_al[l.index()];
                            if ai != u32::MAX {
                                bits::put(&mut scratch, ai as usize, true);
                            }
                        }
                        StmtKind::Assign { value: Value::Borrow { loan, .. }, .. } => {
                            let ai = loan_to_al[loan.index()];
                            if ai != u32::MAX {
                                bits::put(&mut scratch, ai as usize, false);
                            }
                        }
                        _ => {}
                    }
                }
                flow.for_each_block_succ(blk, |tb| {
                    let t = tb.index();
                    let first = !reached[t];
                    if first {
                        reached[t] = true;
                    }
                    let grew = bits::union_into(block_active.row_mut(tb), &scratch);
                    if first || grew {
                        worklist.push(nb - 1 - post_rank[t] as usize);
                    }
                });
            }
            active = ActiveFacts { loan_to_al, reached, block_active, events };
        }
    }
    Ok(Facts { live, active, init })
}

/// Statement `s` invalidates loan `l` at its Start.
pub fn invalidated<A: Allocator>(flow: &Flow<A>, facts: &Facts<A>, s: Stmt, l: Loan) -> bool {
    let activated = facts.is_active(flow, s, l);
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
        let s = at.stmt();
        let mut best: Option<(u32, Stmt, Local)> = None;
        seen.clear();
        seen.resize(flow.locals as usize, false);
        for &(local, origin) in flow.local_origins.iter() {
            if !holds[origin.index()] || !facts.is_live(flow, at, origin) || seen[local.index()] {
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
#[cfg(test)]
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
        const SHORT_SPAN_MAX: u32 = 64;
        const SHORT_REACH_MAX: usize = 16;
        // The planted control keeps each loan in the origin it was issued into only (as does an
        // empty subset table).
        if plant == Plant::TierOneUnclosed || flow.subsets.is_empty() {
            let mut long_loans: Vec<(usize, Loan), A> = Vec::new_in(alloc);
            for (i, row) in flow.loans.iter().enumerate() {
                let loan = Loan(i as u32);
                if let Some((lo, hi)) = facts.origin_start_bounds(flow, row.origin) {
                    if lo > hi {
                        continue;
                    }
                    if hi - lo <= SHORT_SPAN_MAX {
                        let mut refused = false;
                        for s in lo..=hi {
                            let stmt = Stmt(s);
                            let p = Point::start(stmt);
                            if !facts.is_live(flow, p, row.origin) {
                                continue;
                            }
                            let act = facts.is_active(flow, stmt, loan);
                            for_each_access(flow, stmt, |a| {
                                if !refused && conflicts(flow, loan, a, act) {
                                    refused = true;
                                }
                            });
                            if refused {
                                return Ok(TierOne::Refused);
                            }
                        }
                        continue;
                    }
                }
                long_loans.push((flow.place(row.place).local.index(), loan));
            }
            if !long_loans.is_empty() {
                let loans_by_local =
                    Csr::from_iter(flow.locals as usize, long_loans.iter().copied(), alloc);
                for s in 0..flow.stmts.len() {
                    let stmt = Stmt(s as u32);
                    let p = Point::start(stmt);
                    let mut refused = false;
                    for_each_access(flow, stmt, |a| {
                        if refused {
                            return;
                        }
                        let local = flow.place(a.place).local.index();
                        for &loan in loans_by_local.row(local) {
                            if conflicts(flow, loan, a, facts.is_active(flow, stmt, loan))
                                && facts.is_live(flow, p, flow.loan(loan).origin)
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
        } else {
            let edges = subset_edges(flow, alloc);
            let out = Csr::from_iter(o, edges.iter().map(|&(a, b)| (a.index(), b)), alloc);
            // Any origin that can globally reach a placeholder is live at every point; precompute
            // that set once in `O(O + E)` without allocating `O × L` matrices.
            let mut reaches_placeholder = Vec::with_capacity_in(o, alloc);
            reaches_placeholder.resize(o, false);
            let mut dfs_stack = Vec::new_in(alloc);
            let mut has_ph = false;
            for (i, &is_ph) in flow.placeholder.iter().enumerate() {
                if is_ph {
                    reaches_placeholder[i] = true;
                    dfs_stack.push(Origin(i as u32));
                    has_ph = true;
                }
            }
            if has_ph {
                let inc = Csr::from_iter(o, edges.iter().map(|&(a, b)| (b.index(), a)), alloc);
                while let Some(u) = dfs_stack.pop() {
                    for &v in inc.row(u.index()) {
                        if !reaches_placeholder[v.index()] {
                            reaches_placeholder[v.index()] = true;
                            dfs_stack.push(v);
                        }
                    }
                }
            }
            drop(edges);

            let mut visit_epoch = Vec::with_capacity_in(o, alloc);
            visit_epoch.resize(o, 0u64);
            let mut epoch = 0u64;
            let mut visited_buf = Vec::new_in(alloc);
            let mut long_loans: Vec<(usize, Loan), A> = Vec::new_in(alloc);

            // Check short-lived intra-block loans directly over their exact live statement window,
            // avoiding `O(K^2)` scans on hot locals with thousands of temporary borrows.
            for (i, row) in flow.loans.iter().enumerate() {
                let loan = Loan(i as u32);
                let orig = row.origin;
                if !reaches_placeholder[orig.index()] {
                    epoch += 1;
                    visited_buf.clear();
                    dfs_stack.clear();
                    let mut bounded = true;
                    let (mut lo, mut hi) = (u32::MAX, 0u32);
                    visit_epoch[orig.index()] = epoch;
                    visited_buf.push(orig);
                    dfs_stack.push(orig);
                    while let Some(u) = dfs_stack.pop() {
                        match facts.origin_start_bounds(flow, u) {
                            Some((u_lo, u_hi)) => {
                                if u_lo <= u_hi {
                                    lo = lo.min(u_lo);
                                    hi = hi.max(u_hi);
                                    if hi - lo > SHORT_SPAN_MAX {
                                        bounded = false;
                                        break;
                                    }
                                }
                            }
                            None => {
                                bounded = false;
                                break;
                            }
                        }
                        for &v in out.row(u.index()) {
                            let vi = v.index();
                            if visit_epoch[vi] != epoch {
                                if visited_buf.len() >= SHORT_REACH_MAX {
                                    bounded = false;
                                    break;
                                }
                                visit_epoch[vi] = epoch;
                                visited_buf.push(v);
                                dfs_stack.push(v);
                            }
                        }
                        if !bounded {
                            break;
                        }
                    }
                    if bounded {
                        if lo <= hi {
                            let mut refused = false;
                            for s in lo..=hi {
                                let stmt = Stmt(s);
                                let p = Point::start(stmt);
                                if !visited_buf.iter().any(|&v| facts.is_live(flow, p, v)) {
                                    continue;
                                }
                                let act = facts.is_active(flow, stmt, loan);
                                for_each_access(flow, stmt, |a| {
                                    if !refused && conflicts(flow, loan, a, act) {
                                        refused = true;
                                    }
                                });
                                if refused {
                                    return Ok(TierOne::Refused);
                                }
                            }
                        }
                        continue;
                    }
                }
                long_loans.push((flow.place(row.place).local.index(), loan));
            }

            if !long_loans.is_empty() {
                let loans_by_local =
                    Csr::from_iter(flow.locals as usize, long_loans.iter().copied(), alloc);
                // Per-statement forward DFS with `dead_at[v] == s` memoization: each origin is
                // visited at most once per conflicting statement, with zero `O × L` bitset.
                let mut dead_at = Vec::with_capacity_in(o, alloc);
                dead_at.resize(o, u32::MAX);

                for s in 0..flow.stmts.len() {
                    let su = s as u32;
                    let stmt = Stmt(su);
                    let p = Point::start(stmt);
                    let mut refused = false;
                    for_each_access(flow, stmt, |a| {
                        if refused {
                            return;
                        }
                        let local = flow.place(a.place).local.index();
                        for &loan in loans_by_local.row(local) {
                            if !conflicts(flow, loan, a, facts.is_active(flow, stmt, loan)) {
                                continue;
                            }
                            let orig = flow.loan(loan).origin;
                            let oi = orig.index();
                            if reaches_placeholder[oi] || facts.is_live(flow, p, orig) {
                                refused = true;
                                return;
                            }
                            if out.row(oi).is_empty() || dead_at[oi] == su {
                                continue;
                            }
                            epoch += 1;
                            visited_buf.clear();
                            dfs_stack.clear();
                            visit_epoch[oi] = epoch;
                            visited_buf.push(orig);
                            dfs_stack.push(orig);
                            let mut hit = false;
                            while let Some(u) = dfs_stack.pop() {
                                for &v in out.row(u.index()) {
                                    let vi = v.index();
                                    if visit_epoch[vi] == epoch || dead_at[vi] == su {
                                        continue;
                                    }
                                    if facts.is_live(flow, p, v) {
                                        hit = true;
                                        break;
                                    }
                                    visit_epoch[vi] = epoch;
                                    visited_buf.push(v);
                                    dfs_stack.push(v);
                                }
                                if hit {
                                    break;
                                }
                            }
                            if hit {
                                refused = true;
                                return;
                            }
                            for &v in &visited_buf {
                                dead_at[v.index()] = su;
                            }
                        }
                    });
                    if refused {
                        return Ok(TierOne::Refused);
                    }
                }
            }
            outgoing = Some(out);
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
        let points = 2usize.saturating_mul(flow.stmts.len());
        let o = flow.origins as usize;
        let nl = flow.loans.len();
        // Cross-check against the dense `O(P * O * (O + L))` pointwise oracle whenever its dense
        // tables fit comfortably in memory (covers all unit and `blaze-sim` property tests).
        if points.saturating_mul(o).saturating_mul(o.saturating_add(nl)) <= 16_000_000 {
            let expected = tier_two_oracle(flow, facts, plant, alloc)?;
            assert_eq!(found.as_slice(), expected.as_slice(), "tier two disagrees with the pointwise rules");
        }
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
    let live = |p: usize, x: usize| facts.is_live(flow, Point(p as u32), Origin(x as u32));
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
