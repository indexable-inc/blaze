//! The simulations (an oracle and a planted bug each) and the fixtures from the written sources
//! (flow.md: the Reference, RFC 2094, RFC 2025, the Polonius book).

use std::alloc::Global;
use std::collections::BTreeSet;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_sim::{Broke, Config, Tape, check, control};

use crate::flow::{BorrowKind, Flow, FlowBuilder, Local, Operand, Origin, Place, Point, Proj, Span, Stmt, StmtKind};
use crate::loans::{BorrowFinding, Kept, LoanParts, TierOne, facts, tier_one, tier_two};
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
    crate::loans::assert_tiers_equal_for_fixture(&flow);
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
    crate::loans::assert_tiers_equal_for_fixture(flow);
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
    crate::loans::assert_tiers_equal_for_fixture(&flow);
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
        Ok(r) => {
            assert!(r.proof.is_none() && r.borrows.len() == 1);
            // The finding's three parts: the mutable borrow of `_1` at s1, the conflict at s2,
            // and the call at s3 reading `_2` (the reference holding the loan) that keeps it live.
            assert_eq!(
                r.loan_parts.as_slice(),
                &[LoanParts {
                    at: Point::start(Stmt(2)),
                    loan: l,
                    place: Place(1),
                    issued: S,
                    kept: Kept::Use { stmt: Stmt(3), local: Local(2), span: S },
                }]
            );
        }
        Err(e) => panic!("{e:?}"),
    }
}

/// The later use is the nearest along the flow, not the first in the statement array, and a
/// redefinition of the holding local ends the search along its path: `r = &mut x` (s1),
/// `s = &x` (s2, the conflict), then `switch` to bb1 (`r = const; ret`, the redefinition
/// first) or bb2 (`use(r); ret`). Only bb2's call keeps the loan live: the part names it,
/// though bb1's statements come first in the array. CONTROL (rule 23): with bb2's call reading
/// a local the loan never reaches, no finding is made and no part is filed.
#[test]
fn the_later_use_is_the_one_that_keeps_the_loan_live() {
    for control in [false, true] {
        // _0 return, _1 x, _2 r, _3 s, _4 a call's result, _5 an unrelated reference.
        let mut b = FlowBuilder::new_in(6, 1, Global);
        let (or, os, ou) = (b.origin(false), b.origin(false), b.origin(false));
        b.mentions(2, or);
        b.mentions(3, os);
        b.mentions(5, ou);
        let (bb0, bb1, bb2) = (b.block(), b.block(), b.block());
        b.start(bb0);
        b.assign(Place(0), Operand::Const, S);
        let l = b.borrow(Place(2), BorrowKind::Mut, Place(1), or, S);
        b.borrow(Place(3), BorrowKind::Shared, Place(1), os, S);
        b.switch(Place(0), &[bb1, bb2], S);
        b.start(bb1);
        b.assign(Place(2), Operand::Const, S);
        b.ret(S);
        b.start(bb2);
        let read = if control { Place(5) } else { Place(2) };
        let used = b.call(&[Operand::Copy(read)], Place(4), S);
        b.ret(S);
        let flow = finish(b);
        let r = match check_body(&flow, Global) {
            Ok(r) => r,
            Err(e) => panic!("{e:?}"),
        };
        if control {
            assert!(r.borrows.is_empty() && r.loan_parts.is_empty(), "control: {:?} {:?}", r.borrows, r.loan_parts);
            continue;
        }
        assert_eq!(
            r.loan_parts.as_slice(),
            &[LoanParts {
                at: Point::start(Stmt(2)),
                loan: l,
                place: Place(1),
                issued: S,
                kept: Kept::Use { stmt: used, local: Local(2), span: S },
            }]
        );
    }
}

/// The loans invalidated in `flow`, both tiers checked equal first.
fn invalidated(flow: &Flow<Global>) -> Vec<(Stmt, crate::flow::Loan)> {
    let (_, two) = tiers(flow);
    two.iter()
        .filter_map(|f| match *f {
            BorrowFinding::Invalidated { at, loan, .. } => Some((at.stmt(), loan)),
            BorrowFinding::UnprovenOutlives { .. } => None,
        })
        .collect()
}

/// `a = const; r = &a; dead a; mention *r` (`deref`) or `mention r`: the Reference, "Wildcard
/// pattern" (`let _ = *r;`, `let _ = r;`) with `r` dangling. _1 a, _2 r.
fn mention_flow(deref: bool) -> (Flow<Global>, crate::flow::Loan) {
    let mut b = FlowBuilder::new_in(3, 0, Global);
    let o = b.origin(false);
    b.mentions(2, o);
    let star = b.project(Place(2), Proj::Deref);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Const, S);
    b.stmt(StmtKind::StorageLive(Local(1)), S);
    b.assign(Place(1), Operand::Const, S);
    let l = b.borrow(Place(2), BorrowKind::Shared, Place(1), o, S);
    b.stmt(StmtKind::StorageDead(Local(1)), S);
    b.mention(if deref { star } else { Place(2) }, S);
    b.ret(S);
    (finish(b), l)
}

/// A mention through a `Deref` is a use of its local (`StmtKind::Mention`): `let _ = *r;` keeps
/// `r`, so `a`'s loan, live at `dead a`: the loan is invalidated there (E0597). Naming `r` reads
/// nothing through it: `let _ = r;` keeps nothing live, no finding. The pair is the control:
/// the same flow, the `Deref` removed, and the finding goes (rule 23).
#[test]
fn a_mention_through_a_deref_keeps_the_reference_live() {
    let (flow, l) = mention_flow(true);
    let mut text = String::new();
    let _ = flow.print(&mut text);
    assert!(text.contains("mention "), "printed: {text}");
    assert_eq!(invalidated(&flow), [(Stmt(4), l)], "{text}");
    let (flow, _) = mention_flow(false);
    assert_eq!(invalidated(&flow), [], "a mention of the reference itself");
}

/// `x = const; m = &mut x; (mention x | _0 = copy x); *m = const`. _1 x, _2 m.
fn mention_under_mut_flow(read: bool) -> (Flow<Global>, crate::flow::Loan) {
    let mut b = FlowBuilder::new_in(3, 0, Global);
    let o = b.origin(false);
    b.mentions(2, o);
    let star = b.project(Place(2), Proj::Deref);
    let bb = b.block();
    b.start(bb);
    b.assign(Place(1), Operand::Const, S);
    let l = b.borrow(Place(2), BorrowKind::Mut, Place(1), o, S);
    if read {
        b.assign(Place(0), Operand::Copy(Place(1)), S);
    } else {
        b.mention(Place(1), S);
        b.assign(Place(0), Operand::Const, S);
    }
    b.assign(star, Operand::Const, S);
    b.ret(S);
    (finish(b), l)
}

/// A mention is no access: `let m = &mut x; let _ = x; *m = 1;` invalidates nothing (the
/// Reference, "Wildcard pattern": it "does not copy, move or borrow the value it matches").
/// Control: the same flow with the mention a read (`_0 = copy x`) is the conflict (E0503).
#[test]
fn a_mention_conflicts_with_no_loan() {
    let (flow, _) = mention_under_mut_flow(false);
    assert_eq!(invalidated(&flow), [], "a mention under a live `&mut`");
    match check_moves(&flow, Global) {
        Ok(v) => assert!(v.is_empty(), "a mention moves nothing: {v:?}"),
        Err(b) => panic!("{b:?}"),
    }
    let (flow, l) = mention_under_mut_flow(true);
    assert_eq!(invalidated(&flow), [(Stmt(2), l)], "CONTROL: a read under a live `&mut`");
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

// ---------------------------------------------------------------- builder, equivalence & stress

#[test]
fn flow_builder_interning_and_mentions_sorting() {
    use crate::flow::PlaceMark;

    let mut b = FlowBuilder::new_in(4, 2, Global);
    let extra = b.add_local();
    assert_eq!(extra, Local(4));
    let p_extra = b.local(4);

    // Wide children + deep nesting + repeated lookups keep first id and mark.
    let f0 = b.project_in(Place(1), Proj::Field(0), PlaceMark::DropField);
    let f1 = b.project_in(Place(1), Proj::Field(1), PlaceMark::SliceElement);
    let f2 = b.project_in(Place(1), Proj::Field(2), PlaceMark::None);
    let f0_sub = b.project_in(f0, Proj::Field(0), PlaceMark::None);
    let e0 = b.project_in(p_extra, Proj::Index, PlaceMark::SliceElement);

    assert_eq!(b.project_in(Place(1), Proj::Field(0), PlaceMark::None), f0);
    assert_eq!(b.project_in(Place(1), Proj::Field(1), PlaceMark::DropField), f1);
    assert_eq!(b.project_in(Place(1), Proj::Field(2), PlaceMark::None), f2);
    assert_eq!(b.project_in(f0, Proj::Field(0), PlaceMark::DropField), f0_sub);
    assert_eq!(b.project_in(p_extra, Proj::Index, PlaceMark::None), e0);

    let o0 = b.origin(false);
    let o1 = b.origin(false);
    let o2 = b.origin(true);
    // Out-of-order + consecutive and non-consecutive duplicates.
    b.mentions(3, o2);
    b.mentions(3, o2);
    b.mentions(1, o1);
    b.mentions(1, o0);
    b.mentions(3, o0);
    b.mentions(1, o1);
    b.mentions(4, o2);

    let bb = b.block();
    b.start(bb);
    b.assign(Place(0), Operand::Const, S);
    b.ret(S);
    let flow = finish(b);

    assert!(flow.place(f0).of_drop_type && !flow.place(f0).of_slice);
    assert!(flow.place(f1).of_slice && !flow.place(f1).of_drop_type);
    assert!(flow.place(e0).of_slice && !flow.place(e0).of_drop_type);
    assert_eq!(
        flow.local_origins.as_slice(),
        &[(Local(1), o0), (Local(1), o1), (Local(3), o0), (Local(3), o2), (Local(4), o2)]
    );
}

#[test]
fn placeholder_transitive_known_and_unproven_tier_one() {
    // Three placeholders 'a, 'b, 'c with transitive known 'a: 'b and 'b: 'c (through a chain).
    let build = |add_unproven: bool| -> (Flow<Global>, Origin, Origin) {
        let mut b = FlowBuilder::new_in(4, 3, Global);
        let oa = b.origin(true);
        let ob = b.origin(true);
        let oc = b.origin(true);
        let mid1 = b.origin(false);
        let mid2 = b.origin(false);
        b.known(oa, ob);
        b.known(ob, oc);
        b.mentions(0, oc);
        b.mentions(1, oa);
        b.mentions(2, ob);
        b.mentions(3, oc);
        let bb = b.block();
        b.start(bb);
        b.assign(Place(0), Operand::Copy(Place(1)), S);
        // Transitive subset path oa -> mid1 -> mid2 -> oc is proven by oa: ob: oc.
        b.subset(oa, mid1);
        b.subset(mid1, mid2);
        b.subset(mid2, oc);
        if add_unproven {
            // Unproven reverse path oc -> mid1 -> oa.
            b.subset(oc, mid1);
            b.subset(mid1, oa);
        }
        b.ret(S);
        (finish(b), oc, oa)
    };

    let (ok_flow, _, _) = build(false);
    assert_eq!(tiers(&ok_flow), (TierOne::Accepted, Vec::new()));
    let (ignored_one, ignored_two) = tiers_with(&ok_flow, Plant::KnownIgnored);
    assert_eq!(ignored_one, TierOne::Refused);
    assert!(!ignored_two.is_empty());

    let (bad_flow, oc, oa) = build(true);
    let (bad_one, bad_two) = tiers(&bad_flow);
    assert_eq!(bad_one, TierOne::Refused);
    assert!(
        bad_two.iter().any(|f| matches!(*f, BorrowFinding::UnprovenOutlives { from, into, .. } if from == oc && into == oa)),
        "expected UnprovenOutlives({oc} -> {oa}), got {bad_two:?}"
    );
    assert_eq!(tiers_with(&bad_flow, Plant::AllKnown), (TierOne::Accepted, Vec::new()));
}

/// Stress & equivalence test: exercises large place trees (`FlowBuilder::project_in`,
/// `PlaceSubtrees`), out-of-order `mentions`, multi-block diamonds + loops (`moves::inits`,
/// `loans::facts` liveness & activation), and `loans::tier_one` (CSR `close`, `loans_by_local`,
/// transposed `contains_t`, and guarded placeholder DFS).
#[test]
fn large_flow_stress_and_equivalence() {
    use std::time::Instant;

    let t0 = Instant::now();
    // Build a flow with 600 locals, ~2400 places, 600 origins, 400 loans, ~2500 statements across
    // 200 blocks (diamonds + a loop back-edge + drops + two-phase borrows).
    let n_groups: u32 = 200;
    let locals: u32 = 1 + n_groups * 3; // _0 return, plus (val, ref_tmp, scratch) per group
    let args: u32 = n_groups; // _1..=n_groups are args
    let mut b = FlowBuilder::new_in(locals, args, Global);

    // Project 3 nested levels on every group's value local (200 * 9 = 1800 extra places),
    // and re-project every field once to exercise interning hits.
    let mut f0 = Vec::with_capacity(n_groups as usize);
    let mut f1 = Vec::with_capacity(n_groups as usize);
    let mut f0_0 = Vec::with_capacity(n_groups as usize);
    for g in 0..n_groups {
        let base = Place(1 + g);
        let p0 = b.project(base, Proj::Field(0));
        let p1 = b.project(base, Proj::Field(1));
        let p2 = b.project(base, Proj::Field(2));
        let p00 = b.project(p0, Proj::Field(0));
        let p01 = b.project(p0, Proj::Field(1));
        let _p10 = b.project(p1, Proj::Field(0));
        let _p11 = b.project(p1, Proj::Field(1));
        let _p20 = b.project(p2, Proj::Field(0));
        let _p000 = b.project(p00, Proj::Field(0));
        // Re-lookup existing children: must return exact same ids.
        assert_eq!(b.project(base, Proj::Field(0)), p0);
        assert_eq!(b.project(base, Proj::Field(1)), p1);
        assert_eq!(b.project(p0, Proj::Field(0)), p00);
        assert_eq!(b.project(p0, Proj::Field(1)), p01);
        f0.push(p0);
        f1.push(p1);
        f0_0.push(p00);
    }

    // Create 600 origins (including 2 placeholders with a known bound and a bridge) and out-of-order mentions.
    let ph_a = b.origin(true);
    let ph_b = b.origin(true);
    let ph_bridge = b.origin(false);
    b.known(ph_a, ph_b);
    let mut origins = Vec::with_capacity((n_groups * 3) as usize);
    for _ in 0..(n_groups * 3) {
        origins.push(b.origin(false));
    }
    for g in (0..n_groups).rev() {
        let ref_local = 1 + n_groups + g;
        let scratch_local = 1 + 2 * n_groups + g;
        b.mentions(ref_local, origins[(g * 3) as usize]);
        b.mentions(scratch_local, origins[(g * 3 + 1) as usize]);
        b.mentions(ref_local, origins[(g * 3) as usize]); // duplicate
    }
    b.mentions(0, ph_b);

    // Reserve blocks: entry + 200 diamond/chain groups (2 blocks each) + exit.
    let entry = b.block();
    let mut left_bbs = Vec::with_capacity(n_groups as usize);
    let mut join_bbs = Vec::with_capacity(n_groups as usize);
    for _ in 0..n_groups {
        left_bbs.push(b.block());
        join_bbs.push(b.block());
    }
    let exit_bb = b.block();

    b.start(entry);
    b.assign(Place(0), Operand::Const, S);
    b.subset(ph_a, ph_bridge);
    b.subset(ph_bridge, ph_b);
    b.goto(join_bbs[0], S);

    let mut drop_stmts = Vec::with_capacity(n_groups as usize);
    for g in 0..n_groups as usize {
        let val = Place(1 + g as u32);
        let ref_p = Place(1 + n_groups + g as u32);
        let scr_p = Place(1 + 2 * n_groups + g as u32);
        let o0 = origins[g * 3];
        let o1 = origins[g * 3 + 1];
        let o2 = origins[g * 3 + 2];

        b.start(join_bbs[g]);
        // Shared borrow -> subset -> call into scr_p -> call consumes scr_p (both dead afterwards).
        b.borrow(ref_p, BorrowKind::Shared, f0[g], o0, S);
        b.subset(o0, o1);
        b.call(&[Operand::Move(ref_p)], scr_p, S);
        b.call(&[Operand::Move(scr_p)], Place(0), S);
        // Two-phase borrow -> activate -> call consumes ref_p (dead afterwards).
        let tp = b.borrow(ref_p, BorrowKind::TwoPhase, f1[g], o2, S);
        b.stmt(StmtKind::Activate(tp), S);
        b.call(&[Operand::Move(ref_p)], Place(0), S);
        // Move f0_0 on all paths, then drop val (DropKind::Open) and branch to left_bbs[g] or next.
        b.assign(Place(0), Operand::Move(f0_0[g]), S);
        let drop_s = b.stmt(StmtKind::Drop(val), S);
        drop_stmts.push((drop_s, val));
        let next = if g + 1 < n_groups as usize { join_bbs[g + 1] } else { exit_bb };
        b.switch(Place(0), &[left_bbs[g], next], S);

        b.start(left_bbs[g]);
        // Re-init val and jump to next (or back-edge on g == 10 to exercise loop convergence).
        b.assign(val, Operand::Const, S);
        let target = if g == 10 { join_bbs[10] } else { next };
        b.goto(target, S);
    }

    b.start(exit_bb);
    b.ret(S);

    let flow = finish(b);
    assert!(flow.places.len() >= 2400);
    assert!(flow.stmts.len() >= 2000);
    assert_eq!(flow.loans.len(), (n_groups * 2) as usize);

    let rec = check_body(&flow, Global).expect("large stress flow converges");
    assert!(rec.moves.is_empty(), "expected no move findings, got {:?}", &rec.moves[..rec.moves.len().min(5)]);
    assert!(rec.borrows.is_empty(), "expected no borrow findings, got {:?}", &rec.borrows[..rec.borrows.len().min(5)]);
    assert_eq!(rec.proof.map(|p| p.tier()), Some(1), "clean large flow should be accepted by tier one");

    // Verify drop_kind & drop_may_run match brute-force `is_prefix` check on all 200 drops.
    let st = inits(&flow, Plant::None, Global).expect("inits converges");
    for &(s, p) in &drop_stmts {
        let got_kind = drop_kind(&flow, &st, s, p);
        let got_may_run = st.drop_may_run(&flow, s, p);
        let brute_may_run = st.reached[s.index()]
            && (0..flow.places.len()).any(|q| flow.is_prefix(p, Place(q as u32)) && st.init.get(s.index(), q));
        let brute_part_moved = (0..flow.places.len()).any(|q| {
            let q = Place(q as u32);
            q != p && flow.is_prefix(p, q) && st.uninit.get(s.index(), q.index())
        });
        let brute_kind = match (st.init.get(s.index(), p.index()), st.uninit.get(s.index(), p.index())) {
            (false, _) => DropKind::Nothing,
            (true, true) => DropKind::Flag,
            (true, false) if brute_part_moved => DropKind::Open,
            (true, false) => DropKind::Static,
        };
        assert_eq!(got_may_run, brute_may_run);
        assert_eq!(got_kind, brute_kind);
    }
    let elapsed = t0.elapsed();
    std::println!(
        "large_flow_stress: places={} stmts={} loans={} origins={} elapsed={elapsed:?}",
        flow.places.len(),
        flow.stmts.len(),
        flow.loans.len(),
        flow.origins,
    );
}

/// Dropping a non-`Drop` wrapper (`Slot<&'a T>`) does NOT conflict with a live borrow behind
/// `Proj::Deref` (`&*slot.0`) when no intervening ancestor implements `Drop`.
/// NEGATIVE CONTROL (rule 23): when the wrapper implements `Drop` (`of_drop_type = true` on
/// `slot.0`), dropping `slot` DOES conflict with the live borrow behind `Deref` (`Invalidated`).
#[test]
fn a_drop_of_a_non_drop_wrapper_does_not_conflict_behind_deref() {
    let build = |of_drop_type: bool| {
        // _0 return, _1 wrapper `slot`, _2 borrowed ref `r`
        let mut b = FlowBuilder::new_in(3, 1, Global);
        let bb0 = b.block();
        b.start(bb0);
        let slot = Place(1);
        let r = Place(2);
        let o_ref = b.origin(false);
        b.mentions(2, o_ref);
        let field0 = b.project_marked(slot, Proj::Field(0), of_drop_type);
        let deref0 = b.project(field0, Proj::Deref);
        // r = &(*slot.0)
        let _loan = b.borrow(r, BorrowKind::Shared, deref0, o_ref, S);
        // Drop(slot) while `r` is still live (used below)
        b.stmt(StmtKind::Drop(slot), S);
        // Use `r` so `o_ref` is live across `Drop(slot)`
        b.assign(Place(0), Operand::Copy(r), S);
        b.ret(S);
        finish(b)
    };

    let ok_flow = build(false);
    assert_eq!(tiers(&ok_flow), (TierOne::Accepted, Vec::new()), "non-Drop wrapper drop behind Deref must not conflict");

    let bad_flow = build(true);
    let (tier1, findings) = tiers(&bad_flow);
    assert_eq!(tier1, TierOne::Refused, "CONTROL: Drop wrapper must conflict with live borrow behind Deref");
    assert!(
        findings.iter().any(|f| matches!(*f, BorrowFinding::Invalidated { .. })),
        "CONTROL: expected Invalidated when wrapper implements Drop, got {findings:?}"
    );
}

/// Borrowing behind `Proj::Deref` of a shared reference (`&T`, `PlaceMark::SharedDeref`, e.g.
/// `let rest = &*(*self).src;` where `self.src: &'a str`) does NOT lock `*self`, so activating
/// `&mut *self` while `rest` is live is accepted.
/// NEGATIVE CONTROL (rule 23): when `self.src` is `&mut T` (`PlaceMark::None`), activating
/// `&mut *self` while `rest` is live STILL conflicts (`Invalidated`).
#[test]
fn a_borrow_behind_shared_ref_deref_does_not_conflict_with_prefix_mut_borrow() {
    let build = |shared_ref: bool| {
        // _0 return, _1 self: &mut S, _2 rest: &str, _3 m: &mut S
        let mut b = FlowBuilder::new_in(4, 1, Global);
        let bb0 = b.block();
        b.start(bb0);
        let me = Place(1);
        let rest = Place(2);
        let m = Place(3);
        let o_rest = b.origin(false);
        let o_mut = b.origin(false);
        b.mentions(2, o_rest);
        b.mentions(3, o_mut);
        let star_self = b.project(me, Proj::Deref);
        let src = b.project(star_self, Proj::Field(0));
        let mark = if shared_ref { crate::PlaceMark::SharedDeref } else { crate::PlaceMark::None };
        let deref_src = b.project_in(src, Proj::Deref, mark);
        // rest = &*(*self).src
        let _l_rest = b.borrow(rest, BorrowKind::Shared, deref_src, o_rest, S);
        // m = &two_phase mut *self; Activate(m)
        let l_mut = b.borrow(m, BorrowKind::TwoPhase, star_self, o_mut, S);
        b.stmt(StmtKind::Activate(l_mut), S);
        // Use both `m` and `rest` after activation
        b.assign(Place(0), Operand::Copy(rest), S);
        b.ret(S);
        finish(b)
    };

    let ok_flow = build(true);
    assert_eq!(tiers(&ok_flow), (TierOne::Accepted, Vec::new()), "shared-ref deref borrow must not lock prefix");

    let bad_flow = build(false);
    let (tier1, findings) = tiers(&bad_flow);
    assert_eq!(tier1, TierOne::Refused, "CONTROL: &mut-ref deref borrow MUST lock prefix");
    assert!(
        findings.iter().any(|f| matches!(*f, BorrowFinding::Invalidated { .. })),
        "CONTROL: expected Invalidated for &mut-ref deref, got {findings:?}"
    );
}
