//! Solve the two monotone point relations. A point input is a basis of subset edges
//! and nonzero (loan word, origin) rows. At a point, mutually reachable origins
//! have the same loans. Across an edge, project the *closed* relation onto the live
//! origins and pass only those origins' surviving loan words. No origin equivalence
//! outlives the point that proved it.
#![blaze::sans_io]

use blaze_own::{BTreeMap, BTreeSet, Vec};
use core::{alloc::Allocator, cmp::Ordering, mem, num::NonZeroUsize};

use super::components::Components;
use super::filter::{Candidates, candidates};
use super::points::Points;
use super::{BorrowFinding, Facts, conflicts, for_each_access, kills};
use crate::bits;
use crate::flow::{Bound, Flow, Loan, Local, Origin, Point, StmtKind};
use crate::record::Plant;

/// Body-local work, identical on all hosts. Covers the indexed prepass, CFG visits,
/// edges, closure word unions, joins, and diagnostics. 32 million steps leaves
/// several complete scans of 1024 x 1024 same-local access/loan pairs in one body.
pub(super) const LIMIT: u64 = 1 << 25;

#[derive(Debug)]
pub(super) struct Exhausted;

pub(super) struct Meter {
    limit: u64,
    left: u64,
}

impl Meter {
    pub(super) fn new(limit: u64) -> Self { Self { limit, left: limit } }

    #[cfg(test)]
    pub(super) fn used(&self) -> u64 { self.limit - self.left }

    #[inline]
    pub(super) fn charge(&mut self, count: usize) -> Result<(), Exhausted> {
        self.left = self.left.checked_sub(count as u64).ok_or(Exhausted)?;
        Ok(())
    }

    pub(super) fn sort(&mut self, count: usize) -> Result<(), Exhausted> {
        self.charge(count.saturating_mul(count.checked_ilog2().unwrap_or(0) as usize + 1))
    }
}

type Edge = (Origin, Origin);

#[derive(Clone, Copy)]
struct LoanWord {
    word: u32,
    origin: Origin,
    bits: u64,
}

impl LoanWord {
    fn key(&self) -> (u32, Origin) { (self.word, self.origin) }
}

struct Input<A: Allocator> {
    edges: Vec<Edge, A>,
    loans: Vec<LoanWord, A>,
}

impl<A: Allocator + Copy> Input<A> {
    fn new(alloc: A) -> Self {
        Self { edges: Vec::new_in(alloc), loans: Vec::new_in(alloc) }
    }

    fn clear(&mut self) {
        self.edges.clear();
        self.loans.clear();
    }

    fn empty(&self) -> bool { self.edges.is_empty() && self.loans.is_empty() }

    fn normalize(&mut self, meter: &mut Meter) -> Result<(), Exhausted> {
        meter.sort(self.edges.len())?;
        self.edges.sort_unstable();
        self.edges.dedup();
        meter.sort(self.loans.len())?;
        self.loans.sort_unstable_by_key(LoanWord::key);
        self.loans.dedup_by(|new, kept| {
            if new.key() != kept.key() { return false; }
            kept.bits |= new.bits;
            true
        });
        Ok(())
    }
}

/// Linear join of sorted unique rows. A merge scratch is shared by all CFG edges.
/// Preserve the destination buffer on an unchanged join, important on loop backs.
fn join<T: Copy, K: Ord, A: Allocator>(
    dst: &mut Vec<T, A>, src: &[T], scratch: &mut Vec<T, A>,
    key: impl Fn(&T) -> K, union: impl Fn(&mut T, T) -> bool, meter: &mut Meter,
) -> Result<bool, Exhausted> {
    if src.is_empty() { return Ok(false); }
    meter.charge(dst.len().saturating_add(src.len()))?;
    if dst.is_empty() {
        dst.extend_from_slice(src);
        return Ok(true);
    }
    scratch.clear();
    let (mut i, mut j) = (0, 0);
    let mut grew = false;
    while i < dst.len() && j < src.len() {
        match key(&dst[i]).cmp(&key(&src[j])) {
            Ordering::Less => { scratch.push(dst[i]); i += 1; }
            Ordering::Greater => { scratch.push(src[j]); j += 1; grew = true; }
            Ordering::Equal => {
                let mut value = dst[i];
                grew |= union(&mut value, src[j]);
                scratch.push(value);
                i += 1;
                j += 1;
            }
        }
    }
    grew |= j < src.len();
    if grew {
        scratch.extend_from_slice(&dst[i..]);
        scratch.extend_from_slice(&src[j..]);
        mem::swap(dst, scratch);
    }
    Ok(grew)
}

/// Only a point with input owns a slot. Outside a backward-edge interval its last
/// predecessor has already contributed when it is popped in RPO. Recycle the actual
/// buffers so a straight-line chain does not leave one allocation per transfer in a frame.
struct Inputs<A: Allocator> {
    slot: Vec<Option<NonZeroUsize>, A>,
    rows: Vec<Input<A>, A>,
    free: Vec<usize, A>,
}

impl<A: Allocator + Copy> Inputs<A> {
    fn new(points: usize, alloc: A) -> Self {
        Self {
            slot: Vec::from_elem_in(None, points, alloc),
            rows: Vec::new_in(alloc), free: Vec::new_in(alloc),
        }
    }

    fn at(&mut self, p: usize) -> &mut Input<A> {
        let slot = *self.slot[p].get_or_insert_with(|| {
            let index = self.free.pop().unwrap_or_else(|| {
                let i = self.rows.len();
                self.rows.push(Input::new(self.rows.allocator().clone()));
                i
            });
            NonZeroUsize::new(index + 1).expect("an allocated input slot has a one-based index")
        });
        &mut self.rows[slot.get() - 1]
    }
}

struct Scratch<A: Allocator> {
    graph: Components<A>,
    // One word for each SCC, for the current occupied block of 64 loans.
    component_loans: Vec<u64, A>,
    closed: Vec<LoanWord, A>,
    live_loans: Vec<u64, A>,
    live_words: Vec<u32, A>,
    killed: Vec<u64, A>,
    killed_words: Vec<u32, A>,
    returned: Vec<u64, A>,
    return_origin: Vec<bool, A>,
    placeholders: Vec<Origin, A>,
    invalid: BTreeSet<(Point, Loan), A>,
    first: BTreeMap<Edge, Point, A>,
}

impl<A: Allocator + Copy> Scratch<A> {
    fn new(flow: &Flow<A>, alloc: A) -> Self {
        let words = flow.loans.len().div_ceil(64);
        let mut return_origin = Vec::from_elem_in(false, flow.origins as usize, alloc);
        for &(local, origin) in &flow.local_origins {
            if local == Local(0) { return_origin[origin.index()] = true; }
        }
        Self {
            graph: Components::new(alloc), component_loans: Vec::new_in(alloc),
            closed: Vec::new_in(alloc), placeholders: Vec::new_in(alloc),
            live_loans: Vec::from_elem_in(0, words, alloc),
            live_words: Vec::new_in(alloc),
            killed: Vec::from_elem_in(0, words, alloc),
            killed_words: Vec::new_in(alloc),
            returned: Vec::from_elem_in(0, words, alloc), return_origin,
            invalid: BTreeSet::new_in(alloc), first: BTreeMap::new_in(alloc),
        }
    }

    #[inline]
    fn clear_killed(&mut self) {
        for &w in &self.killed_words {
            self.killed[w as usize] = 0;
        }
        self.killed_words.clear();
    }

    #[inline]
    fn clear_live(&mut self) {
        for &w in &self.live_words {
            self.live_loans[w as usize] = 0;
        }
        self.live_words.clear();
    }

    /// Checks accesses and kills at `Start(s)` against the currently populated `self.live_loans` /
    /// `self.live_words`, iterating whichever of `(live loans, loans on local)` is smaller.
    fn check_start_accesses_and_kills(
        &mut self,
        p: Point,
        flow: &Flow<A>,
        facts: &Facts<A>,
        keep: &Candidates<A>,
        total_live: usize,
        meter: &mut Meter,
    ) -> Result<(), Exhausted> {
        let stmt = p.stmt();
        let mut result = Ok(());
        for_each_access(flow, stmt, |a| {
            if result.is_err() { return; }
            let acc_local = flow.place(a.place).local;
            let loans = keep.places.row(acc_local.index());
            if loans.is_empty() { return; }
            if total_live < loans.len() {
                result = meter.charge(1 + total_live);
                if result.is_err() { return; }
                for &w in &self.live_words {
                    let mut bits = self.live_loans[w as usize];
                    let base = (w as usize) * 64;
                    while bits != 0 {
                        let l = base + bits.trailing_zeros() as usize;
                        bits &= bits - 1;
                        let loan = Loan(l as u32);
                        if flow.place(flow.loan(loan).place).local == acc_local
                            && conflicts(flow, loan, a, facts.is_active(flow, stmt, loan))
                        {
                            self.invalid.insert((p, loan));
                        }
                    }
                }
            } else {
                result = meter.charge(1 + loans.len());
                if result.is_err() { return; }
                for &loan in loans {
                    if bits::test(&self.live_loans, loan.index())
                        && conflicts(flow, loan, a, facts.is_active(flow, stmt, loan))
                    {
                        self.invalid.insert((p, loan));
                    }
                }
            }
        });
        result?;
        // Kills apply to Start -> Mid only, after recording the invalidation.
        if let StmtKind::Assign { dest, .. } | StmtKind::Call { dest, .. } = flow.stmt(stmt).kind {
            let dest_local = flow.place(dest).local;
            let loans = keep.places.row(dest_local.index());
            if !loans.is_empty() {
                if total_live < loans.len() {
                    meter.charge(total_live)?;
                    for &w in &self.live_words {
                        let mut bits = self.live_loans[w as usize];
                        let base = (w as usize) * 64;
                        while bits != 0 {
                            let l = base + bits.trailing_zeros() as usize;
                            bits &= bits - 1;
                            let loan = Loan(l as u32);
                            if flow.place(flow.loan(loan).place).local == dest_local
                                && kills(flow, stmt, loan)
                            {
                                let kw = l / 64;
                                if self.killed[kw] == 0 {
                                    self.killed_words.push(kw as u32);
                                }
                                bits::put(&mut self.killed, l, true);
                            }
                        }
                    }
                } else {
                    meter.charge(loans.len())?;
                    for &loan in loans {
                        if bits::test(&self.live_loans, loan.index()) && kills(flow, stmt, loan) {
                            let kw = loan.index() / 64;
                            if self.killed[kw] == 0 {
                                self.killed_words.push(kw as u32);
                            }
                            bits::put(&mut self.killed, loan.index(), true);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn close_loans(&mut self, rows: &[LoanWord], plant: Plant, meter: &mut Meter) -> Result<(), Exhausted> {
        self.closed.clear();
        meter.charge(rows.len())?;
        if self.graph.len() == 0 {
            self.closed.extend_from_slice(rows);
            return Ok(());
        }
        for group in rows.chunk_by(|a, b| a.word == b.word) {
            let word = group[0].word;
            let mut touches_graph = false;
            for row in group {
                if let Some(c) = self.graph.component(row.origin) {
                    if !touches_graph {
                        meter.charge(self.graph.len())?;
                        self.component_loans.resize(self.graph.len(), 0);
                        self.component_loans.fill(0);
                        touches_graph = true;
                    }
                    self.component_loans[c] |= row.bits;
                } else {
                    self.closed.push(*row);
                }
            }
            if !touches_graph { continue; }
            let reverse = plant == Plant::TierTwoReversed;
            for i in 0..self.graph.len() {
                let c = if reverse { self.graph.len() - 1 - i } else { i };
                let successors = self.graph.successors(c);
                meter.charge(successors.len())?;
                for &target in successors {
                    let (from, into) = if reverse { (target, c) } else { (c, target) };
                    let extra = self.component_loans[from];
                    self.component_loans[into] |= extra;
                }
            }
            for (c, &bits) in self.component_loans.iter().enumerate() {
                if bits != 0 {
                    let members = self.graph.members(c);
                    meter.charge(members.len())?;
                    self.closed.extend(members.iter().map(|&origin| LoanWord { word, origin, bits }));
                }
            }
        }
        Ok(())
    }

    fn observe(
        &mut self, p: Point, flow: &Flow<A>, facts: &Facts<A>, keep: &Candidates<A>,
        known: &Components<A>, check_placeholders: bool, meter: &mut Meter,
    ) -> Result<(), Exhausted> {
        if check_placeholders {
            self.placeholders.clear();
            for c in 0..self.graph.len() {
                let members = self.graph.members(c);
                meter.charge(members.len())?;
                self.placeholders.extend(members.iter().copied().filter(|x| flow.placeholder[x.index()]));
            }
            if self.placeholders.len() >= 2 {
                self.graph.index_targets(&self.placeholders, meter)?;
                for &from in &self.placeholders {
                    self.graph.for_each_target(from, meter, |into| {
                        if !known.reaches(from, into) {
                            let first = self.first.entry((from, into)).or_insert(p);
                            *first = (*first).min(p);
                        }
                    })?;
                }
            }
        }
        if self.closed.is_empty() { return Ok(()); }
        meter.charge(self.closed.len())?;
        let mut total_live = 0usize;
        for row in &self.closed {
            if self.return_origin[row.origin.index()] {
                self.returned[row.word as usize] |= row.bits;
            }
            // All input to a Start crossed an edge filtered by that Start's liveness.
            // A subset at Start also has two live endpoints. Mid issues need not be live.
            if !p.is_mid() {
                let w = row.word as usize;
                let was = self.live_loans[w];
                let next = was | row.bits;
                if next != was {
                    if was == 0 {
                        self.live_words.push(row.word);
                    }
                    total_live += (next ^ was).count_ones() as usize;
                    self.live_loans[w] = next;
                }
            }
        }
        if !p.is_mid() {
            self.check_start_accesses_and_kills(p, flow, facts, keep, total_live, meter)?;
        }
        Ok(())
    }

    /// Observe a `Start(s)` whose input is already closed and came from a single earlier
    /// predecessor: `returned` and `first` were already updated there, so only accesses and
    /// kills on kept locals need checking. Returns whether any loan was killed.
    fn observe_start_closed(
        &mut self,
        p: Point,
        flow: &Flow<A>,
        facts: &Facts<A>,
        keep: &Candidates<A>,
        rows: &[LoanWord],
        meter: &mut Meter,
    ) -> Result<bool, Exhausted> {
        if rows.is_empty() {
            return Ok(false);
        }
        let stmt = p.stmt();
        let mut touches = false;
        for_each_access(flow, stmt, |a| {
            if !touches && !keep.places.row(flow.place(a.place).local.index()).is_empty() {
                touches = true;
            }
        });
        if !touches {
            if let StmtKind::Assign { dest, .. } | StmtKind::Call { dest, .. } = flow.stmt(stmt).kind {
                touches = !keep.places.row(flow.place(dest).local.index()).is_empty();
            }
        }
        if !touches {
            return Ok(false);
        }
        meter.charge(rows.len())?;
        let mut total_live = 0usize;
        for row in rows {
            let w = row.word as usize;
            let was = self.live_loans[w];
            let next = was | row.bits;
            if next != was {
                if was == 0 {
                    self.live_words.push(row.word);
                }
                total_live += (next ^ was).count_ones() as usize;
                self.live_loans[w] = next;
            }
        }
        self.check_start_accesses_and_kills(p, flow, facts, keep, total_live, meter)?;
        self.clear_live();
        Ok(!self.killed_words.is_empty())
    }

    fn finish_point(&mut self) {
        self.clear_live();
        self.clear_killed();
    }
}

pub(super) fn solve<A: Allocator + Copy, N>(
    function: N, flow: &Flow<A>, facts: &Facts<A>, plant: Plant, alloc: A, meter: &mut Meter,
) -> Result<Vec<BorrowFinding, A>, Bound<N>> {
    run(flow, facts, plant, alloc, meter).map_err(|Exhausted| Bound::TierTwo {
        function, points: 2 * flow.stmts.len(), origins: flow.origins as usize,
        loans: flow.loans.len(), budget: meter.limit,
    })
}

fn deliver<A: Allocator + Copy>(
    p: usize,
    q: usize,
    edges: &[Edge],
    loans: &[LoanWord],
    inputs: &mut Inputs<A>,
    merge: &mut Input<A>,
    in_degree: &[u8],
    has_seed: &[bool],
    closed_input: &mut [bool],
    cfg: &mut Points<A>,
    meter: &mut Meter,
) -> Result<(), Exhausted> {
    if inputs.slot[q].is_none() {
        meter.charge(edges.len().saturating_add(loans.len()))?;
        let dst = inputs.at(q);
        dst.edges.extend_from_slice(edges);
        dst.loans.extend_from_slice(loans);
        closed_input[q] = in_degree[q] == 1 && !has_seed[q] && p < q;
        cfg.schedule(q);
        return Ok(());
    }
    closed_input[q] = false;
    let dst = inputs.at(q);
    let edges_grew = join(&mut dst.edges, edges, &mut merge.edges, |e| *e, |_, _| false, meter)?;
    let loans_grew = join(
        &mut dst.loans,
        loans,
        &mut merge.loans,
        LoanWord::key,
        |dst, src| {
            let old = dst.bits;
            dst.bits |= src.bits;
            dst.bits != old
        },
        meter,
    )?;
    if edges_grew || loans_grew {
        cfg.schedule(q);
    }
    Ok(())
}

fn run<A: Allocator + Copy>(
    flow: &Flow<A>, facts: &Facts<A>, plant: Plant, alloc: A, meter: &mut Meter,
) -> Result<Vec<BorrowFinding, A>, Exhausted> {
    let keep = candidates(flow, facts, plant, meter, alloc)?;
    let mut out = Vec::new_in(alloc);
    if !keep.origins.iter().any(|&keep| keep) { return Ok(out); }
    let points = flow.stmts.len() * 2;
    let mut cfg = Points::new(flow, meter, alloc)?;
    let mut in_degree = Vec::from_elem_in(0u8, points, alloc);
    for p in 0..points {
        for &q in cfg.successors(p) {
            in_degree[q] = in_degree[q].saturating_add(1).min(2);
        }
    }
    let mut inputs = Inputs::new(points, alloc);
    let mut has_seed = Vec::from_elem_in(false, points, alloc);
    let mut closed_input = Vec::from_elem_in(false, points, alloc);
    meter.charge(flow.subsets.len())?;
    for &(s, a, b) in &flow.subsets {
        if a != b && keep.origins[a.index()] && keep.origins[b.index()] {
            let mid = Point::mid(s).index();
            has_seed[mid] = true;
            inputs.at(mid).edges.push((a, b));
        }
    }
    for (l, row) in flow.loans.iter().enumerate() {
        if keep.loans[l] {
            let mid = Point::mid(row.at).index();
            has_seed[mid] = true;
            inputs.at(mid).loans.push(LoanWord {
                word: (l / 64) as u32, origin: row.origin, bits: 1 << (l % 64),
            });
        }
    }
    for row in &mut inputs.rows { row.normalize(meter)?; }

    let check_placeholders = keep.check_placeholders;
    let mut known = Components::new(alloc);
    if check_placeholders && plant != Plant::KnownIgnored && !flow.known.is_empty() {
        known.build(&flow.known, meter)?;
        let targets = Vec::from_iter_in(
            flow.placeholder.iter().enumerate().filter_map(|(i, &p)| p.then_some(Origin(i as u32))),
            alloc,
        );
        known.index_targets(&targets, meter)?;
    }
    meter.charge(flow.local_origins.len())?;
    let mut scratch = Scratch::new(flow, alloc);
    let mut source = Input::new(alloc);
    let mut sent = Input::new(alloc);
    let mut merge = Input::new(alloc);
    while let Some(p) = cfg.pop() {
        meter.charge(1)?;
        let Some(id) = inputs.slot[p] else { continue; };
        let slot = id.get() - 1;
        mem::swap(&mut inputs.rows[slot], &mut source);
        let pt = Point(p as u32);
        let revisit = cfg.can_revisit(p);

        if closed_input[p] {
            if !pt.is_mid() {
                // `Start(s)` with already-closed input from a single earlier predecessor:
                // its sole successor is `Mid(s) = p + 1`, and `live(Start(s)) ⊆ live(Mid(s))`.
                let any_killed = scratch.observe_start_closed(pt, flow, facts, &keep, &source.loans, meter)?;
                let q = p + 1;
                if !revisit && inputs.slot[q].is_none() {
                    if any_killed {
                        meter.charge(source.loans.len())?;
                        let mut write = 0;
                        for read in 0..source.loans.len() {
                            let mut row = source.loans[read];
                            let bits = row.bits & !scratch.killed[row.word as usize];
                            if bits != 0 {
                                row.bits = bits;
                                source.loans[write] = row;
                                write += 1;
                            }
                        }
                        source.loans.truncate(write);
                        scratch.clear_killed();
                    }
                    if !source.empty() {
                        mem::swap(&mut inputs.rows[slot], &mut source);
                        inputs.slot[p] = None;
                        inputs.slot[q] = Some(id);
                        closed_input[q] = !has_seed[q];
                        cfg.schedule(q);
                        continue;
                    }
                } else if any_killed {
                    sent.clear();
                    meter.charge(source.edges.len().saturating_add(source.loans.len()))?;
                    sent.edges.extend_from_slice(&source.edges);
                    for row in &source.loans {
                        let bits = row.bits & !scratch.killed[row.word as usize];
                        if bits != 0 {
                            sent.loans.push(LoanWord { bits, ..*row });
                        }
                    }
                    scratch.clear_killed();
                    if !sent.empty() {
                        deliver(
                            p, q, &sent.edges, &sent.loans,
                            &mut inputs, &mut merge, &in_degree, &has_seed,
                            &mut closed_input, &mut cfg, meter,
                        )?;
                    }
                } else if !source.empty() {
                    deliver(
                        p, q, &source.edges, &source.loans,
                        &mut inputs, &mut merge, &in_degree, &has_seed,
                        &mut closed_input, &mut cfg, meter,
                    )?;
                }
            } else {
                // `Mid(s)` with already-closed input (no seeded edges or loans at `Mid(s)`).
                let succs_len = cfg.successors(p).len();
                let mut moved = false;
                if succs_len == 1 && !revisit {
                    let q = cfg.successors(p)[0];
                    if inputs.slot[q].is_none() {
                        let qp = Point(q as u32);
                        meter.charge(source.edges.len().saturating_add(source.loans.len()))?;
                        let all_edges_live = source.edges.iter().all(|&(a, b)| {
                            facts.is_live(flow, qp, a) && facts.is_live(flow, qp, b)
                        });
                        let all_loans_live = all_edges_live
                            && source.loans.iter().all(|row| facts.is_live(flow, qp, row.origin));
                        if all_loans_live {
                            mem::swap(&mut inputs.rows[slot], &mut source);
                            inputs.slot[p] = None;
                            inputs.slot[q] = Some(id);
                            closed_input[q] = in_degree[q] == 1 && !has_seed[q] && p < q;
                            cfg.schedule(q);
                            moved = true;
                        }
                    }
                }
                if moved {
                    continue;
                }
                let mut built_graph = false;
                for edge in 0..succs_len {
                    meter.charge(1)?;
                    let q = cfg.successors(p)[edge];
                    let qp = Point(q as u32);
                    meter.charge(source.edges.len().saturating_add(source.loans.len()))?;
                    let all_edges_live = source.edges.iter().all(|&(a, b)| {
                        facts.is_live(flow, qp, a) && facts.is_live(flow, qp, b)
                    });
                    let all_loans_live =
                        source.loans.iter().all(|row| facts.is_live(flow, qp, row.origin));
                    if all_edges_live && all_loans_live {
                        deliver(
                            p, q, &source.edges, &source.loans,
                            &mut inputs, &mut merge, &in_degree, &has_seed,
                            &mut closed_input, &mut cfg, meter,
                        )?;
                        continue;
                    }
                    sent.clear();
                    if all_edges_live {
                        sent.edges.extend_from_slice(&source.edges);
                    } else {
                        if !built_graph {
                            scratch.graph.build(&source.edges, meter)?;
                            built_graph = true;
                        }
                        scratch.graph.project(|o| facts.is_live(flow, qp, o), &mut sent.edges, meter)?;
                        meter.sort(sent.edges.len())?;
                        sent.edges.sort_unstable();
                        sent.edges.dedup();
                    }
                    for row in &source.loans {
                        if facts.is_live(flow, qp, row.origin) {
                            sent.loans.push(*row);
                        }
                    }
                    if sent.empty() {
                        continue;
                    }
                    deliver(
                        p, q, &sent.edges, &sent.loans,
                        &mut inputs, &mut merge, &in_degree, &has_seed,
                        &mut closed_input, &mut cfg, meter,
                    )?;
                }
            }
        } else {
            scratch.graph.build(&source.edges, meter)?;
            scratch.close_loans(&source.loans, plant, meter)?;
            scratch.observe(pt, flow, facts, &keep, &known, check_placeholders, meter)?;
            let is_start = !pt.is_mid();
            for edge in 0..cfg.successors(p).len() {
                meter.charge(1)?;
                let q = cfg.successors(p)[edge];
                let qp = Point(q as u32);
                sent.clear();
                if is_start {
                    // `Start(s) -> Mid(s)`: every origin live at `Start(s)` is live at `Mid(s)`.
                    scratch.graph.project(|_| true, &mut sent.edges, meter)?;
                    meter.charge(scratch.closed.len())?;
                    for row in &scratch.closed {
                        let bits = row.bits & !scratch.killed[row.word as usize];
                        if bits != 0 {
                            sent.loans.push(LoanWord { bits, ..*row });
                        }
                    }
                } else {
                    scratch.graph.project(|o| facts.is_live(flow, qp, o), &mut sent.edges, meter)?;
                    meter.charge(scratch.closed.len())?;
                    for row in &scratch.closed {
                        let bits = row.bits & !scratch.killed[row.word as usize];
                        if bits != 0 && facts.is_live(flow, qp, row.origin) {
                            sent.loans.push(LoanWord { bits, ..*row });
                        }
                    }
                }
                if sent.empty() { continue; }
                sent.normalize(meter)?;
                deliver(
                    p, q, &sent.edges, &sent.loans,
                    &mut inputs, &mut merge, &in_degree, &has_seed,
                    &mut closed_input, &mut cfg, meter,
                )?;
            }
            scratch.finish_point();
        }
        // Start -> Mid and Mid -> Start always have distinct IDs, even on a self-loop.
        if !revisit {
            source.clear();
            inputs.slot[p] = None;
            inputs.free.push(slot);
        }
        mem::swap(&mut inputs.rows[slot], &mut source);
    }
    meter.charge(scratch.first.len() + scratch.invalid.len())?;
    for &(at, loan) in scratch.invalid.iter() {
        out.push(BorrowFinding::Invalidated {
            at, loan, span: flow.stmt(at.stmt()).span, returned: bits::test(&scratch.returned, loan.index()),
        });
    }
    for (&(from, into), &at) in scratch.first.iter() {
        out.push(BorrowFinding::UnprovenOutlives { at, from, into, span: flow.stmt(at.stmt()).span });
    }
    meter.sort(out.len())?;
    out.sort_unstable_by_key(|f| (f.at(), *f));
    Ok(out)
}
