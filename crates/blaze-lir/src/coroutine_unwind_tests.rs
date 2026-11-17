//! A panic inside a poll ([`crate::coroutine::split_with`]; ASYNC.md D-A5), tested as
//! [`super`] tests the pass: random bodies run directly and as the machine, traces equal. A
//! child of `coroutine_tests`, so it shares its interpreter, whose one invoked callee,
//! `may_unwind(i64)`, observes its argument and unwinds when it is odd.
//!
//! The rules: the Reference, "Panic": with `unwind`, "live objects in those frames that
//! implement Drop will have their drop methods called" (the cleanup path is the builder's,
//! and the split must run exactly it, in poll as in the body); and the library,
//! core/src/panicking.rs:203, `panic_const_async_fn_resumed_panic = "`async fn` resumed after
//! panicking"`: a future whose poll unwound is in a state of its own, which a later poll
//! panics on and its drop leaves alone (its cleanup already dropped what was live).

use super::*;

use crate::coroutine::{Panics, RESUMED_TRAP, split_planted_with, split_with};

/// The trap codes the hand-written body's two finished-state blocks stand for the library's
/// two panics with.
const RESUMED_AFTER_COMPLETION: u32 = 0xb1;
const RESUMED_AFTER_PANICKING: u32 = 0xb2;
/// Where the split's tag holds `Panicked`.
const PANICKED_TAG: u64 = 2;

/// A call of `may_unwind(arg)` that lands in a cleanup block: it drops the live locals, the
/// last declared first (here each shows its first cell), may look at a value made before,
/// and resumes. The body goes on in the call's normal block.
fn unwinding_call(g: &mut Gen<'_>, may_unwind: RefId) {
    let normal = g.b.create_block(BlockKind::Normal);
    let pad = g.b.create_block(BlockKind::Cleanup);
    let arg = g.int();
    let sig = g.observe_sig;
    g.b.invoke(Flags::NONE, may_unwind, sig, &[arg], normal, pad);
    g.b.begin(pad);
    let exception = g.b.param(Ty::PTR);
    for s in (0..g.slots.len()).rev() {
        let at = g.b.slot_addr(g.slots[s]);
        let seen = g.load(at, 0);
        g.observe(seen);
    }
    if g.t.below(2, "a value seen on the unwind path") == 0 {
        let value = g.int();
        g.observe(value);
    }
    g.b.resume(exception);
    g.b.begin(normal);
}

/// [`super::random_body`]'s bodies, with calls of `may_unwind` among the statements.
fn unwinding_body(t: &Tape) -> Result<Made, BuildError> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    own_sig(&mut b);
    let observe_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
    let observe = b.reference(RefKind::Func, 0);
    let may_unwind = b.reference(RefKind::Func, 1);
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
        match t.below(5, "an unwinding call") {
            0 => unwinding_call(&mut g, may_unwind),
            _ => g.statement(),
        }
    }
    while !g.open.is_empty() {
        g.close();
    }
    let value = g.int();
    let ready = g.b.iconst(Ty::I64, u128::from(READY));
    g.b.ret(&[ready, value]);
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

fn unwinding_case(t: &Tape, plant: Plant) -> Result<(), Broke> {
    let made = unwinding_body(t).map_err(|e| Broke::Harness(format!("the generator: {e}")))?;
    let view = made.func.view();
    let upvars = [t.below(50, "an upvar"), t.below(50, "an upvar")];
    let split = split_planted_with(view, made.shape, Panics::NONE, plant, Global).map_err(|e| match e {
        CoroutineError::Output(..) => Broke::Oracle(format!("{e}\n{}", print(view))),
        _ => Broke::Harness(format!("the pass refused: {e}\n{}", print(view))),
    })?;
    agree(view, made.shape, &split, upvars)
}

/// The Reference, "Panic": "live objects in those frames that implement Drop will have their
/// drop methods called": a body whose calls may unwind, split, runs its cleanup paths as the
/// body does on every schedule, leaves the state `Panicked` (2) after an unwind, and its drop
/// then does nothing. Prints "test
/// coroutine_tests::unwind::a_split_body_unwinds_as_its_body_on_every_schedule ... ok".
#[test]
fn a_split_body_unwinds_as_its_body_on_every_schedule() {
    check(Config::cases(400), |t| unwinding_case(t, Plant::None));
}

/// Control: a `resume` without the tag stored first leaves the state at its last point (or
/// unresumed), which the machine's check after an unwind catches. Prints "test
/// coroutine_tests::unwind::control_no_poison_on_unwind ... ok".
#[test]
fn control_no_poison_on_unwind() {
    control(Config::cases(400), Plant::None, Plant::NoPoisonOnUnwind, POISONED, unwinding_case);
}

/// One poll of `split`'s `poll` with context `cx`, its frame ended after.
fn poll_once(world: &mut World, split: &Split<Global>, cx: u64) -> Result<Stop, Fault> {
    let poll = split.poll.view();
    let mut frame = world.frame(poll);
    frame.values[STATE] = Some(Val::Ptr { region: 0, at: 0 });
    frame.values[CX] = Some(Val::Int(cx));
    let stop = run(world, poll, &mut frame, Block(0))?;
    world.end(&frame);
    Ok(stop)
}

fn tag_of(world: &mut World, split: &Split<Global>) -> u64 {
    let layout = &split.layout;
    match world.load(Val::Ptr { region: 0, at: 0 }, layout.tag_offset as i32, layout.tag_ty) {
        Ok(Val::Int(tag)) => tag,
        other => panic!("the tag reads as {other:?}"),
    }
}

/// The hand-written body of the test below: two named slots holding 10 and 20, a suspend,
/// then `may_unwind(1)`, which unwinds; its cleanup drops the second slot, then the first;
/// the finished-state blocks trap with the two codes above.
fn unwinding_by_hand() -> (Func<Global>, Shape, Panics) {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    own_sig(&mut b);
    let observe_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
    let observe = b.reference(RefKind::Func, 0);
    let may_unwind = b.reference(RefKind::Func, 1);
    let [entry, resume, normal] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
    let pad = b.create_block(BlockKind::Cleanup);
    let [cancel, unresumed, returned, panicked] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    let (first, second) = (b.slot(8, 3, SlotKind::Named), b.slot(8, 3, SlotKind::Named));
    let drops = |b: &mut Builder<Global, Full>| {
        for slot in [second, first] {
            let at = b.slot_addr(slot);
            let seen = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
            b.call(Flags::NONE, observe, observe_sig, &[seen]);
        }
    };
    b.begin(entry);
    let (_state, _cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    for (slot, value) in [(first, 10), (second, 20)] {
        let at = b.slot_addr(slot);
        let value = b.iconst(Ty::I64, value);
        b.store(Flags::NONE, value, at, 0, MemAttrs::default());
    }
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    b.suspend(0, resume, cancel, &[pending, pending]);
    b.begin(resume);
    let odd = b.iconst(Ty::I64, 1);
    b.invoke(Flags::NONE, may_unwind, observe_sig, &[odd], normal, pad);
    b.begin(normal);
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, odd]);
    b.begin(pad);
    let exception = b.param(Ty::PTR);
    drops(&mut b);
    b.resume(exception);
    b.begin(cancel);
    drops(&mut b);
    b.cancelled();
    b.begin(unresumed);
    b.cancelled();
    b.begin(returned);
    b.trap(RESUMED_AFTER_COMPLETION);
    b.begin(panicked);
    b.trap(RESUMED_AFTER_PANICKING);
    let func = b.finish_checked().expect("the body is LIR");
    // Begun in the order created: entry 0, resume 1, normal 2, pad 3, cancel 4, unresumed 5,
    // returned 6, panicked 7.
    let shape = Shape { state_param: STATE as u32, prefix_size: PREFIX, prefix_align: 3, unresumed: Block(5) };
    (func, shape, Panics { returned: Some(Block(6)), panicked: Some(Block(7)) })
}

/// The Reference, "Panic" (live objects' drops run while unwinding), and core/src/panicking.rs
/// :203 ("`async fn` resumed after panicking"): poll 1 is Pending; poll 2 calls
/// `may_unwind(1)`, which unwinds: the cleanup drops slot 2 (20), then slot 1 (10), in that
/// order, and the tag then reads 2 (`Panicked`); poll 3 goes to the panicked block (trap
/// 0xb2), or traps `RESUMED_TRAP` when the builder passed no block; the drop then does
/// nothing. With the planted `NoPoisonOnUnwind` the tag still names point 0 (3). Prints "test
/// coroutine_tests::unwind::a_poll_that_unwinds_drops_its_live_slots_and_poisons ... ok".
#[test]
fn a_poll_that_unwinds_drops_its_live_slots_and_poisons() {
    let (func, shape, panics) = unwinding_by_hand();
    let view = func.view();
    let run_to_unwind = |split: &Split<Global>| -> (World, u64) {
        let mut world = World::new(split.layout.size as usize, [5, 9]);
        let layout = &split.layout;
        world.store(Val::Int(0), layout.tag_ty, Val::Ptr { region: 0, at: 0 }, layout.tag_offset as i32).expect("the tag is stored");
        match poll_once(&mut world, split, 1).expect("poll 1 runs") {
            Stop::Returned(values) => world.trace.push(Event::Out(values)),
            _ => panic!("poll 1 does not return Pending"),
        }
        assert!(matches!(poll_once(&mut world, split, 2), Ok(Stop::Unwound)), "poll 2 does not unwind");
        assert_eq!(world.trace, [Event::Out(vec![PENDING, PENDING]), Event::Observe(1), Event::Observe(20), Event::Observe(10)]);
        let tag = tag_of(&mut world, split);
        (world, tag)
    };
    let split = split_with(view, shape, panics, Global).expect("the body splits");
    let (mut world, tag) = run_to_unwind(&split);
    assert_eq!(tag, PANICKED_TAG, "after an unwind the tag must be PANICKED (2), found {tag}");
    assert!(matches!(poll_once(&mut world, &split, 3), Ok(Stop::Trapped(RESUMED_AFTER_PANICKING))), "poll 3 does not take the panicked block");
    // The drop of a `Panicked` state returns at once: nothing more is observed.
    let glue = split.drop.view();
    let mut frame = world.frame(glue);
    frame.values[0] = Some(Val::Ptr { region: 0, at: 0 });
    let stop = run(&mut world, glue, &mut frame, Block(0)).expect("the drop runs");
    assert!(matches!(stop, Stop::Returned(ref values) if values.is_empty()), "the drop does not return");
    assert_eq!(world.trace.len(), 4, "the drop of a `Panicked` state observed something: {:?}", world.trace);
    // With no blocks passed, the finished state traps as before.
    let bare = split_with(view, shape, Panics::NONE, Global).expect("the body splits");
    let (mut world, _) = run_to_unwind(&bare);
    assert!(matches!(poll_once(&mut world, &bare, 3), Ok(Stop::Trapped(RESUMED_TRAP))), "poll 3 does not trap");
    // The planted bug: the tag still names point 0.
    let planted = split_planted_with(view, shape, panics, Plant::NoPoisonOnUnwind, Global).expect("the body splits");
    let (_, tag) = run_to_unwind(&planted);
    assert_eq!(tag, 3, "the planted split stored the tag anyway");
    // And the body agrees with its split on every schedule.
    if let Err(broke) = agree(view, shape, &split, [5, 9]) {
        panic!("{broke}");
    }
}

/// A cleanup path may not suspend, as a cancel path may not (D7: no async drop): refused by
/// name. Prints "test coroutine_tests::unwind::a_suspension_in_a_cleanup_path_is_refused ...
/// ok".
#[test]
fn a_suspension_in_a_cleanup_path_is_refused() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    own_sig(&mut b);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
    let may_unwind = b.reference(RefKind::Func, 1);
    // Begun in the order created: entry 0, normal 1, pad 2, resume 3, cancel 4, unresumed 5.
    let [entry, normal] = [(); 2].map(|_| b.create_block(BlockKind::Normal));
    let pad = b.create_block(BlockKind::Cleanup);
    let [resume, cancel, unresumed] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let (_state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    b.invoke(Flags::NONE, may_unwind, sig, &[cx], normal, pad);
    b.begin(normal);
    b.ret(&[cx, cx]);
    b.begin(pad);
    let _exception = b.param(Ty::PTR);
    b.suspend(0, resume, cancel, &[cx, cx]);
    b.begin(resume);
    b.ret(&[cx, cx]);
    b.begin(cancel);
    b.cancelled();
    b.begin(unresumed);
    b.cancelled();
    let func = b.finish_checked().expect("the body is LIR");
    let shape = Shape { state_param: STATE as u32, prefix_size: PREFIX, prefix_align: 3, unresumed: Block(5) };
    match split(func.view(), shape, Global) {
        Err(CoroutineError::Refused { op: Op::Suspend, why, .. }) => assert_eq!(why, "a suspension in a cleanup path"),
        other => panic!("{other:?}"),
    }
}
