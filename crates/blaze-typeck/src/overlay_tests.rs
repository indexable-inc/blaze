//! A body's table is an overlay over the shared table (portable.rs, `PortableTys::over`): it
//! must answer every read exactly as the copy it replaced (`fork_in`, the oracle), keep a
//! body-only type out of the shared table, stay what it was when the shared table grows later
//! (`OwnTys::view`), and cost nothing proportional to the shared table to make.
//!
//! The promise checked (portable.rs, `PortableTys`): "an id below that length means the shared
//! table's type, read in place ... ids the shared table gains later are never visible to that
//! body (its own ids occupy that range)".

use blaze_own::{Alloc, Erased, Vec, testing::Counting};
use blaze_sim::{Broke, Config, Tape};
use std::alloc::Global;
use std::format;

use crate::error::{CheckError, Malformed};
use crate::hash::{ContentHash, StableHasher};
use crate::ids::{IntTy, ItemKey, List, Mutbl};
use crate::portable::{OwnTys, PNode, PTy, PTys, PortableTys};

/// One intern: a node over earlier ids, or a list of earlier ids.
#[derive(Clone, Copy, Debug)]
enum Op {
    Node(PNode),
    List([PTy; 3], usize),
}

/// A node over ids below `len`, or a leaf. `Infer` is a body's own (the base never draws it).
fn draw_node(tape: &Tape, len: u32, lists: &[List], body: bool) -> PNode {
    let id = |tape: &Tape| PTy(tape.below(u64::from(len.max(1)), "child") as u32);
    let list = |tape: &Tape| match lists.len() {
        0 => List::EMPTY,
        n => lists[tape.index(n, "list")],
    };
    let kinds = if body { 9 } else { 8 };
    match tape.below(if len == 0 { 3 } else { kinds }, "node kind") {
        0 => PNode::Bool,
        1 => PNode::Int([IntTy::I8, IntTy::I32, IntTy::Isize][tape.index(3, "int")]),
        2 => PNode::Param(tape.below(4, "param") as u32),
        3 => PNode::Ref(if tape.coin("mut") { Mutbl::Mut } else { Mutbl::Not }, id(tape)),
        4 => PNode::Slice(id(tape)),
        5 => PNode::Tuple(list(tape)),
        6 => PNode::Adt(ItemKey(u128::from(tape.below(5, "adt"))), list(tape)),
        7 => PNode::Array(id(tape), id(tape)),
        _ => PNode::Infer,
    }
}

/// Interns `n` random nodes and lists into `t`, returning what was interned in order.
fn grow(tape: &Tape, t: &mut PortableTys<'_>, n: usize, body: bool, ops: &mut Vec<Op, Global>) -> Result<(), CheckError> {
    let mut lists: Vec<List, Global> = Vec::new_in(Global);
    for _ in 0..n {
        let len = t.len() as u32;
        if len > 0 && tape.one_in(3, "a list") {
            let k = tape.range(1..4, "list len") as usize;
            let mut kids = [PTy(0); 3];
            for kid in kids.iter_mut().take(k) {
                *kid = PTy(tape.below(u64::from(len), "list child") as u32);
            }
            lists.push(t.intern_list(&kids[..k])?);
            ops.push(Op::List(kids, k));
        } else {
            let node = draw_node(tape, len, &lists, body);
            t.intern(node)?;
            ops.push(Op::Node(node));
        }
    }
    Ok(())
}

fn digest(v: PTys<'_>) -> u64 {
    let mut h = StableHasher::new("overlay test");
    v.hash_into(&mut h);
    h.finish()
}

/// Every read of `got` equals `want`'s: lengths, each node, each list entry, the digest.
fn same(got: PTys<'_>, want: PTys<'_>, what: &str) -> Result<(), Broke> {
    if got.len() != want.len() || got.lists_len() != want.lists_len() {
        return Err(Broke::Oracle(format!("{what} differs: {} nodes, {} list entries; the copy has {}, {}", got.len(), got.lists_len(), want.len(), want.lists_len())));
    }
    for i in 0..want.len() as u32 {
        if got.get(PTy(i)) != want.get(PTy(i)) {
            return Err(Broke::Oracle(format!("{what} differs at node {i}: {:?}, the copy {:?}", got.get(PTy(i)), want.get(PTy(i)))));
        }
    }
    for i in 0..want.lists_len() {
        if got.list_slot(i) != want.list_slot(i) {
            return Err(Broke::Oracle(format!("{what} differs at list entry {i}")));
        }
    }
    if digest(got) != digest(want) {
        return Err(Broke::Oracle(format!("{what}'s digest differs from the copy's")));
    }
    Ok(())
}

/// How the kept part is read again: right, or the planted bug (the base at its current length).
#[derive(Clone, Copy, Debug)]
enum Read {
    Truncated,
    Untruncated,
}

fn kept_view<'s>(own: &'s OwnTys<'_>, base: &'s PortableTys<'_>, read: Read) -> Result<PTys<'s>, CheckError> {
    match read {
        Read::Truncated => own.view(base),
        Read::Untruncated => own.view_untruncated(base),
    }
}

fn overlay_property(tape: &Tape, read: Read) -> Result<(), Broke> {
    let harness = |e: CheckError| Broke::Harness(format!("{e:?}"));
    let mut base = PortableTys::new_in(Alloc::global());
    let mut scratch: Vec<Op, Global> = Vec::new_in(Global);
    grow(tape, &mut base, tape.range(0..40, "base size") as usize, false, &mut scratch).map_err(harness)?;
    let base_len = base.len();
    let mut fork = base.fork_in(Alloc::global()).map_err(harness)?;
    let (kept, body_ids) = {
        let mut over = PortableTys::over(&base, Alloc::global()).map_err(harness)?;
        // A body's own type first, so every case has one.
        let mut ops: Vec<Op, Global> = Vec::new_in(Global);
        let first = over.intern(PNode::Infer).map_err(harness)?;
        ops.push(Op::Node(PNode::Infer));
        grow(tape, &mut over, tape.range(0..30, "body size") as usize, true, &mut ops).map_err(harness)?;
        // The same interns into the copy: each answer equal.
        let mut replay = PortableTys::over(&base, Alloc::global()).map_err(harness)?;
        for op in ops.iter() {
            let (a, b) = match *op {
                Op::Node(n) => (replay.intern(n).map(|t| t.0), fork.intern(n).map(|t| t.0)),
                Op::List(kids, k) => (replay.intern_list(&kids[..k]).map(|l| l.start), fork.intern_list(&kids[..k]).map(|l| l.start)),
            };
            if a.map_err(harness)? != b.map_err(harness)? {
                return Err(Broke::Oracle(format!("an intern answered differently over the overlay: {op:?}")));
            }
        }
        same(over.view(), fork.view(), "the overlay")?;
        if first.0 as usize != base_len {
            return Err(Broke::Oracle(format!("the body's first own type is {} where the base has {base_len} nodes", first.0)));
        }
        // A body-only id is not the record table's: refused there.
        if !matches!(base.view().node(first), Err(CheckError::Malformed(Malformed::TypeOrder { .. }))) {
            return Err(Broke::Oracle("a body's own type was read from the records' table".into()));
        }
        // No overlay of an overlay.
        if !matches!(PortableTys::over(&over, Alloc::global()), Err(CheckError::Malformed(Malformed::Overlay { .. }))) {
            return Err(Broke::Oracle("an overlay over an overlay was made".into()));
        }
        (over.keep_in(Alloc::global()).map_err(harness)?, over.len() - base_len)
    };
    if base.len() != base_len {
        return Err(Broke::Oracle(format!("the records' table grew from {base_len} to {} under a body", base.len())));
    }
    if kept.own_len() != body_ids {
        return Err(Broke::Oracle(format!("kept {} own nodes of {body_ids}", kept.own_len())));
    }
    // The shared table gains types after the body began (one at least): the kept body still
    // reads what it read.
    base.intern(PNode::Param(1_000)).map_err(harness)?;
    grow(tape, &mut base, tape.range(0..20, "base growth") as usize, true, &mut scratch).map_err(harness)?;
    let again = kept_view(&kept, &base, read).map_err(harness)?;
    same(again, fork.view(), "the kept body read over a grown base")
}

/// The refinement: a body type table overlay answers every read identically to a full copy of the base table and never reads IDs added to the base after the overlay was created.
#[test]
fn an_overlay_reads_as_the_copy() {
    blaze_sim::check(Config::cases(400), |tape: &Tape| overlay_property(tape, Read::Truncated));
}

/// The planted bug: the kept part read over the base at its current length.
#[test]
fn the_oracle_catches_a_later_base_id() {
    blaze_sim::control(Config::cases(400), Read::Truncated, Read::Untruncated, "over a grown base differs", overlay_property);
}

/// A kept body read over a base shorter than the one it was made over is refused by name.
#[test]
fn a_kept_body_over_a_shorter_base_is_refused() {
    let mut base = PortableTys::new_in(Alloc::global());
    base.intern(PNode::Bool).unwrap();
    let kept = {
        let mut over = PortableTys::over(&base, Alloc::global()).unwrap();
        over.intern(PNode::Infer).unwrap();
        over.keep_in(Alloc::global()).unwrap()
    };
    let shorter = PortableTys::new_in(Alloc::global());
    let got = kept.view(&shorter).map(|v| v.len());
    assert!(matches!(got, Err(CheckError::Malformed(Malformed::Overlay { base: 1, have: 0 }))), "{got:?}");
}

/// Making an overlay allocates nothing, and its own types cost the same over a base of ten
/// nodes as over one of twenty thousand: nothing of the base is copied or hashed again.
#[test]
fn an_overlay_costs_nothing_of_its_base() {
    let own_cost = |base_size: u32| -> (usize, usize, usize) {
        let mut base = PortableTys::new_in(Alloc::global());
        for i in 0..base_size {
            base.intern(PNode::Param(i)).unwrap();
        }
        let counting = Erased::new(Counting::new(std::alloc::System));
        let made;
        {
            let mut over = PortableTys::over(&base, counting.handle()).unwrap();
            made = counting.inner().allocations();
            // In the base: found there, nothing added.
            assert_eq!(over.intern(PNode::Param(3)).unwrap(), PTy(3));
            let infer = over.intern(PNode::Infer).unwrap();
            assert_eq!(infer, PTy(base_size));
            over.intern(PNode::Slice(infer)).unwrap();
            over.intern_list(&[infer, PTy(1)]).unwrap();
            let bytes = counting.inner().live_bytes();
            drop(over);
            (made, bytes, counting.inner().live())
        }
    };
    let (made_small, bytes_small, live_small) = own_cost(10);
    let (made_big, bytes_big, live_big) = own_cost(20_000);
    std::println!("overlay: made with {made_big} allocations; own types {bytes_small} bytes over 10 base nodes, {bytes_big} over 20,000");
    assert_eq!((made_small, made_big), (0, 0), "making an overlay allocated");
    assert_eq!(bytes_small, bytes_big, "an overlay's own types cost more over a bigger base");
    assert_eq!((live_small, live_big), (0, 0), "an overlay left blocks live");
}
