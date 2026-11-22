//! THE SIMULATION (rule 64, D161 step 1): bodies whose facts are met late, made by whichever
//! body's asker claims them first, in an order the tape draws. A world is up to eight facts,
//! each needing some later ones (and, when drawn, one planted cycle of one to three facts that
//! nothing else lies on); up to three bodies each name some facts in their text (the gate set)
//! and meet others after entry. Each pick runs one body's asker for one to three maker steps,
//! so claims are held across yields and other bodies find them running and park.
//!
//! The oracle, after every body is done:
//! - each fact is made at most once ("made twice");
//! - each body is entered exactly once ("entered more than once");
//! - no body and no maker is handed a fact before it is published: the value handed equals the
//!   published one, read by an observer's claim ("before it was published");
//! - a planted cycle some body reaches is reported, and every report names exactly its facts
//!   ("a cycle named"); with none, nothing fails.
//!
//! Controls (rule 53, D165: each plant a parameter): a done fact made again
//! (`Plant::MakeWhatIsDone`), a running fact read without a wait (`Plant::ReadRunning`), a cycle
//! named by one need (`Plant::CycleNamesOne`), and the restart path itself: a body that parked
//! after entry starts over (`P::Restart`). Each planted, each caught.

use std::cell::RefCell;
use std::string::String;
use std::vec::Vec as SVec;
use std::{format, vec};

use blaze_core::claim_table::{Claimed, TaskId};
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};

use crate::ask::{ASK_REFUSED, AskError, Asked, Asker, AskerOpts, Board, Given, Maker, Plant, Step};
use crate::gate::GateSet;
use crate::need::{Fact, Need, Subject};

const MOST_FACTS: usize = 8;

fn need(f: usize) -> Need {
    Need::whole(Fact::Head, Subject(ContentId([f as u8 + 1; 32])))
}

fn fact_of(n: &Need) -> usize {
    usize::from(n.subject.0.0[0]) - 1
}

fn value(f: usize) -> u32 {
    100 + f as u32
}

struct World {
    deps: SVec<SVec<usize>>,
    early: SVec<SVec<usize>>,
    late: SVec<SVec<usize>>,
    /// The planted cycle's facts, ascending; empty when none.
    cycle: SVec<usize>,
}

fn world(tape: &Tape, cycle: bool) -> World {
    let n = tape.int_in(2usize..=MOST_FACTS, "facts");
    let len = if cycle { tape.int_in(1usize..=n.min(3), "cycle length") } else { 0 };
    let mut pool: SVec<usize> = (0..n).collect();
    tape.shuffle(&mut pool, "cycle facts");
    let mut cyc: SVec<usize> = pool[..len].to_vec();
    cyc.sort_unstable();
    let mut deps = vec![SVec::new(); n];
    for (i, d) in deps.iter_mut().enumerate() {
        if cyc.contains(&i) {
            continue;
        }
        for j in i + 1..n {
            if d.len() < 3 && tape.coin("needs") {
                d.push(j);
            }
        }
    }
    for k in 0..len {
        deps[cyc[k]].push(cyc[(k + 1) % len]);
    }
    let bodies = tape.int_in(1usize..=3, "bodies");
    let mut early = SVec::new();
    let mut late = SVec::new();
    for _ in 0..bodies {
        let mut e = SVec::new();
        for _ in 0..tape.int_in(0usize..=2, "gate facts") {
            e.push(tape.index(n, "gate fact"));
        }
        let mut l = SVec::new();
        for _ in 0..tape.int_in(0usize..=3, "late facts") {
            l.push(tape.index(n, "late fact"));
        }
        early.push(e);
        late.push(l);
    }
    World { deps, early, late, cycle: cyc }
}

/// Whether some body's facts lead to the planted cycle.
fn reaches_cycle(w: &World) -> bool {
    let mut seen = vec![false; w.deps.len()];
    let mut work: SVec<usize> = w.early.iter().chain(w.late.iter()).flatten().copied().collect();
    while let Some(f) = work.pop() {
        if seen[f] {
            continue;
        }
        seen[f] = true;
        work.extend(w.deps[f].iter().copied());
    }
    w.cycle.iter().any(|&c| seen[c])
}

struct SimMaker<'w> {
    world: &'w World,
    starts: &'w RefCell<SVec<u32>>,
    broke: &'w RefCell<Option<String>>,
}

struct SimFrame {
    fact: usize,
    next: usize,
    failed: bool,
}

impl Maker for SimMaker<'_> {
    type Frame = SimFrame;

    fn start(&mut self, need: Need) -> SimFrame {
        let fact = fact_of(&need);
        self.starts.borrow_mut()[fact] += 1;
        SimFrame { fact, next: 0, failed: false }
    }

    fn step(&mut self, _need: Need, frame: &mut SimFrame, given: Option<Given>) -> Step {
        let deps = &self.world.deps[frame.fact];
        if let Some(g) = given {
            let dep = deps[frame.next - 1];
            match g {
                Given::Made(v) if v == value(dep) => {}
                Given::Made(v) => {
                    let mut broke = self.broke.borrow_mut();
                    if broke.is_none() {
                        *broke = Some(format!("fact {}'s maker was handed {v} for fact {dep} before it was published", frame.fact));
                    }
                }
                Given::Failed(_) => frame.failed = true,
            }
        }
        if frame.next < deps.len() {
            frame.next += 1;
            return Step::Needs(need(deps[frame.next - 1]));
        }
        if frame.failed { Step::Failed(1) } else { Step::Made(value(frame.fact)) }
    }
}

/// The published word of `n`'s slot, read by the observer (`None`: not published).
fn published(board: &Board<Alloc<'static>>, observer: TaskId, n: &Need) -> Option<u32> {
    match board.table().claim(n.key().claim_key(), observer) {
        Ok(Claimed::Done { value, .. }) => Some(value),
        Ok(Claimed::Mine(claim)) => {
            claim.publish(Given::Failed(ASK_REFUSED).word()).for_each(|_| {});
            None
        }
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum P {
    Clean,
    Ask(Plant),
    /// The restart path: a body that parked after entry is entered again from the top.
    Restart,
}

struct Body<'t> {
    asker: Asker<'t, Alloc<'static>, SimFrame>,
    /// The need asked and not yet answered, and whether it was met after entry.
    pending: Option<(usize, bool)>,
    gate_i: usize,
    late_i: usize,
    in_body: bool,
    entered: u32,
    parked: bool,
    parked_late: bool,
    done: bool,
}

fn case(tape: &Tape, p: P) -> Result<(), Broke> {
    let w = world(tape, tape.coin("plant a cycle"));
    let alloc = blaze_own::testing::heap().alloc();
    let bodies = w.early.len();
    let board = Board::new_in(64, bodies as u32 + 1, alloc).map_err(|e| Broke::Harness(format!("{e}")))?;
    let observer = TaskId(bodies as u32);
    let plant = match p {
        P::Ask(pl) => pl,
        P::Clean | P::Restart => Plant::Clean,
    };
    let mut bs: SVec<Body<'_>> = (0..bodies)
        .map(|b| Body {
            asker: Asker::new(AskerOpts { board: &board, me: TaskId(b as u32), alloc, plant, .. }),
            pending: None,
            gate_i: 0,
            late_i: 0,
            in_body: false,
            entered: 0,
            parked: false,
            parked_late: false,
            done: false,
        })
        .collect();
    let starts = RefCell::new(vec![0u32; w.deps.len()]);
    let broke = RefCell::new(None);
    let mut cycles: SVec<SVec<usize>> = SVec::new();
    let mut failures = 0u32;
    let mut finished = false;
    for _ in 0..4000 {
        let runnable: SVec<usize> = (0..bodies).filter(|&b| !bs[b].done && !bs[b].parked).collect();
        if runnable.is_empty() {
            if bs.iter().any(|b| b.parked) {
                return Err(Broke::Oracle(String::from("a body stays parked with nothing running: a hang")));
            }
            finished = true;
            break;
        }
        let b = runnable[tape.index(runnable.len(), "who runs")];
        let fuel = tape.int_in(1u32..=3, "fuel");
        let mut woken: SVec<TaskId> = SVec::new();
        let mut maker = SimMaker { world: &w, starts: &starts, broke: &broke };
        let body = &mut bs[b];
        let out = match body.pending {
            Some(_) => body.asker.resume(&mut maker, fuel, &mut |t| woken.push(t)),
            None => {
                let (f, late) = if body.gate_i < w.early[b].len() {
                    (w.early[b][body.gate_i], false)
                } else if !body.in_body {
                    let gate = GateSet::of(w.early[b].iter().map(|&f| need(f)), alloc);
                    if let Err(u) = gate.settle(|n| published(&board, observer, n).is_some()) {
                        return Err(Broke::Oracle(format!("body {b} was entered with {} unsettled", u.need)));
                    }
                    body.in_body = true;
                    body.entered += 1;
                    continue;
                } else if body.late_i < w.late[b].len() {
                    (w.late[b][body.late_i], true)
                } else {
                    body.done = true;
                    continue;
                };
                body.pending = Some((f, late));
                body.asker.ask(&mut maker, need(f), fuel, &mut |t| woken.push(t))
            }
        };
        let Some((f, late)) = body.pending else { return Err(Broke::Harness(String::from("no need pending"))) };
        let answered = match out {
            Ok(Asked::Ready(g)) => {
                if published(&board, observer, &need(f)) != Some(g.word()) {
                    return Err(Broke::Oracle(format!("body {b} was handed {g:?} for fact {f} before it was published")));
                }
                match g {
                    Given::Made(v) if v != value(f) => {
                        return Err(Broke::Oracle(format!("body {b} was handed {v} for fact {f} before it was published")));
                    }
                    Given::Made(_) => {}
                    Given::Failed(_) => failures += 1,
                }
                true
            }
            Ok(Asked::Parked { .. }) => {
                body.parked = true;
                body.parked_late |= late;
                false
            }
            Ok(Asked::Yielded) => false,
            Err(AskError::Cycle { needs }) => {
                let mut got: SVec<usize> = needs.iter().map(fact_of).collect();
                got.sort_unstable();
                cycles.push(got);
                failures += 1;
                true
            }
            Err(e) => return Err(Broke::Oracle(format!("body {b}'s ask of fact {f} stopped: {e}"))),
        };
        if answered {
            body.pending = None;
            if late {
                body.late_i += 1;
            } else {
                body.gate_i += 1;
            }
            if p == P::Restart && body.parked_late {
                body.gate_i = 0;
                body.late_i = 0;
                body.in_body = false;
                body.parked_late = false;
            }
        }
        for t in woken {
            if let Some(o) = bs.get_mut(t.0 as usize) {
                o.parked = false;
            }
        }
    }
    if !finished {
        return Err(Broke::Harness(String::from("the bodies did not finish in 4000 picks")));
    }
    if let Some(why) = broke.borrow_mut().take() {
        return Err(Broke::Oracle(why));
    }
    for (f, &n) in starts.borrow().iter().enumerate() {
        if n > 1 {
            return Err(Broke::Oracle(format!("fact {f} was made {n} times: made twice")));
        }
    }
    for (b, body) in bs.iter().enumerate() {
        if body.entered != 1 {
            return Err(Broke::Oracle(format!("body {b} was entered {} times: entered more than once", body.entered)));
        }
    }
    for got in &cycles {
        if *got != w.cycle {
            return Err(Broke::Oracle(format!("a cycle named {got:?}; the planted one is {:?}", w.cycle)));
        }
    }
    if reaches_cycle(&w) {
        if cycles.is_empty() {
            return Err(Broke::Oracle(format!("the planted cycle {:?} was reached and not reported", w.cycle)));
        }
    } else if failures > 0 || !cycles.is_empty() {
        return Err(Broke::Oracle(format!("{failures} asks failed with no cycle reached")));
    }
    let made: u32 = starts.borrow().iter().sum();
    tape.note(format_args!("{bodies} bodies, {} facts, {made} made, cycle {:?}", w.deps.len(), w.cycle));
    Ok(())
}

/// The invariant: late needs are made once under any interleaving of askers, no running fact is read without waiting, every cycle is named, and no parked body restarts from the top.
#[test]
fn late_needs_are_made_once_and_nothing_starts_over() {
    blaze_sim::check(Config::cases(600), |tape| case(tape, P::Clean));
}

#[test]
fn the_oracle_catches_a_done_fact_made_again() {
    blaze_sim::control(Config::cases(600), P::Clean, P::Ask(Plant::MakeWhatIsDone), "made twice", case);
}

#[test]
fn the_oracle_catches_a_running_fact_read_without_a_wait() {
    blaze_sim::control(Config::cases(600), P::Clean, P::Ask(Plant::ReadRunning), "before it was published", case);
}

#[test]
fn the_oracle_catches_a_cycle_named_by_one_need() {
    blaze_sim::control(Config::cases(600), P::Clean, P::Ask(Plant::CycleNamesOne), "a cycle named", case);
}

#[test]
fn the_oracle_catches_the_restart_path() {
    blaze_sim::control(Config::cases(600), P::Clean, P::Restart, "entered more than once", case);
}

/// A chain deeper than the stack's bound is refused naming the need past it, and every claim the
/// asker held is ended (an observer reads each as refused, none left running). Its control: the
/// same chain under a bound it fits is made.
fn chain_of_four(most: usize) -> (Result<Asked, String>, SVec<Option<u32>>) {
    let w = World { deps: vec![vec![1], vec![2], vec![3], vec![]], early: vec![vec![]], late: vec![vec![0]], cycle: vec![] };
    let alloc = blaze_own::testing::heap().alloc();
    let board = Board::new_in(16, 2, alloc).expect("a board of 16 slots");
    let starts = RefCell::new(vec![0u32; 4]);
    let broke = RefCell::new(None);
    let mut maker = SimMaker { world: &w, starts: &starts, broke: &broke };
    let mut asker: Asker<'_, Alloc<'static>, SimFrame> = Asker::new(AskerOpts { board: &board, me: TaskId(0), alloc, most, .. });
    let out = asker.ask(&mut maker, need(0), 100, &mut |_| {}).map_err(|e| format!("{e}"));
    let words = (0..4).map(|f| published(&board, TaskId(1), &need(f))).collect();
    (out, words)
}

#[test]
fn a_chain_past_the_bound_is_refused_and_ends_its_claims() {
    let (out, words) = chain_of_four(2);
    let why = out.expect_err("a chain of four under a bound of two");
    assert!(why.contains("2 deep"), "{why}");
    let refused = Given::Failed(ASK_REFUSED).word();
    assert_eq!(words, vec![Some(refused), Some(refused), Some(refused), None], "every claim held ended refused; fact 3 never claimed");
}

#[test]
fn the_same_chain_under_a_bound_it_fits_is_made() {
    let (out, words) = chain_of_four(8);
    assert_eq!(out, Ok(Asked::Ready(Given::Made(value(0)))));
    assert_eq!(words, (0..4).map(|f| Some(value(f))).collect::<SVec<_>>());
}
