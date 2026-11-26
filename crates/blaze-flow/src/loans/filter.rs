//! Restrict the location-sensitive relations to facts that can contribute to an answer.
//! Dropping CFG edges, kills and subset locations gives a superset of possible holders of a
//! loan. Intersect those holders with the origins live at each actual conflicting access.
//! The `returned` flag still needs every possible holder of a surviving loan, even at a
//! point where that holder is dead.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;

use super::solver::{Exhausted, Meter};
use super::{Facts, conflicts, for_each_access};
use crate::flow::{Flow, Loan, Origin, Point, Stmt};
use crate::record::Plant;

pub(super) struct Candidates<A: Allocator> {
    pub(super) origins: Vec<bool, A>,
    pub(super) loans: Vec<bool, A>,
    pub(super) places: Rows<Loan, A>,
    pub(super) check_placeholders: bool,
}

/// Group a relation by its first column in two linear passes. During construction the
/// offsets are exclusive row ends; filling each row backwards turns them into starts.
pub(super) struct Rows<T, A: Allocator> {
    offsets: Vec<usize, A>,
    entries: Vec<T, A>,
}

impl<T: Copy, A: Allocator + Copy> Rows<T, A> {
    fn new(
        rows: usize,
        relation: impl ExactSizeIterator<Item = (usize, T)> + Clone,
        meter: &mut Meter,
        alloc: A,
    ) -> Result<Self, Exhausted> {
        meter.charge(rows)?;
        meter.charge(relation.len().saturating_mul(2))?;
        let mut offsets = Vec::with_capacity_in(rows + 1, alloc);
        offsets.resize(rows + 1, 0usize);
        let mut entries = Vec::with_capacity_in(relation.len(), alloc);
        for (key, value) in relation.clone() {
            offsets[key] += 1;
            entries.push(value);
        }
        let mut end = 0;
        for count in offsets.iter_mut() {
            end += *count;
            *count = end;
        }
        for (key, value) in relation {
            offsets[key] -= 1;
            entries[offsets[key]] = value;
        }
        Ok(Self { offsets, entries })
    }

    pub(super) fn row(&self, key: usize) -> &[T] {
        &self.entries[self.offsets[key]..self.offsets[key + 1]]
    }
}

/// A generation per walk avoids clearing O origins for every sparse live row.
struct Walk<A: Allocator> {
    seen: Vec<usize, A>,
    stack: Vec<Origin, A>,
}

impl<A: Allocator + Copy> Walk<A> {
    fn seed(&mut self, origin: Origin, generation: usize) {
        if self.seen[origin.index()] != generation {
            self.seen[origin.index()] = generation;
            self.stack.push(origin);
        }
    }

    fn close(
        &mut self,
        graph: &Rows<Origin, A>,
        generation: usize,
        restrict_to: Option<usize>,
        meter: &mut Meter,
    ) -> Result<(), Exhausted> {
        while let Some(origin) = self.stack.pop() {
            let row = graph.row(origin.index());
            meter.charge(1 + row.len())?;
            for &next in row {
                if restrict_to.is_none_or(|allowed| self.seen[next.index()] == allowed) {
                    self.seed(next, generation);
                }
            }
        }
        Ok(())
    }
}

/// Tier one declines a whole flow and hands on no candidate list. Here the reverse
/// location-insensitive subset walk is shared by every conflicting loan at a Start.
pub(super) fn candidates<A: Allocator + Copy>(
    flow: &Flow<A>,
    facts: &Facts<A>,
    plant: Plant,
    meter: &mut Meter,
    alloc: A,
) -> Result<Candidates<A>, Exhausted> {
    let count = flow.origins as usize;
    meter.charge(count)?;
    meter.charge(flow.loans.len())?;
    let mut origins = Vec::with_capacity_in(count, alloc);
    origins.resize(count, false);
    let mut loans = Vec::with_capacity_in(flow.loans.len(), alloc);
    loans.resize(flow.loans.len(), false);
    let mut placeholders = Vec::new_in(alloc);
    for (i, &is_ph) in flow.placeholder.iter().enumerate() {
        if is_ph {
            placeholders.push(Origin(i as u32));
        }
    }
    let may_have_placeholders =
        plant != Plant::AllKnown && placeholders.len() >= 2 && !flow.subsets.is_empty();
    if loans.is_empty() && !may_have_placeholders {
        let places = Rows::new(flow.locals as usize, core::iter::empty(), meter, alloc)?;
        return Ok(Candidates { origins, loans, places, check_placeholders: false });
    }

    let forward = Rows::new(count, flow.subsets.iter().map(|&(_, a, b)| (a.index(), b)), meter, alloc)?;
    let backward = Rows::new(count, flow.subsets.iter().map(|&(_, a, b)| (b.index(), a)), meter, alloc)?;
    let (outgoing, incoming) = if plant == Plant::TierTwoReversed {
        (&backward, &forward)
    } else {
        (&forward, &backward)
    };
    let mut seen = Vec::with_capacity_in(count, alloc);
    seen.resize(count, usize::MAX);
    let mut walk = Walk { seen, stack: Vec::with_capacity_in(count, alloc) };

    if !loans.is_empty() {
        const SHORT_SPAN_MAX: u32 = 64;
        const SHORT_REACH_MAX: usize = 16;

        // Precompute which origins can reach any placeholder along `outgoing` (live at every point).
        let mut reaches_placeholder = Vec::with_capacity_in(count, alloc);
        reaches_placeholder.resize(count, false);
        if !placeholders.is_empty() {
            let ph_gen = 0usize;
            for &ph in &placeholders {
                walk.seed(ph, ph_gen);
            }
            walk.close(incoming, ph_gen, None, meter)?;
            for (i, slot) in reaches_placeholder.iter_mut().enumerate() {
                *slot = walk.seen[i] == ph_gen;
            }
        }

        // A loan issued at Mid cannot reach its own Start or an earlier Start in an
        // increasing CFG. On a back edge statement order gives no such proof.
        let mut increasing = true;
        meter.charge(flow.stmts.len())?;
        meter.charge(flow.succ.len())?;
        for s in 0..flow.stmts.len() {
            flow.for_each_succ(Stmt(s as u32), |next| increasing &= next.index() > s);
            if !increasing {
                break;
            }
        }

        let mut visit_epoch = Vec::with_capacity_in(count, alloc);
        visit_epoch.resize(count, 0u64);
        let mut epoch = 0u64;
        let mut visited_buf = Vec::new_in(alloc);
        let mut dfs_stack = Vec::new_in(alloc);
        let mut long_loans: Vec<(usize, Loan), A> = Vec::new_in(alloc);

        // First handle short-lived intra-block loans directly over their exact live statement
        // window, avoiding `O(K^2)` scans on hot locals with thousands of temporary borrows.
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
                    let nexts = outgoing.row(u.index());
                    meter.charge(1 + nexts.len())?;
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
                    for &v in nexts {
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
                        let start = if increasing { lo.max(row.at.0 + 1) } else { lo };
                        for s in start..=hi {
                            meter.charge(1)?;
                            let stmt = Stmt(s);
                            let pt = Point::start(stmt);
                            if !visited_buf.iter().any(|&v| facts.is_live(flow, pt, v)) {
                                continue;
                            }
                            let act = facts.is_active(flow, stmt, loan);
                            let mut hit = false;
                            for_each_access(flow, stmt, |access| {
                                if !hit && conflicts(flow, loan, access, act) {
                                    hit = true;
                                }
                            });
                            if hit {
                                loans[i] = true;
                                break;
                            }
                        }
                    }
                    continue;
                }
            }
            long_loans.push((flow.place(row.place).local.index(), loan));
        }

        if !long_loans.is_empty() {
            let long_places = Rows::new(
                flow.locals as usize,
                long_loans.iter().copied(),
                meter,
                alloc,
            )?;
            let mut dead_at = Vec::with_capacity_in(count, alloc);
            dead_at.resize(count, u32::MAX);

            meter.charge(flow.stmts.len())?;
            for s in 0..flow.stmts.len() {
                let su = s as u32;
                let stmt = Stmt(su);
                let pt = Point::start(stmt);
                let mut visit = |access: super::Access| -> Result<(), Exhausted> {
                    let same_local = long_places.row(flow.place(access.place).local.index());
                    meter.charge(1 + same_local.len())?;
                    for &loan in same_local {
                        let row = flow.loan(loan);
                        if loans[loan.index()]
                            || (increasing && row.at >= stmt)
                            || !conflicts(flow, loan, access, facts.is_active(flow, stmt, loan))
                        {
                            continue;
                        }
                        let oi = row.origin.index();
                        if reaches_placeholder[oi] || facts.is_live(flow, pt, row.origin) {
                            loans[loan.index()] = true;
                            continue;
                        }
                        if outgoing.row(oi).is_empty() || dead_at[oi] == su {
                            continue;
                        }
                        epoch += 1;
                        visited_buf.clear();
                        dfs_stack.clear();
                        visit_epoch[oi] = epoch;
                        visited_buf.push(row.origin);
                        dfs_stack.push(row.origin);
                        let mut found_live = false;
                        while let Some(u) = dfs_stack.pop() {
                            let nexts = outgoing.row(u.index());
                            meter.charge(1 + nexts.len())?;
                            for &v in nexts {
                                let vi = v.index();
                                if visit_epoch[vi] == epoch || dead_at[vi] == su {
                                    continue;
                                }
                                if facts.is_live(flow, pt, v) {
                                    found_live = true;
                                    break;
                                }
                                visit_epoch[vi] = epoch;
                                visited_buf.push(v);
                                dfs_stack.push(v);
                            }
                            if found_live {
                                break;
                            }
                        }
                        if found_live {
                            loans[loan.index()] = true;
                        } else {
                            for &v in &visited_buf {
                                dead_at[v.index()] = su;
                            }
                        }
                    }
                    Ok(())
                };
                let mut visited = Ok(());
                for_each_access(flow, stmt, |access| {
                    if visited.is_ok() {
                        visited = visit(access);
                    }
                });
                visited?;
            }
        }
    }

    // Build `places` over only surviving loans so solver observation never scans dropped loans.
    let mut kept_places: Vec<(usize, Loan), A> = Vec::new_in(alloc);
    for (i, row) in flow.loans.iter().enumerate() {
        if loans[i] {
            kept_places.push((flow.place(row.place).local.index(), Loan(i as u32)));
        }
    }
    let places = Rows::new(flow.locals as usize, kept_places.iter().copied(), meter, alloc)?;

    // Every intermediate on a path transferring a retained loan is reachable from its
    // issue origin. Independently, placeholder errors can require intermediates that
    // never contain any loan. Follow their subset edges in the stated direction even
    // in the control that reverses loan transfer.
    let mut walk_gen = 1usize;
    let loan_walk = walk_gen;
    walk_gen += 1;
    for (i, row) in flow.loans.iter().enumerate() {
        if loans[i] {
            walk.seed(row.origin, loan_walk);
        }
    }
    walk.close(outgoing, loan_walk, None, meter)?;
    for (i, keep) in origins.iter_mut().enumerate() {
        *keep = walk.seen[i] == loan_walk;
    }

    // Only keep placeholder-bridge origins (and enable per-point placeholder closure in the
    // solver) when at least one globally reachable placeholder pair is NOT already proven by
    // `flow.known`.
    let mut check_placeholders = false;
    if may_have_placeholders {
        let known_out = (plant != Plant::KnownIgnored && !flow.known.is_empty())
            .then(|| Rows::new(count, flow.known.iter().map(|&(a, b)| (a.index(), b)), meter, alloc))
            .transpose()?;
        let mut unproven_from = Vec::new_in(alloc);
        let mut unproven_into = Vec::new_in(alloc);
        let mut into_marked = Vec::with_capacity_in(count, alloc);
        into_marked.resize(count, false);
        let mut reached_ph = Vec::new_in(alloc);

        for &from in &placeholders {
            let reach_gen = walk_gen;
            walk_gen += 1;
            reached_ph.clear();
            walk.seed(from, reach_gen);
            while let Some(u) = walk.stack.pop() {
                let row = forward.row(u.index());
                meter.charge(1 + row.len())?;
                for &v in row {
                    if walk.seen[v.index()] != reach_gen {
                        walk.seen[v.index()] = reach_gen;
                        if flow.placeholder[v.index()] {
                            reached_ph.push(v);
                        }
                        walk.stack.push(v);
                    }
                }
            }
            if reached_ph.is_empty() {
                continue;
            }
            let known_gen = if let Some(ref known_out) = known_out {
                let kg = walk_gen;
                walk_gen += 1;
                walk.seed(from, kg);
                walk.close(known_out, kg, None, meter)?;
                Some(kg)
            } else {
                None
            };
            let mut from_unproven = false;
            for &into in &reached_ph {
                let is_known = known_gen.is_some_and(|kg| walk.seen[into.index()] == kg);
                if !is_known {
                    from_unproven = true;
                    if !into_marked[into.index()] {
                        into_marked[into.index()] = true;
                        unproven_into.push(into);
                    }
                }
            }
            if from_unproven {
                unproven_from.push(from);
            }
        }

        if !unproven_from.is_empty() {
            check_placeholders = true;
            let placeholder_walk = walk_gen;
            walk_gen += 1;
            for &from in &unproven_from {
                walk.seed(from, placeholder_walk);
            }
            walk.close(&forward, placeholder_walk, None, meter)?;
            // Intersect forward reach from unproven sources with backward reach from unproven
            // targets. A dead branch or an already-proven pair cannot witness an unproven relation.
            let between_placeholders = walk_gen;
            for &into in &unproven_into {
                walk.seed(into, between_placeholders);
            }
            walk.close(&backward, between_placeholders, Some(placeholder_walk), meter)?;
            for (i, keep) in origins.iter_mut().enumerate() {
                *keep |= walk.seen[i] == between_placeholders;
            }
        }
    }
    Ok(Candidates { origins, loans, places, check_placeholders })
}
