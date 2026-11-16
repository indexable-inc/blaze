//! The borrow check (flow.md sections 3 to 5): accesses and conflicts, liveness, two-phase
//! activation, tier one (location-insensitive) and tier two (the Polonius rules as stated).
//!
//! Tier two is the definition. Each rule of the Polonius book (rules/loans) is one marked step of
//! [`tier_two`]; tier one is the same rules with the point and `loan_killed_at` dropped, so every
//! fact of tier two is a fact of tier one, and tier one accepting a body means tier two does.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::bits::Bits;
use crate::flow::{
    BorrowKind, Bound, Flow, Loan, Local, Operand, Origin, Place, Point, Proj, Span, Stmt,
    StmtKind, Value,
};
use crate::moves::for_each_operand;
use crate::record::Plant;

/// An access: a place, read or write, deep or shallow (RFC 2094).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Access {
    pub place: Place,
    pub write: bool,
    pub deep: bool,
    /// An activation does not conflict with its own loan.
    pub except: Option<Loan>,
}

fn access(place: Place, write: bool, deep: bool) -> Access {
    Access { place, write, deep, except: None }
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
        StmtKind::Drop(p) => f(access(p, true, true)),
        StmtKind::Activate(l) => {
            f(Access { place: flow.loan(l).place, write: true, deep: true, except: Some(l) })
        }
        StmtKind::Switch { on, .. } => f(access(on, false, false)),
        _ => {}
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

fn uses_and_def<A: Allocator>(flow: &Flow<A>, s: Stmt, mut used: impl FnMut(Local)) -> Option<Local> {
    let local = |p: Place| flow.place(p).local;
    let dest_def =|dest: Place, used: &mut dyn FnMut(Local)| {
        if flow.place(dest).depth == 0 {
            Some(local(dest))
        } else {
            used(local(dest));
            None
        }
    };
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
            dest_def(dest, &mut used)
        }
        StmtKind::Call { args, dest } => {
            for o in flow.ops(args) {
                if let Operand::Copy(p) | Operand::Move(p) = o {
                    used(local(*p));
                }
            }
            dest_def(dest, &mut used)
        }
        StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => Some(l),
        // Drop-liveness: every origin of a dropped local is taken as drop-live (no `may_dangle`).
        StmtKind::Drop(p) => {
            used(local(p));
            None
        }
        StmtKind::Activate(l) => {
            used(local(flow.loan(l).place));
            None
        }
        StmtKind::Switch { on, .. } => {
            used(local(on));
            None
        }
        StmtKind::Return => {
            used(Local(0));
            None
        }
        _ => None,
    }
}

/// What both tiers read: origins live at each point, and two-phase loans activated on entry.
pub struct Facts<A: Allocator> {
    /// Rows are points, columns origins: `origin_live_on_entry` or a placeholder.
    pub live: Bits<A>,
    /// Rows are statements, columns loans.
    pub active: Bits<A>,
}

/// Local liveness (backward, passes in reverse order; bound `2 * stmts * locals + 1` passes),
/// origin liveness from it (RFC 2094: "a lifetime L is live at a point P if there is some
/// variable `p` which is live at P, and L appears in the type of `p`"), and activation
/// (forward; bound `stmts * loans + stmts + 1` passes).
pub fn facts<A: Allocator + Clone>(flow: &Flow<A>, alloc: A) -> Result<Facts<A>, Bound> {
    let (n, nl, o) = (flow.stmts.len(), flow.loans.len(), flow.origins as usize);
    let locals = flow.locals as usize;
    let mut live_in = Bits::new_in(n, locals, alloc.clone());
    let mut live_out = Bits::new_in(n, locals, alloc.clone());
    let mut scratch_l = Vec::with_capacity_in(live_in.words(), alloc.clone());
    scratch_l.resize(live_in.words(), 0u64);
    let limit = (2 * n * locals + 1) as u64;
    let mut passes = 0u64;
    loop {
        passes += 1;
        if passes > limit {
            return Err(Bound { what: "liveness passes", limit });
        }
        let mut changed = false;
        for s in (0..n).rev() {
            // live_out(s) = union of live_in(t) over successors t;
            // live_in(s) = (live_out(s) - def(s)) + use(s).
            scratch_l.fill(0);
            flow.for_each_succ(Stmt(s as u32), |t| {
                crate::bits::union_into(&mut scratch_l, live_in.row(t.index()));
            });
            changed |= crate::bits::union_into(live_out.row_mut(s), &scratch_l);
            if let Some(d) = uses_and_def(flow, Stmt(s as u32), |_| {}) {
                crate::bits::put(&mut scratch_l, d.index(), false);
            }
            uses_and_def(flow, Stmt(s as u32), |l| crate::bits::put(&mut scratch_l, l.index(), true));
            changed |= crate::bits::union_into(live_in.row_mut(s), &scratch_l);
        }
        if !changed {
            break;
        }
    }
    let mut live = Bits::new_in(2 * n, o, alloc.clone());
    for s in 0..n {
        for &(l, origin) in flow.local_origins.iter() {
            if live_in.get(s, l.index()) {
                live.set(Point::start(Stmt(s as u32)).index(), origin.index());
            }
            // Mid is during the statement: what it reads and what lives after it.
            if live_in.get(s, l.index()) || live_out.get(s, l.index()) {
                live.set(Point::mid(Stmt(s as u32)).index(), origin.index());
            }
        }
        for origin in 0..o {
            if flow.placeholder[origin] {
                live.set(2 * s, origin);
                live.set(2 * s + 1, origin);
            }
        }
    }
    let mut active = Bits::new_in(n, nl, alloc.clone());
    let mut reached = Vec::with_capacity_in(n, alloc.clone());
    reached.resize(n, false);
    reached[flow.entry().index()] = true;
    let mut scratch = Vec::with_capacity_in(active.words(), alloc);
    scratch.resize(active.words(), 0u64);
    let limit = (n * nl + n + 1) as u64;
    let mut passes = 0u64;
    loop {
        passes += 1;
        if passes > limit {
            return Err(Bound { what: "activation passes", limit });
        }
        let mut changed = false;
        for s in 0..n {
            if !reached[s] {
                continue;
            }
            scratch.copy_from_slice(active.row(s));
            match flow.stmt(Stmt(s as u32)).kind {
                StmtKind::Activate(l) => crate::bits::put(&mut scratch, l.index(), true),
                StmtKind::Assign { value: Value::Borrow { loan, .. }, .. } => {
                    crate::bits::put(&mut scratch, loan.index(), false)
                }
                _ => {}
            }
            flow.for_each_succ(Stmt(s as u32), |t| {
                let t = t.index();
                if !reached[t] {
                    reached[t] = true;
                    changed = true;
                }
                changed |= crate::bits::union_into(active.row_mut(t), &scratch);
            });
        }
        if !changed {
            break;
        }
    }
    Ok(Facts { live, active })
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
    /// `errors(L, P)`: a live loan is invalidated.
    Invalidated { at: Point, loan: Loan, span: Span },
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

/// Tier one's answer: it accepts, or it cannot and tier two decides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TierOne {
    Accepted,
    Refused,
}

/// Closes `rows` over `edges` (`(from, into)`: row `into` |= row `from`) with a work list of
/// origins. Bound: pops are at most `origins + origins * columns` (a re-push needs growth).
fn close<A: Allocator + Clone>(
    edges: &[(Origin, Origin)],
    rows: &mut Bits<A>,
    origins: usize,
    cols: usize,
    alloc: A,
) -> Result<(), Bound> {
    let mut queue = Vec::new_in(alloc.clone());
    let mut queued = Vec::with_capacity_in(origins, alloc);
    queued.resize(origins, true);
    queue.extend((0..origins as u32).map(Origin));
    let limit = (origins + origins * cols) as u64;
    let mut head = 0usize;
    while head < queue.len() {
        if head as u64 >= limit {
            return Err(Bound { what: "tier one work list pops", limit });
        }
        let from = queue[head];
        head += 1;
        queued[from.index()] = false;
        for &(f, into) in edges {
            if f == from && rows.union_rows(into.index(), from.index()) && !queued[into.index()] {
                queued[into.index()] = true;
                queue.push(into);
            }
        }
    }
    Ok(())
}

fn subset_edges<A: Allocator + Clone>(flow: &Flow<A>, alloc: A) -> Vec<(Origin, Origin), A> {
    let mut edges = Vec::new_in(alloc);
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
fn known_closure<A: Allocator + Clone>(flow: &Flow<A>, plant: Plant, alloc: A) -> Result<Bits<A>, Bound> {
    let o = flow.origins as usize;
    let mut known = Bits::new_in(o, o, alloc.clone());
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
pub fn tier_one<A: Allocator + Clone>(
    flow: &Flow<A>,
    facts: &Facts<A>,
    plant: Plant,
    alloc: A,
) -> Result<TierOne, Bound> {
    let (o, nl) = (flow.origins as usize, flow.loans.len());
    let edges = subset_edges(flow, alloc.clone());
    let mut contains = Bits::new_in(o, nl, alloc.clone());
    for (l, row) in flow.loans.iter().enumerate() {
        contains.set(row.origin.index(), l);
    }
    // The planted control keeps each loan in the origin it was issued into only.
    if plant != Plant::TierOneUnclosed {
        close(&edges, &mut contains, o, nl, alloc.clone())?;
    }
    for s in 0..flow.stmts.len() {
        let stmt = Stmt(s as u32);
        for l in 0..nl {
            if invalidated(flow, facts, stmt, Loan(l as u32))
                && (0..o).any(|x| contains.get(x, l) && facts.live.get(Point::start(stmt).index(), x))
            {
                return Ok(TierOne::Refused);
            }
        }
    }
    let mut reach = Bits::new_in(o, o, alloc.clone());
    for x in 0..o {
        reach.set(x, x);
    }
    close(&edges, &mut reach, o, o, alloc.clone())?;
    let known = known_closure(flow, plant, alloc)?;
    for into in 0..o {
        for from in 0..o {
            if from != into
                && flow.placeholder[from]
                && flow.placeholder[into]
                && reach.get(into, from)
                && !known.get(into, from)
            {
                return Ok(TierOne::Refused);
            }
        }
    }
    Ok(TierOne::Accepted)
}

/// Tier two: the Polonius rules evaluated as stated, over every point, to a fixed point.
/// `subset` is one bit per (point, origin, origin), `contains` one per (point, origin, loan).
/// Bound: passes in point order, each but the last adding a fact, so at most
/// `points * (origins^2 + origins * loans) + 1`.
pub fn tier_two<A: Allocator + Clone>(
    flow: &Flow<A>,
    facts: &Facts<A>,
    plant: Plant,
    alloc: A,
) -> Result<Vec<BorrowFinding, A>, Bound> {
    let (n, o, nl) = (flow.stmts.len(), flow.origins as usize, flow.loans.len());
    let points = 2 * n;
    let mut subset = Bits::new_in(points, o * o, alloc.clone());
    let mut contains = Bits::new_in(points, o * nl, alloc.clone());
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
            return Err(Bound { what: "tier two passes", limit });
        }
        let mut changed = false;
        for p in 0..points {
            // subset(O1, O3, P) :- subset(O1, O2, P), subset(O2, O3, P).
            // Each round adds a fact or ends: at most o^2 + 1 rounds.
            loop {
                let mut grew = false;
                for a in 0..o {
                    for b in 0..o {
                        if subset.get(p, a * o + b) {
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
                for b in 0..o {
                    if subset.get(p, a * o + b) {
                        let (from, into) = if plant == Plant::TierTwoReversed { (b, a) } else { (a, b) };
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
                    for b in 0..o {
                        if subset.get(p, a * o + b) && live(q, a) && live(q, b) {
                            changed |= subset.set(q, a * o + b);
                        }
                    }
                }
                // ..(O, L, Q) :- ..(O, L, P), !loan_killed_at(L, P), cfg_edge(P, Q), live(O, Q).
                for a in 0..o {
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
    let mut out = Vec::new_in(alloc.clone());
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
                out.push(BorrowFinding::Invalidated {
                    at: p,
                    loan: Loan(l as u32),
                    span: flow.stmts[s].span,
                });
            }
        }
    }
    // subset_errors(O1, O2, P) :- subset(O1, O2, P), placeholder(O1), placeholder(O2),
    //   !known_placeholder_subset(O1, O2). Reported once per pair, at its first point.
    let known = known_closure(flow, plant, alloc.clone())?;
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
