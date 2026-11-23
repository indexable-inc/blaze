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
    let places = Rows::new(
        flow.locals as usize,
        flow.loans.iter().enumerate().map(|(i, row)| (flow.place(row.place).local.index(), Loan(i as u32))),
        meter,
        alloc,
    )?;
    let check_placeholders = plant != Plant::AllKnown
        && flow.placeholder.iter().filter(|p| **p).take(2).count() == 2;
    if loans.is_empty() && !check_placeholders {
        return Ok(Candidates { origins, loans, places });
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
        meter.charge(flow.stmts.len())?;
        for s in 0..flow.stmts.len() {
            let stmt = Stmt(s as u32);
            let live = facts.live.row(Point::start(stmt).index());
            let mut closed = false;
            let mut visit = |access: super::Access| -> Result<(), Exhausted> {
                let same_local = places.row(flow.place(access.place).local.index());
                meter.charge(1 + same_local.len())?;
                for &loan in same_local {
                    let row = flow.loan(loan);
                    if loans[loan.index()] || (increasing && row.at >= stmt)
                        || !conflicts(flow, loan, access, facts.active.get(s, loan.index()))
                    {
                        continue;
                    }
                    if crate::bits::test(live, row.origin.index()) {
                        loans[loan.index()] = true;
                        continue;
                    }
                    if outgoing.row(row.origin.index()).is_empty() {
                        continue;
                    }
                    if !closed {
                        meter.charge(live.len())?;
                        for (word, &bits) in live.iter().enumerate() {
                            let mut bits = bits;
                            while bits != 0 {
                                let origin = word * 64 + bits.trailing_zeros() as usize;
                                if origin >= count {
                                    break;
                                }
                                walk.seed(Origin(origin as u32), s);
                                bits &= bits - 1;
                            }
                        }
                        walk.close(incoming, s, None, meter)?;
                        closed = true;
                    }
                    loans[loan.index()] = walk.seen[row.origin.index()] == s;
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

    // Every intermediate on a path transferring a retained loan is reachable from its
    // issue origin. Independently, placeholder errors can require intermediates that
    // never contain any loan. Follow their subset edges in the stated direction even
    // in the control that reverses loan transfer.
    let loan_walk = flow.stmts.len();
    for (i, row) in flow.loans.iter().enumerate() {
        if loans[i] {
            walk.seed(row.origin, loan_walk);
        }
    }
    walk.close(outgoing, loan_walk, None, meter)?;
    for (i, keep) in origins.iter_mut().enumerate() {
        *keep = walk.seen[i] == loan_walk;
    }
    if check_placeholders {
        let placeholder_walk = loan_walk + 1;
        for (i, &placeholder) in flow.placeholder.iter().enumerate() {
            if placeholder {
                walk.seed(Origin(i as u32), placeholder_walk);
            }
        }
        walk.close(&forward, placeholder_walk, None, meter)?;
        // Intersect forward reach with backward reach from all placeholders. A dead
        // branch of the placeholder graph cannot witness any unproven relation.
        let between_placeholders = placeholder_walk + 1;
        for (i, &placeholder) in flow.placeholder.iter().enumerate() {
            if placeholder {
                walk.seed(Origin(i as u32), between_placeholders);
            }
        }
        walk.close(&backward, between_placeholders, Some(placeholder_walk), meter)?;
        for (i, keep) in origins.iter_mut().enumerate() {
            *keep |= walk.seen[i] == between_placeholders;
        }
    }
    Ok(Candidates { origins, loans, places })
}
