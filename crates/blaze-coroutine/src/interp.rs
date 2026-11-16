//! The simulation's two runs (coroutines.md, "Proof by simulation"). One executor gives the
//! statements their meaning; two drivers use it.
//!
//! - [`run_direct`] runs a body as written: one frame holds every local and stays alive across
//!   suspensions. `Suspend` hands back "yielded at k"; the driver goes on at its resume block,
//!   or cancels by running its cancel path.
//! - [`run_machine`] drives a [`Machine`] as its user will: between polls only the state's
//!   slots and the tag exist. Every poll gets a new frame, enters by the tag, and leaves at a
//!   `Park` or a `Return`. Cancelling runs the drop glue's entry for the tag, in a new frame.
//!
//! A reference is an address: a slot, or a local of one poll's frame. Reading through an
//! address of an earlier poll's frame is a fault: that frame is gone. So is reading or dropping
//! a local that holds nothing.

use core::alloc::Allocator;
use core::fmt;
use std::vec::Vec;

use crate::ir::{BlockId, Body, LocalId, Operand, PointId, Stmt, Term};
use crate::layout::{Home, Tag};
use crate::transform::{DropEntry, Entry, Machine};

/// Steps one run may take: a bound, so a broken machine cannot hang the test (rule 18).
const FUEL: u32 = 20_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Addr {
    State(u32),
    Frame { serial: u32, local: u32 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Val {
    Int(i64),
    Ref(Addr),
}

/// What a run can be seen to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// A call, with its arguments (a reference is shown as what it points at).
    Call { func: u32, args: Vec<i64> },
    /// A poll of the future holding this value.
    Poll(i64),
    /// A destructor run: the local and the value it held.
    Drop { local: u32, value: i64 },
    /// `Poll::Pending` returned at this point.
    Yield(u32),
    /// `Poll::Ready` with this value.
    Return(i64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    Uninit(LocalId),
    DropUninit(LocalId),
    Dangling(LocalId),
    NotAnInt(LocalId),
    NotARef(LocalId),
    Steps,
    Shape(&'static str),
    ResumedAfterCompletion,
    NoSuchTag(u32),
    UpvarInFrame(LocalId),
    Args,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Fault::Uninit(l) => write!(f, "read of an uninitialised local {l}"),
            Fault::DropUninit(l) => write!(f, "drop of an uninitialised local {l}"),
            Fault::Dangling(l) => write!(f, "dangling reference in {l}: it points into the frame of an earlier poll"),
            Fault::NotAnInt(l) => write!(f, "{l} does not hold an integer"),
            Fault::NotARef(l) => write!(f, "{l} does not hold a reference"),
            Fault::Steps => write!(f, "more than {FUEL} steps"),
            Fault::Shape(what) => write!(f, "{what}"),
            Fault::ResumedAfterCompletion => write!(f, "resumed after completion"),
            Fault::NoSuchTag(t) => write!(f, "no entry for tag {t}"),
            Fault::UpvarInFrame(l) => write!(f, "upvar {l} has no slot"),
            Fault::Args => write!(f, "the arguments are not one per upvar"),
        }
    }
}

/// What a call returns: `100 * func + the sum of its arguments`.
pub fn call_result(func: u32, args: &[i64]) -> i64 {
    let mut acc = i64::from(func).wrapping_mul(100);
    for &a in args {
        acc = acc.wrapping_add(a);
    }
    acc
}

/// Where the executor stopped.
enum Stop {
    Suspended { point: PointId, resume: BlockId, drop: BlockId },
    Parked(PointId),
    Returned(i64),
    Cancelled,
}

struct Run<'a> {
    home: Vec<Home>,
    state: Vec<Option<Val>>,
    frame: Vec<Option<Val>>,
    serial: u32,
    events: Vec<Event>,
    answers: &'a [bool],
    polls: usize,
    fuel: u32,
}

impl Run<'_> {
    fn new_frame(&mut self) {
        self.serial += 1;
        for cell in self.frame.iter_mut() {
            *cell = None;
        }
    }

    fn addr(&self, l: LocalId) -> Addr {
        match self.home[l.index()] {
            Home::State(s) => Addr::State(s.0),
            Home::Frame => Addr::Frame { serial: self.serial, local: l.0 },
        }
    }

    fn get(&self, l: LocalId) -> Option<Val> {
        match self.home[l.index()] {
            Home::State(s) => self.state[s.index()],
            Home::Frame => self.frame[l.index()],
        }
    }

    fn set(&mut self, l: LocalId, v: Option<Val>) {
        match self.home[l.index()] {
            Home::State(s) => self.state[s.index()] = v,
            Home::Frame => self.frame[l.index()] = v,
        }
    }

    fn read(&self, l: LocalId) -> Result<Val, Fault> {
        self.get(l).ok_or(Fault::Uninit(l))
    }

    fn int(&self, l: LocalId) -> Result<i64, Fault> {
        match self.read(l)? {
            Val::Int(i) => Ok(i),
            Val::Ref(_) => Err(Fault::NotAnInt(l)),
        }
    }

    fn reference(&self, l: LocalId) -> Result<Addr, Fault> {
        match self.read(l)? {
            Val::Ref(a) => Ok(a),
            Val::Int(_) => Err(Fault::NotARef(l)),
        }
    }

    /// The cell an address names; `via` is the local holding the reference.
    fn cell(&mut self, a: Addr, via: LocalId) -> Result<&mut Option<Val>, Fault> {
        match a {
            Addr::State(s) => Ok(&mut self.state[s as usize]),
            Addr::Frame { serial, local } => {
                if serial != self.serial {
                    return Err(Fault::Dangling(via));
                }
                Ok(&mut self.frame[local as usize])
            }
        }
    }

    fn load(&mut self, a: Addr, via: LocalId) -> Result<Val, Fault> {
        let cell = *self.cell(a, via)?;
        cell.ok_or(Fault::Uninit(via))
    }

    fn operand(&mut self, o: Operand) -> Result<Val, Fault> {
        match o {
            Operand::Const(c) => Ok(Val::Int(c)),
            Operand::Copy(l) => self.read(l),
            Operand::Move(l) => {
                let v = self.read(l)?;
                self.set(l, None);
                Ok(v)
            }
        }
    }

    /// An operand as the integer a call sees: a reference shows what it points at.
    fn seen(&mut self, o: Operand) -> Result<i64, Fault> {
        let at = match o {
            Operand::Copy(l) | Operand::Move(l) => l,
            Operand::Const(c) => return Ok(c),
        };
        match self.operand(o)? {
            Val::Int(i) => Ok(i),
            Val::Ref(a) => match self.load(a, at)? {
                Val::Int(i) => Ok(i),
                Val::Ref(_) => Err(Fault::NotAnInt(at)),
            },
        }
    }

    fn tick(&mut self) -> Result<(), Fault> {
        if self.fuel == 0 {
            return Err(Fault::Steps);
        }
        self.fuel -= 1;
        Ok(())
    }

    fn drop_local(&mut self, l: LocalId) -> Result<(), Fault> {
        match self.get(l) {
            Some(Val::Int(value)) => {
                self.events.push(Event::Drop { local: l.0, value });
                self.set(l, None);
                Ok(())
            }
            Some(Val::Ref(_)) => Err(Fault::NotAnInt(l)),
            None => Err(Fault::DropUninit(l)),
        }
    }

    fn stmt<A: Allocator>(&mut self, body: &Body<A>, s: Stmt) -> Result<(), Fault> {
        match s {
            Stmt::Use { dest, src } => {
                let v = self.operand(src)?;
                self.set(dest, Some(v));
            }
            Stmt::Step { dest, src, by } => {
                let i = self.int(src)?;
                self.set(dest, Some(Val::Int(i.wrapping_add(by))));
            }
            Stmt::Borrow { dest, of } => {
                let a = self.addr(of);
                self.set(dest, Some(Val::Ref(a)));
            }
            Stmt::Load { dest, via } => {
                let a = self.reference(via)?;
                let v = self.load(a, via)?;
                self.set(dest, Some(v));
            }
            Stmt::Store { via, src } => {
                let a = self.reference(via)?;
                let v = self.operand(src)?;
                *self.cell(a, via)? = Some(v);
            }
            Stmt::Call { dest, func, args } => {
                let mut seen = Vec::new();
                for &o in body.args(args) {
                    seen.push(self.seen(o)?);
                }
                let result = call_result(func, &seen);
                self.events.push(Event::Call { func, args: seen });
                self.set(dest, Some(Val::Int(result)));
            }
            Stmt::Poll { dest, fut } => {
                let held = self.int(fut)?;
                self.events.push(Event::Poll(held));
                let pending = self.answers.get(self.polls).copied().unwrap_or(false);
                self.polls += 1;
                self.set(dest, Some(Val::Int(if pending { 0 } else { 1 })));
            }
            Stmt::Drop(l) => self.drop_local(l)?,
            Stmt::StorageDead(l) => self.set(l, None),
        }
        Ok(())
    }

    /// Runs from `from` to the next terminator that leaves the body.
    fn exec<A: Allocator>(&mut self, body: &Body<A>, from: BlockId) -> Result<Stop, Fault> {
        let mut b = from;
        loop {
            for &s in body.stmts_of(b) {
                self.tick()?;
                self.stmt(body, s)?;
            }
            self.tick()?;
            match body.term(b) {
                Term::Goto(t) => b = t,
                Term::Switch { on, targets } => {
                    let v = self.int(on)?;
                    let targets = body.targets_of(targets);
                    let i = usize::try_from(v.max(0)).unwrap_or(usize::MAX).min(targets.len() - 1);
                    b = targets[i];
                }
                Term::Return(o) => {
                    return match self.operand(o)? {
                        Val::Int(i) => Ok(Stop::Returned(i)),
                        Val::Ref(_) => Err(Fault::Shape("a reference was returned")),
                    };
                }
                Term::Suspend { point, resume, drop } => return Ok(Stop::Suspended { point, resume, drop }),
                Term::Cancelled => return Ok(Stop::Cancelled),
                Term::Park(p) => return Ok(Stop::Parked(p)),
                Term::Unreachable => return Err(Fault::Shape("`Unreachable` was reached")),
            }
        }
    }
}

/// Runs a body as written. `answers[i]` is whether the i-th poll is Pending (past the end:
/// Ready). `cancel_at`: drop the coroutine once it has yielded that many times (0: before its
/// first poll); a run that returns first is not cancelled.
pub fn run_direct<A: Allocator>(
    body: &Body<A>,
    args: &[i64],
    answers: &[bool],
    cancel_at: Option<u32>,
) -> Result<Vec<Event>, Fault> {
    let n = body.locals.len();
    let upvars = body.upvars as usize;
    if args.len() != upvars {
        return Err(Fault::Args);
    }
    let mut run = Run {
        home: std::vec![Home::Frame; n],
        state: Vec::new(),
        frame: std::vec![None; n],
        serial: 0,
        events: Vec::new(),
        answers,
        polls: 0,
        fuel: FUEL,
    };
    for (i, &a) in args.iter().enumerate() {
        run.frame[i] = Some(Val::Int(a));
    }
    if cancel_at == Some(0) {
        // Never polled: the upvars, in declaration order (decision D5).
        for i in 0..upvars {
            if body.locals[i].needs_drop {
                run.drop_local(LocalId(i as u32))?;
            }
        }
        return Ok(run.events);
    }
    let mut yields = 0u32;
    let mut from = body.entry();
    loop {
        match run.exec(body, from)? {
            Stop::Suspended { point, resume, drop } => {
                run.events.push(Event::Yield(point.0));
                yields += 1;
                if cancel_at == Some(yields) {
                    return match run.exec(body, drop)? {
                        Stop::Cancelled => Ok(run.events),
                        _ => Err(Fault::Shape("a cancel path did not end in `Cancelled`")),
                    };
                }
                from = resume;
            }
            Stop::Returned(v) => {
                run.events.push(Event::Return(v));
                return Ok(run.events);
            }
            Stop::Parked(_) => return Err(Fault::Shape("`Park` in a direct run")),
            Stop::Cancelled => return Err(Fault::Shape("`Cancelled` on a resume path")),
        }
    }
}

/// Drives a machine: poll until it returns, or drop it at `cancel_at` (as [`run_direct`]).
pub fn run_machine<A: Allocator>(
    machine: &Machine<A>,
    args: &[i64],
    answers: &[bool],
    cancel_at: Option<u32>,
) -> Result<Vec<Event>, Fault> {
    let body = machine.body();
    let layout = machine.layout();
    let upvars = body.upvars as usize;
    if args.len() != upvars {
        return Err(Fault::Args);
    }
    let mut run = Run {
        home: layout.home.iter().copied().collect(),
        state: std::vec![None; layout.slots.len()],
        frame: std::vec![None; layout.home.len()],
        serial: 0,
        events: Vec::new(),
        answers,
        polls: 0,
        fuel: FUEL,
    };
    for (i, &a) in args.iter().enumerate() {
        match run.home[i] {
            Home::State(s) => run.state[s.index()] = Some(Val::Int(a)),
            Home::Frame => return Err(Fault::UpvarInFrame(LocalId(i as u32))),
        }
    }
    let mut tag = Tag::UNRESUMED;
    let mut yields = 0u32;
    loop {
        if cancel_at == Some(yields) {
            match machine.drop_entry(tag) {
                DropEntry::Run(b) => {
                    run.new_frame();
                    match run.exec(body, b)? {
                        Stop::Cancelled => {}
                        _ => return Err(Fault::Shape("the drop glue did not end in `Cancelled`")),
                    }
                }
                DropEntry::Nothing => {}
                DropEntry::NoSuchTag => return Err(Fault::NoSuchTag(tag.0)),
            }
            return Ok(run.events);
        }
        let from = match machine.entry(tag) {
            Entry::Run(b) => b,
            Entry::Completed => return Err(Fault::ResumedAfterCompletion),
            Entry::Panicked => return Err(Fault::Shape("resumed after panicking")),
            Entry::NoSuchTag => return Err(Fault::NoSuchTag(tag.0)),
        };
        run.new_frame();
        match run.exec(body, from)? {
            Stop::Parked(p) => {
                tag = Tag::point(p);
                run.events.push(Event::Yield(p.0));
                yields += 1;
            }
            Stop::Returned(v) => {
                tag = Tag::RETURNED;
                run.events.push(Event::Return(v));
                // Dropped after it finished: the glue has nothing to do.
                return match machine.drop_entry(tag) {
                    DropEntry::Nothing => Ok(run.events),
                    _ => Err(Fault::Shape("the drop glue of a finished coroutine is not empty")),
                };
            }
            Stop::Suspended { .. } => return Err(Fault::Shape("`Suspend` in a machine")),
            Stop::Cancelled => return Err(Fault::Shape("`Cancelled` reached by a poll")),
        }
    }
}
