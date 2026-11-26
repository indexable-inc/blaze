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

use crate::bits::{self, BitWorklist, BlockBits};
use crate::flow::{Block, Bound, Flow, Local, Operand, Place, Point, Proj, Span, Stmt, StmtKind, Value};
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

    /// Whether `a` is `b` or an ancestor of `b` in the place forest, in `O(1)`.
    #[inline]
    pub(crate) fn is_prefix(&self, a: Place, b: Place) -> bool {
        let ea = self.enter[a.index()];
        let eb = self.enter[b.index()];
        ea <= eb && eb < self.exit[a.index()]
    }
}

/// Calls `f` on each local whose initialization state statement `s` may modify.
#[inline]
fn for_each_mod_local<B: Allocator>(flow: &Flow<B>, s: Stmt, mut f: impl FnMut(Local)) {
    match flow.stmt(s).kind {
        StmtKind::Assign { dest, value } => {
            f(flow.place(dest).local);
            for_each_operand(flow, value, |o| {
                if let Operand::Move(p) = o {
                    f(flow.place(p).local);
                }
            });
        }
        StmtKind::Call { args, dest } => {
            f(flow.place(dest).local);
            for &o in flow.ops(args) {
                if let Operand::Move(p) = o {
                    f(flow.place(p).local);
                }
            }
        }
        StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => f(l),
        StmtKind::Drop(p) => f(flow.place(p).local),
        _ => {}
    }
}

/// Compact CSR mapping each `Local` to the sorted (non-decreasing) `Stmt` indices in the body
/// that initialize or deinitialize any place of that local. Built once in `O(S + ops)`.
pub(crate) struct LocalMods<A: Allocator> {
    offsets: Vec<u32, A>,
    stmts: Vec<Stmt, A>,
}

impl<A: Allocator + Copy> LocalMods<A> {
    fn new_in<B: Allocator>(flow: &Flow<B>, alloc: A) -> Self {
        let locals = usize::try_from(flow.locals).unwrap_or(0);
        let n = flow.stmts.len();
        let mut offsets = Vec::with_capacity_in(locals + 1, alloc);
        offsets.resize(locals + 1, 0u32);
        if locals == 0 || n == 0 {
            return Self { offsets, stmts: Vec::new_in(alloc) };
        }
        for s in 0..n {
            for_each_mod_local(flow, Stmt(s as u32), |l| {
                offsets[l.index()] += 1;
            });
        }
        let mut total = 0u32;
        for c in offsets[..locals].iter_mut() {
            total += *c;
            *c = total;
        }
        offsets[locals] = total;
        let mut stmts = Vec::with_capacity_in(total as usize, alloc);
        stmts.resize(total as usize, Stmt(0));
        // Iterate statements in reverse so decrementing CSR cursors leaves each row sorted ascending.
        for s in (0..n).rev() {
            let stmt = Stmt(s as u32);
            for_each_mod_local(flow, stmt, |l| {
                let li = l.index();
                offsets[li] -= 1;
                stmts[offsets[li] as usize] = stmt;
            });
        }
        Self { offsets, stmts }
    }
}

impl<A: Allocator> LocalMods<A> {
    #[inline]
    fn row(&self, l: Local) -> &[Stmt] {
        let i = l.index();
        &self.stmts[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }
}

/// The two dataflows' entry states, one row per basic block.
pub struct Inits<A: Allocator> {
    pub init: BlockBits<A>,
    pub uninit: BlockBits<A>,
    pub reached: Vec<bool, A>,
    pub(crate) subtrees: PlaceSubtrees<A>,
    pub(crate) local_mods: LocalMods<A>,
}

impl<A: Allocator> Inits<A> {
    /// Returns `(maybe_init, maybe_uninit)` for place `q` on entry to statement `s` inside
    /// reachable basic block `b`.
    ///
    /// Because intra-block transfer only overwrites each place independently with `(true, false)`
    /// or `(false, true)`, the latest statement in `block.start..s` whose target or moved operand
    /// is a prefix of `q` determines `q`'s state; if none does, block `b`'s entry bits hold.
    fn place_state_at<B: Allocator>(&self, flow: &Flow<B>, b: Block, s: Stmt, q: Place) -> (bool, bool) {
        let start = flow.blocks[b.index()].start;
        if s.0 > start {
            let mods = self.local_mods.row(flow.place(q).local);
            let end = mods.partition_point(|&t| t < s);
            let mut prev = u32::MAX;
            for &t in mods[..end].iter().rev() {
                if t.0 < start {
                    break;
                }
                if t.0 == prev {
                    continue;
                }
                prev = t.0;
                match flow.stmt(t).kind {
                    StmtKind::Assign { dest, value } => {
                        if self.subtrees.is_prefix(dest, q) {
                            return (true, false);
                        }
                        let mut moved = false;
                        for_each_operand(flow, value, |o| {
                            if !moved
                                && let Operand::Move(mp) = o
                                && self.subtrees.is_prefix(mp, q)
                            {
                                moved = true;
                            }
                        });
                        if moved {
                            return (false, true);
                        }
                    }
                    StmtKind::Call { args, dest } => {
                        if self.subtrees.is_prefix(dest, q) {
                            return (true, false);
                        }
                        for &o in flow.ops(args) {
                            if let Operand::Move(mp) = o
                                && self.subtrees.is_prefix(mp, q)
                            {
                                return (false, true);
                            }
                        }
                    }
                    StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => {
                        if self.subtrees.is_prefix(flow.local_place(l), q) {
                            return (false, true);
                        }
                    }
                    StmtKind::Drop(dp) => {
                        if self.subtrees.is_prefix(dp, q) {
                            return (false, true);
                        }
                    }
                    _ => {}
                }
            }
        }
        (self.init.get(b, q.index()), self.uninit.get(b, q.index()))
    }

    /// `drop p` at `s` may run a destructor: `s` is reached and some part of `p` (itself or a
    /// place below it) may be initialized on entry to it. A drop of a place moved out on every
    /// path is no use of it (drop elaboration removes it; RFC 2094, "liveness": a variable is
    /// drop-live only where it may be initialized).
    pub fn drop_may_run<B: Allocator>(&self, flow: &Flow<B>, s: Stmt, p: Place) -> bool {
        let b = flow.stmt_block[s.index()];
        if !self.reached[b.index()] {
            return false;
        }
        self.subtrees.subtree(p).iter().any(|&q| self.place_state_at(flow, b, s, Place(q)).0)
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

/// The two forward dataflows, join = union, solved at basic-block grain with an RPO worklist over
/// reachable blocks. Bound: every pop corresponds to initial reach or at least one monotone bit
/// change in a block's entry row, so pops are at most `2 * blocks * places + blocks + 1`.
pub fn inits<A: Allocator + Copy>(flow: &Flow<A>, plant: Plant, alloc: A) -> Result<Inits<A>, Bound> {
    let (nb, n, m) = (flow.blocks.len(), flow.stmts.len(), flow.places.len());
    let subtrees = PlaceSubtrees::new_in(flow, alloc);
    let local_mods = LocalMods::new_in(flow, alloc);
    let mut init = BlockBits::new_in(flow, m, alloc);
    let mut uninit = BlockBits::new_in(flow, m, alloc);
    let mut reached = Vec::with_capacity_in(nb, alloc);
    reached.resize(nb, false);
    if n == 0 || nb == 0 || flow.blocks[0].end <= flow.blocks[0].start {
        return Ok(Inits { init, uninit, reached, subtrees, local_mods });
    }
    let eb_blk = flow.stmt_block[flow.entry().index()];
    let eb = eb_blk.index();
    reached[eb] = true;
    for q in 0..m {
        let l = flow.places[q].local.0;
        if l >= 1 && l <= flow.args {
            init.set(eb_blk, q);
        } else {
            uninit.set(eb_blk, q);
        }
    }
    // Compute RPO of basic blocks reachable from `eb` via iterative DFS.
    let mut seen = Vec::with_capacity_in(nb, alloc);
    seen.resize(nb, false);
    let mut rpo = Vec::with_capacity_in(nb, alloc);
    let mut dfs = Vec::with_capacity_in(nb, alloc);
    seen[eb] = true;
    dfs.push((eb as u32, 0u32));
    while let Some((node, next)) = dfs.last_mut() {
        let b = Block(*node);
        if let Some(t) = flow.nth_block_succ(b, *next) {
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
    let mut rank = Vec::with_capacity_in(nb, alloc);
    rank.resize(nb, u32::MAX);
    for (r, &b) in rpo.iter().enumerate() {
        rank[b as usize] = r as u32;
    }

    let w = init.words();
    let mut cur_init = Vec::with_capacity_in(w, alloc);
    cur_init.resize(w, 0u64);
    let mut cur_uninit = Vec::with_capacity_in(w, alloc);
    cur_uninit.resize(w, 0u64);
    let mut worklist = BitWorklist::new_in(k, alloc);
    worklist.push(rank[eb] as usize);

    let limit = (nb as u64)
        .saturating_mul(m as u64)
        .saturating_mul(2)
        .saturating_add(nb as u64)
        .saturating_add(1);
    let mut pops = 0u64;
    while let Some(r) = worklist.pop_min() {
        pops += 1;
        if pops > limit {
            return Err(Bound::Passes { what: "move dataflow passes", limit });
        }
        let blk = Block(rpo[r]);
        let range = flow.blocks[blk.index()];
        if range.end <= range.start {
            continue;
        }
        cur_init.copy_from_slice(init.row(blk));
        cur_uninit.copy_from_slice(uninit.row(blk));
        for s in range.start..range.end {
            transfer_with(&subtrees, flow, Stmt(s), &mut cur_init, &mut cur_uninit);
        }
        flow.for_each_block_succ(blk, |tb| {
            let t = tb.index();
            let changed = if !reached[t] {
                reached[t] = true;
                init.row_mut(tb).copy_from_slice(&cur_init);
                uninit.row_mut(tb).copy_from_slice(&cur_uninit);
                true
            } else if plant == Plant::MovesIntersect {
                bits::intersect_into(init.row_mut(tb), &cur_init)
                    | bits::intersect_into(uninit.row_mut(tb), &cur_uninit)
            } else {
                bits::union_into(init.row_mut(tb), &cur_init)
                    | bits::union_into(uninit.row_mut(tb), &cur_uninit)
            };
            if changed {
                worklist.push(rank[t] as usize);
            }
        });
    }
    Ok(Inits { init, uninit, reached, subtrees, local_mods })
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

/// Checks one statement `stmt` against its entry `uninit` bitset and appends any findings to `out`.
#[allow(clippy::too_many_arguments)]
fn check_stmt<A: Allocator>(
    flow: &Flow<A>,
    st: &Inits<A>,
    stmt: Stmt,
    uninit: &[u64],
    plant: Plant,
    seen: &mut [u64],
    scratch: &mut [u64],
    out: &mut Vec<MoveFinding, A>,
) {
    let (at, span) = (Point::start(stmt), flow.stmt(stmt).span);
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
                || maybe_uninit_with(&st.subtrees, flow, seen, p)
            {
                found(p, MoveFindingKind::UseOfMoved);
            }
            if let (Operand::Move(_), Some(why)) = (o, move_out(flow, p)) {
                found(p, MoveFindingKind::MoveOut(why));
            }
            if matches!(o, Operand::Move(_)) && plant != Plant::OperandsAtEntry {
                deinit_sub(&st.subtrees, p, scratch, seen);
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

/// The move check's findings from a precomputed [`Inits`], in statement order, then operand order,
/// then the destination.
pub(crate) fn check_moves_with_inits<A: Allocator + Copy>(
    flow: &Flow<A>,
    st: &Inits<A>,
    plant: Plant,
    alloc: A,
) -> Vec<MoveFinding, A> {
    // A statement's operands are read in order, each after the moves of those before it (the
    // Reference, "Evaluation order of operands": left to right; MIR borrowck checks `(move p,
    // move p)` operand by operand, E0382). Checked all against the statement's entry state,
    // `(p, p)` and `h(p, p)` of a non-`Copy` `p` were clean (lane B11's `twice_p`). `seen` is
    // the entry state with the earlier operands' moves applied; `scratch` takes `deinit`'s
    // init half, unused.
    let w = st.uninit.words();
    let mut cur_init: Vec<u64, A> = Vec::with_capacity_in(w, alloc);
    cur_init.resize(w, 0u64);
    let mut cur_uninit: Vec<u64, A> = Vec::with_capacity_in(w, alloc);
    cur_uninit.resize(w, 0u64);
    let mut seen: Vec<u64, A> = Vec::with_capacity_in(w, alloc);
    seen.resize(w, 0u64);
    let mut scratch: Vec<u64, A> = Vec::with_capacity_in(w, alloc);
    scratch.resize(w, 0u64);
    let mut out = Vec::new_in(alloc);
    // Walk blocks in statement order (`flow.stmts` is partitioned into contiguous block ranges).
    let mut s = 0usize;
    while s < flow.stmts.len() {
        let blk = flow.stmt_block[s];
        let b = blk.index();
        let range = flow.blocks[b];
        if st.reached[b] && range.start < range.end {
            cur_init.copy_from_slice(st.init.row(blk));
            cur_uninit.copy_from_slice(st.uninit.row(blk));
            for stmt_idx in range.start..range.end {
                let stmt = Stmt(stmt_idx);
                check_stmt(flow, st, stmt, &cur_uninit, plant, &mut seen, &mut scratch, &mut out);
                if stmt_idx + 1 < range.end {
                    transfer_with(&st.subtrees, flow, stmt, &mut cur_init, &mut cur_uninit);
                }
            }
        }
        s = (range.end as usize).max(s + 1);
    }
    out
}

#[doc(hidden)]
pub fn check_moves_planted<A: Allocator + Copy>(
    flow: &Flow<A>,
    plant: Plant,
    alloc: A,
) -> Result<Vec<MoveFinding, A>, Bound> {
    let st = inits(flow, plant, alloc)?;
    Ok(check_moves_with_inits(flow, &st, plant, alloc))
}

/// What drop elaboration does at statement `s` for `p`, from the same dataflows.
pub fn drop_kind<A: Allocator>(flow: &Flow<A>, st: &Inits<A>, s: Stmt, p: Place) -> DropKind {
    let b = flow.stmt_block[s.index()];
    if !st.reached[b.index()] {
        return DropKind::Nothing;
    }
    let (i, u) = st.place_state_at(flow, b, s, p);
    match (i, u) {
        (false, _) => DropKind::Nothing,
        (true, true) => DropKind::Flag,
        (true, false) => {
            if st.subtrees.descendants(p).iter().any(|&q| st.place_state_at(flow, b, s, Place(q)).1) {
                DropKind::Open
            } else {
                DropKind::Static
            }
        }
    }
}
