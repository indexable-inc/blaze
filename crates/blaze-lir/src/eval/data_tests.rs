//! The program's data and functions in the evaluator ([`super::data`]): a string datum read
//! by byte, a static whose relocation points at a second datum, read-only data refused a write
//! and a mutable static keeping one, a table of function pointers called through, a signature
//! mismatch, and function pointer equality. Each expectation is from LIR's own semantics
//! (docs/v2/plan/design/lir.md), quoted at the test.

use std::alloc::Global;

use super::{Bodies, BodyId, Outcome, Val, run_calls_in, run_in};
use crate::{
    View,
    body::{Datum, DatumId, DatumReloc, Target},
    builder::{Builder, Full, MemAttrs, Param, Variadic},
    flags::{Flags, IntCond},
    func::{BlockKind, Conv, Func, MemAttrsRec, RefId, RefKind, SigId, Value},
    ty::Ty,
};

const FUEL: u64 = 100_000;
const F: BodyId = BodyId([1; 32]);
const G1: BodyId = BodyId([2; 32]);
const G2: BodyId = BodyId([3; 32]);
/// A static whose 8 bytes are a pointer to `T` plus 1.
const S: DatumId = DatumId([4; 32]);
/// The bytes `xyz`.
const T: DatumId = DatumId([5; 32]);
/// A mutable static `i32` holding 1.
const M: DatumId = DatumId([6; 32]);
/// A table of two function pointers, `G1`'s then `G2`'s.
const V: DatumId = DatumId([7; 32]);

const S_RELOCS: [DatumReloc; 1] = [DatumReloc { offset: 0, target: Target::Datum(T), addend: 1 }];
const V_RELOCS: [DatumReloc; 2] = [
    DatumReloc { offset: 0, target: Target::Func(G1), addend: 0 },
    DatumReloc { offset: 8, target: Target::Func(G2), addend: 0 },
];

/// A builder of a function taking `params` and returning `rets`, its entry block begun.
fn start(params: &[Ty], rets: &[Ty]) -> Builder<Global, Full> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = signature(&mut b, params, rets);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    b
}

fn signature(b: &mut Builder<Global, Full>, params: &[Ty], rets: &[Ty]) -> SigId {
    let params: Vec<Param> = params.iter().map(|&it| Param::of(it)).collect();
    let rets: Vec<Param> = rets.iter().map(|&it| Param::of(it)).collect();
    b.sig(Conv::C, Variadic::No, &params, &rets)
}

/// `fn() -> i32 { n }`.
fn returning(n: u128) -> Func<Global> {
    let mut b = start(&[], &[Ty::I32]);
    let n = b.iconst(Ty::I32, n);
    b.ret(&[n]);
    b.finish_checked().expect("the callee builds")
}

/// The program: `f`'s references by what they name, the data above, `G1` returning 10 and `G2`
/// returning 20.
struct Prog {
    refs: Vec<(RefId, Target)>,
    g1: Func<Global>,
    g2: Func<Global>,
}

impl Prog {
    fn new(refs: Vec<(RefId, Target)>) -> Prog {
        Prog { refs, g1: returning(10), g2: returning(20) }
    }
}

impl Bodies for Prog {
    fn callee(&self, _from: BodyId, _reference: RefId) -> Option<(BodyId, View<'_>)> {
        None
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        let found = self.refs.iter().find(|it| it.0 == reference)?;
        (from == F).then_some(found.1)
    }

    fn datum(&self, id: DatumId) -> Option<Datum<'_>> {
        let (bytes, align, relocs, writable): (&[u8], u8, &[DatumReloc], bool) = match id {
            S => (&[0; 8], 3, &S_RELOCS, false),
            T => (b"xyz", 0, &[], false),
            M => (&[1, 0, 0, 0], 2, &[], true),
            V => (&[0; 16], 3, &V_RELOCS, false),
            _ => return None,
        };
        Some(Datum { bytes, align, relocs, writable })
    }

    fn body(&self, id: BodyId) -> Option<View<'_>> {
        match id {
            G1 => Some(self.g1.view()),
            G2 => Some(self.g2.view()),
            _ => None,
        }
    }
}

fn undefined(inst: Value, why: &'static str) -> Outcome<Global> {
    Outcome::Undefined { inst: inst.0 as usize, why }
}

/// `b"hello"[2]` from the function's own data. lir.md: "`func_addr`, `data_addr` and
/// `tls_addr` of a reference", and the tables hold "the function's data and its relocations":
/// `data_addr` of the data row points at its first byte, so the byte at offset 2 is `l`.
/// Expected: `test eval::data_tests::a_string_datums_third_byte_loads ... ok`.
#[test]
fn a_string_datums_third_byte_loads() {
    let mut b = start(&[], &[Ty::I8]);
    let hello = b.data(b"hello", 0, &[]);
    let p = b.data_addr(hello);
    let third = b.load(Ty::I8, Flags::NONE, p, 2, MemAttrs::aligned(0));
    b.ret(&[third]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_in(f.view(), &[], FUEL, Global).expect("the evaluator runs it");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(u128::from(b'l'))]));
}

/// A static whose word is the address of a second datum plus 1, read through. lir.md: "the
/// function's data and its relocations"; blaze-build's form (data.rs): "an 8-byte
/// little-endian word ... becomes the address of the target plus that addend": the pointer
/// loaded from the static reaches `xyz` at 1, so the byte read is `y`.
/// Expected: `test eval::data_tests::a_statics_relocation_reaches_a_second_datum ... ok`.
#[test]
fn a_statics_relocation_reaches_a_second_datum() {
    let mut b = start(&[], &[Ty::I8]);
    let s = b.reference(RefKind::Static, 0);
    let p = b.data_addr(s);
    let q = b.load(Ty::PTR, Flags::NONE, p, 0, MemAttrs::aligned(3));
    let y = b.load(Ty::I8, Flags::NONE, q, 0, MemAttrs::aligned(0));
    b.ret(&[y]);
    let f = b.finish_checked().expect("f builds");
    let prog = Prog::new(vec![(s, Target::Datum(S))]);
    let outcome = run_calls_in(f.view(), F, &[], FUEL, &prog, Global).expect("the evaluator runs it");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(u128::from(b'y'))]));
}

/// A write to the function's own data is undefined; a mutable static keeps a write, and a
/// second `data_addr` of it is the same datum (placed once per run). lir.md: "`data_addr` ...
/// of a reference"; the Reference's undefined behaviour list: "Mutating immutable bytes";
/// blaze-build's form: a datum says "whether it may be written (a `static mut`, ...)".
/// Expected: `test eval::data_tests::read_only_data_refuses_a_write_and_a_mutable_static_keeps_one ... ok`.
#[test]
fn read_only_data_refuses_a_write_and_a_mutable_static_keeps_one() {
    let mut b = start(&[], &[]);
    let ab = b.data(b"ab", 0, &[]);
    let p = b.data_addr(ab);
    let seven = b.iconst(Ty::I8, 7);
    b.store(Flags::NONE, seven, p, 0, MemAttrs::aligned(0));
    b.ret(&[]);
    let f = b.finish_checked().expect("f builds");
    let outcome = run_in(f.view(), &[], FUEL, Global).expect("the evaluator runs it");
    let store = Value(seven.0 + 1);
    assert_eq!(outcome, undefined(store, "a write to read-only data"));

    let mut b = start(&[], &[Ty::I32]);
    let m = b.reference(RefKind::Static, 0);
    let p = b.data_addr(m);
    let v = b.iconst(Ty::I32, 41);
    b.store(Flags::NONE, v, p, 0, MemAttrs::aligned(2));
    let again = b.data_addr(m);
    let x = b.load(Ty::I32, Flags::NONE, again, 0, MemAttrs::aligned(2));
    b.ret(&[x]);
    let g = b.finish_checked().expect("g builds");
    let prog = Prog::new(vec![(m, Target::Datum(M))]);
    let outcome = run_calls_in(g.view(), F, &[], FUEL, &prog, Global).expect("the evaluator runs it");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(41)]));
}

/// The vtable shape: a read-only table of two function pointers, each loaded (`invariant`,
/// `nonnull`, as blaze-build's vtable loads are) and called through. lir.md: "`call`,
/// `call_indirect`": the call runs the function the pointer is, so the two give 10 and 20.
/// Expected: `test eval::data_tests::a_table_of_function_pointers_calls_each_callee ... ok`.
#[test]
fn a_table_of_function_pointers_calls_each_callee() {
    let mut b = start(&[], &[Ty::I32, Ty::I32]);
    let sig = signature(&mut b, &[], &[Ty::I32]);
    let v = b.reference(RefKind::Vtable, 0);
    let table = b.data_addr(v);
    let attrs = MemAttrs { bits: MemAttrsRec::INVARIANT | MemAttrsRec::NONNULL, ..MemAttrs::aligned(3) };
    let first = b.load(Ty::PTR, Flags::NONE, table, 0, attrs);
    let x = b.call_indirect(Flags::NONE, first, sig, &[]);
    let second = b.load(Ty::PTR, Flags::NONE, table, 8, attrs);
    let y = b.call_indirect(Flags::NONE, second, sig, &[]);
    b.ret(&[x, y]);
    let f = b.finish_checked().expect("f builds");
    let prog = Prog::new(vec![(v, Target::Datum(V))]);
    let outcome = run_calls_in(f.view(), F, &[], FUEL, &prog, Global).expect("the evaluator runs it");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(10), Val::Int(20)]));
}

/// A call through `G1`'s pointer as if it returned `i64`. lir.md: "`call`, `call_indirect`";
/// the Reference: "Calling a function with the wrong call ABI" is undefined, so the call is
/// undefined behaviour by name, at the call.
/// Expected: `test eval::data_tests::a_call_through_a_pointer_of_another_signature_is_undefined ... ok`.
#[test]
fn a_call_through_a_pointer_of_another_signature_is_undefined() {
    let mut b = start(&[], &[Ty::I64]);
    let wide = signature(&mut b, &[], &[Ty::I64]);
    let g = b.reference(RefKind::Func, 0);
    let p = b.func_addr(g);
    let x = b.call_indirect(Flags::NONE, p, wide, &[]);
    b.ret(&[x]);
    let f = b.finish_checked().expect("f builds");
    let prog = Prog::new(vec![(g, Target::Func(G1))]);
    let outcome = run_calls_in(f.view(), F, &[], FUEL, &prog, Global).expect("the evaluator runs it");
    assert_eq!(outcome, undefined(x, "a call through a function pointer of another signature"));
}

/// Two references to `G1` give equal pointers; `G1`'s and `G2`'s differ; neither is null.
/// lir.md: "`func_addr` ... of a reference": the pointer IS the function, one per function in
/// a run (blaze folds no two functions together), so: equal, unequal, not null.
/// Expected: `test eval::data_tests::function_pointers_are_equal_by_function ... ok`.
#[test]
fn function_pointers_are_equal_by_function() {
    let mut b = start(&[], &[Ty::I1, Ty::I1, Ty::I1]);
    let (r0, r1, r2) = (b.reference(RefKind::Func, 0), b.reference(RefKind::Func, 1), b.reference(RefKind::Func, 2));
    let (a, a2, c) = (b.func_addr(r0), b.func_addr(r1), b.func_addr(r2));
    let null = b.null();
    let same = b.icmp(IntCond::Eq, a, a2);
    let other = b.icmp(IntCond::Eq, a, c);
    let not_null = b.icmp(IntCond::Ne, a, null);
    b.ret(&[same, other, not_null]);
    let f = b.finish_checked().expect("f builds");
    let prog = Prog::new(vec![(r0, Target::Func(G1)), (r1, Target::Func(G1)), (r2, Target::Func(G2))]);
    let outcome = run_calls_in(f.view(), F, &[], FUEL, &prog, Global).expect("the evaluator runs it");
    assert_eq!(outcome, Outcome::<Global>::Returned(vec![Val::Int(1), Val::Int(0), Val::Int(1)]));
}
