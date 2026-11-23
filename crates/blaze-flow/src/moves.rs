//! The move check (flow.md section 2): maybe-initialized and maybe-uninitialized as forward
//! bitset dataflows over the places, the three findings, and the drop kind drop elaboration reads.
//!
//! The rule (the Reference, "Moved and copied types"): "After moving out of a place expression
//! that evaluates to a local variable, the location is deinitialized and cannot be read from
//! again until it is reinitialized", and "Only the following place expressions may be moved out
//! of: ... Fields of a place expression which can be moved out of and don't implement `Drop`.
//! The result of dereferencing an expression with type `Box<T>` ...".
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::bits::{self, BitWorklist, Bits};
use crate::flow::{Bound, Flow, Operand, Place, Point, Proj, Span, Stmt, StmtKind, Value};
use crate::record::Plant;

/// Why a place may not be moved out of.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum MoveOut {
    /// Through a reference or raw pointer: a borrow.
    Deref,
    Index,
    /// A field of a type that implements `Drop`.
    DropField,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum MoveFindingKind {
    /// A read, move or borrow of a place that may be (partly) uninitialized.
    UseOfMoved,
    MoveOut(MoveOut),
    /// An assignment to a part of a place that may be uninitialized.
    AssignToMovedPart,
}

/// A move finding: what, which place, where (an item-relative range).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MoveFinding {
    pub at: Point,
    pub place: Place,
    pub kind: MoveFindingKind,
    pub span: Span,
}

/// What drop elaboration does at a `Drop p` (flow.md section 2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DropKind {
    /// Initialized on every path: drop it.
    Static,
    /// Initialized on no path: nothing.
    Nothing,
    /// Initialized on some paths: a drop behind a flag.
    Flag,
    /// Initialized but a part moved out: drop the initialized parts.
    Open,
}

/// Compact DFS Euler-tour index of the place forest, built once in `O(P)`:
/// `order[enter[p]..exit[p]]` is `p` followed by all strict descendants of `p`.
pub(crate) struct PlaceSubtrees<A: Allocator> {
    enter: Vec<u32, A>,
    exit: Vec<u32, A>,
    order: Vec<u32, A>,
}

impl<A: Allocator + Copy> PlaceSubtrees<A> {
    pub(crate) fn new_in<B: Allocator>(flow: &Flow<B>, alloc: A) -> Self {
        let m = flow.places.len();
        let mut enter = Vec::with_capacity_in(m, alloc);
        enter.resize(m, 0u32);
        let mut exit = Vec::with_capacity_in(m, alloc);
        exit.resize(m, 0u32);
        let mut order = Vec::with_capacity_in(m, alloc);
        if m == 0 {
            return Self { enter, exit, order };
        }
        // Build CSR of direct children for each place.
        let mut child_off = Vec::with_capacity_in(m + 1, alloc);
        child_off.resize(m + 1, 0u32);
        for row in flow.places.iter() {
            if let Some(par) = row.parent {
                child_off[par.index()] += 1;
            }
        }
        let mut end = 0u32;
        for c in child_off[..m].iter_mut() {
            end += *c;
            *c = end;
        }
        child_off[m] = end;
        let mut children = Vec::with_capacity_in(end as usize, alloc);
        children.resize(end as usize, 0u32);
        for q in (0..m).rev() {
            if let Some(par) = flow.places[q].parent {
                let p = par.index();
                child_off[p] -= 1;
                children[child_off[p] as usize] = q as u32;
            }
        }
        // Iterative DFS over all roots (`parent == None`).
        let mut stack = Vec::with_capacity_in(m, alloc);
        for root in 0..m {
            if flow.places[root].parent.is_some() {
                continue;
            }
            enter[root] = order.len() as u32;
            order.push(root as u32);
            stack.push((root as u32, child_off[root]));
            while let Some((node, next)) = stack.last_mut() {
                let u = *node as usize;
                if *next < child_off[u + 1] {
                    let c = children[*next as usize] as usize;
                    *next += 1;
                    enter[c] = order.len() as u32;
                    order.push(c as u32);
                    stack.push((c as u32, child_off[c]));
                } else {
                    exit[u] = order.len() as u32;
                    stack.pop();
                }
            }
        }
        Self { enter, exit, order }
    }
}

impl<A: Allocator> PlaceSubtrees<A> {
    #[inline]
    pub(crate) fn subtree(&self, p: Place) -> &[u32] {
        let i = p.index();
        &self.order[self.enter[i] as usize..self.exit[i] as usize]
    }

    #[inline]
    pub(crate) fn descendants(&self, p: Place) -> &[u32] {
        let i = p.index();
        &self.order[self.enter[i] as usize + 1..self.exit[i] as usize]
    }
}

/// The two dataflows' entry states, one row per statement.
pub struct Inits<A: Allocator> {
    pub init: Bits<A>,
    pub uninit: Bits<A>,
    pub reached: Vec<bool, A>,
    pub(crate) subtrees: PlaceSubtrees<A>,
}

impl<A: Allocator> Inits<A> {
    /// `drop p` at `s` may run a destructor: `s` is reached and some part of `p` (itself or a
    /// place below it) may be initialized on entry to it. A drop of a place moved out on every
    /// path is no use of it (drop elaboration removes it; RFC 2094, "liveness": a variable is
    /// drop-live only where it may be initialized).
    pub fn drop_may_run<B: Allocator>(&self, _flow: &Flow<B>, s: Stmt, p: Place) -> bool {
        if !self.reached[s.index()] {
            return false;
        }
        let row = self.init.row(s.index());
        self.subtrees.subtree(p).iter().any(|&q| bits::test(row, q as usize))
    }
}

/// Calls `f` on each operand a value reads.
pub fn for_each_operand<A: Allocator>(flow: &Flow<A>, value: Value, mut f: impl FnMut(Operand)) {
    match value {
        Value::Use(o) => f(o),
        Value::Borrow { .. } => {}
        Value::Aggregate(ops) => {
            for o in flow.ops(ops) {
                f(*o);
            }
        }
    }
}

fn deinit<A: Allocator>(flow: &Flow<A>, p: Place, init: &mut [u64], uninit: &mut [u64]) {
    for q in 0..flow.places.len() {
        if flow.is_prefix(p, Place(q as u32)) {
            bits::put(uninit, q, true);
            bits::put(init, q, false);
        }
    }
}

fn initialize<A: Allocator>(flow: &Flow<A>, p: Place, init: &mut [u64], uninit: &mut [u64]) {
    for q in 0..flow.places.len() {
        if flow.is_prefix(p, Place(q as u32)) {
            bits::put(init, q, true);
            bits::put(uninit, q, false);
        }
    }
}

#[inline]
fn deinit_sub<A: Allocator>(subtrees: &PlaceSubtrees<A>, p: Place, init: &mut [u64], uninit: &mut [u64]) {
    for &q in subtrees.subtree(p) {
        let q = q as usize;
        bits::put(uninit, q, true);
        bits::put(init, q, false);
    }
}

#[inline]
fn initialize_sub<A: Allocator>(subtrees: &PlaceSubtrees<A>, p: Place, init: &mut [u64], uninit: &mut [u64]) {
    for &q in subtrees.subtree(p) {
        let q = q as usize;
        bits::put(init, q, true);
        bits::put(uninit, q, false);
    }
}

/// The transfer of one statement on (init, uninit).
pub fn transfer<A: Allocator>(flow: &Flow<A>, s: Stmt, init: &mut [u64], uninit: &mut [u64]) {
    let moved = |init: &mut [u64], uninit: &mut [u64], o: Operand| {
        if let Operand::Move(p) = o {
            deinit(flow, p, init, uninit);
        }
    };
    match flow.stmt(s).kind {
        StmtKind::Assign { dest, value } => {
            for_each_operand(flow, value, |o| moved(init, uninit, o));
            initialize(flow, dest, init, uninit);
        }
        StmtKind::Call { args, dest } => {
            for o in flow.ops(args) {
                moved(init, uninit, *o);
            }
            initialize(flow, dest, init, uninit);
        }
        StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => {
            deinit(flow, flow.local_place(l), init, uninit);
        }
        StmtKind::Drop(p) => deinit(flow, p, init, uninit),
        _ => {}
    }
}

fn transfer_with<A: Allocator, B: Allocator>(
    subtrees: &PlaceSubtrees<B>,
    flow: &Flow<A>,
    s: Stmt,
    init: &mut [u64],
    uninit: &mut [u64],
) {
    let moved = |init: &mut [u64], uninit: &mut [u64], o: Operand| {
        if let Operand::Move(p) = o {
            deinit_sub(subtrees, p, init, uninit);
        }
    };
    match flow.stmt(s).kind {
        StmtKind::Assign { dest, value } => {
            for_each_operand(flow, value, |o| moved(init, uninit, o));
            initialize_sub(subtrees, dest, init, uninit);
        }
        StmtKind::Call { args, dest } => {
            for o in flow.ops(args) {
                moved(init, uninit, *o);
            }
            initialize_sub(subtrees, dest, init, uninit);
        }
        StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => {
            deinit_sub(subtrees, flow.local_place(l), init, uninit);
        }
        StmtKind::Drop(p) => deinit_sub(subtrees, p, init, uninit),
        _ => {}
    }
}

/// The two forward dataflows, join = union, solved with an RPO worklist over reachable statements.
/// Bound: every pop corresponds to initial reach or at least one monotone bit change, so pops
/// are at most `2 * stmts * places + stmts + 1`.
pub fn inits<A: Allocator + Copy>(flow: &Flow<A>, plant: Plant, alloc: A) -> Result<Inits<A>, Bound> {
    let (n, m) = (flow.stmts.len(), flow.places.len());
    let subtrees = PlaceSubtrees::new_in(flow, alloc);
    let mut init = Bits::new_in(n, m, alloc);
    let mut uninit = Bits::new_in(n, m, alloc);
    let mut reached = Vec::with_capacity_in(n, alloc);
    reached.resize(n, false);
    if n == 0 {
        return Ok(Inits { init, uninit, reached, subtrees });
    }
    let e = flow.entry().index();
    reached[e] = true;
    for q in 0..m {
        let l = flow.places[q].local.0;
        if l >= 1 && l <= flow.args {
            init.set(e, q);
        } else {
            uninit.set(e, q);
        }
    }
    // Compute RPO of statements reachable from `entry` via iterative DFS.
    let mut seen = Vec::with_capacity_in(n, alloc);
    seen.resize(n, false);
    let mut rpo = Vec::with_capacity_in(n, alloc);
    let mut dfs = Vec::with_capacity_in(n, alloc);
    seen[e] = true;
    dfs.push((e as u32, 0u32));
    while let Some((node, next)) = dfs.last_mut() {
        let s = Stmt(*node);
        if let Some(t) = flow.nth_succ(s, *next) {
            *next += 1;
            let ti = t.index();
            if !seen[ti] {
                seen[ti] = true;
                dfs.push((t.0, 0u32));
            }
        } else {
            rpo.push(*node);
            dfs.pop();
        }
    }
    drop(dfs);
    drop(seen);
    rpo.reverse();
    let k = rpo.len();
    let mut rank = Vec::with_capacity_in(n, alloc);
    rank.resize(n, u32::MAX);
    for (r, &s) in rpo.iter().enumerate() {
        rank[s as usize] = r as u32;
    }

    let w = init.words();
    let mut si = Vec::with_capacity_in(w, alloc);
    si.resize(w, 0u64);
    let mut su = Vec::with_capacity_in(w, alloc);
    su.resize(w, 0u64);
    let mut worklist = BitWorklist::new_in(k, alloc);
    worklist.push(rank[e] as usize);

    let limit = (2 * n * m + n + 1) as u64;
    let mut pops = 0u64;
    while let Some(r) = worklist.pop_min() {
        pops += 1;
        if pops > limit {
            return Err(Bound::Passes { what: "move dataflow passes", limit });
        }
        let s = rpo[r] as usize;
        si.copy_from_slice(init.row(s));
        su.copy_from_slice(uninit.row(s));
        transfer_with(&subtrees, flow, Stmt(s as u32), &mut si, &mut su);
        flow.for_each_succ(Stmt(s as u32), |t| {
            let t = t.index();
            let changed = if !reached[t] {
                reached[t] = true;
                init.row_mut(t).copy_from_slice(&si);
                uninit.row_mut(t).copy_from_slice(&su);
                true
            } else if plant == Plant::MovesIntersect {
                bits::intersect_into(init.row_mut(t), &si) | bits::intersect_into(uninit.row_mut(t), &su)
            } else {
                bits::union_into(init.row_mut(t), &si) | bits::union_into(uninit.row_mut(t), &su)
            };
            if changed {
                worklist.push(rank[t] as usize);
            }
        });
    }
    Ok(Inits { init, uninit, reached, subtrees })
}

/// `q`, a prefix of `q` or a part of `q` may be uninitialized in `uninit`.
pub fn maybe_uninit<A: Allocator>(flow: &Flow<A>, uninit: &[u64], q: Place) -> bool {
    (0..flow.places.len()).any(|r| {
        let r = Place(r as u32);
        bits::test(uninit, r.index()) && (flow.is_prefix(r, q) || flow.is_prefix(q, r))
    })
}

fn maybe_uninit_with<A: Allocator, B: Allocator>(
    subtrees: &PlaceSubtrees<B>,
    flow: &Flow<A>,
    uninit: &[u64],
    q: Place,
) -> bool {
    for &r in subtrees.subtree(q) {
        if bits::test(uninit, r as usize) {
            return true;
        }
    }
    let mut cur = q;
    while let Some(parent) = flow.place(cur).parent {
        if bits::test(uninit, parent.index()) {
            return true;
        }
        cur = parent;
    }
    false
}

/// Why `p` may not be moved out of, nearest projection first; `None` if it may.
pub fn move_out<A: Allocator>(flow: &Flow<A>, mut p: Place) -> Option<MoveOut> {
    loop {
        let row = flow.place(p);
        match row.proj {
            Some(Proj::Deref) => return Some(MoveOut::Deref),
            Some(Proj::Index) => return Some(MoveOut::Index),
            // An element or a subslice of an unsized `[T]` (the lowering marks the row
            // `of_slice`; an array's is moved out of freely): E0508.
            Some(Proj::ConstantIndex { .. } | Proj::Subslice { .. }) if row.of_slice => return Some(MoveOut::Index),
            Some(Proj::Field(_)) if row.of_drop_type => return Some(MoveOut::DropField),
            _ => {}
        }
        p = row.parent?;
    }
}

/// The move check's findings, in statement order, then operand order, then the destination.
pub fn check_moves<A: Allocator + Copy>(flow: &Flow<A>, alloc: A) -> Result<Vec<MoveFinding, A>, Bound> {
    check_moves_planted(flow, Plant::None, alloc)
}

#[doc(hidden)]
pub fn check_moves_planted<A: Allocator + Copy>(
    flow: &Flow<A>,
    plant: Plant,
    alloc: A,
) -> Result<Vec<MoveFinding, A>, Bound> {
    let st = inits(flow, plant, alloc)?;
    // A statement's operands are read in order, each after the moves of those before it (the
    // Reference, "Evaluation order of operands": left to right; MIR borrowck checks `(move p,
    // move p)` operand by operand, E0382). Checked all against the statement's entry state,
    // `(p, p)` and `h(p, p)` of a non-`Copy` `p` were clean (lane B11's `twice_p`). `seen` is
    // the entry state with the earlier operands' moves applied; `scratch` takes `deinit`'s
    // init half, unused.
    let w = st.uninit.words();
    let mut seen: Vec<u64, A> = Vec::with_capacity_in(w, alloc);
    seen.resize(w, 0u64);
    let mut scratch: Vec<u64, A> = Vec::with_capacity_in(w, alloc);
    scratch.resize(w, 0u64);
    let mut out = Vec::new_in(alloc);
    for s in 0..flow.stmts.len() {
        if !st.reached[s] {
            continue;
        }
        let stmt = Stmt(s as u32);
        let (at, span) = (Point::start(stmt), flow.stmt(stmt).span);
        let uninit = st.uninit.row(s);
        if plant == Plant::OperandsAtEntry {
            seen.fill(0);
        } else {
            seen.copy_from_slice(uninit);
        }
        let mut found =
            |place: Place, kind: MoveFindingKind| out.push(MoveFinding { at, place, kind, span });
        let mut operand = |o: Operand, found: &mut dyn FnMut(Place, MoveFindingKind)| match o {
            Operand::Copy(p) | Operand::Move(p) => {
                if maybe_uninit_with(&st.subtrees, flow, uninit, p)
                    || maybe_uninit_with(&st.subtrees, flow, &seen, p)
                {
                    found(p, MoveFindingKind::UseOfMoved);
                }
                if let (Operand::Move(_), Some(why)) = (o, move_out(flow, p)) {
                    found(p, MoveFindingKind::MoveOut(why));
                }
                if matches!(o, Operand::Move(_)) && plant != Plant::OperandsAtEntry {
                    deinit_sub(&st.subtrees, p, &mut scratch, &mut seen);
                }
            }
            Operand::Const => {}
        };
        let dest = match flow.stmt(stmt).kind {
            StmtKind::Assign { dest, value } => {
                for_each_operand(flow, value, |o| operand(o, &mut found));
                if let Value::Borrow { place, .. } = value
                    && maybe_uninit_with(&st.subtrees, flow, uninit, place)
                {
                    found(place, MoveFindingKind::UseOfMoved);
                }
                Some(dest)
            }
            StmtKind::Call { args, dest } => {
                for o in flow.ops(args) {
                    operand(*o, &mut found);
                }
                Some(dest)
            }
            StmtKind::Switch { on, .. } => {
                if maybe_uninit_with(&st.subtrees, flow, uninit, on) {
                    found(on, MoveFindingKind::UseOfMoved);
                }
                None
            }
            StmtKind::Return => {
                if maybe_uninit_with(&st.subtrees, flow, uninit, Place(0)) {
                    found(Place(0), MoveFindingKind::UseOfMoved);
                }
                None
            }
            _ => None,
        };
        // An assignment into a part: each proper prefix that may be uninitialized is a finding,
        // a use of the moved pointer when a deref lies between it and the destination, else one
        // assignment to a moved-from part. Each is "on some path", so none hides another.
        if let Some(dest) = dest {
            let (mut cur, mut through, mut part) = (dest, false, false);
            while let Some(parent) = flow.place(cur).parent {
                through |= matches!(flow.place(cur).proj, Some(Proj::Deref | Proj::DerefBox));
                cur = parent;
                if bits::test(uninit, cur.index()) {
                    if through {
                        found(cur, MoveFindingKind::UseOfMoved);
                    } else {
                        part = true;
                    }
                }
            }
            if part {
                found(dest, MoveFindingKind::AssignToMovedPart);
            }
        }
    }
    Ok(out)
}

/// What drop elaboration does at statement `s` for `p`, from the same dataflows.
pub fn drop_kind<A: Allocator>(_flow: &Flow<A>, st: &Inits<A>, s: Stmt, p: Place) -> DropKind {
    let (init, uninit) = (st.init.row(s.index()), st.uninit.row(s.index()));
    let (i, u) = (bits::test(init, p.index()), bits::test(uninit, p.index()));
    match (i, u) {
        (false, _) => DropKind::Nothing,
        (true, true) => DropKind::Flag,
        (true, false) => {
            if st.subtrees.descendants(p).iter().any(|&q| bits::test(uninit, q as usize)) {
                DropKind::Open
            } else {
                DropKind::Static
            }
        }
    }
}
