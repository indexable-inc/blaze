//! `ptr_add inbounds` evaluates (eval/promises.rs): a step inside its slot or to one past its
//! end computes the same address as without the flag; a step beyond is undefined behaviour
//! named by the promise it breaks; the same step without the flag is defined (it wraps);
//! `ptr_add nuw` stays refused by name.
//!
//! The rule, library/core/src/ptr/docs/offset.md in the pinned core: "If the computed offset
//! is non-zero, then `self` must be derived from a pointer to some allocation, and the entire
//! memory range between `self` and `result` ... must be in bounds of that allocation."; and
//! "`vec.as_ptr().offset(vec.len() as isize)` (for `vec: Vec<T>`) is always safe" (one past
//! the end is in bounds).

use std::alloc::Global;

use super::{EvalError, Outcome, Val, run_in};
use crate::{
    builder::{Builder, Full, MemAttrs, Param, Variadic},
    flags::Flags,
    func::{BlockKind, Conv, Func, SlotKind, Value},
    op::Op,
    ty::Ty,
};

const FUEL: u64 = 10_000;

/// `f(by: i64) -> i32`: an 8-byte slot holds `0x11` at byte 0 and `0x22` at byte 4;
/// `p = ptr_add <flags> base, by`, `q = ptr_add <flags> p, 4 - by` (back to byte 4), and `f`
/// returns the `i32` at `q`: `0x22` whenever both steps are defined. Also `p`'s instruction.
fn stepper(flags: Flags) -> (Func<Global>, Value) {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let by = b.param(Ty::I64);
    let slot = b.slot(8, 2, SlotKind::Temp);
    let base = b.slot_addr(slot);
    let first = b.iconst(Ty::I32, 0x11);
    b.store(Flags::NONE, first, base, 0, MemAttrs::default());
    let second = b.iconst(Ty::I32, 0x22);
    b.store(Flags::NONE, second, base, 4, MemAttrs::default());
    let p = b.ptr_add(flags, base, by);
    let four = b.iconst(Ty::I64, 4);
    let back = b.binary(Op::Sub, Flags::NONE, four, by);
    let q = b.ptr_add(flags, p, back);
    let read = b.load(Ty::I32, Flags::NONE, q, 0, MemAttrs::default());
    b.ret(&[read]);
    (b.finish_checked().expect("the stepper builds"), p)
}

fn run(f: &Func<Global>, by: i64) -> Outcome<Global> {
    run_in(f.view(), &[Val::Int(u128::from(by as u64))], FUEL, Global)
        .expect("the evaluator models `ptr_add inbounds`")
}

/// Expected: `test eval::promises_tests::ptr_add_inbounds_evaluates_and_a_step_beyond_its_slot_is_undefined ... ok`.
/// Without promises.rs's wiring every case is refused: "the evaluator models `ptr_add
/// inbounds`: Unsupported { .., op: PtrAdd, what: \"an instruction with flags\" }".
#[test]
fn ptr_add_inbounds_evaluates_and_a_step_beyond_its_slot_is_undefined() {
    let (f, p) = stepper(Flags::INBOUNDS);
    let read = Outcome::<Global>::Returned(vec![Val::Int(0x22)]);
    // In bounds: to byte 4, then a zero step.
    assert_eq!(run(&f, 4), read, "a step inside the slot");
    // To byte 0 and back: a zero step first.
    assert_eq!(run(&f, 0), read, "a zero step");
    // One past the end (byte 8) is in bounds; the step back to byte 4 is too.
    assert_eq!(run(&f, 8), read, "a step to one past the end");
    // Byte 9 and byte -1 are outside the slot: undefined at `p`, named.
    let beyond = Outcome::<Global>::Undefined { inst: p.index(), why: "an `inbounds` step past its slot's bounds" };
    assert_eq!(run(&f, 9), beyond, "a step past one past the end");
    assert_eq!(run(&f, -1), beyond, "a step before the start");

    // Without the flag the same steps are defined: the address wraps out and back.
    let (plain, _) = stepper(Flags::NONE);
    assert_eq!(run(&plain, 9), read, "no promise, no undefined behaviour");
    assert_eq!(run(&plain, -1), read, "no promise, no undefined behaviour");
}

/// `ptr_add nuw` promises of the absolute address, which a run does not hold: refused by name.
/// Expected: `test eval::promises_tests::ptr_add_nuw_is_refused_by_name ... ok`.
#[test]
fn ptr_add_nuw_is_refused_by_name() {
    let (f, p) = stepper(Flags::INBOUNDS.union(Flags::NUW));
    let refused = run_in(f.view(), &[Val::Int(4)], FUEL, Global);
    assert_eq!(
        refused.map(|_| ()),
        Err(EvalError::Unsupported {
            inst: p.index(),
            op: Op::PtrAdd,
            what: "`nuw` on a `ptr_add` (its promise is about the absolute address, which a run does not hold)",
        })
    );
}
