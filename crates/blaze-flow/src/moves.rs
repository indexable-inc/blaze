//! The move check (flow.md section 2): maybe-initialized and maybe-uninitialized as forward
//! bitset dataflows over the places, the three findings, and the drop kind drop elaboration reads.
//!
//! The rule (the Reference, "Moved and copied types"): "After moving out of a place expression
//! that evaluates to a local variable, the location is deinitialized and cannot be read from
//! again until it is reinitialized", and "Only the following place expressions may be moved out
//! of: ... Fields of a place expression which can be moved out of and don't implement `Drop`.
//! The result of dereferencing an expression with type `Box<T>` ...".
#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::Allocator;

use crate::bits::{self, Bits};
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

/// The two dataflows' entry states, one row per statement.
pub struct Inits<A: Allocator> {
    pub init: Bits<A>,
    pub uninit: Bits<A>,
    pub reached: Vec<bool, A>,
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

/// The two forward dataflows, join = union, as passes over the statements in order until
/// nothing changes. Bound: each pass but the last adds a bit or reaches a statement, so passes
/// are at most `2 * stmts * places + stmts + 1`.
pub fn inits<A: Allocator + Clone>(flow: &Flow<A>, plant: Plant, alloc: A) -> Result<Inits<A>, Bound> {
    let (n, m) = (flow.stmts.len(), flow.places.len());
    let mut init = Bits::new_in(n, m, alloc.clone());
    let mut uninit = Bits::new_in(n, m, alloc.clone());
    let mut reached = Vec::with_capacity_in(n, alloc.clone());
    reached.resize(n, false);
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
    let w = init.words();
    let mut si = Vec::with_capacity_in(w, alloc.clone());
    si.resize(w, 0u64);
    let mut su = Vec::with_capacity_in(w, alloc);
    su.resize(w, 0u64);
    let limit = (2 * n * m + n + 1) as u64;
    let mut passes = 0u64;
    loop {
        passes += 1;
        if passes > limit {
            return Err(Bound { what: "move dataflow passes", limit });
        }
        let mut changed = false;
        for s in 0..n {
            if !reached[s] {
                continue;
            }
            si.copy_from_slice(init.row(s));
            su.copy_from_slice(uninit.row(s));
            transfer(flow, Stmt(s as u32), &mut si, &mut su);
            flow.for_each_succ(Stmt(s as u32), |t| {
                let t = t.index();
                if !reached[t] {
                    reached[t] = true;
                    init.row_mut(t).copy_from_slice(&si);
                    uninit.row_mut(t).copy_from_slice(&su);
                    changed = true;
                } else if plant == Plant::MovesIntersect {
                    changed |= bits::intersect_into(init.row_mut(t), &si);
                    changed |= bits::intersect_into(uninit.row_mut(t), &su);
                } else {
                    changed |= bits::union_into(init.row_mut(t), &si);
                    changed |= bits::union_into(uninit.row_mut(t), &su);
                }
            });
        }
        if !changed {
            return Ok(Inits { init, uninit, reached });
        }
    }
}

/// `q`, a prefix of `q` or a part of `q` may be uninitialized in `uninit`.
pub fn maybe_uninit<A: Allocator>(flow: &Flow<A>, uninit: &[u64], q: Place) -> bool {
    (0..flow.places.len()).any(|r| {
        let r = Place(r as u32);
        bits::test(uninit, r.index()) && (flow.is_prefix(r, q) || flow.is_prefix(q, r))
    })
}

/// Why `p` may not be moved out of, nearest projection first; `None` if it may.
pub fn move_out<A: Allocator>(flow: &Flow<A>, mut p: Place) -> Option<MoveOut> {
    loop {
        let row = flow.place(p);
        match row.proj {
            Some(Proj::Deref) => return Some(MoveOut::Deref),
            Some(Proj::Index) => return Some(MoveOut::Index),
            Some(Proj::Field(_)) if row.of_drop_type => return Some(MoveOut::DropField),
            _ => {}
        }
        p = row.parent?;
    }
}

/// The move check's findings, in statement order, then operand order, then the destination.
pub fn check_moves<A: Allocator + Clone>(flow: &Flow<A>, alloc: A) -> Result<Vec<MoveFinding, A>, Bound> {
    check_moves_planted(flow, Plant::None, alloc)
}

#[doc(hidden)]
pub fn check_moves_planted<A: Allocator + Clone>(
    flow: &Flow<A>,
    plant: Plant,
    alloc: A,
) -> Result<Vec<MoveFinding, A>, Bound> {
    let st = inits(flow, plant, alloc.clone())?;
    let mut out = Vec::new_in(alloc);
    for s in 0..flow.stmts.len() {
        if !st.reached[s] {
            continue;
        }
        let stmt = Stmt(s as u32);
        let (at, span) = (Point::start(stmt), flow.stmt(stmt).span);
        let uninit = st.uninit.row(s);
        let mut found =
            |place: Place, kind: MoveFindingKind| out.push(MoveFinding { at, place, kind, span });
        let operand =|o: Operand, found: &mut dyn FnMut(Place, MoveFindingKind)| match o {
            Operand::Copy(p) | Operand::Move(p) => {
                if maybe_uninit(flow, uninit, p) {
                    found(p, MoveFindingKind::UseOfMoved);
                }
                if let (Operand::Move(_), Some(why)) = (o, move_out(flow, p)) {
                    found(p, MoveFindingKind::MoveOut(why));
                }
            }
            Operand::Const => {}
        };
        let dest = match flow.stmt(stmt).kind {
            StmtKind::Assign { dest, value } => {
                for_each_operand(flow, value, |o| operand(o, &mut found));
                if let Value::Borrow { place, .. } = value
                    && maybe_uninit(flow, uninit, place)
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
                if maybe_uninit(flow, uninit, on) {
                    found(on, MoveFindingKind::UseOfMoved);
                }
                None
            }
            StmtKind::Return => {
                if maybe_uninit(flow, uninit, Place(0)) {
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
pub fn drop_kind<A: Allocator>(flow: &Flow<A>, st: &Inits<A>, s: Stmt, p: Place) -> DropKind {
    let (init, uninit) = (st.init.row(s.index()), st.uninit.row(s.index()));
    let (i, u) = (bits::test(init, p.index()), bits::test(uninit, p.index()));
    let part_moved = (0..flow.places.len()).any(|q| {
        let q = Place(q as u32);
        q != p && flow.is_prefix(p, q) && bits::test(uninit, q.index())
    });
    match (i, u) {
        (false, _) => DropKind::Nothing,
        (true, true) => DropKind::Flag,
        (true, false) if part_moved => DropKind::Open,
        (true, false) => DropKind::Static,
    }
}
