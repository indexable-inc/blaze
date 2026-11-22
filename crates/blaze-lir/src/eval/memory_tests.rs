//! The evaluator's memory ([`super::memory`]) and the values wider than an integer it now runs:
//! pointers stored and loaded with their provenance, a value stored whole and loaded by parts,
//! pointers passed into and out of calls, a call's several values, the bulk operations, and
//! undefined behaviour refused by name. Each expectation is from LIR's own semantics
//! (docs/v2/plan/design/lir.md), quoted at the test.

use std::alloc::Global;

use super::{Bodies, BodyId, EvalError, Outcome, Val, run_calls_in, run_in};
use crate::{
    View,
    builder::{Builder, Full, MemAttrs, Param, Variadic},
    flags::{Flags, IntCond},
    func::{BlockKind, Conv, Func, RefId, RefKind, SlotKind, Value},
    op::Op,
    ty::Ty,
};

const FUEL: u64 = 100_000;
const F: BodyId = BodyId([1; 32]);
const G: BodyId = BodyId([2; 32]);

/// A builder of a function taking `params` and returning `rets`, its entry block begun.
fn start(params: &[Ty], rets: &[Ty]) -> Builder<Global, Full> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let params: Vec<Param> = params.iter().map(|&it| Param::of(it)).collect();
    let rets: Vec<Param> = rets.iter().map(|&it| Param::of(it)).collect();
    let sig = b.sig(Conv::C, Variadic::No, &params, &rets);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    b
}

/// `f` calls `g` by its reference 0; nothing else is answered.
struct FG {
    g: Func<Global>,
}

impl Bodies for FG {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        match (from, reference) {
            (F, RefId(0)) => Some((G, self.g.view())),
            _ => None,
        }
    }
}

fn undefined(inst: Value, why: &'static str) -> Outcome<Global> {
    Outcome::Undefined { inst: inst.0 as usize, why }
}

/// The shape blaze-build's `let &(p, q) = r` emits (hello-K-1): a pointer stored into a slot,
/// loaded back and loaded through. lir.md: "`ptr`: the target's pointer, distinct from its
/// integer. This is what carries provenance", and "A load reads the bytes there": the loaded
/// pointer is the stored one, so the `i32` at its offset 4 is the 6 stored there.
/// Expected: `test eval::memory_tests::a_stored_pointer_loads_back_with_its_slot ... ok`.
#[test]
fn a_stored_pointer_loads_back_with_its_slot() {
    let mut b = start(&[], &[Ty::I32]);
    let pair = b.slot(8, 2, SlotKind::Named);
    let r = b.slot(8, 3, SlotKind::Named);
    let p = b.slot_addr(pair);
    let (five, six) = (b.iconst(Ty::I32, 5), b.iconst(Ty::I32, 6));
    b.store(Flags::NONE, five, p, 0, MemAttrs::aligned(2));
    b.store(Flags::NONE, six, p, 4, MemAttrs::aligned(2));
    let q = b.slot_addr(r);
    b.store(Flags::NONE, p, q, 0, MemAttrs::aligned(3));
    let loaded = b.load(Ty::PTR, Flags::NONE, q, 0, MemAttrs::aligned(3));
    let second = b.load(Ty::I32, Flags::NONE, loaded, 4, MemAttrs::aligned(2));
    b.ret(&[second]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_in(f.view(), &[], FUEL, Global).expect("the evaluator runs it");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(6)]), "r = &pair; (*r).1 = 6");
}

/// A two-word value stored whole and loaded by its parts, and a wide pointer (a pointer and a
/// length) stored by parts and loaded back. "A load reads the bytes there", little-endian on
/// every target blaze has: the low `i64` of `2 << 64 | 1` is 1 and the high one 2; the wide
/// pointer's data pointer still reaches the 7 it points at, and its length is 3.
/// Expected: `test eval::memory_tests::a_two_word_value_stores_whole_and_loads_by_parts ... ok`.
#[test]
fn a_two_word_value_stores_whole_and_loads_by_parts() {
    let mut b = start(&[], &[Ty::I64, Ty::I64, Ty::I32, Ty::I64]);
    let wide = b.slot(16, 3, SlotKind::Temp);
    let data = b.slot(4, 2, SlotKind::Temp);
    let pair = b.slot(16, 3, SlotKind::Temp);
    let w = b.slot_addr(wide);
    let value = b.iconst(Ty::I128, (2 << 64) | 1);
    b.store(Flags::NONE, value, w, 0, MemAttrs::aligned(3));
    let low = b.load(Ty::I64, Flags::NONE, w, 0, MemAttrs::aligned(3));
    let high = b.load(Ty::I64, Flags::NONE, w, 8, MemAttrs::aligned(3));
    let d = b.slot_addr(data);
    let seven = b.iconst(Ty::I32, 7);
    b.store(Flags::NONE, seven, d, 0, MemAttrs::aligned(2));
    let p = b.slot_addr(pair);
    let three = b.iconst(Ty::I64, 3);
    b.store(Flags::NONE, d, p, 0, MemAttrs::aligned(3));
    b.store(Flags::NONE, three, p, 8, MemAttrs::aligned(3));
    let ptr = b.load(Ty::PTR, Flags::NONE, p, 0, MemAttrs::aligned(3));
    let len = b.load(Ty::I64, Flags::NONE, p, 8, MemAttrs::aligned(3));
    let through = b.load(Ty::I32, Flags::NONE, ptr, 0, MemAttrs::aligned(2));
    b.ret(&[low, high, through, len]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_in(f.view(), &[], FUEL, Global).expect("the evaluator runs it");
    let want = vec![Val::Int(1), Val::Int(2), Val::Int(7), Val::Int(3)];
    assert_eq!(outcome, Outcome::<Global>::Returned(want));
}

/// `g(p) { *p = 9; p + 4 }` and `f() { s[1] = 30; let q = g(&s); *q + s[0] }`: a pointer passes
/// into a call, the callee writes through it, and the pointer it returns is loaded through in
/// the caller. The slot is the caller's, live across the call: 30 + 9 = 39.
/// Expected: `test eval::memory_tests::a_pointer_passes_into_and_out_of_a_call ... ok`.
#[test]
fn a_pointer_passes_into_and_out_of_a_call() {
    let mut b = start(&[Ty::PTR], &[Ty::PTR]);
    let p = b.param(Ty::PTR);
    let nine = b.iconst(Ty::I32, 9);
    b.store(Flags::NONE, nine, p, 0, MemAttrs::aligned(2));
    let four = b.iconst(Ty::I64, 4);
    let q = b.ptr_add(Flags::NONE, p, four);
    b.ret(&[q]);
    let g = b.finish_checked().expect("g builds");

    let mut b = start(&[], &[Ty::I32]);
    let g_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::PTR)]);
    let g_ref = b.reference(RefKind::Func, 0);
    let s = b.slot(8, 2, SlotKind::Named);
    let p = b.slot_addr(s);
    let thirty = b.iconst(Ty::I32, 30);
    b.store(Flags::NONE, thirty, p, 4, MemAttrs::aligned(2));
    let q = b.call(Flags::NONE, g_ref, g_sig, &[p]);
    let a = b.load(Ty::I32, Flags::NONE, q, 0, MemAttrs::aligned(2));
    let c = b.load(Ty::I32, Flags::NONE, p, 0, MemAttrs::aligned(2));
    let sum = b.binary(Op::Add, Flags::NONE, a, c);
    b.ret(&[sum]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_calls_in(f.view(), F, &[], FUEL, &FG { g }, Global).expect("the evaluator runs it");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(39)]));
}

/// `g() -> &i32` returning its own slot's address, loaded through by `f`: the slot ended with
/// `g`'s frame, so the load is undefined behaviour, named, at the load (lir.md: "pointers carry
/// provenance, as in Rust's abstract machine"; a returned call's locals are no longer
/// allocated).
/// Expected: `test eval::memory_tests::a_load_through_a_returned_calls_slot_is_undefined ... ok`.
#[test]
fn a_load_through_a_returned_calls_slot_is_undefined() {
    let mut b = start(&[], &[Ty::PTR]);
    let s = b.slot(4, 2, SlotKind::Named);
    let p = b.slot_addr(s);
    let one = b.iconst(Ty::I32, 1);
    b.store(Flags::NONE, one, p, 0, MemAttrs::aligned(2));
    b.ret(&[p]);
    let g = b.finish_checked().expect("g builds");

    let mut b = start(&[], &[Ty::I32]);
    let g_sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::PTR)]);
    let g_ref = b.reference(RefKind::Func, 0);
    let q = b.call(Flags::NONE, g_ref, g_sig, &[]);
    let v = b.load(Ty::I32, Flags::NONE, q, 0, MemAttrs::aligned(2));
    b.ret(&[v]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_calls_in(f.view(), F, &[], FUEL, &FG { g }, Global).expect("the evaluator runs it");
    assert_eq!(outcome, undefined(v, "an access through a pointer to a returned call's slot"));
}

/// An 8-byte slot aligned to 8, zeroed, read by one load of `ty` at `offset` that says it is
/// aligned to `1 << align`: the function and its load.
fn one_load(ty: Ty, offset: i32, align: u8) -> (Func<Global>, Value) {
    let mut b = start(&[], &[ty]);
    let s = b.slot(8, 3, SlotKind::Temp);
    let p = b.slot_addr(s);
    let zero = b.iconst(Ty::I64, 0);
    b.store(Flags::NONE, zero, p, 0, MemAttrs::aligned(3));
    let x = b.load(ty, Flags::NONE, p, offset, MemAttrs::aligned(align));
    b.ret(&[x]);
    (b.finish_checked().expect("it builds"), x)
}

/// The access rules: an aligned load inside the slot reads its bytes; one that says it is
/// aligned to 4 at offset 2 breaks its promise (lir.md's table: "alignment of every access ...
/// `MemAttrs.align`", a promise, so breaking it is undefined); a load of 8 bytes at offset 4
/// of an 8-byte slot leaves it. A byte-aligned load at offset 2 is defined.
/// Expected: `test eval::memory_tests::a_misaligned_or_outside_access_is_undefined_by_name ... ok`.
#[test]
fn a_misaligned_or_outside_access_is_undefined_by_name() {
    let run = |ty, offset, align| {
        let (f, x) = one_load(ty, offset, align);
        (run_in(f.view(), &[], FUEL, Global).expect("the evaluator runs it"), x)
    };
    let (inside, _) = run(Ty::I32, 4, 2);
    assert_eq!(inside, Outcome::<Global>::Returned(vec![Val::Int(0)]));
    let (bytewise, _) = run(Ty::I32, 2, 0);
    assert_eq!(bytewise, Outcome::<Global>::Returned(vec![Val::Int(0)]));
    let (misaligned, x) = run(Ty::I32, 2, 2);
    assert_eq!(misaligned, undefined(x, "an access less aligned than it says"));
    let (outside, x) = run(Ty::I64, 4, 2);
    assert_eq!(outside, undefined(x, "an access out of its slot"));
}

/// `memset` four bytes to 0xAB, `memcpy` them to another slot, and a pointer to that slot
/// copied by `memcpy` and loaded through: "The destination, the byte (`i8`) and the length";
/// a copy keeps a pointer's provenance, so the copied pointer reaches 0xABABABAB. A `memcpy` of
/// a slot onto itself overlaps, which `memcpy` (unlike `memmove`) does not allow.
/// Expected: `test eval::memory_tests::bulk_operations_copy_bytes_with_their_provenance ... ok`.
#[test]
fn bulk_operations_copy_bytes_with_their_provenance() {
    let mut b = start(&[], &[Ty::I32, Ty::I32]);
    let [a, c] = [(); 2].map(|_| b.slot(4, 2, SlotKind::Temp));
    let [w, v] = [(); 2].map(|_| b.slot(8, 3, SlotKind::Temp));
    let (pa, pc, pw, pv) = (b.slot_addr(a), b.slot_addr(c), b.slot_addr(w), b.slot_addr(v));
    let (byte, four, eight) = (b.iconst(Ty::I8, 0xAB), b.iconst(Ty::I64, 4), b.iconst(Ty::I64, 8));
    b.bulk(Op::Memset, Flags::NONE, pa, byte, four, MemAttrs::aligned(2));
    b.bulk(Op::Memcpy, Flags::NONE, pc, pa, four, MemAttrs { src_align: 2, ..MemAttrs::aligned(2) });
    let x = b.load(Ty::I32, Flags::NONE, pc, 0, MemAttrs::aligned(2));
    b.store(Flags::NONE, pc, pw, 0, MemAttrs::aligned(3));
    b.bulk(Op::Memmove, Flags::NONE, pv, pw, eight, MemAttrs { src_align: 3, ..MemAttrs::aligned(3) });
    let copied = b.load(Ty::PTR, Flags::NONE, pv, 0, MemAttrs::aligned(3));
    let y = b.load(Ty::I32, Flags::NONE, copied, 0, MemAttrs::aligned(2));
    b.ret(&[x, y]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_in(f.view(), &[], FUEL, Global).expect("the evaluator runs it");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(0xABAB_ABAB), Val::Int(0xABAB_ABAB)]));

    let mut b = start(&[], &[]);
    let a = b.slot(4, 2, SlotKind::Temp);
    let pa = b.slot_addr(a);
    let four = b.iconst(Ty::I64, 4);
    b.bulk(Op::Memcpy, Flags::NONE, pa, pa, four, MemAttrs::aligned(0));
    b.ret(&[]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_in(f.view(), &[], FUEL, Global).expect("the evaluator runs it");
    // The memcpy is the instruction after the entry's `slot_addr` and `iconst`.
    assert_eq!(outcome, Outcome::<Global>::Undefined { inst: four.0 as usize + 1, why: "a `memcpy` of overlapping bytes" });
}

/// What the evaluator cannot answer it refuses by name: an `i64` load of a stored pointer's
/// bytes would be its address, and a slot has none in the evaluator (lir.md: "A transmute from
/// a pointer to an integer is `ptr_addr`"). A load through `null` is undefined.
/// Expected: `test eval::memory_tests::a_pointers_bytes_as_an_integer_are_refused_and_null_is_undefined ... ok`.
#[test]
fn a_pointers_bytes_as_an_integer_are_refused_and_null_is_undefined() {
    let mut b = start(&[], &[Ty::I64]);
    let w = b.slot(8, 3, SlotKind::Temp);
    let p = b.slot_addr(w);
    b.store(Flags::NONE, p, p, 0, MemAttrs::aligned(3));
    let x = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
    b.ret(&[x]);
    let f = b.finish_checked().expect("f builds");
    let refused = run_in(f.view(), &[], FUEL, Global).map(|_| ());
    let what = "an integer or float load of a pointer's bytes: a slot has no address";
    assert_eq!(refused, Err(EvalError::Unsupported { inst: x.0 as usize, op: Op::Load, what }));

    let mut b = start(&[], &[Ty::I32]);
    let null = b.null();
    let x = b.load(Ty::I32, Flags::NONE, null, 0, MemAttrs::aligned(2));
    b.ret(&[x]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_in(f.view(), &[], FUEL, Global).expect("the evaluator runs it");
    assert_eq!(outcome, undefined(x, "an access through a null pointer"));
}

/// `g(x) = (x + 1, x + 2)` and `f(x) = { let (a, b) = g(x); b * 10 + a }`: "A call returning
/// several values ... yield[s] a compound that `project k` instructions read", so f(5) = 76.
/// Expected: `test eval::memory_tests::a_call_returning_two_values_is_projected ... ok`.
#[test]
fn a_call_returning_two_values_is_projected() {
    let mut b = start(&[Ty::I32], &[Ty::I32, Ty::I32]);
    let x = b.param(Ty::I32);
    let (one, two) = (b.iconst(Ty::I32, 1), b.iconst(Ty::I32, 2));
    let (a, c) = (b.binary(Op::Add, Flags::NONE, x, one), b.binary(Op::Add, Flags::NONE, x, two));
    b.ret(&[a, c]);
    let g = b.finish_checked().expect("g builds");

    let mut b = start(&[Ty::I32], &[Ty::I32]);
    let g_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I32), Param::of(Ty::I32)]);
    let g_ref = b.reference(RefKind::Func, 0);
    let x = b.param(Ty::I32);
    let both = b.call(Flags::NONE, g_ref, g_sig, &[x]);
    let (a, c) = (b.project(both, 0), b.project(both, 1));
    let ten = b.iconst(Ty::I32, 10);
    let tens = b.binary(Op::Mul, Flags::NONE, c, ten);
    let r = b.binary(Op::Add, Flags::NONE, tens, a);
    b.ret(&[r]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_calls_in(f.view(), F, &[Val::Int(5)], FUEL, &FG { g }, Global).expect("the evaluator runs it");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(76)]));
}

/// Pointer comparisons the evaluator can decide: two pointers into one slot by their offsets
/// (`&s[0] < &s[1]`), and a pointer into a slot against `null` (never equal: no allocation is
/// at address 0).
/// Expected: `test eval::memory_tests::pointers_into_one_slot_compare_by_offset ... ok`.
#[test]
fn pointers_into_one_slot_compare_by_offset() {
    let mut b = start(&[], &[Ty::I1, Ty::I1]);
    let s = b.slot(8, 2, SlotKind::Temp);
    let p = b.slot_addr(s);
    let four = b.iconst(Ty::I64, 4);
    let q = b.ptr_add(Flags::NONE, p, four);
    let below = b.icmp(IntCond::Ult, p, q);
    let null = b.null();
    let same = b.icmp(IntCond::Eq, p, null);
    b.ret(&[below, same]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_in(f.view(), &[], FUEL, Global).expect("the evaluator runs it");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(1), Val::Int(0)]));
}
