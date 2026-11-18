//! The simulations (an oracle and a planted bug each) and the fixtures from the written sources
//! (flow.md: the Reference, RFC 2094, RFC 2025, the Polonius book).

use std::alloc::Global;
use std::collections::BTreeSet;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_sim::{Broke, Config, Tape, check, control};

use crate::flow::{BorrowKind, Flow, FlowBuilder, Local, Operand, Origin, Place, Point, Proj, Span, Stmt, StmtKind};
use crate::loans::{BorrowFinding, TierOne, facts, tier_one, tier_two};
use crate::moves::{DropKind, MoveFindingKind, MoveOut, check_moves, check_moves_planted, drop_kind, inits, move_out};
use crate::record::{Plant, check as check_body};

const S: Span = Span { lo: 0, hi: 0 };

// ---------------------------------------------------------------- random flows from a tape

/// A random small flow: up to 5 locals, 4 projections, 4 origins, 5 blocks of up to 4 rows.
fn random_flow(t: &Tape) -> Flow<Global> {
    let locals = 2 + t.below(4, "locals") as u32;
    let args = t.below(u64::from(locals), "args") as u32;
    let mut b = FlowBuilder::new_in(locals, args, Global);
    let mut places: Vec<Place> = (0..locals).map(Place).collect();
    for _ in 0..t.below(5, "projections") {
        let parent = places[t.index(places.len(), "parent")];
        let proj = match t.below(6, "proj") {
            0 => Proj::Field(0),
            1 => Proj::Field(1),
            2 => Proj::Deref,
            3 => Proj::DerefBox,
            4 => Proj::Index,
            _ => Proj::Downcast(0),
        };
        let marked = matches!(proj, Proj::Field(_)) && t.one_in(4, "drop type");
        let p = b.project_marked(parent, proj, marked);
        if !places.contains(&p) {
            places.push(p);
        }
    }
    let origins: Vec<_> =
        (0..1 + t.below(4, "origins")).map(|_| b.origin(t.one_in(3, "placeholder"))).collect();
    for l in 0..locals {
        for &o in &origins {
            if t.coin("mentions") {
                b.mentions(l, o);
            }
        }
    }
    if t.one_in(3, "known") {
        let (x, y) = (origins[t.index(origins.len(), "kf")], origins[t.index(origins.len(), "ki")]);
        b.known(x, y);
    }
    let blocks: Vec<_> = (0..1 + t.below(5, "blocks")).map(|_| b.block()).collect();
    let mut loans = Vec::new();
    let place = |t: &Tape, what| places[t.index(places.len(), what)];
    let operand = |t: &Tape| match t.below(3, "operand") {
        0 => Operand::Copy(place(t, "copy")),
        1 => Operand::Move(place(t, "move")),
        _ => Operand::Const,
    };
    for &bb in &blocks {
        b.start(bb);
        for _ in 0..t.below(5, "rows") {
            match t.below(8, "stmt") {
                0 | 1 => {
                    let (dest, op) = (place(t, "dest"), operand(t));
                    b.assign(dest, op, S);
                }
                2 | 3 => {
                    let kind = *t
                        .pick(
                            &[BorrowKind::Shared, BorrowKind::Mut, BorrowKind::TwoPhase, BorrowKind::Shallow],
                            "kind",
                        )
                        .unwrap_or(&BorrowKind::Shared);
                    let (dest, of) = (place(t, "bdest"), place(t, "borrowed"));
                    let o = origins[t.index(origins.len(), "origin")];
                    loans.push(b.borrow(dest, kind, of, o, S));
                }
                4 => {
                    // A call relating an argument's origin to its destination's.
                    let (x, y) = (origins[t.index(origins.len(), "from")], origins[t.index(origins.len(), "into")]);
                    let (arg, dest) = (operand(t), place(t, "rdest"));
                    b.call(&[arg], dest, S);
                    b.subset(x, y);
                }
                5 => {
                    let l = crate::flow::Local(t.below(u64::from(locals), "storage") as u32);
                    let kind = if t.coin("dead") { StmtKind::StorageDead(l) } else { StmtKind::StorageLive(l) };
                    b.stmt(kind, S);
                }
                6 => {
                    let p = place(t, "drop");
                    b.stmt(StmtKind::Drop(p), S);
                }
                _ => {
                    if let Some(&l) = t.pick(&loans, "activate") {
                        b.stmt(StmtKind::Activate(l), S);
                    } else {
                        let (x, y, dest) = (operand(t), operand(t), place(t, "cdest"));
                        b.call(&[x, y], dest, S);
                    }
                }
            }
            // An assignment's or reborrow's own constraint, on any row.
            if t.coin("own subset") {
                let (x, y) = (origins[t.index(origins.len(), "sf")], origins[t.index(origins.len(), "si")]);
                b.subset(x, y);
            }
        }
        match t.below(3, "terminator") {
            0 => {
                let to = blocks[t.index(blocks.len(), "goto")];
                b.goto(to, S);
            }
            1 => {
                let (x, y) = (blocks[t.index(blocks.len(), "t0")], blocks[t.index(blocks.len(), "t1")]);
                let on = place(t, "on");
                b.switch(on, &[x, y], S);
            }
            _ => {
                b.ret(S);
            }
        }
    }
    match b.finish() {
        Ok(f) => f,
        Err(e) => panic!("the generator made a malformed flow: {e:?}"),
    }
}

// ---------------------------------------------------------------- the move oracle

type Found = BTreeSet<(Point, Place, MoveFindingKind)>;

/// The plainest move check: walk every path from the entry with each block entered at most three
/// times (enough for a gen/kill fact: a path to the last move, then a cycle-free path on to the
/// use, may share each block twice and the use's block once more), keep one concrete
/// uninitialized flag per place, and record a finding wherever the rule holds on that path.
fn oracle_moves(flow: &Flow<Global>) -> Result<Found, Broke> {
    let m = flow.places.len();
    let mut found = Found::new();
    let entry: Vec<bool> = (0..m)
        .map(|q| {
            let l = flow.places[q].local.0;
            !(l >= 1 && l <= flow.args)
        })
        .collect();
    let mut visits = std::vec![0u8; flow.blocks.len()];
    visits[0] = 1;
    let mut stack = std::vec![(flow.entry(), entry, visits)];
    let mut steps = 0u32;
    while let Some((s, mut un, visits)) = stack.pop() {
        steps += 1;
        if steps > 3_000_000 {
            return Err(Broke::Harness(String::from("the path oracle passed 3e6 steps")));
        }
        let at = Point::start(s);
        let maybe = |un: &[bool], q: Place| {
            (0..m).any(|r| un[r] && (flow.is_prefix(Place(r as u32), q) || flow.is_prefix(q, Place(r as u32))))
        };
        let mut operand = |un: &[bool], o: Operand| {
            if let Operand::Copy(p) | Operand::Move(p) = o {
                if maybe(un, p) {
                    found.insert((at, p, MoveFindingKind::UseOfMoved));
                }
                if let (Operand::Move(_), Some(why)) = (o, move_out(flow, p)) {
                    found.insert((at, p, MoveFindingKind::MoveOut(why)));
                }
            }
        };
        let set = |un: &mut Vec<bool>, p: Place, to: bool| {
            for q in 0..m {
                if flow.is_prefix(p, Place(q as u32)) {
                    un[q] = to;
                }
            }
        };
        let mut dest = None;
        let mut moved = Vec::new();
        // Operands are read left to right, each after the moves of those before it.
        let mut seen = un.clone();
        match flow.stmt(s).kind {
            StmtKind::Assign { dest: d, value } => {
                crate::moves::for_each_operand(flow, value, |o| {
                    operand(&seen, o);
                    if let Operand::Move(p) = o {
                        set(&mut seen, p, true);
                    }
                    moved.push(o);
                });
                if let crate::flow::Value::Borrow { place, .. } = value
                    && maybe(&un, place)
                {
                    found.insert((at, place, MoveFindingKind::UseOfMoved));
                }
                dest = Some(d);
            }
            StmtKind::Call { args, dest: d } => {
                for o in flow.ops(args) {
                    operand(&seen, *o);
                    if let Operand::Move(p) = *o {
                        set(&mut seen, p, true);
                    }
                    moved.push(*o);
                }
                dest = Some(d);
            }
            StmtKind::Switch { on, .. } if maybe(&un, on) => {
                found.insert((at, on, MoveFindingKind::UseOfMoved));
            }
            StmtKind::Return if maybe(&un, Place(0)) => {
                found.insert((at, Place(0), MoveFindingKind::UseOfMoved));
            }
            StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => set(&mut un, Place(l.0), true),
            StmtKind::Drop(p) => set(&mut un, p, true),
            _ => {}
        }
        if let Some(d) = dest {
            let (mut cur, mut through) = (d, false);
            while let Some(parent) = flow.place(cur).parent {
                through |= matches!(flow.place(cur).proj, Some(Proj::Deref | Proj::DerefBox));
                cur = parent;
                if un[cur.index()] {
                    let kind = if through { MoveFindingKind::UseOfMoved } else { MoveFindingKind::AssignToMovedPart };
                    found.insert((at, if through { cur } else { d }, kind));
                }
            }
            for o in moved {
                if let Operand::Move(p) = o {
                    set(&mut un, p, true);
                }
            }
            set(&mut un, d, false);
        }
        let terminator = flow.stmt(s).kind.is_terminator();
        flow.for_each_succ(s, |t| {
            let mut v = visits.clone();
            if terminator {
                let b = flow.stmt_block[t.index()].index();
                if v[b] >= 3 {
                    return;
                }
                v[b] += 1;
            }
            stack.push((t, un.clone(), v));
        });
    }
    Ok(found)
}

fn moves_case(t: &Tape, plant: Plant) -> Result<(), Broke> {
    let flow = random_flow(t);
    let got: Found = match check_moves_planted(&flow, plant, Global) {
        Ok(v) => v.iter().map(|f| (f.at, f.place, f.kind)).collect(),
        Err(b) => return Err(Broke::Harness(format!("{b:?}"))),
    };
    let want = oracle_moves(&flow)?;
    if got != want {
        let missed: Vec<_> = want.difference(&got).collect();
        let extra: Vec<_> = got.difference(&want).collect();
        let mut text = String::new();
        let _ = flow.print(&mut text);
        return Err(Broke::Oracle(format!("move check missed {missed:?}, extra {extra:?} in\n{text}")));
    }
    Ok(())
}

/// The move check equals the path oracle on random flows, loops included (the Reference: "After
/// moving out of a place expression that evaluates to a local variable, the location is
/// deinitialized and cannot be read from again until it is reinitialized").
#[test]
fn move_check_equals_path_oracle() {
    check(Config::cases(400), |t| moves_case(t, Plant::None));
}

/// Planted: the join intersects where it must unite; the oracle sees findings missed.
#[test]
fn move_check_control_intersecting_join() {
    control(Config::cases(400), Plant::None, Plant::MovesIntersect, "move check missed [(", moves_case);
}

// ---------------------------------------------------------------- the tiers' obligation

fn tiers_case(t: &Tape, plant: Plant) -> Result<(), Broke> {
    let flow = random_flow(t);
    let harness = |b| Broke::Harness(format!("{b:?}"));
    let facts = facts(&flow, Global).map_err(harness)?;
    let one = tier_one(&flow, &facts, plant, Global).map_err(harness)?;
    let two = tier_two(&flow, &facts, plant, Global).map_err(harness)?;
    if one == TierOne::Accepted && !two.is_empty() {
        let mut text = String::new();
        let _ = flow.print(&mut text);
        return Err(Broke::Oracle(format!("tier one accepted what tier two refuses: {two:?} in\n{text}")));
    }
    Ok(())
}

/// flow.md 5: every fact tier two derives is one tier one derives (the Polonius rules without
/// the point and `loan_killed_at`), so tier one accepting a body means tier two accepts it.
#[test]
fn tier_one_accepts_only_what_tier_two_accepts() {
    check(Config::cases(400), |t| tiers_case(t, Plant::None));
}

/// Planted in tier two: the second contains rule run backwards (loans flow against a subset).
#[test]
fn tiers_control_tier_two_reversed() {
    control(Config::cases(5000), Plant::None, Plant::TierTwoReversed, "tier one accepted", tiers_case);
}

/// Planted in tier one: contained loans not closed over the subset graph.
#[test]
fn tiers_control_tier_one_unclosed() {
    control(Config::cases(5000), Plant::None, Plant::TierOneUnclosed, "tier one accepted", tiers_case);
}

// ---------------------------------------------------------------- fixtures: the Reference's moves

fn finish(b: FlowBuilder<Global>) -> Flow<Global> {
    match b.finish() {
        Ok(f) => f,
        Err(e) => panic!("{e:?}"),
    }
}

fn kinds(flow: &Flow<Global>) -> Vec<(Stmt, Place, MoveFindingKind)> {
    match check_moves(flow, Global) {
        Ok(v) => v.iter().map(|f| (f.at.stmt(), f.place, f.kind)).collect(),
        Err(b) => panic!("{b:?}"),
    }
}

/// "After moving out of a place expression that evaluates to a local variable, the location is
/// deinitialized and cannot be read from again until it is reinitialized." `let a = s; let b = s;`
/// is refused at the second move; with `s = new` between, accepted.
#[test]
fn reference_moved_local_until_reinitialized() {
    let mut b = FlowBuilder::new_in(4, 1, Global);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Const, S);
    b.assign(Place(2), Operand::Move(Place(1)), S);
    b.assign(Place(3), Operand::Move(Place(1)), S);
    b.ret(S);
    let flow = finish(b);
    assert_eq!(kinds(&flow), [(Stmt(2), Place(1), MoveFindingKind::UseOfMoved)]);
    let mut text = String::new();
    let _ = flow.print(&mut text);
    assert_eq!(text, "bb0:\n  s0 _0 = const\n  s1 _2 = move _1\n  s2 _3 = move _1\n  s3 return\n");

    let mut b = FlowBuilder::new_in(4, 1, Global);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Const, S);
    b.assign(Place(2), Operand::Move(Place(1)), S);
    b.assign(Place(1), Operand::Const, S);
    b.assign(Place(3), Operand::Move(Place(1)), S);
    b.ret(S);
    assert_eq!(kinds(&finish(b)), []);
}

/// "Fields of a place expression which can be moved out of and don't implement `Drop`": moving
/// `x.0` leaves `x.1` usable and `x` partly moved; a field of a `Drop` type is refused; "The result
/// of dereferencing an expression with type `Box<T>`" may be moved out of, a reference's may not;
/// assigning into a moved-from local's field is refused.
#[test]
fn reference_fields_boxes_and_borrows() {
    let mut b = FlowBuilder::new_in(6, 3, Global);
    let (x0, x1) = (b.project(Place(1), Proj::Field(0)), b.project(Place(1), Proj::Field(1)));
    let r = b.project(Place(2), Proj::Deref);
    let boxed = b.project(Place(3), Proj::DerefBox);
    let df = b.project_marked(Place(3), Proj::Field(0), true);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Const, S);
    b.assign(Place(4), Operand::Move(x0), S); // s1: fine
    b.assign(Place(5), Operand::Copy(x1), S); // s2: fine
    b.assign(Place(5), Operand::Move(Place(1)), S); // s3: x partly moved
    b.assign(Place(5), Operand::Move(r), S); // s4: out of a borrow
    b.assign(Place(5), Operand::Move(boxed), S); // s5: out of a box: fine
    b.assign(Place(5), Operand::Move(df), S); // s6: a Drop type's field (disjoint from the box)
    b.assign(x0, Operand::Const, S); // s7: x is moved: assign to a moved part
    b.ret(S);
    let flow = finish(b);
    assert_eq!(
        kinds(&flow),
        [
            (Stmt(3), Place(1), MoveFindingKind::UseOfMoved),
            (Stmt(4), r, MoveFindingKind::MoveOut(MoveOut::Deref)),
            (Stmt(6), df, MoveFindingKind::MoveOut(MoveOut::DropField)),
            (Stmt(7), x0, MoveFindingKind::AssignToMovedPart),
        ]
    );
}

/// Destructors: "If a variable has been partially initialized, only its initialized fields are
/// dropped." Drop elaboration reads the same dataflows: moved on one arm is a flag, a part moved
/// is an open drop.
#[test]
fn drop_kinds_from_the_same_result() {
    let mut b = FlowBuilder::new_in(4, 3, Global);
    let x0 = b.project(Place(2), Proj::Field(0));
    let (b0, b1, b2) = (b.block(), b.block(), b.block());
    b.start(b0);
    b.assign(Place(0), Operand::Const, S);
    b.assign(Place(3), Operand::Move(x0), S);
    b.switch(Place(3), &[b1, b2], S);
    b.start(b1);
    b.assign(Place(3), Operand::Move(Place(1)), S);
    b.goto(b2, S);
    b.start(b2);
    let d = b.stmt(StmtKind::Drop(Place(1)), S);
    let e = b.stmt(StmtKind::Drop(Place(2)), S);
    b.ret(S);
    let flow = finish(b);
    let st = match inits(&flow, Plant::None, Global) {
        Ok(st) => st,
        Err(e) => panic!("{e:?}"),
    };
    assert_eq!(drop_kind(&flow, &st, d, Place(1)), DropKind::Flag);
    assert_eq!(drop_kind(&flow, &st, e, Place(2)), DropKind::Open);
}

// ---------------------------------------------------------------- fixtures: RFC 2094's problem cases

fn tiers(flow: &Flow<Global>) -> (TierOne, Vec<BorrowFinding>) {
    let f = match facts(flow, Global) {
        Ok(f) => f,
        Err(e) => panic!("{e:?}"),
    };
    let one = tier_one(flow, &f, Plant::None, Global);
    let two = tier_two(flow, &f, Plant::None, Global);
    match (one, two) {
        (Ok(one), Ok(two)) => (one, two.iter().copied().collect()),
        (a, b) => panic!("{a:?} {b:?}"),
    }
}

/// RFC 2094, "Problem case #1: references assigned into a variable": `let slice = &mut
/// data[..]; capitalize(slice); data.push('d');`. The slice is dead after `capitalize`, so the
/// two-phase borrow for `push` (RFC 2025) conflicts with no live loan: tier one accepts.
#[test]
fn rfc2094_problem_case_1() {
    // _1 data (arg), _2 slice, _3 the receiver temporary, _4 unit.
    let mut b = FlowBuilder::new_in(5, 1, Global);
    let (os, o2) = (b.origin(false), b.origin(false));
    b.mentions(2, os);
    b.mentions(3, o2);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Const, S);
    b.borrow(Place(2), BorrowKind::Mut, Place(1), os, S);
    b.call(&[Operand::Move(Place(2))], Place(4), S);
    let push = b.borrow(Place(3), BorrowKind::TwoPhase, Place(1), o2, S);
    b.stmt(StmtKind::Activate(push), S);
    b.call(&[Operand::Move(Place(3)), Operand::Const], Place(4), S);
    b.ret(S);
    assert_eq!(tiers(&finish(b)), (TierOne::Accepted, Vec::new()));
}

/// RFC 2094, "Problem case #2: conditional control flow": `match map.get_mut(&key) { Some(v) =>
/// process(v), None => { map.insert(key, v); } }`. The result is dead on the `None` arm: tier one
/// accepts.
#[test]
fn rfc2094_problem_case_2() {
    // _1 map (arg), _2 receiver temp, _3 result, _4 insert's receiver, _5 unit.
    let mut b = FlowBuilder::new_in(6, 1, Global);
    let (o1, or, o2) = (b.origin(false), b.origin(false), b.origin(false));
    b.mentions(2, o1);
    b.mentions(3, or);
    b.mentions(4, o2);
    let (b0, some, none, end) = (b.block(), b.block(), b.block(), b.block());
    b.start(b0);
    b.assign(Place(0), Operand::Const, S);
    b.borrow(Place(2), BorrowKind::Mut, Place(1), o1, S);
    b.call(&[Operand::Move(Place(2))], Place(3), S);
    b.subset(o1, or);
    b.switch(Place(3), &[some, none], S);
    b.start(some);
    b.call(&[Operand::Move(Place(3))], Place(5), S);
    b.goto(end, S);
    b.start(none);
    let ins = b.borrow(Place(4), BorrowKind::TwoPhase, Place(1), o2, S);
    b.stmt(StmtKind::Activate(ins), S);
    b.call(&[Operand::Move(Place(4)), Operand::Const], Place(5), S);
    b.goto(end, S);
    b.start(end);
    b.ret(S);
    assert_eq!(tiers(&finish(b)), (TierOne::Accepted, Vec::new()));
}

/// RFC 2094, "Problem case #3: conditional control flow across functions": `get_default` returns
/// `map.get_mut(&key)`'s result on the `Some` arm and inserts on the `None` arm. The loan flows
/// into the placeholder `'m` only on the `Some` arm: tier one (no points) cannot accept; tier two
/// (Polonius, location-sensitive) accepts.
#[test]
fn rfc2094_problem_case_3() {
    // _0 the returned &'m mut V, _1 map: &'m mut HashMap (arg), _2 temp, _3 result,
    // _4 insert's receiver, _5 unit, _6 the second lookup's receiver.
    let mut b = FlowBuilder::new_in(7, 1, Global);
    let om = b.origin(true);
    let (o1, or, o2, o3) = (b.origin(false), b.origin(false), b.origin(false), b.origin(false));
    b.mentions(0, om);
    b.mentions(1, om);
    b.mentions(2, o1);
    b.mentions(3, or);
    b.mentions(4, o2);
    b.mentions(6, o3);
    let star = b.project(Place(1), Proj::Deref);
    let (b0, some, none, end) = (b.block(), b.block(), b.block(), b.block());
    b.start(b0);
    b.borrow(Place(2), BorrowKind::Mut, star, o1, S);
    b.subset(om, o1);
    b.call(&[Operand::Move(Place(2))], Place(3), S);
    b.subset(o1, or);
    b.switch(Place(3), &[some, none], S);
    b.start(some);
    b.assign(Place(0), Operand::Move(Place(3)), S);
    b.subset(or, om);
    b.goto(end, S);
    b.start(none);
    let ins = b.borrow(Place(4), BorrowKind::TwoPhase, star, o2, S);
    b.stmt(StmtKind::Activate(ins), S);
    b.call(&[Operand::Move(Place(4)), Operand::Const], Place(5), S);
    b.borrow(Place(6), BorrowKind::Mut, star, o3, S);
    b.call(&[Operand::Move(Place(6))], Place(0), S);
    b.subset(o3, om);
    b.goto(end, S);
    b.start(end);
    b.ret(S);
    assert_eq!(tiers(&finish(b)), (TierOne::Refused, Vec::new()));
}

/// RFC 2094, "Problem case #4: mutating &mut references": "The current borrow checker forbids
/// reassigning an `&mut` variable `x` when the referent (`*x`) has been borrowed." In `to_refs`,
/// `list = n` kills the loans of `(*list).value` and `(*list).next` (Polonius `loan_killed_at`:
/// "a prefix of the path borrowed in loan is assigned"). Tier one ignores kills and cannot
/// accept; tier two accepts.
#[test]
fn rfc2094_problem_case_4() {
    // _0 result: Vec<&'a mut T>, _1 list (arg), _2 &mut value temp, _3 &mut next temp,
    // _4 n: Option<&mut List>, _5 unit.
    let mut b = FlowBuilder::new_in(6, 1, Global);
    let oa = b.origin(true);
    let (ol, o1, o2, on) = (b.origin(false), b.origin(false), b.origin(false), b.origin(false));
    b.mentions(0, oa);
    b.mentions(1, ol);
    b.mentions(2, o1);
    b.mentions(3, o2);
    b.mentions(4, on);
    let star = b.project(Place(1), Proj::Deref);
    let (value, next) = (b.project(star, Proj::Field(0)), b.project(star, Proj::Field(1)));
    let (b0, head, step, out) = (b.block(), b.block(), b.block(), b.block());
    b.start(b0);
    b.assign(Place(0), Operand::Const, S);
    b.goto(head, S);
    b.start(head);
    b.borrow(Place(2), BorrowKind::Mut, value, o1, S);
    b.subset(ol, o1);
    b.call(&[Operand::Move(Place(2))], Place(5), S);
    b.subset(o1, oa);
    b.borrow(Place(3), BorrowKind::Mut, next, o2, S);
    b.subset(ol, o2);
    b.call(&[Operand::Move(Place(3))], Place(4), S);
    b.subset(o2, on);
    b.switch(Place(4), &[step, out], S);
    b.start(step);
    b.assign(Place(1), Operand::Move(Place(4)), S);
    b.subset(on, ol);
    b.goto(head, S);
    b.start(out);
    b.ret(S);
    assert_eq!(tiers(&finish(b)), (TierOne::Refused, Vec::new()));
}

/// RFC 2094's error condition (Polonius: "It is an error to invalidate a loan that is live at a
/// given point"): `let r = &mut x; let s = &x; use(r);` is refused by both tiers, the finding
/// naming the mutable loan at the shared borrow.
#[test]
fn live_mutable_loan_then_shared_borrow_is_refused() {
    let mut b = FlowBuilder::new_in(5, 1, Global);
    let (or, os) = (b.origin(false), b.origin(false));
    b.mentions(2, or);
    b.mentions(3, os);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Const, S);
    let l = b.borrow(Place(2), BorrowKind::Mut, Place(1), or, S);
    b.borrow(Place(3), BorrowKind::Shared, Place(1), os, S);
    b.call(&[Operand::Move(Place(2))], Place(4), S);
    b.ret(S);
    let flow = finish(b);
    assert_eq!(
        tiers(&flow),
        (TierOne::Refused, std::vec![BorrowFinding::Invalidated { at: Point::start(Stmt(2)), loan: l, span: S, returned: false }])
    );
    match check_body(&flow, Global) {
        Ok(r) => assert!(r.proof.is_none() && r.borrows.len() == 1),
        Err(e) => panic!("{e:?}"),
    }
}

// ---------------------------------------------------------------- fixtures: the signature's origins
//
// A signature's lifetimes are placeholders (flow.md section 3), live at every point, and the
// relations the signature states among them (`'b: 'a` written, or implied by a well-formed
// argument type) are the `known` table. The body is right when every `subset` it derives between
// two placeholders is in that table's closure (Polonius `subset_errors`), and when no loan of a
// local reaches a placeholder (the loan is live at the local's `StorageDead`). The source of the
// rule is the design (RFC 2094, "Named lifetimes" and Polonius' `known_placeholder_subset`): the
// Reference's "Lifetime elision" and "Lifetime bounds" say only what the signature means.

fn tiers_with(flow: &Flow<Global>, plant: Plant) -> (TierOne, Vec<BorrowFinding>) {
    let f = match facts(flow, Global) {
        Ok(f) => f,
        Err(e) => panic!("{e:?}"),
    };
    let one = tier_one(flow, &f, plant, Global);
    let two = tier_two(flow, &f, plant, Global);
    match (one, two) {
        (Ok(one), Ok(two)) => (one, two.iter().copied().collect()),
        (a, b) => panic!("{a:?} {b:?}"),
    }
}

/// The one finding is `from` flowing into `into` without the signature saying `from: into`.
fn unproven(found: &(TierOne, Vec<BorrowFinding>), from: Origin, into: Origin) -> bool {
    found.0 == TierOne::Refused
        && matches!(found.1[..], [BorrowFinding::UnprovenOutlives { from: f, into: i, .. }] if f == from && i == into)
}

/// `fn first<'a>(x: &'a (u8, u8)) -> &'a u8 { &x.0 }`. Right: the reborrow `&(*x).0` lives no
/// longer than `'a` (RFC 2094, "Reborrow constraints": `'a: 'r`), and the result goes back into
/// `'a` itself, so no two distinct placeholders are related. Accepted by tier one.
#[test]
fn signature_returns_a_field_of_its_argument() {
    // _0 the returned &'a u8, _1 x: &'a (u8, u8), _2 the reborrow.
    let mut b = FlowBuilder::new_in(3, 1, Global);
    let oa = b.origin(true);
    let or = b.origin(false);
    b.mentions(0, oa);
    b.mentions(1, oa);
    b.mentions(2, or);
    let star = b.project(Place(1), Proj::Deref);
    let field = b.project(star, Proj::Field(0));
    let bb = b.block();
    b.start(bb);
    b.borrow(Place(2), BorrowKind::Shared, field, or, S);
    b.subset(oa, or);
    b.assign(Place(0), Operand::Move(Place(2)), S);
    b.subset(or, oa);
    b.ret(S);
    assert_eq!(tiers(&finish(b)), (TierOne::Accepted, Vec::new()));
}

/// `fn pick<'a, 'b>(x: &'a u8, y: &'b u8) -> &'a u8 { y }`: the flow, with `'a` and `'b`.
fn pick_flow() -> (Flow<Global>, Origin, Origin) {
    // _0 the returned &'a u8, _1 x: &'a u8, _2 y: &'b u8.
    let mut b = FlowBuilder::new_in(3, 2, Global);
    let (oa, ob) = (b.origin(true), b.origin(true));
    b.mentions(0, oa);
    b.mentions(1, oa);
    b.mentions(2, ob);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Copy(Place(2)), S);
    b.subset(ob, oa);
    b.ret(S);
    (finish(b), oa, ob)
}

/// `pick` is wrong: the signature states no relation between `'a` and `'b`, and the body needs
/// `'b: 'a` (rustc's E0621 family: "lifetime may not live long enough"). Both tiers refuse, and
/// the finding names the two lifetimes: from `'b` into `'a`.
#[test]
fn signature_refuses_the_other_arguments_lifetime() {
    let (flow, oa, ob) = pick_flow();
    let found = tiers(&flow);
    assert!(unproven(&found, ob, oa), "{found:?}");
    match check_body(&flow, Global) {
        Ok(r) => assert!(r.proof.is_none()),
        Err(e) => panic!("{e:?}"),
    }
}

/// `fn f(x: &u8) -> &u8 { x }`. The Reference, "Lifetime elision": "If there is exactly one
/// lifetime used in the parameters (elided or not), that lifetime is assigned to all elided
/// output lifetimes." So the output's origin is the input's: one placeholder. Accepted.
#[test]
fn signature_elided_output_takes_the_one_input() {
    let mut b = FlowBuilder::new_in(2, 1, Global);
    let o = b.origin(true);
    b.mentions(0, o);
    b.mentions(1, o);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Copy(Place(1)), S);
    b.subset(o, o);
    b.ret(S);
    assert_eq!(tiers(&finish(b)), (TierOne::Accepted, Vec::new()));
}

/// `fn g<'a, 'b: 'a>(x: &'b u8) -> &'a u8 { x }`: the flow, with `'a` and `'b`.
fn declared_bound_flow() -> (Flow<Global>, Origin, Origin) {
    let mut b = FlowBuilder::new_in(2, 1, Global);
    let (oa, ob) = (b.origin(true), b.origin(true));
    b.mentions(0, oa);
    b.mentions(1, ob);
    // The Reference, "Lifetime bounds": "`'a: 'b` is usually read as 'a outlives 'b".
    b.known(ob, oa);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Copy(Place(1)), S);
    b.subset(ob, oa);
    b.ret(S);
    (finish(b), oa, ob)
}

/// `g` is right by the bound it declares. Accepted by tier one.
#[test]
fn signature_declared_bound_is_known() {
    assert_eq!(tiers(&declared_bound_flow().0), (TierOne::Accepted, Vec::new()));
}

/// `fn name(&self) -> &Name { &self.name }`. The Reference, "Lifetime elision": "If the receiver
/// has type `&Self` or `&mut Self`, then the lifetime of that reference to `Self` is assigned to
/// all elided output lifetime parameters." One placeholder; the reborrow of the field goes back
/// into it. Accepted.
#[test]
fn signature_method_returns_a_borrow_of_self() {
    // _0 the returned &'s Name, _1 self: &'s Self, _2 other: &'o u8 (unused), _3 the reborrow.
    let mut b = FlowBuilder::new_in(4, 2, Global);
    let (os, oo) = (b.origin(true), b.origin(true));
    let or = b.origin(false);
    b.mentions(0, os);
    b.mentions(1, os);
    b.mentions(2, oo);
    b.mentions(3, or);
    let star = b.project(Place(1), Proj::Deref);
    let name = b.project(star, Proj::Field(1));
    let bb = b.block();
    b.start(bb);
    b.borrow(Place(3), BorrowKind::Shared, name, or, S);
    b.subset(os, or);
    b.assign(Place(0), Operand::Move(Place(3)), S);
    b.subset(or, os);
    b.ret(S);
    assert_eq!(tiers(&finish(b)), (TierOne::Accepted, Vec::new()));
}

/// `fn f<'a>() -> &'a u8 { let v = 0; &v }` is wrong, the classic (rustc's E0515, "cannot return
/// reference to local variable"): the loan of `v` is in `'a`, a placeholder, live at every point,
/// when `v`'s storage dies. Both tiers refuse; the finding names the loan at the `StorageDead`,
/// held by the return place's origin (`returned`: E0515's case, not E0597's).
#[test]
fn signature_refuses_a_reference_to_a_local() {
    // _0 the returned &'a u8, _1 v, _2 the borrow.
    let mut b = FlowBuilder::new_in(3, 0, Global);
    let oa = b.origin(true);
    let or = b.origin(false);
    b.mentions(0, oa);
    b.mentions(2, or);
    let bb = b.block();
    b.start(bb);
    b.stmt(StmtKind::StorageLive(Local(1)), S);
    b.assign(Place(1), Operand::Const, S);
    let l = b.borrow(Place(2), BorrowKind::Shared, Place(1), or, S);
    b.assign(Place(0), Operand::Move(Place(2)), S);
    b.subset(or, oa);
    b.stmt(StmtKind::StorageDead(Local(1)), S);
    b.ret(S);
    let flow = finish(b);
    assert_eq!(
        tiers(&flow),
        (TierOne::Refused, std::vec![BorrowFinding::Invalidated { at: Point::start(Stmt(4)), loan: l, span: S, returned: true }])
    );
}

/// `fn h<'a>(x: &'a mut (u8, u8)) -> &'a mut u8 { &mut x.0 }`. Right: a mutable reborrow through
/// a `&'a mut` may live as long as `'a` (RFC 2094, "Reborrow constraints": the supporting prefix
/// `*x` adds `'a: 'r`), and nothing uses `x` again. Accepted, by tier two: tier one has no
/// points, so the loan, in `'a` and so live everywhere, looks live at the very borrow that
/// issues it, a deep write of its own place. Tier one may not excuse a loan's own statement (in
/// a loop the earlier iteration's loan is really live there), so this is tier two's to accept.
#[test]
fn signature_returns_a_reborrow_through_a_mutable_reference() {
    let mut b = FlowBuilder::new_in(3, 1, Global);
    let oa = b.origin(true);
    let or = b.origin(false);
    b.mentions(0, oa);
    b.mentions(1, oa);
    b.mentions(2, or);
    let star = b.project(Place(1), Proj::Deref);
    let field = b.project(star, Proj::Field(0));
    let bb = b.block();
    b.start(bb);
    b.borrow(Place(2), BorrowKind::Mut, field, or, S);
    b.subset(oa, or);
    b.assign(Place(0), Operand::Move(Place(2)), S);
    b.subset(or, oa);
    b.ret(S);
    let flow = finish(b);
    assert_eq!(tiers(&flow), (TierOne::Refused, Vec::new()));
    match check_body(&flow, Global) {
        Ok(r) => assert_eq!(r.proof.map(|p| p.tier()), Some(2)),
        Err(e) => panic!("{e:?}"),
    }
}

/// `fn inner<'a, 'b>(x: &'a &'b u8) -> &'a u8 { *x }` (`wrong` false) and its nearest wrong
/// program `fn bad<'a, 'b>(x: &'a &'b u8, y: &'a u8) -> &'b u8 { y }` (`wrong` true). The type
/// `&'a &'b u8` is well formed only when `'b: 'a` (the Reference, "Lifetime bounds": "`&'a T`
/// requires `T: 'a`", an implied bound of the signature), so `'b: 'a` is known and `'a: 'b` is not.
fn implied_bound_flow(wrong: bool) -> (Flow<Global>, Origin, Origin) {
    // _0 the result, _1 x: &'a &'b u8, _2 y: &'a u8.
    let mut b = FlowBuilder::new_in(3, 2, Global);
    let (oa, ob) = (b.origin(true), b.origin(true));
    b.mentions(0, if wrong { ob } else { oa });
    b.mentions(1, oa);
    b.mentions(1, ob);
    b.mentions(2, oa);
    b.known(ob, oa);
    let star = b.project(Place(1), Proj::Deref);
    let bb = b.block();
    b.start(bb);
    if wrong {
        b.assign(Place(0), Operand::Copy(Place(2)), S);
        b.subset(oa, ob);
    } else {
        b.assign(Place(0), Operand::Copy(star), S);
        b.subset(ob, oa);
    }
    b.ret(S);
    (finish(b), oa, ob)
}

/// `inner` is right by the implied bound; `bad` needs the bound's reverse and is refused, the
/// finding naming `'a` flowing into `'b`.
#[test]
fn signature_implied_bound_of_a_nested_reference() {
    assert_eq!(tiers(&implied_bound_flow(false).0), (TierOne::Accepted, Vec::new()));
    let (flow, oa, ob) = implied_bound_flow(true);
    let found = tiers(&flow);
    assert!(unproven(&found, oa, ob), "{found:?}");
}

/// Control: with the signature's relations ignored (`Plant::KnownIgnored`) the two programs that
/// are right only by a known relation (the declared bound, the implied bound) are refused: the
/// accept fixtures above rest on the `known` table and nothing else.
#[test]
fn signature_control_relations_ignored() {
    let (flow, oa, ob) = declared_bound_flow();
    let found = tiers_with(&flow, Plant::KnownIgnored);
    assert!(unproven(&found, ob, oa), "{found:?}");
    let (flow, oa, ob) = implied_bound_flow(false);
    let found = tiers_with(&flow, Plant::KnownIgnored);
    assert!(unproven(&found, ob, oa), "{found:?}");
}

/// Control: with a relation the signature does not state taken as known (`Plant::AllKnown`),
/// `pick` and `bad` wrongly pass both tiers: the refuse fixtures above rest on the table being
/// exactly the signature's.
#[test]
fn signature_control_missing_relation_taken_as_known() {
    assert_eq!(tiers_with(&pick_flow().0, Plant::AllKnown), (TierOne::Accepted, Vec::new()));
    assert_eq!(tiers_with(&implied_bound_flow(true).0, Plant::AllKnown), (TierOne::Accepted, Vec::new()));
}
