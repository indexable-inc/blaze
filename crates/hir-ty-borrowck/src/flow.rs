//! `BLAZE_BORROWCK=polonius-flow`: loan liveness and the universal regions' check as datalog
//! Polonius computes them (polonius-engine's naive rules, rustc's `-Zpolonius=legacy`), for a
//! body the alpha ([`super::polonius`]) rejects.
//!
//! What it has that the alpha does not:
//!
//! - kills in the traversal: a loan stops flowing out of a point that overwrites or ends the
//!   storage of what it borrows ([`Loans::killed_at`]), which is what accepts a cursor that
//!   reborrows through itself (`p = &mut (*p).next` in a loop);
//! - subsets that flow forward along the CFG while both of their regions are live, instead of
//!   a region's loans following it backwards through contravariant and invariant edges, which
//!   is what accepts `*x = match (*x).get() { Some(s) => s, None => (*x).new() }`;
//! - location-sensitive subset errors among universal regions: `'a: 'x` at one point and
//!   `'x: 'b` at another do not make `'a: 'b` unless the subset lives from one to the other.
//!
//! The rules, over blaze's points (one per statement or terminator) and region liveness:
//!
//! ```text
//! subset(A, B, P)       :- outlives(A, B, P).                 // located at P, or everywhere
//! subset(A, C, P)       :- subset(A, B, P), subset(B, C, P).
//! subset(A, B, Q)       :- subset(A, B, P), edge(P, Q), live(A, Q), live(B, Q).
//! contains(R, L, P)     :- issued(L, R, P).
//! contains(B, L, P)     :- contains(A, L, P), subset(A, B, P).
//! contains(R, L, Q)     :- contains(R, L, P), !killed(L, P) or issued(L, _, P),
//!                          edge(P, Q), live(R, Q).
//! loan_live(L, P)       :- contains(R, L, P), live(R, P).
//! subset_error(A, B, P) :- subset(A, B, P), universal(A), universal(B), !known(A, B).
//! ```
//!
//! A universal region is live everywhere. A loan made at a point that also kills it still flows
//! out of it, as the `Borrows` dataflow gens after it kills. A subset made from the constraints
//! that hold everywhere alone holds at every point and is not carried: [`Flow::carried`] keeps
//! the pairs whose path uses a constraint of a point or a carried pair.
//!
//! The caller intersects each loan's live points with the alpha's, so the mode only ever
//! accepts more than the alpha: each is a sound over-approximation of where the loan's reference
//! may still be used, and so is their intersection.

use hir_ty::mir::{BasicBlockId, MirBody};

use super::{
    Ctx,
    bitset::{BitSet, bits},
    cfg::{Cfg, index},
    intervals::IntervalSet,
    loans::Loans,
    mem::{FDeque, FSet, FVec, Frame, filled, set},
    regions::{Regions, UniversalKind},
    relation::Csr,
};

/// The steps (an edge followed, a pair carried, a region looked at) the analysis takes in one
/// body before it stops. A count of steps, not a time, so a body's outcome is the same on every
/// run and machine.
pub(super) const BUDGET: u64 = 1 << 24;

pub(super) enum Outcome<'a> {
    Done {
        /// The points each loan is live at, by loan.
        live_loans: FVec<'a, IntervalSet<'a>>,
        /// No subset among universal regions holds that the signature does not give, at any
        /// point, and the body is one whose universal regions this checks (see
        /// [`universals_checkable`]).
        universals_hold: bool,
    },
    /// The budget ran out.
    GaveUp { during: &'static str, steps: u64 },
}

/// Whether the datalog's subset errors are all the universal regions' check asks of this body:
/// not a closure or `const` block, whose regions' errors go to its creator; no placeholder of a
/// `for<'a>`, which datalog Polonius checks against points; no member constraint of an `impl
/// Trait`, whose choice reads region values.
pub(super) fn universals_checkable(ctx: &Ctx<'_, '_>, regions: &Regions<'_, '_>) -> bool {
    regions.check_universals
        && ctx.closure_local.is_none()
        && !ctx.inline_const
        && regions.members.is_empty()
        && !regions.universals.iter().any(|&(_, kind)| kind == UniversalKind::Placeholder)
}

struct Fuel {
    used: u64,
}

impl Fuel {
    /// Takes `steps`; false when that passes the budget.
    #[inline]
    fn take(&mut self, steps: usize) -> bool {
        self.used += steps as u64;
        self.used <= BUDGET
    }
}

/// What the passes read: the graph, liveness and the subsets each point starts with.
struct Flow<'a, 'r, 'db> {
    cfg: &'r Cfg<'r>,
    body: &'r MirBody<'db>,
    count: usize,
    universal: BitSet<'a>,
    liveness: &'r [IntervalSet<'r>],
    /// The constraints that hold everywhere, by sup, and by sub.
    everywhere: Csr<'a>,
    reverse: Csr<'a>,
    /// The constraints that hold at one point, by point, each `(sup, sub)`, sorted.
    at_point: Csr<'a, (u32, u32)>,
    /// The subsets each point starts with, `(sup, sub)` sorted: those carried from its
    /// predecessors whose path uses a constraint of a point, both regions live at it.
    carried: FVec<'a, FVec<'a, (u32, u32)>>,
}

impl Flow<'_, '_, '_> {
    #[inline]
    fn live(&self, rv: usize, point: usize) -> bool {
        self.universal.contains(rv) || self.liveness.get(rv).is_some_and(|it| it.contains(point))
    }

    /// The points that follow `point`: the next in its block, or the starts of its block's
    /// successors.
    fn successors(&self, point: usize, out: &mut FVec<'_, usize>) {
        out.clear();
        let location = self.cfg.location(point);
        if point < self.cfg.block_end(self.body, location.block) {
            out.push(point + 1);
        } else {
            out.extend(
                self.cfg.successors(location.block).iter().map(|&it| self.cfg.block_start(it)),
            );
        }
    }
}

/// The edges out of `region` among `edges`, sorted by sup.
fn out_of(edges: &[(u32, u32)], region: u32) -> &[(u32, u32)] {
    let from = edges.partition_point(|&(sup, _)| sup < region);
    let to = from + edges[from..].partition_point(|&(sup, _)| sup == region);
    &edges[from..to]
}

/// Loan liveness and the universal check of datalog Polonius, as the module says.
pub(super) fn analyze<'a, 'db>(
    ctx: &Ctx<'a, 'db>,
    body: &MirBody<'db>,
    cfg: &Cfg<'_>,
    regions: &Regions<'_, 'db>,
    count: usize,
    liveness: &[IntervalSet<'_>],
    loans: &Loans<'_>,
) -> Outcome<'a> {
    let frame = ctx.frame;
    let mut fuel = Fuel { used: 0 };
    let mut universal = bits(frame, count);
    for &(rv, _) in &regions.universals {
        // An answered placeholder holds no point (`inference::answer_placeholders`).
        if rv < count && !regions.answered_placeholders.contains(&rv) {
            universal.insert(rv);
        }
    }
    // A constraint of a region past `count` is one region inference does not read either
    // (`Constraints::subs_of`).
    let constraints = &regions.outlives;
    let mut everywhere: FVec<'_, (u32, u32)> = Vec::new_in(frame);
    let mut located: FVec<'_, (u32, (u32, u32))> = Vec::new_in(frame);
    for at in 0..constraints.len() {
        let (sup, sub) = (constraints.sup(at), constraints.sub(at));
        if sup >= count || sub >= count || sup == sub {
            continue;
        }
        match constraints.location(at) {
            Some(location) => located.push((cfg.point(location) as u32, (sup as u32, sub as u32))),
            None => everywhere.push((sup as u32, sub as u32)),
        }
    }
    everywhere.sort_unstable();
    everywhere.dedup();
    located.sort_unstable();
    located.dedup();
    if !fuel.take(constraints.len() + cfg.points) {
        return Outcome::GaveUp { during: "gathering constraints", steps: fuel.used };
    }
    let mut flow = Flow {
        cfg,
        body,
        count,
        universal,
        liveness,
        everywhere: Csr::build(frame, count, everywhere.iter().copied()),
        reverse: Csr::build(frame, count, everywhere.iter().map(|&(sup, sub)| (sub, sup))),
        at_point: Csr::build(frame, cfg.points, located.iter().copied()),
        carried: filled(frame, cfg.points, Vec::new_in(frame)),
    };
    let check_universals = universals_checkable(ctx, regions);
    let mut failing = false;
    if check_universals {
        match everywhere_universal_errors(frame, &flow, regions, &mut fuel) {
            Some(it) => failing |= it,
            None => {
                return Outcome::GaveUp { during: "checking universal regions", steps: fuel.used };
            }
        }
    }
    match carry_subsets(ctx, &mut flow, regions, check_universals, &mut fuel) {
        Some(it) => failing |= it,
        None => return Outcome::GaveUp { during: "carrying subsets", steps: fuel.used },
    }
    let Some(live_loans) = loan_liveness(ctx, &flow, loans, &mut fuel) else {
        return Outcome::GaveUp { during: "following loans", steps: fuel.used };
    };
    Outcome::Done { live_loans, universals_hold: check_universals && !failing }
}

/// Whether `longer: shorter` among universal regions is one the signature does not give: the
/// datalog's `subset_error`. `'static` outlives everything, and the body's own region is
/// outlived by everything, so neither is checked as the longer.
fn fails(regions: &Regions<'_, '_>, longer: usize, shorter: usize) -> bool {
    let (Some(longer), Some(shorter)) = (regions.universal(longer), regions.universal(shorter))
    else {
        return false;
    };
    longer != shorter
        && !matches!(regions.universals[longer].1, UniversalKind::Static | UniversalKind::FnBody)
        && !regions.knows(longer, shorter)
}

/// The subset errors the constraints that hold everywhere make alone, which hold at every
/// point. None when the budget runs out.
fn everywhere_universal_errors(
    frame: &Frame,
    flow: &Flow<'_, '_, '_>,
    regions: &Regions<'_, '_>,
    fuel: &mut Fuel,
) -> Option<bool> {
    let mut seen = bits(frame, flow.count);
    let mut stack: FVec<'_, u32> = Vec::new_in(frame);
    for &(longer, _) in &regions.universals {
        if longer >= flow.count {
            continue;
        }
        seen.clear();
        seen.insert(longer);
        stack.push(longer as u32);
        while let Some(region) = stack.pop() {
            let subs = flow.everywhere.of(region as usize);
            if !fuel.take(subs.len() + 1) {
                return None;
            }
            for &sub in subs {
                if seen.insert(sub as usize) {
                    if fails(regions, longer, sub as usize) {
                        return Some(true);
                    }
                    stack.push(sub);
                }
            }
        }
    }
    Some(false)
}

/// Fills [`Flow::carried`] to the fixpoint, block by block: a block's points are walked again
/// when what its start is carried grows. Returns whether a subset error was found (looked for
/// only when `check_universals`); None when the budget runs out.
fn carry_subsets(
    ctx: &Ctx<'_, '_>,
    flow: &mut Flow<'_, '_, '_>,
    regions: &Regions<'_, '_>,
    check_universals: bool,
    fuel: &mut Fuel,
) -> Option<bool> {
    let frame = ctx.frame;
    let blocks = flow.body.basic_blocks.len();
    let mut queued = bits(frame, blocks);
    let mut work: FDeque<'_, BasicBlockId> = FDeque::new_in(frame);
    // The reachable blocks in reverse postorder, then the others, which a loan made in one
    // still flows through.
    for &block in flow.cfg.order.iter() {
        queued.insert(index(block));
        work.push_back(block);
    }
    for (block, _) in flow.body.basic_blocks.iter() {
        if queued.insert(index(block)) {
            work.push_back(block);
        }
    }
    let mut failing = false;
    let mut local: FVec<'_, (u32, u32)> = Vec::new_in(frame);
    let mut out: FVec<'_, (u32, u32)> = Vec::new_in(frame);
    let mut kept: FVec<'_, (u32, u32)> = Vec::new_in(frame);
    let mut merged: FVec<'_, (u32, u32)> = Vec::new_in(frame);
    let mut successors: FVec<'_, usize> = Vec::new_in(frame);
    let mut closure = Closure::new(frame, flow.count);
    while let Some(block) = work.pop_front() {
        queued.remove(index(block));
        let (start, end) = (flow.cfg.block_start(block), flow.cfg.block_end(flow.body, block));
        for point in start..=end {
            local.clear();
            local.extend_from_slice(&flow.carried[point]);
            local.extend_from_slice(flow.at_point.of(point));
            local.sort_unstable();
            local.dedup();
            flow.successors(point, &mut successors);
            failing |= closure.close(
                flow,
                regions,
                check_universals,
                &local,
                &successors,
                &mut out,
                fuel,
            )?;
            for &next in successors.iter() {
                kept.clear();
                kept.extend(
                    out.iter().copied().filter(|&(a, b)| {
                        flow.live(a as usize, next) && flow.live(b as usize, next)
                    }),
                );
                if !fuel.take(out.len() + 1) {
                    return None;
                }
                if point < end {
                    // The next point of the block has this one as its only predecessor.
                    let into = &mut flow.carried[next];
                    into.clear();
                    into.extend_from_slice(&kept);
                    continue;
                }
                let into = &mut flow.carried[next];
                merged.clear();
                let (mut i, mut j) = (0, 0);
                while i < into.len() || j < kept.len() {
                    let take_old = j == kept.len() || (i < into.len() && into[i] <= kept[j]);
                    let it = if take_old { into[i] } else { kept[j] };
                    if take_old {
                        i += 1;
                        if j < kept.len() && kept[j] == it {
                            j += 1;
                        }
                    } else {
                        j += 1;
                    }
                    merged.push(it);
                }
                if merged.len() > into.len() {
                    into.clear();
                    into.extend_from_slice(&merged);
                    let target = flow.cfg.location(next).block;
                    if queued.insert(index(target)) {
                        work.push_back(target);
                    }
                }
            }
        }
    }
    Some(failing)
}

/// The scratch of [`Closure::close`], kept from one point to the next: a stamp per region,
/// and per region and whether the path used a constraint of the point, so clearing is one
/// increment.
struct Closure<'a> {
    stamp: u32,
    seen: FVec<'a, u32>,
    sources: FVec<'a, u32>,
    stack: FVec<'a, (u32, bool)>,
}

impl<'a> Closure<'a> {
    fn new(frame: &'a Frame, count: usize) -> Self {
        Closure {
            stamp: 0,
            seen: filled(frame, count * 2, 0),
            sources: Vec::new_in(frame),
            stack: Vec::new_in(frame),
        }
    }

    fn next_stamp(&mut self) -> u32 {
        if self.stamp == u32::MAX {
            self.seen.fill(0);
            self.stamp = 0;
        }
        self.stamp += 1;
        self.stamp
    }

    /// The subsets at a point whose path uses one of `local` (its constraints and what it is
    /// carried), between regions live at one of `successors`, sorted, into `out`. Returns
    /// whether one among universal regions fails, when `check_universals`.
    #[allow(clippy::too_many_arguments)]
    fn close(
        &mut self,
        flow: &Flow<'_, '_, '_>,
        regions: &Regions<'_, '_>,
        check_universals: bool,
        local: &[(u32, u32)],
        successors: &[usize],
        out: &mut FVec<'_, (u32, u32)>,
        fuel: &mut Fuel,
    ) -> Option<bool> {
        out.clear();
        if local.is_empty() {
            return Some(false);
        }
        let live_after = |rv: usize| successors.iter().any(|&next| flow.live(rv, next));
        // The regions that reach the sup of a local edge by constraints that hold everywhere:
        // only a path from one of them uses a local edge.
        let stamp = self.next_stamp();
        self.sources.clear();
        for &(sup, _) in local {
            if self.seen[sup as usize * 2] != stamp {
                self.seen[sup as usize * 2] = stamp;
                self.sources.push(sup);
            }
        }
        let mut at = 0;
        while let Some(&region) = self.sources.get(at) {
            at += 1;
            let sups = flow.reverse.of(region as usize);
            if !fuel.take(sups.len() + 1) {
                return None;
            }
            for &sup in sups {
                if self.seen[sup as usize * 2] != stamp {
                    self.seen[sup as usize * 2] = stamp;
                    self.sources.push(sup);
                }
            }
        }
        let mut failing = false;
        for at in 0..self.sources.len() {
            let source = self.sources[at] as usize;
            let universal_source = flow.universal.contains(source);
            if !universal_source && !live_after(source) {
                continue;
            }
            let stamp = self.next_stamp();
            self.stack.clear();
            self.seen[source * 2] = stamp;
            self.stack.push((source as u32, false));
            while let Some((region, used_local)) = self.stack.pop() {
                let region = region as usize;
                if used_local && region != source {
                    if live_after(region) {
                        out.push((source as u32, region as u32));
                    }
                    if check_universals && universal_source && fails(regions, source, region) {
                        failing = true;
                    }
                }
                let subs = flow.everywhere.of(region);
                let edges = out_of(local, region as u32);
                if !fuel.take(subs.len() + edges.len() + 1) {
                    return None;
                }
                for &sub in subs {
                    let key = sub as usize * 2 + used_local as usize;
                    if self.seen[key] != stamp {
                        self.seen[key] = stamp;
                        self.stack.push((sub, used_local));
                    }
                }
                for &(_, sub) in edges {
                    let key = sub as usize * 2 + 1;
                    if self.seen[key] != stamp {
                        self.seen[key] = stamp;
                        self.stack.push((sub, true));
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        Some(failing)
    }
}

/// The points each loan is live at: where a region that holds it is live. None when the
/// budget runs out.
fn loan_liveness<'a>(
    ctx: &Ctx<'a, '_>,
    flow: &Flow<'_, '_, '_>,
    loans: &Loans<'_>,
    fuel: &mut Fuel,
) -> Option<FVec<'a, IntervalSet<'a>>> {
    let frame = ctx.frame;
    let mut live = Vec::with_capacity_in(loans.loans.len(), frame);
    let mut visited: FSet<'_, (u32, u32)> = set(frame);
    let mut stack: FVec<'_, (u32, u32)> = Vec::new_in(frame);
    let mut successors: FVec<'_, usize> = Vec::new_in(frame);
    for (id, loan) in loans.loans.iter().enumerate() {
        let mut points = IntervalSet::new(frame);
        visited.clear();
        let issued = flow.cfg.point(loan.location);
        if loan.region < flow.count {
            visited.insert((loan.region as u32, issued as u32));
            stack.push((loan.region as u32, issued as u32));
        }
        while let Some((region, point)) = stack.pop() {
            let (rv, at) = (region as usize, point as usize);
            if flow.live(rv, at) {
                points.insert(at);
            }
            let subs = flow.everywhere.of(rv);
            let carried = out_of(&flow.carried[at], region);
            let located = out_of(flow.at_point.of(at), region);
            if !fuel.take(subs.len() + carried.len() + located.len() + 1) {
                return None;
            }
            let mut visit = |next: (u32, u32)| {
                if visited.insert(next) {
                    stack.push(next);
                }
            };
            for &sub in subs {
                visit((sub, point));
            }
            for &(_, sub) in carried.iter().chain(located) {
                visit((sub, point));
            }
            if at != issued && loans.killed_at(at).contains(&(id as u32)) {
                continue;
            }
            flow.successors(at, &mut successors);
            for &next in successors.iter() {
                if flow.live(rv, next) {
                    visit((region, next as u32));
                }
            }
        }
        live.push(points);
    }
    Some(live)
}
