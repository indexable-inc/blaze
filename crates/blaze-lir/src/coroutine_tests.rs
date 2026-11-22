//! The coroutine pass ([`crate::coroutine`]) against its oracle (docs/v2/plan/design/coroutines.md,
//! "Proof by simulation"; decision D73: "the LIR pass is tested the same way, on LIR").
//!
//! One interpreter gives the operations their meaning; two drivers use it. The direct run is
//! the body as the builder wrote it: one frame alive from the first poll to the end, a
//! `suspend` being "this poll returned its values; the next poll's context replaces the
//! context parameter; go on at the resume block, or at the cancel block if the state is
//! dropped here". The machine run keeps only the state between polls: a new frame for each
//! call of `poll`, then `drop` when the schedule cancels. A pointer into the frame of an
//! earlier call is a fault, as is a load of bytes nothing wrote. For random bodies (slots,
//! stores and loads, a slot's address held in a value and in memory, calls of `observe`,
//! branches with a joined value, counted loops with a suspend inside, early returns) and
//! every schedule (to the end; dropped before the first poll; dropped after the n-th
//! Pending), the two traces are equal.
//!
//! `crate::eval` is not used: it has no calls, no pointers in memory and no memory outside
//! the frame, so the interpreter here is the test's own, for the operations the generator and
//! the pass write and no other.
//!
//! The rules tested are the Reference's (Await expressions): "If the call to poll returns
//! Poll::Pending, then the future returns Poll::Pending, suspending its state so that, when
//! the surrounding async context is re-polled, execution returns to step 3", and
//! (Destructors) "When control flow leaves a drop scope all variables associated to that
//! scope are dropped": the cancel path is the builder's, and the pass must run exactly it.

use std::alloc::Global;

use blaze_sim::{Broke, Config, Tape, check, control};

use crate::{
    Block, BlockKind, BuildError, Builder, Flags, Full, Func, IntCond, Op, Ty, Value, View,
    builder::{MemAttrs, Param, Variadic},
    coroutine::{CoroutineError, PANICKED, Plant, Saved, Shape, Split, StateField, split, split_planted},
    func::{Conv, RefId, RefKind, SigId, SlotId, SlotKind},
    op::Target,
    parse, print, verify,
};

/// The bytes of the two `i64` upvars every body here has.
const PREFIX: u32 = 16;
/// The first of a body's two returns: `Poll`'s tag as these bodies write it.
const PENDING: u64 = 0;
const READY: u64 = 1;
/// The entry parameters: the state pointer, then the context.
const STATE: usize = 0;
const CX: usize = 1;
const FUEL: u64 = 200_000;

type Fault = &'static str;
const UNINIT: Fault = "a load of uninitialised bytes";
const UNDEF: Fault = "a use of an undefined value";
const OLD_FRAME: Fault = "a pointer into the frame of an earlier poll";
const DIVERGED: &str = "the two runs diverged";
const FAULTED: &str = "the machine run faulted";
/// The machine's check after an unwind (ASYNC.md D-A5).
const POISONED: Fault = "after an unwind the tag must be PANICKED (2)";
/// The exception `may_unwind` throws: what its landing block's parameter holds.
const EXCEPTION: u64 = 0xe0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Val {
    Int(u64),
    Ptr { region: u32, at: i64 },
    Undef,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Byte {
    Uninit,
    Int(u8),
    /// Byte `part` of a pointer.
    Ptr { region: u32, at: i64, part: u8 },
}

/// The state (region 0), or one slot of one frame.
struct Region {
    bytes: Vec<Byte>,
    live: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Event {
    Observe(u64),
    /// What a poll returned.
    Out(Vec<u64>),
    Cancelled,
    Trap(u32),
    /// The machine was polled more often than the body yields.
    Overrun,
    /// A `resume`: the unwind left the poll.
    Unwound,
}

struct World {
    regions: Vec<Region>,
    trace: Vec<Event>,
    fuel: u64,
}

/// One call's values and its slots' regions.
struct Frame {
    values: Vec<Option<Val>>,
    slots: Vec<u32>,
}

enum Stop {
    Returned(Vec<u64>),
    Suspended { resume: Block, cancel: Block, pending: Vec<u64> },
    Cancelled,
    Trapped(u32),
    /// A `resume`: the unwind left the call.
    Unwound,
}

impl World {
    /// A state of `state_bytes` bytes whose first sixteen are the upvars.
    fn new(state_bytes: usize, upvars: [u64; 2]) -> World {
        let mut bytes = vec![Byte::Uninit; state_bytes];
        for (i, upvar) in upvars.iter().enumerate() {
            for j in 0..8 {
                bytes[8 * i + j] = Byte::Int((upvar >> (8 * j)) as u8);
            }
        }
        World { regions: vec![Region { bytes, live: true }], trace: Vec::new(), fuel: FUEL }
    }

    fn frame(&mut self, view: View<'_>) -> Frame {
        let mut slots = Vec::new();
        for rec in view.slots {
            slots.push(self.regions.len() as u32);
            self.regions.push(Region { bytes: vec![Byte::Uninit; rec.size as usize], live: true });
        }
        Frame { values: vec![None; view.len()], slots }
    }

    /// A call returned: its frame is gone.
    fn end(&mut self, frame: &Frame) {
        for &region in &frame.slots {
            self.regions[region as usize].live = false;
        }
    }

    fn span(&mut self, ptr: Val, offset: i32, bytes: usize) -> Result<&mut [Byte], Fault> {
        match ptr {
            Val::Ptr { region, at } => {
                let region = &mut self.regions[region as usize];
                if !region.live {
                    return Err(OLD_FRAME);
                }
                let first = at + i64::from(offset);
                if first < 0 || first as usize + bytes > region.bytes.len() {
                    return Err("an access out of its region");
                }
                Ok(&mut region.bytes[first as usize..first as usize + bytes])
            }
            Val::Undef => Err(UNDEF),
            Val::Int(_) => Err("an integer used as a pointer"),
        }
    }

    fn load(&mut self, ptr: Val, offset: i32, ty: Ty) -> Result<Val, Fault> {
        if ty == Ty::PTR {
            let bytes = self.span(ptr, offset, 8)?;
            return match bytes[0] {
                Byte::Uninit => Err(UNINIT),
                Byte::Int(_) => Err("a pointer loaded from integer bytes"),
                Byte::Ptr { region, at, .. } => {
                    let whole = bytes.iter().enumerate().all(|(i, &byte)| byte == Byte::Ptr { region, at, part: i as u8 });
                    if whole { Ok(Val::Ptr { region, at }) } else { Err("a pointer loaded from parts of two") }
                }
            };
        }
        let bytes = self.span(ptr, offset, (ty.bits(64) / 8) as usize)?;
        let mut value = 0u64;
        for (i, &byte) in bytes.iter().enumerate() {
            match byte {
                Byte::Int(byte) => value |= u64::from(byte) << (8 * i),
                Byte::Uninit => return Err(UNINIT),
                Byte::Ptr { .. } => return Err("an integer loaded from pointer bytes"),
            }
        }
        Ok(Val::Int(value))
    }

    fn store(&mut self, value: Val, ty: Ty, ptr: Val, offset: i32) -> Result<(), Fault> {
        match value {
            Val::Undef => Err(UNDEF),
            Val::Ptr { region, at } => {
                for (i, byte) in self.span(ptr, offset, 8)?.iter_mut().enumerate() {
                    *byte = Byte::Ptr { region, at, part: i as u8 };
                }
                Ok(())
            }
            Val::Int(value) => {
                for (i, byte) in self.span(ptr, offset, (ty.bits(64) / 8) as usize)?.iter_mut().enumerate() {
                    *byte = Byte::Int((value >> (8 * i)) as u8);
                }
                Ok(())
            }
        }
    }
}

fn get(frame: &Frame, value: Value) -> Result<Val, Fault> {
    frame.values[value.index()].ok_or("a use of a value this run never made")
}

fn int(frame: &Frame, value: Value) -> Result<u64, Fault> {
    match get(frame, value)? {
        Val::Int(it) => Ok(it),
        Val::Undef => Err(UNDEF),
        Val::Ptr { .. } => Err("a pointer used as an integer"),
    }
}

fn mask(value: u64, ty: Ty) -> u64 {
    match ty.bits(64) {
        64.. => value,
        bits => value & ((1u64 << bits) - 1),
    }
}

/// The target's parameters take its arguments, all read before any is written.
fn enter(frame: &mut Frame, view: View<'_>, to: Target<'_>) -> Result<Block, Fault> {
    let mut args = Vec::new();
    for value in to.args.iter() {
        args.push(get(frame, value)?);
    }
    for (param, arg) in view.params(to.block).zip(args) {
        frame.values[param] = Some(arg);
    }
    Ok(to.block)
}

fn ints(frame: &Frame, values: crate::op::Values<'_>) -> Result<Vec<u64>, Fault> {
    values.iter().map(|value| int(frame, value)).collect()
}

/// Runs `view` in `frame` from `block`, whose parameters are set, to where the call stops.
fn run(world: &mut World, view: View<'_>, frame: &mut Frame, mut block: Block) -> Result<Stop, Fault> {
    loop {
        let mut next = None;
        for inst in view.params(block).end..view.block(block).end {
            world.fuel = world.fuel.checked_sub(1).ok_or("out of fuel")?;
            let op = view.op(inst);
            let ty = view.ty(Value(inst as u32));
            let mut o = view.operands(inst);
            let made = match op {
                Op::Iconst => {
                    let word = o.imm();
                    Some(Val::Int(if ty.bits(64) > 32 { word as i32 as i64 as u64 } else { u64::from(word) }))
                }
                Op::Uninit => Some(Val::Undef),
                Op::Add | Op::Sub => {
                    let (a, b) = (int(frame, o.value())?, int(frame, o.value())?);
                    Some(Val::Int(mask(if op == Op::Add { a.wrapping_add(b) } else { a.wrapping_sub(b) }, ty)))
                }
                Op::Icmp => {
                    let cond = o.int_cond();
                    let (a, b) = (int(frame, o.value())?, int(frame, o.value())?);
                    let yes = match cond {
                        IntCond::Eq => a == b,
                        IntCond::Ne => a != b,
                        IntCond::Ult => a < b,
                        _ => return Err("a condition the test's interpreter has not"),
                    };
                    Some(Val::Int(u64::from(yes)))
                }
                Op::Zext => Some(Val::Int(int(frame, o.value())?)),
                Op::Trunc => Some(Val::Int(mask(int(frame, o.value())?, ty))),
                Op::SlotAddr => Some(Val::Ptr { region: frame.slots[o.slot().index()], at: 0 }),
                Op::SlotLive | Op::SlotDead => {
                    let region = frame.slots[o.slot().index()];
                    world.regions[region as usize].bytes.fill(Byte::Uninit);
                    None
                }
                Op::PtrAdd => {
                    let ptr = get(frame, o.value())?;
                    let by = int(frame, o.value())?;
                    match ptr {
                        Val::Ptr { region, at } => Some(Val::Ptr { region, at: at.wrapping_add(by as i64) }),
                        Val::Undef => return Err(UNDEF),
                        Val::Int(_) => return Err("an integer used as a pointer"),
                    }
                }
                Op::Load => {
                    let ptr = get(frame, o.value())?;
                    Some(world.load(ptr, o.offset(), ty)?)
                }
                Op::Store => {
                    let stored = o.value();
                    let value = get(frame, stored)?;
                    let ptr = get(frame, o.value())?;
                    world.store(value, view.ty(stored), ptr, o.offset())?;
                    None
                }
                // The one callee: `observe(i64)`.
                Op::Call => {
                    o.reference();
                    o.sig();
                    let seen = int(frame, o.values().get(0))?;
                    world.trace.push(Event::Observe(seen));
                    None
                }
                Op::Jump => {
                    next = Some(enter(frame, view, o.target())?);
                    None
                }
                Op::Br => {
                    let cond = int(frame, o.value())?;
                    let (then, otherwise) = (o.target(), o.target());
                    next = Some(enter(frame, view, if cond != 0 { then } else { otherwise })?);
                    None
                }
                Op::Switch => {
                    let scrutinee = o.value();
                    let on = int(frame, scrutinee)?;
                    let default = o.target();
                    let to = o.cases(scrutinee).find(|(words, _)| u64::from(words[0]) == on).map_or(default, |(_, to)| to);
                    next = Some(enter(frame, view, to)?);
                    None
                }
                Op::Return => return Ok(Stop::Returned(ints(frame, o.values())?)),
                Op::Trap => return Ok(Stop::Trapped(o.imm())),
                Op::Suspend => {
                    o.imm();
                    let (resume, cancel) = (o.block(), o.block());
                    return Ok(Stop::Suspended { resume, cancel, pending: ints(frame, o.values())? });
                }
                Op::Cancelled => return Ok(Stop::Cancelled),
                // The one invoked callee: `may_unwind(i64)`, which observes its argument and
                // unwinds when it is odd (coroutine_unwind_tests.rs).
                Op::Invoke => {
                    o.reference();
                    o.sig();
                    let (normal, unwind) = (o.block(), o.block());
                    let seen = int(frame, o.values().get(0))?;
                    world.trace.push(Event::Observe(seen));
                    if seen % 2 == 1 {
                        frame.values[view.params(unwind).start] = Some(Val::Int(EXCEPTION));
                        next = Some(unwind);
                    } else {
                        next = Some(normal);
                    }
                    None
                }
                Op::Resume => match get(frame, o.value())? {
                    Val::Int(EXCEPTION) => return Ok(Stop::Unwound),
                    _ => return Err("a `resume` of something not the exception"),
                },
                _ => return Err("an operation the test's interpreter has not"),
            };
            if let Some(value) = made {
                frame.values[inst] = Some(value);
            }
        }
        match next {
            Some(to) => block = to,
            None => return Err("a block that ends in no terminator"),
        }
    }
}

/// The body as it is written: `cancel` is `None` to run to the end, `Some(0)` to drop the
/// state before its first poll, `Some(n)` to drop it parked at its n-th Pending.
fn direct(view: View<'_>, shape: Shape, upvars: [u64; 2], cancel: Option<u32>) -> Result<Vec<Event>, Fault> {
    let mut world = World::new(PREFIX as usize, upvars);
    let mut frame = world.frame(view);
    frame.values[STATE] = Some(Val::Ptr { region: 0, at: 0 });
    if cancel == Some(0) {
        return match run(&mut world, view, &mut frame, shape.unresumed)? {
            Stop::Cancelled => {
                world.trace.push(Event::Cancelled);
                Ok(world.trace)
            }
            _ => Err("an unresumed block that does not end in `cancelled`"),
        };
    }
    let (mut polls, mut yields) = (1u64, 0u32);
    frame.values[CX] = Some(Val::Int(polls));
    let mut block = Block(0);
    loop {
        match run(&mut world, view, &mut frame, block)? {
            Stop::Returned(values) => {
                world.trace.push(Event::Out(values));
                break;
            }
            Stop::Suspended { resume, cancel: to, pending } => {
                world.trace.push(Event::Out(pending));
                yields += 1;
                if cancel == Some(yields) {
                    block = to;
                } else {
                    // The next poll passes its own context.
                    polls += 1;
                    frame.values[CX] = Some(Val::Int(polls));
                    block = resume;
                }
            }
            Stop::Cancelled => {
                world.trace.push(Event::Cancelled);
                break;
            }
            Stop::Trapped(code) => {
                world.trace.push(Event::Trap(code));
                break;
            }
            Stop::Unwound => {
                world.trace.push(Event::Unwound);
                break;
            }
        }
    }
    Ok(world.trace)
}

/// The pass's outputs, driven as a state machine: `poll` until it is not Pending, or `drop`
/// where `cancel` says; a state that returned is dropped too, which must do nothing.
fn machine(split: &Split<Global>, upvars: [u64; 2], cancel: Option<u32>, most_polls: u64) -> Result<Vec<Event>, Fault> {
    let (poll, glue) = (split.poll.view(), split.drop.view());
    let layout = &split.layout;
    let mut world = World::new(layout.size as usize, upvars);
    let state = Val::Ptr { region: 0, at: 0 };
    // Whoever makes the state stores `Unresumed`.
    world.store(Val::Int(0), layout.tag_ty, state, layout.tag_offset as i32)?;
    let dropped = |world: &mut World| -> Result<(), Fault> {
        let mut frame = world.frame(glue);
        frame.values[0] = Some(state);
        let stop = run(world, glue, &mut frame, Block(0))?;
        world.end(&frame);
        match stop {
            Stop::Returned(values) if values.is_empty() => Ok(()),
            Stop::Trapped(code) => {
                world.trace.push(Event::Trap(code));
                Ok(())
            }
            _ => Err("a drop glue that does not return"),
        }
    };
    if cancel == Some(0) {
        dropped(&mut world)?;
        world.trace.push(Event::Cancelled);
        return Ok(world.trace);
    }
    let (mut polls, mut yields) = (0u64, 0u32);
    loop {
        polls += 1;
        if polls > most_polls {
            world.trace.push(Event::Overrun);
            break;
        }
        let mut frame = world.frame(poll);
        frame.values[STATE] = Some(state);
        frame.values[CX] = Some(Val::Int(polls));
        let stop = run(&mut world, poll, &mut frame, Block(0))?;
        world.end(&frame);
        match stop {
            Stop::Returned(values) => {
                let ready = values.first() != Some(&PENDING);
                world.trace.push(Event::Out(values));
                if ready {
                    dropped(&mut world)?;
                    break;
                }
                yields += 1;
                if cancel == Some(yields) {
                    dropped(&mut world)?;
                    world.trace.push(Event::Cancelled);
                    break;
                }
            }
            Stop::Trapped(code) => {
                world.trace.push(Event::Trap(code));
                break;
            }
            // An unwind out of a poll leaves the state `Panicked`; the owner then drops the
            // future while the panic passes, and that drop must do nothing.
            Stop::Unwound => {
                world.trace.push(Event::Unwound);
                if world.load(state, layout.tag_offset as i32, layout.tag_ty)? != Val::Int(u64::from(PANICKED)) {
                    return Err(POISONED);
                }
                dropped(&mut world)?;
                break;
            }
            Stop::Suspended { .. } | Stop::Cancelled => return Err("a `suspend` or `cancelled` left in an output"),
        }
    }
    Ok(world.trace)
}

/// Whether the body and its split agree on every schedule.
fn agree(view: View<'_>, shape: Shape, split: &Split<Global>, upvars: [u64; 2]) -> Result<(), Broke> {
    let texts = || format!("{}\npoll:\n{}\ndrop:\n{}", print(view), print(split.poll.view()), print(split.drop.view()));
    for (name, func) in [("poll", &split.poll), ("drop", &split.drop)] {
        verify(func.view()).map_err(|e| Broke::Oracle(format!("the {name} is not LIR: {e}\n{}", texts())))?;
        if func.ops.iter().any(|&op| op == Op::Suspend as u8 || op == Op::Cancelled as u8) {
            return Err(Broke::Oracle(format!("a `suspend` or `cancelled` left in the {name}\n{}", texts())));
        }
    }
    let full = direct(view, shape, upvars, None).map_err(|f| Broke::Harness(format!("the direct run: {f}\n{}", texts())))?;
    let yields = full.iter().filter(|event| matches!(event, Event::Out(values) if values.first() == Some(&PENDING))).count() as u32;
    let mut schedules = vec![None];
    schedules.extend((0..=yields.min(4)).map(Some));
    if yields > 4 {
        schedules.push(Some(yields));
    }
    for cancel in schedules {
        let want = direct(view, shape, upvars, cancel)
            .map_err(|f| Broke::Harness(format!("the direct run: {f} (cancel {cancel:?})\n{}", texts())))?;
        let got = machine(split, upvars, cancel, u64::from(yields) + 2)
            .map_err(|f| Broke::Oracle(format!("{FAULTED}: {f} (cancel {cancel:?}, upvars {upvars:?})\n{}", texts())))?;
        if want != got {
            return Err(Broke::Oracle(format!(
                "{DIVERGED} (cancel {cancel:?}, upvars {upvars:?})\nthe body:    {want:?}\nthe machine: {got:?}\n{}",
                texts()
            )));
        }
    }
    Ok(())
}

/// The `i64` cells of each of a random body's integer slots.
const CELLS: u32 = 2;

/// A construct a random body is inside of.
enum Open {
    If { otherwise: Block, join: Block, in_else: bool, ints: usize, ptrs: usize },
    Loop { head: Block, exit: Block, i: Value, trips: u64 },
}

/// A random body being written. Every value in `ints` and `ptrs` dominates where the next
/// statement goes.
struct Gen<'t> {
    t: &'t Tape,
    b: Builder<Global, Full>,
    state: Value,
    cx: Value,
    observe: RefId,
    observe_sig: SigId,
    /// The integer slots, every cell written in the entry block.
    slots: Vec<SlotId>,
    /// A slot that always holds a pointer to a cell.
    holder: SlotId,
    ints: Vec<Value>,
    /// Pointers to cells, held in values.
    ptrs: Vec<Value>,
    open: Vec<Open>,
    points: u32,
    /// Slots made live and dead inside one statement: the state must keep none.
    temps: Vec<SlotId>,
}

impl Gen<'_> {
    fn int(&self) -> Value {
        self.ints[self.t.below(self.ints.len() as u64, "an integer") as usize]
    }

    /// The address of a slot, made here, and a cell's offset in it.
    fn cell(&mut self) -> (Value, i32) {
        let slot = self.slots[self.t.below(self.slots.len() as u64, "a slot") as usize];
        let cell = self.t.below(u64::from(CELLS), "a cell") as i32;
        (self.b.slot_addr(slot), 8 * cell)
    }

    fn observe(&mut self, value: Value) {
        self.b.call(Flags::NONE, self.observe, self.observe_sig, &[value]);
    }

    fn load(&mut self, ptr: Value, offset: i32) -> Value {
        self.b.load(Ty::I64, Flags::NONE, ptr, offset, MemAttrs::default())
    }

    fn store(&mut self, value: Value, ptr: Value, offset: i32) {
        self.b.store(Flags::NONE, value, ptr, offset, MemAttrs::default());
    }

    /// The pointer the holder holds.
    fn held(&mut self) -> Value {
        let holder = self.b.slot_addr(self.holder);
        self.b.load(Ty::PTR, Flags::NONE, holder, 0, MemAttrs::default())
    }

    /// A pointer to a cell, as a new value.
    fn pointer(&mut self) -> Value {
        let (slot, at) = self.cell();
        let by = self.b.iconst(Ty::I64, at as u128);
        self.b.ptr_add(Flags::NONE, slot, by)
    }

    /// Observes something a path can see: a value, a cell, what a held pointer points at.
    fn look(&mut self) {
        let seen = match self.t.below(4, "a look") {
            0 => self.int(),
            1 => {
                let (slot, at) = self.cell();
                self.load(slot, at)
            }
            2 if !self.ptrs.is_empty() => {
                let ptr = self.ptrs[self.t.below(self.ptrs.len() as u64, "a pointer") as usize];
                self.load(ptr, 0)
            }
            _ => {
                let ptr = self.held();
                self.load(ptr, 0)
            }
        };
        self.observe(seen);
    }

    /// A suspension point, its cancel path, and the resume block the body goes on in.
    fn suspend(&mut self) {
        let (resume, cancel) = (self.b.create_block(BlockKind::Normal), self.b.create_block(BlockKind::Normal));
        let pending = self.b.iconst(Ty::I64, u128::from(PENDING));
        self.b.suspend(self.points, resume, cancel, &[pending, pending]);
        self.points += 1;
        self.b.begin(cancel);
        let looks = self.t.below(3, "cancel looks");
        for _ in 0..looks {
            self.look();
        }
        self.b.cancelled();
        self.b.begin(resume);
    }

    fn open_if(&mut self) {
        if self.open.len() >= 3 {
            return;
        }
        let (a, b) = (self.int(), self.int());
        let cond = self.b.icmp(IntCond::Ult, a, b);
        let [then, otherwise, join] = [(); 3].map(|_| self.b.create_block(BlockKind::Normal));
        self.b.br(Flags::NONE, cond, (then, &[]), (otherwise, &[]));
        self.b.begin(then);
        self.open.push(Open::If { otherwise, join, in_else: false, ints: self.ints.len(), ptrs: self.ptrs.len() });
    }

    fn open_loop(&mut self) {
        if self.open.len() >= 3 {
            return;
        }
        let trips = 1 + self.t.below(3, "trips");
        let zero = self.b.iconst(Ty::I64, 0);
        let (head, exit) = (self.b.create_block(BlockKind::Normal), self.b.create_block(BlockKind::Normal));
        self.b.jump((head, &[zero]));
        self.b.begin(head);
        let i = self.b.param(Ty::I64);
        self.ints.push(i);
        self.open.push(Open::Loop { head, exit, i, trips });
    }

    /// Ends the innermost construct's current part: an `if`'s then arm (by an early return or
    /// at the join), its else arm, or a loop's body.
    fn close(&mut self) {
        let Some(top) = self.open.pop() else { return };
        match top {
            Open::If { otherwise, join, in_else: false, ints, ptrs } => {
                let value = self.int();
                if self.t.below(4, "an early return") == 0 {
                    let ready = self.b.iconst(Ty::I64, u128::from(READY));
                    self.b.ret(&[ready, value]);
                } else {
                    self.b.jump((join, &[value]));
                }
                self.ints.truncate(ints);
                self.ptrs.truncate(ptrs);
                self.b.begin(otherwise);
                self.open.push(Open::If { otherwise, join, in_else: true, ints, ptrs });
            }
            Open::If { join, ints, ptrs, .. } => {
                let value = self.int();
                self.b.jump((join, &[value]));
                self.ints.truncate(ints);
                self.ptrs.truncate(ptrs);
                self.b.begin(join);
                let joined = self.b.param(Ty::I64);
                self.ints.push(joined);
            }
            Open::Loop { head, exit, i, trips } => {
                let one = self.b.iconst(Ty::I64, 1);
                let next = self.b.binary(Op::Add, Flags::NONE, i, one);
                let most = self.b.iconst(Ty::I64, u128::from(trips));
                let again = self.b.icmp(IntCond::Ult, next, most);
                self.b.br(Flags::NONE, again, (head, &[next]), (exit, &[]));
                self.b.begin(exit);
            }
        }
    }

    /// A slot live for this statement alone.
    fn temp(&mut self) {
        let slot = self.b.slot(8, 3, SlotKind::Temp);
        self.temps.push(slot);
        self.b.slot_live(slot);
        let at = self.b.slot_addr(slot);
        let value = self.int();
        self.store(value, at, 0);
        let back = self.load(at, 0);
        self.b.slot_dead(slot);
        self.ints.push(back);
    }

    fn statement(&mut self) {
        match self.t.below(17, "a statement") {
            0 => {
                let value = self.b.iconst(Ty::I64, u128::from(self.t.below(100, "a constant")));
                self.ints.push(value);
            }
            1 => {
                let (a, b) = (self.int(), self.int());
                let sum = self.b.binary(Op::Add, Flags::NONE, a, b);
                self.ints.push(sum);
            }
            2 => {
                let value = self.int();
                let (slot, at) = self.cell();
                self.store(value, slot, at);
            }
            3 => {
                let (slot, at) = self.cell();
                let value = self.load(slot, at);
                self.ints.push(value);
            }
            4 => self.look(),
            5 => {
                let cx = self.cx;
                self.observe(cx);
            }
            // A cell's address held in a value.
            6 => {
                let ptr = self.pointer();
                self.ptrs.push(ptr);
            }
            7 => {
                if !self.ptrs.is_empty() {
                    let ptr = self.ptrs[self.t.below(self.ptrs.len() as u64, "a pointer") as usize];
                    let value = self.int();
                    self.store(value, ptr, 0);
                }
            }
            // A cell's address held in memory, and a write through it.
            8 => {
                let ptr = self.pointer();
                let holder = self.b.slot_addr(self.holder);
                self.b.store(Flags::NONE, ptr, holder, 0, MemAttrs::default());
            }
            9 => {
                let ptr = self.held();
                let value = self.int();
                self.store(value, ptr, 0);
            }
            10..=12 => self.suspend(),
            13 => self.open_if(),
            14 => self.open_loop(),
            15 => self.close(),
            _ => self.temp(),
        }
    }
}

/// A random body, its shape, and the slots the state must not keep.
struct Made {
    func: Func<Global>,
    shape: Shape,
    temps: Vec<SlotId>,
}

/// `(state: ptr, cx: i64) -> (i64, i64)`: the returns are `Poll`'s tag and its value.
fn own_sig(b: &mut Builder<Global, Full>) {
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::PTR), Param::of(Ty::I64)],
        &[Param::of(Ty::I64), Param::of(Ty::I64)],
    );
    b.set_signature(sig);
}

fn random_body(t: &Tape) -> Result<Made, BuildError> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    own_sig(&mut b);
    let observe_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
    let observe = b.reference(RefKind::Func, 0);
    let (entry, unresumed) = (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal));
    b.begin(entry);
    let (state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let mut slots = Vec::new();
    for _ in 0..1 + t.below(3, "slots") {
        slots.push(b.slot(8 * CELLS, 3, SlotKind::Named));
    }
    let holder = b.slot(8, 3, SlotKind::Temp);
    let mut g = Gen {
        t,
        b,
        state,
        cx,
        observe,
        observe_sig,
        slots,
        holder,
        ints: Vec::new(),
        ptrs: Vec::new(),
        open: Vec::new(),
        points: 0,
        temps: Vec::new(),
    };
    // The upvars, read once; every cell and the holder, written before anything reads one.
    for at in [0, 8] {
        let state = g.state;
        let upvar = g.load(state, at);
        g.ints.push(upvar);
    }
    for s in 0..g.slots.len() {
        for cell in 0..CELLS {
            let slot = g.b.slot_addr(g.slots[s]);
            let value = g.b.iconst(Ty::I64, 10 * s as u128 + u128::from(cell) + 1);
            g.store(value, slot, 8 * cell as i32);
        }
    }
    let first = g.b.slot_addr(g.slots[0]);
    let holder = g.b.slot_addr(g.holder);
    g.b.store(Flags::NONE, first, holder, 0, MemAttrs::default());
    let statements = t.below(40, "statements");
    for _ in 0..statements {
        g.statement();
    }
    while !g.open.is_empty() {
        g.close();
    }
    let value = g.int();
    let ready = g.b.iconst(Ty::I64, u128::from(READY));
    g.b.ret(&[ready, value]);
    // The unresumed block drops the upvars: here it shows them.
    g.b.begin(unresumed);
    for at in [0, 8] {
        let state = g.state;
        let upvar = g.load(state, at);
        g.observe(upvar);
    }
    g.b.cancelled();
    let Gen { b, temps, .. } = g;
    let func = b.finish_checked()?;
    // Blocks are numbered in the order they were begun: the unresumed block is the last.
    let unresumed = Block(func.block_starts.len() as u32 - 1);
    Ok(Made { func, shape: Shape { state_param: STATE as u32, prefix_size: PREFIX, prefix_align: 3, unresumed }, temps })
}

fn case(t: &Tape, plant: Plant) -> Result<(), Broke> {
    let made = random_body(t).map_err(|e| Broke::Harness(format!("the generator: {e}")))?;
    let view = made.func.view();
    let upvars = [t.below(50, "an upvar"), t.below(50, "an upvar")];
    let split = split_planted(view, made.shape, plant, Global).map_err(|e| match e {
        CoroutineError::Output(..) => Broke::Oracle(format!("{e}\n{}", print(view))),
        _ => Broke::Harness(format!("the pass refused: {e}\n{}", print(view))),
    })?;
    for field in split.layout.fields.iter() {
        if let Saved::Slot(slot) = field.what {
            if made.temps.contains(&slot) {
                return Err(Broke::Oracle(format!("the state keeps {slot}, dead at every suspend\n{}", print(view))));
            }
        }
    }
    agree(view, made.shape, &split, upvars)
}

/// The Reference, Await expressions: "suspending its state so that, when the surrounding
/// async context is re-polled, execution returns to step 3". Prints "test
/// coroutine_tests::a_split_body_runs_as_its_body_on_every_schedule ... ok".
#[test]
fn a_split_body_runs_as_its_body_on_every_schedule() {
    check(Config::cases(400), |t| case(t, Plant::None));
}

/// Control: a slot whose address a suspend may carry, left on the stack, is read in the
/// frame of a later poll, where nothing wrote it.
#[test]
fn control_a_live_slot_left_out_of_the_state() {
    control(Config::cases(400), Plant::None, Plant::SlotLeftOut, UNINIT, case);
}

/// Control: a value the cancel path uses, not loaded from the state there.
#[test]
fn control_a_kept_value_not_reloaded_on_the_cancel_path() {
    control(Config::cases(400), Plant::None, Plant::NoReloadOnCancel, UNDEF, case);
}

/// Control: a suspend that returns before its tag is stored starts again from the top.
#[test]
fn control_the_tag_stored_after_the_return() {
    control(Config::cases(400), Plant::None, Plant::TagAfterReturn, DIVERGED, case);
}

/// Control: the tag of point `k` resuming at point `k + 1`.
#[test]
fn control_resuming_at_the_next_point() {
    control(Config::cases(400), Plant::None, Plant::ResumeNext, DIVERGED, case);
}

/// A hand-written body: one slot written before the suspend and read after it, one value
/// used after it on both paths, a constant used after it, and a slot first addressed after
/// it. The state is the sixteen bytes of upvars, the slot at 16, the value at 24 and an `i8`
/// tag at 32, forty bytes aligned to eight; the constant is made again and the late slot
/// stays on the stack of each function.
#[test]
fn the_state_holds_what_crosses_a_suspend_and_nothing_else() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    own_sig(&mut b);
    let observe_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
    let observe = b.reference(RefKind::Func, 0);
    let [entry, resume, cancel, unresumed] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    let (kept, late) = (b.slot(8, 3, SlotKind::Named), b.slot(8, 3, SlotKind::Temp));
    b.begin(entry);
    let (state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let at = b.slot_addr(kept);
    let upvar = b.load(Ty::I64, Flags::NONE, state, 0, MemAttrs::default());
    b.store(Flags::NONE, upvar, at, 0, MemAttrs::default());
    let sum = b.binary(Op::Add, Flags::NONE, upvar, cx);
    let seven = b.iconst(Ty::I64, 7);
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(0, resume, cancel, &[pending, pending]);
    b.begin(resume);
    let back = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
    let more = b.binary(Op::Add, Flags::NONE, back, sum);
    let most = b.binary(Op::Add, Flags::NONE, more, seven);
    let late_at = b.slot_addr(late);
    b.store(Flags::NONE, most, late_at, 0, MemAttrs::default());
    let last = b.load(Ty::I64, Flags::NONE, late_at, 0, MemAttrs::default());
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, last]);
    b.begin(cancel);
    b.call(Flags::NONE, observe, observe_sig, &[sum]);
    b.cancelled();
    b.begin(unresumed);
    b.cancelled();
    let func = b.finish_checked().expect("the body is LIR");
    let shape = Shape { state_param: STATE as u32, prefix_size: PREFIX, prefix_align: 3, unresumed: Block(3) };
    let split = split(func.view(), shape, Global).expect("the body splits");
    let layout = &split.layout;
    assert_eq!((layout.size, layout.align, layout.tag_offset, layout.tag_ty, layout.points), (40, 3, 32, Ty::I8, 1));
    assert_eq!(
        &layout.fields[..],
        &[
            StateField { what: Saved::Slot(kept), offset: 16, size: 8, align: 3 },
            StateField { what: Saved::Value(sum), offset: 24, size: 8, align: 3 },
        ]
    );
    assert_eq!((split.poll.slots.len(), split.drop.slots.len()), (1, 1));
    // Upvar 5, contexts 1 and 2: Pending, then Ready(5 + (5 + 1) + 7); cancelled at the
    // suspend, the cancel path sees 5 + 1.
    let direct_full = direct(func.view(), shape, [5, 9], None).expect("the body runs");
    assert_eq!(direct_full, [Event::Out(vec![PENDING, PENDING]), Event::Out(vec![READY, 18])]);
    let cancelled = machine(&split, [5, 9], Some(1), 3).expect("the machine runs");
    assert_eq!(cancelled, [Event::Out(vec![PENDING, PENDING]), Event::Observe(6), Event::Cancelled]);
    if let Err(broke) = agree(func.view(), shape, &split, [5, 9]) {
        panic!("{broke}");
    }
}

/// A `suspend` is LIR's text like any operation, and its blocks take no parameters.
#[test]
fn a_suspend_reads_back_and_its_blocks_take_no_parameters() {
    let body = |resume_takes: bool| {
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        own_sig(&mut b);
        let [entry, resume, cancel] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let (_state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
        b.suspend(0, resume, cancel, &[cx, cx]);
        b.begin(resume);
        if resume_takes {
            b.param(Ty::I64);
        }
        b.ret(&[cx, cx]);
        b.begin(cancel);
        b.cancelled();
        b.finish_checked()
    };
    let func = body(false).expect("a suspend to plain blocks is LIR");
    let text = print(func.view());
    assert!(text.contains("suspend 0") && text.contains("cancelled"), "{text}");
    assert_eq!(parse(&text).expect("the text parses").view(), func.view());
    match body(true) {
        Err(BuildError::Verify(error)) => {
            assert_eq!(error.rule, "a suspend whose resume or cancel block has parameters or is a landing block");
        }
        other => panic!("{other:?}"),
    }
}

/// The design's D7: a cancel path may not return a value, and a resume path may not reach
/// `cancelled`; both are refused by name.
#[test]
fn paths_that_end_the_wrong_way_are_refused() {
    let body = |cancel_returns: bool| {
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        own_sig(&mut b);
        let [entry, resume, cancel, unresumed] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let (_state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
        b.suspend(0, resume, cancel, &[cx, cx]);
        b.begin(resume);
        if cancel_returns {
            b.ret(&[cx, cx]);
        } else {
            b.cancelled();
        }
        b.begin(cancel);
        if cancel_returns {
            b.ret(&[cx, cx]);
        } else {
            b.cancelled();
        }
        b.begin(unresumed);
        b.cancelled();
        b.finish_checked().expect("the body is LIR")
    };
    let shape = Shape { state_param: STATE as u32, prefix_size: PREFIX, prefix_align: 3, unresumed: Block(3) };
    assert_eq!(split(body(true).view(), shape, Global).err(), Some(CoroutineError::ReturnOnCancel { block: 2 }));
    assert_eq!(split(body(false).view(), shape, Global).err(), Some(CoroutineError::CancelledOnResume { block: 1 }));
}

/// A panic inside a poll (ASYNC.md D-A5), on this file's interpreter.
#[path = "coroutine_unwind_tests.rs"]
mod unwind;

/// Overlap of kept slots (ASYNC.md D-A6), on this file's interpreter.
#[path = "coroutine_overlap_tests.rs"]
mod overlap;

/// A split into a state laid out before the LIR, its slots named by the builder
/// (coroutine_region.rs; ASYNC.md P8, async-P8-4).
#[path = "coroutine_region_tests.rs"]
mod region;
