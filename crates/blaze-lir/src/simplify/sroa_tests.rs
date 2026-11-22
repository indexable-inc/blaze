//! Scalar replacement ([`super::sroa`]) against the evaluator: on generated programs whose
//! slots are stored and loaded straight, through constant `ptr_add`s, across branches and
//! counted loops and around `slot_live`/`slot_dead`, the output verifies, keeps only the slot
//! whose address is offset by a value, and runs as the input does (`eval::run_in` on both, the
//! same arguments). A program the evaluator finds undefined says nothing and passes. The
//! control plants a stale store (`Plant::KeepStaleStore`) and the oracle must catch it.
//!
//! The rule checked is the Reference's (Behavior considered undefined; Place expressions): a
//! place holds the value last written to it, wherever that value now lives.

use std::alloc::Global;

use blaze_sim::{Broke, Config, Tape, check, control};

use super::sroa::{Plant, scalar_replace, scalar_replace_planted};
use crate::{
    Block, BlockKind, BuildError, Builder, Flags, Full, Func, IntCond, Op, Ty, Value,
    builder::{MemAttrs, Param, Variadic},
    eval::{Outcome, Val, run_in},
    func::{Conv, SlotId, SlotKind},
    print, verify,
};

/// Far past what a generated run takes (at most nine passes of a loop body).
const FUEL: u64 = 200_000;
/// Each replaced slot is three `i64` cells, at offsets 0, 8 and 16.
const CELLS: u32 = 3;

/// A construct the generator has opened and not yet closed.
enum Open {
    /// In the `then` block: `otherwise` and `join` still to come.
    Then { otherwise: Block, join: Block, mark: usize },
    /// In the `else` block.
    Else { join: Block, mark: usize },
    /// In a loop's body, its counter `i` at `header`.
    Loop { header: Block, exit: Block, i: Value, mark: usize },
}

struct Gen<'t> {
    t: &'t Tape,
    b: Builder<Global, Full>,
    /// The slots the pass must replace.
    slots: Vec<SlotId>,
    /// The slot whose cells are reached through a `ptr_add` by a value: it must stay.
    escaping: SlotId,
    /// The `i64` values usable here: what dominates the current block.
    ints: Vec<Value>,
    open: Vec<Open>,
}

impl Gen<'_> {
    fn int(&mut self) -> Value {
        self.ints[self.t.index(self.ints.len(), "an int")]
    }

    /// A pointer and an access offset that together reach cell `cell` of `slot`: straight,
    /// through a `ptr_add` by an `iconst`, or split between the two.
    fn place(&mut self, slot: SlotId, cell: u32) -> (Value, i32) {
        let base = self.b.slot_addr(slot);
        match self.t.below(3, "addressing") {
            0 => (base, 8 * cell as i32),
            1 => {
                let by = self.b.iconst(Ty::I64, u128::from(8 * cell));
                (self.b.ptr_add(Flags::NONE, base, by), 0)
            }
            _ => {
                let by = self.b.iconst(Ty::I64, 8);
                let inner = self.b.ptr_add(Flags::NONE, base, by);
                (inner, 8 * cell as i32 - 8)
            }
        }
    }

    fn store(&mut self, slot: SlotId, cell: u32, value: Value) {
        let (ptr, offset) = self.place(slot, cell);
        self.b.store(Flags::NONE, value, ptr, offset, MemAttrs::default());
    }

    fn load(&mut self, slot: SlotId, cell: u32) -> Value {
        let (ptr, offset) = self.place(slot, cell);
        self.b.load(Ty::I64, Flags::NONE, ptr, offset, MemAttrs::default())
    }

    /// A pointer to cell 0 or 1 of the escaping slot, picked by a value.
    fn escaping_cell(&mut self) -> Value {
        let value = self.int();
        let one = self.b.iconst(Ty::I64, 1);
        let bit = self.b.binary(Op::And, Flags::NONE, value, one);
        let eight = self.b.iconst(Ty::I64, 8);
        let by = self.b.binary(Op::Mul, Flags::NONE, bit, eight);
        let base = self.b.slot_addr(self.escaping);
        self.b.ptr_add(Flags::NONE, base, by)
    }

    fn statement(&mut self) {
        if !self.open.is_empty() && self.t.one_in(5, "close") {
            self.close();
            return;
        }
        let (slots, depth) = (self.slots.len(), self.open.len());
        match self.t.below(9, "statement") {
            0 | 1 => {
                let slot = self.slots[self.t.index(slots, "a slot")];
                let cell = self.t.below(u64::from(CELLS), "a cell") as u32;
                let value = self.int();
                self.store(slot, cell, value);
            }
            2 | 3 => {
                let slot = self.slots[self.t.index(slots, "a slot")];
                let cell = self.t.below(u64::from(CELLS), "a cell") as u32;
                let value = self.load(slot, cell);
                self.ints.push(value);
            }
            4 => {
                let op = [Op::Add, Op::Sub, Op::Mul, Op::Xor][self.t.index(4, "an operation")];
                let (a, b) = (self.int(), self.int());
                let value = self.b.binary(op, Flags::NONE, a, b);
                self.ints.push(value);
            }
            5 if depth < 2 => {
                let (a, b) = (self.int(), self.int());
                let cond = self.b.icmp(IntCond::Slt, a, b);
                let [then, otherwise, join] = [(); 3].map(|_| self.b.create_block(BlockKind::Normal));
                self.b.br(Flags::NONE, cond, (then, &[]), (otherwise, &[]));
                self.b.begin(then);
                self.open.push(Open::Then { otherwise, join, mark: self.ints.len() });
            }
            6 if depth < 2 => {
                let count = 1 + self.t.below(3, "a trip count");
                let [header, body, exit] = [(); 3].map(|_| self.b.create_block(BlockKind::Normal));
                let zero = self.b.iconst(Ty::I64, 0);
                self.b.jump((header, &[zero]));
                self.b.begin(header);
                let i = self.b.param(Ty::I64);
                let limit = self.b.iconst(Ty::I64, u128::from(count));
                let more = self.b.icmp(IntCond::Slt, i, limit);
                self.b.br(Flags::NONE, more, (body, &[]), (exit, &[]));
                self.b.begin(body);
                let mark = self.ints.len();
                self.ints.push(i);
                self.open.push(Open::Loop { header, exit, i, mark });
            }
            7 => {
                // The slot's bytes start again uninitialised; most times every cell is
                // written again before anything reads it.
                let slot = self.slots[self.t.index(slots, "a slot")];
                if self.t.one_in(2, "dead first") {
                    self.b.slot_dead(slot);
                }
                self.b.slot_live(slot);
                let skipped = match self.t.one_in(4, "a cell left unwritten") {
                    true => self.t.below(u64::from(CELLS), "which cell") as u32,
                    false => CELLS,
                };
                for cell in (0..CELLS).filter(|&cell| cell != skipped) {
                    let value = self.int();
                    self.store(slot, cell, value);
                }
            }
            _ => {
                let ptr = self.escaping_cell();
                match self.t.one_in(2, "escaping store") {
                    true => {
                        let value = self.int();
                        self.b.store(Flags::NONE, value, ptr, 0, MemAttrs::default());
                    }
                    false => {
                        let value = self.b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::default());
                        self.ints.push(value);
                    }
                }
            }
        }
    }

    fn close(&mut self) {
        let Some(open) = self.open.pop() else { return };
        match open {
            Open::Then { otherwise, join, mark } => {
                let value = self.int();
                self.b.jump((join, &[value]));
                self.ints.truncate(mark);
                self.b.begin(otherwise);
                self.open.push(Open::Else { join, mark });
            }
            Open::Else { join, mark } => {
                let value = self.int();
                self.b.jump((join, &[value]));
                self.ints.truncate(mark);
                self.b.begin(join);
                let merged = self.b.param(Ty::I64);
                self.ints.push(merged);
            }
            Open::Loop { header, exit, i, mark } => {
                let one = self.b.iconst(Ty::I64, 1);
                let next = self.b.binary(Op::Add, Flags::NONE, i, one);
                self.b.jump((header, &[next]));
                self.ints.truncate(mark);
                self.b.begin(exit);
            }
        }
    }
}

/// `(i64, i64) -> i64`, as every function here is.
fn signature(b: &mut Builder<Global, Full>) {
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
}

fn random_body(t: &Tape) -> Result<Func<Global>, BuildError> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    signature(&mut b);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::I64), b.param(Ty::I64));
    let mut slots = Vec::new();
    for _ in 0..1 + t.below(3, "slots") {
        slots.push(b.slot(8 * CELLS, 3, SlotKind::Temp));
    }
    let escaping = b.slot(16, 3, SlotKind::Temp);
    let mut g = Gen { t, b, slots, escaping, ints: vec![x, y], open: Vec::new() };
    // Every cell written before anything reads one.
    for s in 0..g.slots.len() {
        for cell in 0..CELLS {
            let value = g.b.iconst(Ty::I64, 10 * s as u128 + u128::from(cell) + 1);
            let slot = g.slots[s];
            g.store(slot, cell, value);
        }
    }
    for cell in 0..2 {
        let value = g.b.iconst(Ty::I64, 100 + cell);
        let base = g.b.slot_addr(g.escaping);
        g.b.store(Flags::NONE, value, base, 8 * cell as i32, MemAttrs::default());
    }
    for _ in 0..1 + t.below(30, "statements") {
        g.statement();
    }
    while !g.open.is_empty() {
        g.close();
    }
    // What the run returns: a value here, one cell of the escaping slot (read through a
    // pointer offset by a value, so the slot escapes in every program) and one of each slot.
    let first = g.int();
    let ptr = g.escaping_cell();
    let escaped = g.b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::default());
    let mut sum = g.b.binary(Op::Add, Flags::NONE, first, escaped);
    for s in 0..g.slots.len() {
        let slot = g.slots[s];
        let cell = t.below(u64::from(CELLS), "a returned cell") as u32;
        let value = g.load(slot, cell);
        sum = g.b.binary(Op::Add, Flags::NONE, sum, value);
    }
    g.b.ret(&[sum]);
    g.b.finish_checked()
}

fn case(t: &Tape, plant: Plant) -> Result<(), Broke> {
    let func = random_body(t).map_err(|e| Broke::Harness(format!("the generator: {e}")))?;
    let view = func.view();
    let out = scalar_replace_planted(view, plant, Global)
        .map_err(|e| Broke::Oracle(format!("the pass refused a verified function: {e}\n{}", print(view))))?;
    verify(out.view())
        .map_err(|e| Broke::Oracle(format!("the output does not verify: {e}\n{}", print(out.view()))))?;
    if out.slots.len() != 1 {
        return Err(Broke::Oracle(format!(
            "{} slots kept, where only the escaping one must be\n{}",
            out.slots.len(),
            print(out.view())
        )));
    }
    let args = [Val::Int(u128::from(t.below(100, "x"))), Val::Int(u128::from(t.below(100, "y")))];
    let before = match run_in(view, &args, FUEL, Global) {
        Ok(Outcome::Undefined { .. } | Outcome::OutOfFuel) => return Ok(()),
        Ok(outcome) => outcome,
        Err(e) => return Err(Broke::Harness(format!("the evaluator refused the input: {e}\n{}", print(view)))),
    };
    let after = run_in(out.view(), &args, FUEL, Global)
        .map_err(|e| Broke::Oracle(format!("the evaluator refused the output: {e}\n{}", print(out.view()))))?;
    if before != after {
        return Err(Broke::Oracle(format!(
            "differs after scalar replacement: {before:?} became {after:?}\n{}\n{}",
            print(view),
            print(out.view())
        )));
    }
    Ok(())
}

/// Every generated program runs the same after its slots are replaced. Prints "test
/// simplify::sroa_tests::a_replaced_program_runs_as_its_input ... ok".
#[test]
fn a_replaced_program_runs_as_its_input() {
    check(Config::cases(512), |t| case(t, Plant::None));
}

/// Control: a second store to a field not seen by the loads after it. Prints "plant
/// KeepStaleStore: caught at case k of 512" on standard error, then "test
/// simplify::sroa_tests::control_a_stale_store_kept ... ok".
#[test]
fn control_a_stale_store_kept() {
    control(Config::cases(512), Plant::None, Plant::KeepStaleStore, "differs after scalar replacement", case);
}

/// How many instructions of `func` are `op`.
fn count(func: &Func<Global>, op: Op) -> usize {
    let view = func.view();
    (0..view.len()).filter(|&at| view.op(at) == op).count()
}

/// `f(a, b)`: two cells of one slot written with `a` and `b`, read back, subtracted. The slot
/// and every memory instruction go; `f(7, 3)` still returns 4. Prints "test
/// simplify::sroa_tests::a_slot_only_loaded_and_stored_becomes_values ... ok".
#[test]
fn a_slot_only_loaded_and_stored_becomes_values() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    signature(&mut b);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::I64), b.param(Ty::I64));
    let slot = b.slot(16, 3, SlotKind::Named);
    let ptr = b.slot_addr(slot);
    b.store(Flags::NONE, x, ptr, 0, MemAttrs::default());
    b.store(Flags::NONE, y, ptr, 8, MemAttrs::default());
    let (p, q) = (
        b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::default()),
        b.load(Ty::I64, Flags::NONE, ptr, 8, MemAttrs::default()),
    );
    let difference = b.binary(Op::Sub, Flags::NONE, p, q);
    b.ret(&[difference]);
    let func = b.finish_checked().expect("the function builds");
    let out = scalar_replace(func.view(), Global).expect("the pass takes a verified function");
    verify(out.view()).expect("the output verifies");
    assert_eq!(out.slots.len(), 0, "the slot is gone:\n{}", print(out.view()));
    for op in [Op::SlotAddr, Op::Load, Op::Store] {
        assert_eq!(count(&out, op), 0, "no `{}` is left:\n{}", op.name(), print(out.view()));
    }
    let outcome = run_in(out.view(), &[Val::Int(7), Val::Int(3)], FUEL, Global).expect("the output runs");
    assert!(matches!(outcome, Outcome::Returned(ref values) if values[..] == [Val::Int(4)]), "{outcome:?}");
}

/// A cell written on both arms of a branch and read where they meet: the meeting block gains
/// one parameter and each arm passes its value. Prints "test
/// simplify::sroa_tests::a_cell_written_on_two_arms_meets_in_a_parameter ... ok".
#[test]
fn a_cell_written_on_two_arms_meets_in_a_parameter() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    signature(&mut b);
    let [entry, then, otherwise, join] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let (x, y) = (b.param(Ty::I64), b.param(Ty::I64));
    let slot = b.slot(8, 3, SlotKind::Temp);
    let cond = b.icmp(IntCond::Slt, x, y);
    b.br(Flags::NONE, cond, (then, &[]), (otherwise, &[]));
    b.begin(then);
    let ptr = b.slot_addr(slot);
    b.store(Flags::NONE, x, ptr, 0, MemAttrs::default());
    b.jump((join, &[]));
    b.begin(otherwise);
    let ptr = b.slot_addr(slot);
    b.store(Flags::NONE, y, ptr, 0, MemAttrs::default());
    b.jump((join, &[]));
    b.begin(join);
    let ptr = b.slot_addr(slot);
    let value = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::default());
    b.ret(&[value]);
    let func = b.finish_checked().expect("the function builds");
    let out = scalar_replace(func.view(), Global).expect("the pass takes a verified function");
    verify(out.view()).expect("the output verifies");
    assert_eq!(out.view().params(join).len(), 1, "the meeting block's parameter:\n{}", print(out.view()));
    for (args, want) in [([2, 5], 2), ([9, 4], 4)] {
        let args = args.map(Val::Int);
        let outcome = run_in(out.view(), &args, FUEL, Global).expect("the output runs");
        assert!(matches!(outcome, Outcome::Returned(ref values) if values[..] == [Val::Int(want)]), "{outcome:?}");
    }
}

/// A slot reached through a `ptr_add` by a parameter, and one read as `i32` inside an `i64`
/// cell, stay in memory as they were. Prints "test
/// simplify::sroa_tests::a_slot_offset_by_a_value_or_read_across_types_stays ... ok".
#[test]
fn a_slot_offset_by_a_value_or_read_across_types_stays() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    signature(&mut b);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::I64), b.param(Ty::I64));
    let (offset_by_value, two_types) = (b.slot(16, 3, SlotKind::Temp), b.slot(8, 3, SlotKind::Temp));
    let base = b.slot_addr(offset_by_value);
    b.store(Flags::NONE, x, base, 0, MemAttrs::default());
    b.store(Flags::NONE, x, base, 8, MemAttrs::default());
    let eight = b.iconst(Ty::I64, 8);
    let zero = b.iconst(Ty::I64, 0);
    let small = b.icmp(IntCond::Slt, y, x);
    let by = b.select(Flags::NONE, small, eight, zero);
    let ptr = b.ptr_add(Flags::NONE, base, by);
    let first = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::default());
    let other = b.slot_addr(two_types);
    b.store(Flags::NONE, y, other, 0, MemAttrs::default());
    let low = b.load(Ty::I32, Flags::NONE, other, 0, MemAttrs::default());
    let wide = b.convert(Op::Zext, Flags::NONE, Ty::I64, low);
    let sum = b.binary(Op::Add, Flags::NONE, first, wide);
    b.ret(&[sum]);
    let func = b.finish_checked().expect("the function builds");
    let out = scalar_replace(func.view(), Global).expect("the pass takes a verified function");
    verify(out.view()).expect("the output verifies");
    assert_eq!(out.slots.len(), 2, "both slots stay:\n{}", print(out.view()));
    assert_eq!(count(&out, Op::Load), 2, "both loads stay:\n{}", print(out.view()));
    let args = [Val::Int(5), Val::Int(1)];
    let before = run_in(func.view(), &args, FUEL, Global).expect("the input runs");
    let after = run_in(out.view(), &args, FUEL, Global).expect("the output runs");
    assert_eq!(before, after, "the same run");
}

/// A function with no slots takes the early-return fast path and keeps its exact semantics.
#[test]
fn a_function_with_no_slots_takes_the_fast_path() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    signature(&mut b);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::I64), b.param(Ty::I64));
    let sum = b.binary(Op::Add, Flags::NONE, x, y);
    b.ret(&[sum]);
    let func = b.finish_checked().expect("the function builds");
    let out = scalar_replace(func.view(), Global).expect("the pass takes a verified function");
    verify(out.view()).expect("the output verifies");
    assert_eq!(out.slots.len(), 0);
    let args = [Val::Int(19), Val::Int(23)];
    assert_eq!(
        run_in(out.view(), &args, FUEL, Global).expect("the output runs"),
        Outcome::<Global>::Returned(vec![Val::Int(42)])
    );
}
