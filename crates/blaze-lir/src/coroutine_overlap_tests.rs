//! Overlap of kept slots (coroutine_overlap.rs; ASYNC.md D-A6), tested as [`super`] tests the
//! pass: bodies run directly and as the machine, traces equal. A child of `coroutine_tests`,
//! so it shares its interpreter, in which `slot_live` and `slot_dead` make a slot's bytes
//! uninitialised in the direct run, while the machine's state keeps whatever was stored last:
//! two slots wrongly sharing bytes show as a value of one read back as the other's.
//!
//! The rules: the Reference, "Await expressions": a future is "roughly equivalent to an enum
//! with one variant per await point", so futures awaited one after another need no bytes of
//! their own each; and the Reference, "Destructors": a value is dropped "when control flow
//! leaves a drop scope", after which its storage holds nothing a program may read.
//!
//! Overlap is always on in the pass, so [`super`]'s own random bodies (slots never ended,
//! addresses held in values and memory, loops, ifs: every kept slot there conflicts with every
//! other it meets) run with it in `a_split_body_runs_as_its_body_on_every_schedule`.

use super::*;

/// The most `i64` cells a random future here has.
const FUTURE_CELLS_MOST: u64 = 4;

/// The value stored at `cell` of future `i` the `made`-th time a future is made live: distinct
/// for every store, so a byte read back from another future's store shows in the trace.
fn cell_value(made: u64, i: usize, cell: u32) -> u128 {
    u128::from(1000 * made + 10 * i as u64 + u64::from(cell) + 1)
}

/// A body writing futures as the builder writes an `.await`'s: a slot made live and filled,
/// suspends while it is live (each cancel path showing the first cell of every live future,
/// as their drops would), then read whole and ended with `slot_dead`. Futures live in any
/// interleaving: one held across several others, others in sequence.
fn awaits_body(t: &Tape) -> Result<(Func<Global>, Shape), BuildError> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    own_sig(&mut b);
    let observe_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
    let observe = b.reference(RefKind::Func, 0);
    let (entry, unresumed) = (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal));
    b.begin(entry);
    let (_state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    let count = 2 + t.below(3, "futures") as usize;
    let mut futures = Vec::new();
    for _ in 0..count {
        let cells = 1 + t.below(FUTURE_CELLS_MOST, "a future's cells") as u32;
        futures.push((b.slot(8 * cells, 3, SlotKind::Temp), cells));
    }
    let mut live = vec![false; count];
    let (mut made, mut points) = (0u64, 0u32);
    let finish = |b: &mut Builder<Global, Full>, (slot, cells): (SlotId, u32)| {
        for cell in 0..cells {
            let at = b.slot_addr(slot);
            let seen = b.load(Ty::I64, Flags::NONE, at, 8 * cell as i32, MemAttrs::default());
            b.call(Flags::NONE, observe, observe_sig, &[seen]);
        }
        b.slot_dead(slot);
    };
    for _ in 0..t.below(30, "events") {
        let i = t.below(count as u64, "a future") as usize;
        match t.below(4, "an event") {
            0 if !live[i] => {
                let (slot, cells) = futures[i];
                b.slot_live(slot);
                made += 1;
                for cell in 0..cells {
                    let at = b.slot_addr(slot);
                    let value = b.iconst(Ty::I64, cell_value(made, i, cell));
                    b.store(Flags::NONE, value, at, 8 * cell as i32, MemAttrs::default());
                }
                live[i] = true;
            }
            1 if live[i] => {
                finish(&mut b, futures[i]);
                live[i] = false;
            }
            2 if live[i] => {
                let (slot, cells) = futures[i];
                let cell = t.below(u64::from(cells), "a cell") as i32;
                let at = b.slot_addr(slot);
                let seen = b.load(Ty::I64, Flags::NONE, at, 8 * cell, MemAttrs::default());
                b.call(Flags::NONE, observe, observe_sig, &[seen]);
            }
            _ => {
                let (resume, cancel) = (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal));
                let pending = b.iconst(Ty::I64, u128::from(PENDING));
                b.suspend(points, resume, cancel, &[pending, pending]);
                points += 1;
                b.begin(cancel);
                for (j, &(slot, _)) in futures.iter().enumerate() {
                    if live[j] {
                        let at = b.slot_addr(slot);
                        let seen = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
                        b.call(Flags::NONE, observe, observe_sig, &[seen]);
                    }
                }
                b.cancelled();
                b.begin(resume);
            }
        }
    }
    for i in 0..count {
        if live[i] {
            finish(&mut b, futures[i]);
        }
    }
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, cx]);
    b.begin(unresumed);
    b.cancelled();
    let func = b.finish_checked()?;
    // Blocks are numbered in the order they were begun: the unresumed block is the last.
    let unresumed = Block(func.block_starts.len() as u32 - 1);
    Ok((func, Shape { state_param: STATE as u32, prefix_size: PREFIX, prefix_align: 3, unresumed }))
}

fn awaits_case(t: &Tape, plant: Plant) -> Result<(), Broke> {
    let (func, shape) = awaits_body(t).map_err(|e| Broke::Harness(format!("the generator: {e}")))?;
    let view = func.view();
    let upvars = [t.below(50, "an upvar"), t.below(50, "an upvar")];
    let split = split_planted(view, shape, plant, Global).map_err(|e| match e {
        CoroutineError::Output(..) => Broke::Oracle(format!("{e}\n{}", print(view))),
        _ => Broke::Harness(format!("the pass refused: {e}\n{}", print(view))),
    })?;
    agree(view, shape, &split, upvars)
}

/// The Reference, "Await expressions" and "Destructors": futures made live and ended in any
/// interleaving across suspends, their kept slots overlapping where no point has two live,
/// run as the body does on every schedule. Prints "test
/// coroutine_tests::overlap::kept_slots_that_share_bytes_run_as_the_body_on_every_schedule ...
/// ok".
#[test]
fn kept_slots_that_share_bytes_run_as_the_body_on_every_schedule() {
    check(Config::cases(400), |t| awaits_case(t, Plant::None));
}

/// Control: every kept slot placed at one offset, conflicts ignored: a future held across
/// another's life reads back the other's bytes. Prints "test
/// coroutine_tests::overlap::control_conflicting_slots_placed_on_each_other ... ok".
#[test]
fn control_conflicting_slots_placed_on_each_other() {
    control(Config::cases(400), Plant::None, Plant::OverlapConflicting, DIVERGED, awaits_case);
}

/// A body of two sequential awaits' futures of 64 bytes, each made live and filled, held
/// across one suspend, read and ended; and, when `hold` says, `held`, a slot of 8 bytes made
/// live before the first and read after the last. No prefix: what the two tests below lay out.
fn sequential_awaits(hold: bool) -> (Func<Global>, Shape, [SlotId; 2], SlotId) {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    own_sig(&mut b);
    let observe_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
    let observe = b.reference(RefKind::Func, 0);
    let [entry, r0, c0, r1, c1, unresumed] = [(); 6].map(|_| b.create_block(BlockKind::Normal));
    let futures = [b.slot(64, 3, SlotKind::Temp), b.slot(64, 3, SlotKind::Temp)];
    let held = b.slot(8, 3, SlotKind::Named);
    b.begin(entry);
    let (_state, cx) = (b.param(Ty::PTR), b.param(Ty::I64));
    if hold {
        b.slot_live(held);
        let at = b.slot_addr(held);
        let value = b.iconst(Ty::I64, 77);
        b.store(Flags::NONE, value, at, 0, MemAttrs::default());
    }
    let pending = b.iconst(Ty::I64, u128::from(PENDING));
    let blocks = [(r0, c0), (r1, c1)];
    for (k, &future) in futures.iter().enumerate() {
        b.slot_live(future);
        for cell in 0..8u32 {
            let at = b.slot_addr(future);
            let value = b.iconst(Ty::I64, cell_value(1, k, cell));
            b.store(Flags::NONE, value, at, 8 * cell as i32, MemAttrs::default());
        }
        let (resume, cancel) = blocks[k];
        b.suspend(k as u32, resume, cancel, &[pending, pending]);
        // The cancel path drops the future (here: shows its first and last cells), and the
        // held slot when there is one.
        b.begin(cancel);
        for cell in [0, 7] {
            let at = b.slot_addr(future);
            let seen = b.load(Ty::I64, Flags::NONE, at, 8 * cell, MemAttrs::default());
            b.call(Flags::NONE, observe, observe_sig, &[seen]);
        }
        if hold {
            let at = b.slot_addr(held);
            let seen = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
            b.call(Flags::NONE, observe, observe_sig, &[seen]);
        }
        b.cancelled();
        // Ready: the payload moved out (every cell shown), then the future's storage ends.
        b.begin(resume);
        for cell in 0..8 {
            let at = b.slot_addr(future);
            let seen = b.load(Ty::I64, Flags::NONE, at, 8 * cell, MemAttrs::default());
            b.call(Flags::NONE, observe, observe_sig, &[seen]);
        }
        b.slot_dead(future);
    }
    if hold {
        let at = b.slot_addr(held);
        let seen = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
        b.call(Flags::NONE, observe, observe_sig, &[seen]);
        b.slot_dead(held);
    }
    let ready = b.iconst(Ty::I64, u128::from(READY));
    b.ret(&[ready, cx]);
    b.begin(unresumed);
    b.cancelled();
    let func = b.finish_checked().expect("the body is LIR");
    let shape = Shape { state_param: STATE as u32, prefix_size: 0, prefix_align: 0, unresumed: Block(5) };
    (func, shape, futures, held)
}

/// The byte range a kept slot takes in `split`'s state.
fn range_of(split: &Split<Global>, slot: SlotId) -> (u32, u32) {
    let field = split
        .layout
        .fields
        .iter()
        .find(|field| field.what == Saved::Slot(slot))
        .unwrap_or_else(|| panic!("{slot} is not kept in the state"));
    (field.offset, field.offset + field.size)
}

/// The Reference, "Await expressions": "one variant per await point": two 64-byte futures
/// awaited one after the other share their bytes, so the state is 64 bytes, the `i8` tag at
/// 64, 72 in all. With overlap off (`Plant::NoOverlap`) it is 64 + 64 + the tag, 136; the
/// assertion's text says so. Prints "test
/// coroutine_tests::overlap::two_awaits_in_sequence_share_bytes ... ok".
#[test]
fn two_awaits_in_sequence_share_bytes() {
    let (func, shape, futures, _) = sequential_awaits(false);
    let split = split(func.view(), shape, Global).expect("the body splits");
    let size = split.layout.size;
    assert!(size <= 72, "state of two sequential 64-byte awaits must be at most 72 bytes, was {size}");
    assert_eq!(range_of(&split, futures[0]), range_of(&split, futures[1]), "the two futures' bytes are one range");
    if let Err(broke) = agree(func.view(), shape, &split, [5, 9]) {
        panic!("{broke}");
    }
    // The control: overlap off, the same body's state is the sum.
    let planted = split_planted(func.view(), shape, Plant::NoOverlap, Global).expect("the body splits");
    assert_eq!(planted.layout.size, 136, "with overlap off the state is 64 + 64 + the tag, aligned to 8");
}

/// A slot live across both awaits (its address taken before the first, read after the last)
/// conflicts with both futures, so it shares bytes with neither, while the futures still share
/// theirs. Prints "test coroutine_tests::overlap::a_slot_borrowed_across_both_awaits_does_not_share
/// ... ok".
#[test]
fn a_slot_borrowed_across_both_awaits_does_not_share() {
    let (func, shape, futures, held) = sequential_awaits(true);
    let split = split(func.view(), shape, Global).expect("the body splits");
    let kept = range_of(&split, held);
    for future in futures {
        let other = range_of(&split, future);
        assert!(
            kept.1 <= other.0 || other.1 <= kept.0,
            "the held slot {held} at {kept:?} shares bytes with the future {future} at {other:?}"
        );
    }
    assert_eq!(range_of(&split, futures[0]), range_of(&split, futures[1]), "the two futures' bytes are one range");
    assert_eq!(split.layout.size, 80, "64 for the futures, 8 for the held slot, the `i8` tag, aligned to 8");
    if let Err(broke) = agree(func.view(), shape, &split, [5, 9]) {
        panic!("{broke}");
    }
}
