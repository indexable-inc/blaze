//! The simulation (an oracle and five planted bugs) and ten hand-written bodies with their
//! traces written out (coroutines.md, "Proof by simulation"). The oracle is the direct run of
//! the body as written; the subject is the machine the transform makes of it.

use std::alloc::Global;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_sim::{Broke, Config, Tape, check, control};

use crate::Plant;
use crate::interp::{Event, run_direct, run_machine};
use crate::ir::{BlockId, Body, BodyBuilder, IrError, LocalId, Operand, PointId, Stmt, Term};
use crate::layout::{Home, Tag};
use crate::transform::{DropEntry, Entry, Machine, TransformError, transform, transform_planted};

/// The words every finding of the oracle starts with.
const DIVERGED: &str = "the state machine diverged from the direct run";

// ---------------------------------------------------------------- random bodies from a tape

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Init {
    Yes,
    No,
    /// Moved on some paths: dropped through its flag.
    Maybe,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Int,
    /// A value with a destructor, and the flag local that says whether it is initialised.
    Guard { flag: LocalId },
    Ref,
    /// The future an await is polling: in scope only while its cancel path is written.
    Fut,
}

#[derive(Clone, Copy, Debug)]
struct Var {
    local: LocalId,
    kind: Kind,
    init: Init,
    borrowed: bool,
}

enum What {
    Root,
    Then { before: Vec<Init>, otherwise: BlockId, join: BlockId },
    Else { then: Vec<Init>, join: BlockId },
    Loop { head: BlockId, exit: BlockId, floor_before: usize },
}

/// An open construct: `left` more statements, then it closes. Its scope's variables are
/// `vars[start..]`.
struct Open {
    left: u32,
    start: usize,
    what: What,
}

/// Writes a random body the way the adapter will: scopes whose variables are dropped in
/// reverse declaration order when control leaves them (the Reference, Destructors), a flag
/// where a move happens on some paths only, `StorageDead` at a scope's end, and for every
/// await a cancel path that is the exit of every open scope. No recursion: a stack of open
/// constructs.
struct Gen<'t> {
    t: &'t Tape,
    b: BodyBuilder<Global>,
    vars: Vec<Var>,
    stack: Vec<Open>,
    /// Guards below this index were declared outside the innermost open loop: a move of one
    /// would be a use after move on the next iteration, so they are not moved.
    floor: usize,
    ret: LocalId,
    budget: u32,
}

impl Gen<'_> {
    fn pick(&self, what: &'static str, ok: impl Fn(usize, &Var) -> bool) -> Option<usize> {
        let found: Vec<usize> = (0..self.vars.len()).filter(|&i| ok(i, &self.vars[i])).collect();
        if found.is_empty() { None } else { Some(found[self.t.index(found.len(), what)]) }
    }

    fn small(&self, what: &'static str) -> i64 {
        self.t.below(3, what) as i64
    }

    fn let_int(&mut self) -> usize {
        let l = self.b.local(false);
        let value = self.small("int");
        self.b.push(Stmt::Use { dest: l, src: Operand::Const(value) });
        self.vars.push(Var { local: l, kind: Kind::Int, init: Init::Yes, borrowed: false });
        self.vars.len() - 1
    }

    fn new_guard(&mut self, src: Operand) {
        let l = self.b.local(true);
        let flag = self.b.local(false);
        self.b.push(Stmt::Use { dest: l, src });
        self.b.push(Stmt::Use { dest: flag, src: Operand::Const(1) });
        self.vars.push(Var { local: l, kind: Kind::Guard { flag }, init: Init::Yes, borrowed: false });
    }

    fn let_guard(&mut self) {
        let value = 10 + self.vars.len() as i64;
        self.new_guard(Operand::Const(value));
    }

    fn readable(v: &Var) -> bool {
        match v.kind {
            Kind::Int | Kind::Ref => true,
            Kind::Guard { .. } => v.init == Init::Yes,
            Kind::Fut => false,
        }
    }

    fn call(&mut self) {
        let mut args = Vec::new();
        for _ in 0..self.t.below(3, "call args") {
            match self.pick("call arg", |_, v| Gen::readable(v)) {
                Some(i) if self.t.coin("arg is a local") => args.push(Operand::Copy(self.vars[i].local)),
                _ => args.push(Operand::Const(self.small("const arg"))),
            }
        }
        let func = 1 + self.t.below(5, "func") as u32;
        let dest = self.b.local(false);
        self.b.call(dest, func, &args);
        self.vars.push(Var { local: dest, kind: Kind::Int, init: Init::Yes, borrowed: false });
    }

    fn borrow(&mut self) {
        let ok = |_: usize, v: &Var| match v.kind {
            Kind::Int => true,
            Kind::Guard { .. } => v.init == Init::Yes,
            Kind::Ref | Kind::Fut => false,
        };
        let Some(i) = self.pick("borrow of", ok) else {
            self.let_int();
            return;
        };
        let r = self.b.local(false);
        self.b.push(Stmt::Borrow { dest: r, of: self.vars[i].local });
        self.vars[i].borrowed = true;
        self.vars.push(Var { local: r, kind: Kind::Ref, init: Init::Yes, borrowed: false });
    }

    fn load(&mut self) {
        let Some(i) = self.pick("load via", |_, v| v.kind == Kind::Ref) else {
            self.let_int();
            return;
        };
        let v = self.b.local(false);
        self.b.push(Stmt::Load { dest: v, via: self.vars[i].local });
        self.vars.push(Var { local: v, kind: Kind::Int, init: Init::Yes, borrowed: false });
    }

    fn store(&mut self) {
        let Some(i) = self.pick("store via", |_, v| v.kind == Kind::Ref) else {
            self.let_int();
            return;
        };
        let value = self.small("stored");
        self.b.push(Stmt::Store { via: self.vars[i].local, src: Operand::Const(value) });
    }

    /// A move out of a guard: into a call, or into a new guard. A borrowed guard is never
    /// moved (the Reference, Moved and copied types: only "Variables which are not currently
    /// borrowed" may be moved out of).
    fn move_guard(&mut self) {
        let floor = self.floor;
        let ok = |i: usize, v: &Var| {
            i >= floor && matches!(v.kind, Kind::Guard { .. }) && v.init == Init::Yes && !v.borrowed
        };
        let Some(i) = self.pick("moved", ok) else {
            self.let_guard();
            return;
        };
        let Var { local, kind, .. } = self.vars[i];
        let Kind::Guard { flag } = kind else { return };
        self.vars[i].init = Init::No;
        if self.t.coin("moved into a call") {
            let dest = self.b.local(false);
            self.b.call(dest, 9, &[Operand::Move(local)]);
            self.b.push(Stmt::Use { dest: flag, src: Operand::Const(0) });
            self.vars.push(Var { local: dest, kind: Kind::Int, init: Init::Yes, borrowed: false });
        } else {
            self.new_guard(Operand::Move(local));
            self.b.push(Stmt::Use { dest: flag, src: Operand::Const(0) });
        }
    }

    fn condition(&mut self) -> LocalId {
        let i = match self.pick("condition", |_, v| v.kind == Kind::Int) {
            Some(i) if self.t.coin("old condition") => i,
            _ => self.let_int(),
        };
        self.vars[i].local
    }

    fn open_if(&mut self) {
        let cond = self.condition();
        let (then, otherwise, join) = (self.b.block(), self.b.block(), self.b.block());
        self.b.switch(cond, &[otherwise, then]);
        self.b.begin(then);
        let before = self.vars.iter().map(|v| v.init).collect();
        let left = self.t.below(4, "then statements") as u32;
        self.stack.push(Open { left, start: self.vars.len(), what: What::Then { before, otherwise, join } });
    }

    fn open_loop(&mut self) {
        let counter = self.b.local(false);
        let times = 1 + self.small("loop times");
        self.b.push(Stmt::Use { dest: counter, src: Operand::Const(times) });
        let (head, body, exit) = (self.b.block(), self.b.block(), self.b.block());
        self.b.end(Term::Goto(head));
        self.b.begin(head);
        self.b.switch(counter, &[exit, body]);
        self.b.begin(body);
        self.b.push(Stmt::Step { dest: counter, src: counter, by: -1 });
        let left = 1 + self.t.below(4, "loop statements") as u32;
        let what = What::Loop { head, exit, floor_before: self.floor };
        self.stack.push(Open { left, start: self.vars.len(), what });
        self.floor = self.vars.len();
    }

    /// `e.await`, as the Reference's steps: make the future, then loop { poll; Ready leaves;
    /// Pending suspends and comes back to the poll }. The cancel path drops the future, then
    /// leaves every open scope.
    fn await_point(&mut self) {
        let fut = self.b.local(true);
        let answer = self.b.local(false);
        let point = self.b.point();
        let arg = self.small("await arg");
        self.b.call(fut, 7, &[Operand::Const(arg)]);
        let (head, pending, cancel, ready) = (self.b.block(), self.b.block(), self.b.block(), self.b.block());
        self.b.end(Term::Goto(head));
        self.b.begin(head);
        self.b.push(Stmt::Poll { dest: answer, fut });
        self.b.switch(answer, &[pending, ready]);
        self.b.begin(pending);
        self.b.end(Term::Suspend { point, resume: head, drop: cancel });
        self.b.begin(cancel);
        self.vars.push(Var { local: fut, kind: Kind::Fut, init: Init::Yes, borrowed: false });
        self.exit_drops();
        self.vars.pop();
        self.b.end(Term::Cancelled);
        self.b.begin(ready);
        self.b.push(Stmt::Drop(fut));
        self.b.push(Stmt::StorageDead(fut));
    }

    fn early_return(&mut self) {
        let Some(i) = self.pick("return condition", |_, v| v.kind == Kind::Int) else {
            self.let_int();
            return;
        };
        let cond = self.vars[i].local;
        let (out, on) = (self.b.block(), self.b.block());
        self.b.switch(cond, &[on, out]);
        self.b.begin(out);
        let value = 1 + self.small("returned");
        self.b.push(Stmt::Use { dest: self.ret, src: Operand::Const(value) });
        self.exit_drops();
        self.b.end(Term::Return(Operand::Copy(self.ret)));
        self.b.begin(on);
    }

    /// Drops variable `i` if it is initialised here: at once, never, or by its flag.
    fn drop_var(&mut self, i: usize) {
        let v = self.vars[i];
        match (v.kind, v.init) {
            (Kind::Guard { .. } | Kind::Fut, Init::Yes) => self.b.push(Stmt::Drop(v.local)),
            (Kind::Guard { flag }, Init::Maybe) => {
                let (dropping, after) = (self.b.block(), self.b.block());
                self.b.switch(flag, &[after, dropping]);
                self.b.begin(dropping);
                self.b.push(Stmt::Drop(v.local));
                self.b.end(Term::Goto(after));
                self.b.begin(after);
            }
            _ => {}
        }
    }

    /// The drops of an exit that leaves every scope (a `return`, a cancel): every variable,
    /// last declared first.
    fn exit_drops(&mut self) {
        for i in (0..self.vars.len()).rev() {
            self.drop_var(i);
        }
    }

    /// A scope's ordinary end: its variables dropped last declared first, then their storage
    /// ended (not at the root: the function ends there).
    fn close_scope(&mut self, start: usize, dead: bool) {
        for i in (start..self.vars.len()).rev() {
            self.drop_var(i);
        }
        if dead {
            for i in (start..self.vars.len()).rev() {
                let v = self.vars[i];
                self.b.push(Stmt::StorageDead(v.local));
                if let Kind::Guard { flag } = v.kind {
                    self.b.push(Stmt::StorageDead(flag));
                }
            }
        }
        self.vars.truncate(start);
    }

    fn close(&mut self) {
        let Some(top) = self.stack.pop() else { return };
        match top.what {
            What::Root => {
                if let Some(i) = self.pick("result", |_, v| v.kind == Kind::Int) {
                    self.b.push(Stmt::Use { dest: self.ret, src: Operand::Copy(self.vars[i].local) });
                }
                self.close_scope(0, false);
                self.b.end(Term::Return(Operand::Copy(self.ret)));
            }
            What::Then { before, otherwise, join } => {
                self.close_scope(top.start, true);
                let then: Vec<Init> = self.vars.iter().map(|v| v.init).collect();
                for (v, &was) in self.vars.iter_mut().zip(before.iter()) {
                    v.init = was;
                }
                self.b.end(Term::Goto(join));
                self.b.begin(otherwise);
                let left = self.t.below(4, "else statements") as u32;
                self.stack.push(Open { left, start: top.start, what: What::Else { then, join } });
            }
            What::Else { then, join } => {
                self.close_scope(top.start, true);
                for (v, &other) in self.vars.iter_mut().zip(then.iter()) {
                    if v.init != other {
                        v.init = Init::Maybe;
                    }
                }
                self.b.end(Term::Goto(join));
                self.b.begin(join);
            }
            What::Loop { head, exit, floor_before } => {
                self.close_scope(top.start, true);
                self.floor = floor_before;
                self.b.end(Term::Goto(head));
                self.b.begin(exit);
            }
        }
    }

    fn statement(&mut self) {
        let nest = self.stack.len() < 4;
        match self.t.weighted(&[3, 3, 3, 2, 2, 2, 2, 2, 2, 4, 1], "statement") {
            0 => {
                self.let_int();
            }
            1 => self.let_guard(),
            2 => self.call(),
            3 => self.borrow(),
            4 => self.load(),
            5 => self.store(),
            6 => self.move_guard(),
            7 if nest => self.open_if(),
            8 if nest => self.open_loop(),
            9 => self.await_point(),
            10 => self.early_return(),
            _ => self.call(),
        }
    }
}

/// A random body and its arguments (one per upvar).
fn random_body(t: &Tape) -> Result<(Body<Global>, Vec<i64>), IrError> {
    let mut b = BodyBuilder::new_in(Global);
    let mut upvars = Vec::new();
    let mut args = Vec::new();
    for _ in 0..t.below(3, "upvars") {
        let guard = t.coin("upvar is a guard");
        upvars.push((b.upvar(guard), guard));
        args.push(t.below(4, "argument") as i64 - 1);
    }
    let ret = b.local(false);
    let entry = b.block();
    b.begin(entry);
    b.push(Stmt::Use { dest: ret, src: Operand::Const(0) });
    let mut vars = Vec::new();
    for (local, guard) in upvars {
        let kind = if guard {
            let flag = b.local(false);
            b.push(Stmt::Use { dest: flag, src: Operand::Const(1) });
            Kind::Guard { flag }
        } else {
            Kind::Int
        };
        vars.push(Var { local, kind, init: Init::Yes, borrowed: false });
    }
    let mut g = Gen { t, b, vars, stack: Vec::new(), floor: 0, ret, budget: 24 };
    g.stack.push(Open { left: 3 + t.below(8, "root statements") as u32, start: 0, what: What::Root });
    loop {
        let Some(top) = g.stack.last_mut() else { break };
        if top.left == 0 || g.budget == 0 {
            g.close();
            continue;
        }
        top.left -= 1;
        g.budget -= 1;
        g.statement();
    }
    Ok((g.b.finish()?, args))
}

// ---------------------------------------------------------------- the oracle

fn text(body: &Body<Global>) -> String {
    let mut s = String::new();
    let _ = body.print(&mut s);
    s
}

/// Runs both and compares, for one schedule and one cancel point. The direct run's trace.
fn compare(
    body: &Body<Global>,
    machine: &Machine<Global>,
    args: &[i64],
    answers: &[bool],
    cancel: Option<u32>,
) -> Result<Vec<Event>, Broke> {
    let want = run_direct(body, args, answers, cancel).map_err(|f| {
        Broke::Harness(format!("the direct run: {f} (cancel {cancel:?}, answers {answers:?}, args {args:?})\n{}", text(body)))
    })?;
    let got = run_machine(machine, args, answers, cancel).map_err(|f| {
        Broke::Oracle(format!("{DIVERGED}: {f} (cancel {cancel:?}, answers {answers:?}, args {args:?})\n{}", text(body)))
    })?;
    if got != want {
        return Err(Broke::Oracle(format!(
            "{DIVERGED}: the traces differ (cancel {cancel:?}, answers {answers:?}, args {args:?}): direct {want:?}, machine {got:?}\n{}",
            text(body)
        )));
    }
    Ok(want)
}

fn case(t: &Tape, plant: Plant) -> Result<(), Broke> {
    let (body, args) = random_body(t).map_err(|e| Broke::Harness(format!("the generator: {e}")))?;
    let mut answers = Vec::new();
    for _ in 0..t.below(7, "answers") {
        answers.push(t.coin("pending"));
    }
    let machine = transform_planted(&body, plant, Global)
        .map_err(|e| Broke::Harness(format!("the transform refused: {e}\n{}", text(&body))))?;
    let whole = compare(&body, &machine, &args, &answers, None)?;
    let yields = whole.iter().filter(|e| matches!(e, Event::Yield(_))).count() as u32;
    // Before the first poll, after each yield, and one past: after the return.
    for cancel in 0..=yields + 1 {
        compare(&body, &machine, &args, &answers, Some(cancel))?;
    }
    Ok(())
}

/// The machine does what the body does, for every schedule and every cancel. The Reference,
/// Await expressions, step 5: "If the call to poll returns Poll::Pending, then the future
/// returns Poll::Pending, suspending its state so that, when the surrounding async context is
/// re-polled, execution returns to step 3"; Destructors: "When control flow leaves a drop scope
/// all variables associated to that scope are dropped in reverse order of declaration".
#[test]
fn a_machine_equals_its_body_for_every_schedule_and_cancel() {
    check(Config::cases(600), |t| case(t, Plant::None));
}

/// Planted: a local live across a suspension gets no slot. Rule: every local read after a
/// resume before it is written is saved ("suspending its state": the state is what resuming
/// needs).
#[test]
fn control_a_live_local_left_out_of_the_state() {
    control(Config::cases(600), Plant::None, Plant::LiveLeftOut, "read of an uninitialised local", case);
}

/// Planted: a borrowed local is not kept. Rule (coroutines.md, design 3): a local that may be
/// borrowed at a suspension is saved, since a saved reference may point at it.
#[test]
fn control_borrows_not_counted() {
    control(Config::cases(600), Plant::None, Plant::BorrowsNotCounted, "dangling reference", case);
}

/// Planted: cancelling drops every saved local. Rule: dropping a coroutine parked at a point
/// drops exactly what is initialised there (the Reference, Moved and copied types: "After
/// moving out of a place expression that evaluates to a local variable, the location is
/// deinitialized").
#[test]
fn control_cancel_drops_an_uninitialised_local() {
    control(Config::cases(600), Plant::None, Plant::CancelDropsAll, "drop of an uninitialised local", case);
}

/// Planted: the tag of point k resumes at point k + 1. Rule: "when the surrounding async
/// context is re-polled, execution returns to step 3" of the SAME await.
#[test]
fn control_resuming_at_the_next_point() {
    control(Config::cases(600), Plant::None, Plant::ResumeNext, DIVERGED, case);
}

/// Planted: a borrowed saved local is copied out at a suspension and back at a resume, so it
/// comes back at another address. Rule: the state is self-referential and does not move once
/// polled (`fn poll(self: Pin<&mut Self>, ..)`); a saved local's home never changes.
#[test]
fn control_a_saved_local_moved_under_a_reference() {
    control(Config::cases(600), Plant::None, Plant::SavedByCopy, "dangling reference", case);
}

// ---------------------------------------------------------------- hand-written bodies

struct Await {
    fut: LocalId,
    point: PointId,
    cancel: BlockId,
    ready: BlockId,
}

/// `func(arg).await` up to its suspension: ends the open block; the caller writes the cancel
/// path (block `cancel`) and goes on in block `ready`. Neither drops the future yet.
fn await_open(b: &mut BodyBuilder<Global>, func: u32, arg: Operand) -> Await {
    let fut = b.local(true);
    let answer = b.local(false);
    let point = b.point();
    let (head, pending, cancel, ready) = (b.block(), b.block(), b.block(), b.block());
    b.call(fut, func, &[arg]);
    b.end(Term::Goto(head));
    b.begin(head);
    b.push(Stmt::Poll { dest: answer, fut });
    b.switch(answer, &[pending, ready]);
    b.begin(pending);
    b.end(Term::Suspend { point, resume: head, drop: cancel });
    Await { fut, point, cancel, ready }
}

/// `func(arg).await` whole: the cancel path drops the future, then runs `cancel`; block
/// `ready` is left open after the future's drop.
fn await_whole(b: &mut BodyBuilder<Global>, func: u32, arg: Operand, cancel: &[Stmt]) -> Await {
    let a = await_open(b, func, arg);
    b.begin(a.cancel);
    b.push(Stmt::Drop(a.fut));
    for &s in cancel {
        b.push(s);
    }
    b.end(Term::Cancelled);
    b.begin(a.ready);
    b.push(Stmt::Drop(a.fut));
    a
}

fn called(func: u32, args: &[i64]) -> Event {
    Event::Call { func, args: args.to_vec() }
}

fn dropped(local: LocalId, value: i64) -> Event {
    Event::Drop { local: local.0, value }
}

/// Both runs give exactly `want`.
#[track_caller]
fn both(body: &Body<Global>, args: &[i64], answers: &[bool], cancel: Option<u32>, want: &[Event]) {
    let machine = transform(body, Global).expect("the transform");
    let direct = run_direct(body, args, answers, cancel).expect("the direct run");
    assert_eq!(direct.as_slice(), want, "the direct run\n{}", text(body));
    let driven = run_machine(&machine, args, answers, cancel).expect("the machine");
    assert_eq!(driven.as_slice(), want, "the machine\n{}", text(body));
}

/// `async fn(g) { f7(1).await; 9 }` with `g` a value that needs drop.
fn one_await() -> (Body<Global>, LocalId, LocalId) {
    let mut b = BodyBuilder::new_in(Global);
    let g = b.upvar(true);
    let entry = b.block();
    b.begin(entry);
    let a = await_whole(&mut b, 7, Operand::Const(1), &[Stmt::Drop(g)]);
    b.push(Stmt::Drop(g));
    b.end(Term::Return(Operand::Const(9)));
    (b.finish().expect("a body"), g, a.fut)
}

/// No await: one poll runs the body to its value. The Reference, Async blocks: "The final
/// expression of the block, if present, determines the result value of the future." A future
/// never polled runs nothing: "its immediate effect is to produce and return an anonymous type".
#[test]
fn hand_01_no_await_runs_in_one_poll() {
    let mut b = BodyBuilder::new_in(Global);
    let a = b.upvar(false);
    let t = b.local(false);
    let entry = b.block();
    b.begin(entry);
    b.call(t, 1, &[Operand::Copy(a)]);
    b.end(Term::Return(Operand::Copy(a)));
    let body = b.finish().expect("a body");
    both(&body, &[5], &[], None, &[called(1, &[5]), Event::Return(5)]);
    both(&body, &[5], &[], Some(0), &[]);
}

/// An await that is Ready at once never suspends. The Reference, Await expressions, step 6:
/// "Otherwise the call to poll must have returned Poll::Ready, in which case the value
/// contained in the Poll::Ready variant is used as the result of the await expression itself."
#[test]
fn hand_02_ready_at_once() {
    let (body, g, fut) = one_await();
    let want = [called(7, &[1]), Event::Poll(701), dropped(fut, 701), dropped(g, 3), Event::Return(9)];
    both(&body, &[3], &[], None, &want);
}

/// Pending twice: the same future is polled three times and made once. Step 5: "when the
/// surrounding async context is re-polled, execution returns to step 3" (the pin and the poll,
/// not step 1, the making of the future).
#[test]
fn hand_03_pending_twice_polls_the_same_future() {
    let (body, g, fut) = one_await();
    let want = [
        called(7, &[1]),
        Event::Poll(701),
        Event::Yield(0),
        Event::Poll(701),
        Event::Yield(0),
        Event::Poll(701),
        dropped(fut, 701),
        dropped(g, 3),
        Event::Return(9),
    ];
    both(&body, &[3], &[true, true], None, &want);
}

/// Cancelled while parked: the awaited future, then the upvar ("dropped in reverse order of
/// declaration (for variables) or creation (for temporaries)"). Cancelled before its first
/// poll: the upvar alone (the Reference, Async functions: the block "captures all function
/// parameters", so the future owns them from the call on).
#[test]
fn hand_04_cancel_drops_what_is_live() {
    let (body, g, fut) = one_await();
    let parked = [called(7, &[1]), Event::Poll(701), Event::Yield(0), dropped(fut, 701), dropped(g, 3)];
    both(&body, &[3], &[true], Some(1), &parked);
    both(&body, &[3], &[true], Some(0), &[dropped(g, 3)]);
}

/// Only what is used after the await is in the state (coroutines.md, design 3): `x` is,
/// `y` (read only before) and the call's result are ordinary locals of `resume`.
#[test]
fn hand_05_only_locals_live_across_are_saved() {
    let mut b = BodyBuilder::new_in(Global);
    let (x, y, t) = (b.local(false), b.local(false), b.local(false));
    let entry = b.block();
    b.begin(entry);
    b.push(Stmt::Use { dest: x, src: Operand::Const(4) });
    b.push(Stmt::Use { dest: y, src: Operand::Const(6) });
    b.call(t, 1, &[Operand::Copy(y)]);
    let a = await_whole(&mut b, 7, Operand::Const(0), &[]);
    b.call(t, 2, &[Operand::Copy(x)]);
    b.end(Term::Return(Operand::Copy(x)));
    let body = b.finish().expect("a body");
    let want = [
        called(1, &[6]),
        called(7, &[0]),
        Event::Poll(700),
        Event::Yield(0),
        Event::Poll(700),
        dropped(a.fut, 700),
        called(2, &[4]),
        Event::Return(4),
    ];
    both(&body, &[], &[true], None, &want);
    let machine = transform(&body, Global).expect("the transform");
    let layout = machine.layout();
    assert!(layout.is_saved(x), "x is read after the await");
    assert!(layout.is_saved(a.fut), "the awaited future is polled again");
    assert_eq!(layout.home_of(y), Home::Frame, "y is not read after the await");
    assert_eq!(layout.home_of(t), Home::Frame, "t is written before it is read");
}

/// A borrow held across an await keeps the borrowed local in the state, at one address: the
/// write through the reference after the resume is seen by the read after it. `poll` takes
/// `self: Pin<&mut Self>` because of this.
#[test]
fn hand_06_a_borrow_held_across_an_await() {
    let mut b = BodyBuilder::new_in(Global);
    let (x, r, v, t) = (b.local(false), b.local(false), b.local(false), b.local(false));
    let entry = b.block();
    b.begin(entry);
    b.push(Stmt::Use { dest: x, src: Operand::Const(1) });
    b.push(Stmt::Borrow { dest: r, of: x });
    let a = await_whole(&mut b, 7, Operand::Const(0), &[]);
    b.push(Stmt::Store { via: r, src: Operand::Const(5) });
    b.push(Stmt::Load { dest: v, via: r });
    b.call(t, 3, &[Operand::Copy(v)]);
    b.end(Term::Return(Operand::Const(0)));
    let body = b.finish().expect("a body");
    let want = [
        called(7, &[0]),
        Event::Poll(700),
        Event::Yield(0),
        Event::Poll(700),
        dropped(a.fut, 700),
        called(3, &[5]),
        Event::Return(0),
    ];
    both(&body, &[], &[true], None, &want);
    let machine = transform(&body, Global).expect("the transform");
    let layout = machine.layout();
    assert!(layout.is_saved(r), "the reference is used after the await");
    assert!(layout.is_saved(x), "what it points at stays with it");
    assert!(layout.across.borrowed.get(a.point.index(), x.index()), "x is kept by the borrow");
    assert_eq!(layout.home_of(v), Home::Frame);
}

/// An await inside a loop: one point, parked at and resumed more than once, the counter in
/// the state.
#[test]
fn hand_07_an_await_inside_a_loop() {
    let mut b = BodyBuilder::new_in(Global);
    let c = b.local(false);
    let (entry, head, body_block, exit) = (b.block(), b.block(), b.block(), b.block());
    b.begin(entry);
    b.push(Stmt::Use { dest: c, src: Operand::Const(2) });
    b.end(Term::Goto(head));
    b.begin(head);
    b.switch(c, &[exit, body_block]);
    b.begin(body_block);
    b.push(Stmt::Step { dest: c, src: c, by: -1 });
    let a = await_whole(&mut b, 7, Operand::Copy(c), &[]);
    b.end(Term::Goto(head));
    b.begin(exit);
    b.end(Term::Return(Operand::Copy(c)));
    let body = b.finish().expect("a body");
    let want = [
        called(7, &[1]),
        Event::Poll(701),
        Event::Yield(0),
        Event::Poll(701),
        dropped(a.fut, 701),
        called(7, &[0]),
        Event::Poll(700),
        dropped(a.fut, 700),
        Event::Return(0),
    ];
    both(&body, &[], &[true], None, &want);
}

/// A value moved on one path before the await: the cancel path tests its flag, and the flag
/// is in the state (decision D2). "After moving out of a place expression that evaluates to a
/// local variable, the location is deinitialized": a cancel must not drop it again.
#[test]
fn hand_08_moved_on_one_path_before_the_await() {
    let mut b = BodyBuilder::new_in(Global);
    let g = b.upvar(true);
    let a = b.upvar(false);
    let (flag, t) = (b.local(false), b.local(false));
    let (entry, moving, join) = (b.block(), b.block(), b.block());
    b.begin(entry);
    b.push(Stmt::Use { dest: flag, src: Operand::Const(1) });
    b.switch(a, &[join, moving]);
    b.begin(moving);
    b.call(t, 4, &[Operand::Move(g)]);
    b.push(Stmt::Use { dest: flag, src: Operand::Const(0) });
    b.end(Term::Goto(join));
    b.begin(join);
    let aw = await_open(&mut b, 7, Operand::Const(0));
    let (cancel_drop, cancel_done) = (b.block(), b.block());
    b.begin(aw.cancel);
    b.push(Stmt::Drop(aw.fut));
    b.switch(flag, &[cancel_done, cancel_drop]);
    b.begin(cancel_drop);
    b.push(Stmt::Drop(g));
    b.end(Term::Goto(cancel_done));
    b.begin(cancel_done);
    b.end(Term::Cancelled);
    let (end_drop, end) = (b.block(), b.block());
    b.begin(aw.ready);
    b.push(Stmt::Drop(aw.fut));
    b.switch(flag, &[end, end_drop]);
    b.begin(end_drop);
    b.push(Stmt::Drop(g));
    b.end(Term::Goto(end));
    b.begin(end);
    b.end(Term::Return(Operand::Const(0)));
    let body = b.finish().expect("a body");

    let moved_then_cancelled =
        [called(4, &[3]), called(7, &[0]), Event::Poll(700), Event::Yield(0), dropped(aw.fut, 700)];
    both(&body, &[3, 1], &[true], Some(1), &moved_then_cancelled);
    let kept_then_cancelled =
        [called(7, &[0]), Event::Poll(700), Event::Yield(0), dropped(aw.fut, 700), dropped(g, 3)];
    both(&body, &[3, 0], &[true], Some(1), &kept_then_cancelled);
    let moved_and_finished = [
        called(4, &[3]),
        called(7, &[0]),
        Event::Poll(700),
        Event::Yield(0),
        Event::Poll(700),
        dropped(aw.fut, 700),
        Event::Return(0),
    ];
    both(&body, &[3, 1], &[true], None, &moved_and_finished);
    both(&body, &[3, 1], &[true], Some(0), &[dropped(g, 3)]);
    let machine = transform(&body, Global).expect("the transform");
    assert!(machine.layout().is_saved(flag), "the flag is read on the cancel path");
    assert_eq!(machine.layout().home_of(t), Home::Frame);
}

/// A value moved after the await: a cancel at the await still owns it and drops it; a run to
/// the end hands it to the call and does not.
#[test]
fn hand_09_moved_after_the_await() {
    let mut b = BodyBuilder::new_in(Global);
    let g = b.upvar(true);
    let t = b.local(false);
    let entry = b.block();
    b.begin(entry);
    let a = await_whole(&mut b, 7, Operand::Const(0), &[Stmt::Drop(g)]);
    b.call(t, 4, &[Operand::Move(g)]);
    b.end(Term::Return(Operand::Const(0)));
    let body = b.finish().expect("a body");
    let finished = [
        called(7, &[0]),
        Event::Poll(700),
        Event::Yield(0),
        Event::Poll(700),
        dropped(a.fut, 700),
        called(4, &[3]),
        Event::Return(0),
    ];
    both(&body, &[3], &[true], None, &finished);
    let cancelled = [called(7, &[0]), Event::Poll(700), Event::Yield(0), dropped(a.fut, 700), dropped(g, 3)];
    both(&body, &[3], &[true], Some(1), &cancelled);
}

/// An early return, and two awaits with a cancel at the second. The Reference, Async blocks:
/// "return <expr> from within an async block will return the result of <expr> as the output of
/// the future". Parked at the second await the first future is long dropped: only the second
/// and `x` are.
#[test]
fn hand_10_early_return_and_a_cancel_at_the_second_await() {
    let mut b = BodyBuilder::new_in(Global);
    let a = b.upvar(false);
    let x = b.local(true);
    let (entry, early, go) = (b.block(), b.block(), b.block());
    b.begin(entry);
    b.push(Stmt::Use { dest: x, src: Operand::Const(11) });
    b.switch(a, &[go, early]);
    b.begin(early);
    b.push(Stmt::Drop(x));
    b.end(Term::Return(Operand::Const(1)));
    b.begin(go);
    let first = await_whole(&mut b, 7, Operand::Const(0), &[Stmt::Drop(x)]);
    let second = await_whole(&mut b, 8, Operand::Const(0), &[Stmt::Drop(x)]);
    b.push(Stmt::Drop(x));
    b.end(Term::Return(Operand::Const(2)));
    let body = b.finish().expect("a body");

    both(&body, &[1], &[], None, &[dropped(x, 11), Event::Return(1)]);
    let cancelled_at_second = [
        called(7, &[0]),
        Event::Poll(700),
        Event::Yield(0),
        Event::Poll(700),
        dropped(first.fut, 700),
        called(8, &[0]),
        Event::Poll(800),
        Event::Yield(1),
        dropped(second.fut, 800),
        dropped(x, 11),
    ];
    both(&body, &[0], &[true, false, true], Some(2), &cancelled_at_second);
    let finished = [
        called(7, &[0]),
        Event::Poll(700),
        dropped(first.fut, 700),
        called(8, &[0]),
        Event::Poll(800),
        dropped(second.fut, 800),
        dropped(x, 11),
        Event::Return(2),
    ];
    both(&body, &[0], &[], None, &finished);
    assert_eq!(Tag::point(second.point), Tag(4));
}

// ---------------------------------------------------------------- the tags and the refusals

/// The head `match`: start, the point's resume block, and the library's panic for a finished
/// future (library/core/src/panicking.rs: `panic_const_async_fn_resumed = "`async fn` resumed
/// after completion"`). A finished future's drop does nothing.
#[test]
fn the_entry_tables_by_tag() {
    let (body, _, _) = one_await();
    let machine = transform(&body, Global).expect("the transform");
    assert_eq!(machine.entry(Tag::UNRESUMED), Entry::Run(BlockId(0)));
    assert_eq!(machine.entry(Tag::RETURNED), Entry::Completed);
    assert_eq!(machine.entry(Tag::PANICKED), Entry::Panicked);
    assert!(matches!(machine.entry(Tag(3)), Entry::Run(_)));
    assert_eq!(machine.entry(Tag(4)), Entry::NoSuchTag);
    assert!(matches!(machine.drop_entry(Tag::UNRESUMED), DropEntry::Run(_)));
    assert!(matches!(machine.drop_entry(Tag(3)), DropEntry::Run(_)));
    assert_eq!(machine.drop_entry(Tag::RETURNED), DropEntry::Nothing);
    assert_eq!(machine.drop_entry(Tag(4)), DropEntry::NoSuchTag);
    assert_eq!(Tag::point(PointId(0)), Tag(3));
    assert_eq!(Tag(3).as_point(1), Some(PointId(0)));
    assert_eq!(Tag(2).as_point(1), None);
}

/// Decision D7, refused by name: a cancel path that suspends, a cancel path that returns, a
/// resume path that ends as a cancel path does.
#[test]
fn the_transform_refuses_crossed_paths_by_name() {
    // A cancel path that suspends.
    let mut b = BodyBuilder::new_in(Global);
    let (entry, on, cancel, inner) = (b.block(), b.block(), b.block(), b.block());
    let (p0, p1) = (b.point(), b.point());
    b.begin(entry);
    b.end(Term::Suspend { point: p0, resume: on, drop: cancel });
    b.begin(on);
    b.end(Term::Return(Operand::Const(0)));
    b.begin(cancel);
    b.end(Term::Suspend { point: p1, resume: on, drop: inner });
    b.begin(inner);
    b.end(Term::Cancelled);
    let body = b.finish().expect("a body");
    assert_eq!(transform(&body, Global).err(), Some(TransformError::SuspendOnCancelPath(cancel)));

    // A cancel path that returns.
    let mut b = BodyBuilder::new_in(Global);
    let (entry, on, cancel) = (b.block(), b.block(), b.block());
    let p0 = b.point();
    b.begin(entry);
    b.end(Term::Suspend { point: p0, resume: on, drop: cancel });
    b.begin(on);
    b.end(Term::Return(Operand::Const(0)));
    b.begin(cancel);
    b.end(Term::Return(Operand::Const(1)));
    let body = b.finish().expect("a body");
    assert_eq!(transform(&body, Global).err(), Some(TransformError::ReturnOnCancelPath(cancel)));

    // A resume path that ends in `Cancelled`.
    let mut b = BodyBuilder::new_in(Global);
    let entry = b.block();
    b.begin(entry);
    b.end(Term::Cancelled);
    let body = b.finish().expect("a body");
    assert_eq!(transform(&body, Global).err(), Some(TransformError::CancelledOnResumePath(entry)));
}

/// The builder names a malformed body: a block never filled, a point used twice.
#[test]
fn the_builder_refuses_malformed_bodies_by_name() {
    let mut b = BodyBuilder::new_in(Global);
    let (entry, never) = (b.block(), b.block());
    b.begin(entry);
    b.end(Term::Return(Operand::Const(0)));
    assert_eq!(b.finish().err(), Some(IrError::UnfilledBlock(never)));

    let mut b = BodyBuilder::new_in(Global);
    let (entry, on, cancel) = (b.block(), b.block(), b.block());
    let p = b.point();
    b.begin(entry);
    b.end(Term::Suspend { point: p, resume: on, drop: cancel });
    b.begin(on);
    b.end(Term::Suspend { point: p, resume: on, drop: cancel });
    b.begin(cancel);
    b.end(Term::Cancelled);
    assert_eq!(b.finish().err(), Some(IrError::PointTwice(p)));
}
