//! The evaluator's tests: bodies in blaze-body's text form, run over a table world (the old
//! evaluator's fixture world for ADTs and layout facts, plus a table of bodies). Each test
//! prints the value or the error it checks; the line it should print is in its doc.

use std::format;
use std::println;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_body::{AssertKind, BinOp, Body, Stmt, verify};
use blaze_consteval::fixture::World;
use blaze_consteval::{
    Answer, ConstRecord, ConstWorld, DataRecord, FnKind, Intrinsic, Layouts, LayoutFacts, Limits, Missing, Op, RecordKey, Refusal,
    ReprAttrs, ValidRange,
};
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_own::testing::heap;
use blaze_typeck::{Adt, Fetch, ItemKey, List, PTy, PortableTys};

use crate::{Bodied, BodyWorld, DropGlue, EvalError, Fault, eval_body};

const ROOT: ItemKey = ItemKey(0xb0d1);
const CALLEE: ItemKey = ItemKey(0xb0d2);

/// The fixture world, plus bodies in the text form by item.
struct Tw {
    w: World,
    bodies: StdVec<(ItemKey, Body<Alloc<'static>>)>,
}

/// The types the bodies name, as `tN` handles of the world's table.
struct Tys {
    unit: u32,
    u8: u32,
    u32: u32,
    usize: u32,
    bool: u32,
    char: u32,
    str_ref: u32,
    pair: u32,
    p: u32,
    r: u32,
    e: u32,
    isize: u32,
    arr4: u32,
}

fn world() -> (Tw, Tys) {
    let mut w = World::new();
    let b = w.fx.ty(blaze_typeck::PNode::Bool);
    let unit = w.fx.tuple(&[]);
    w.intrinsic("assume", 0, &[b], unit, Intrinsic::Assume);
    w.strukt("P", &[("a", "u32"), ("b", "u32")]);
    w.enum_("E", None, &[("A", &[], None), ("B", &["u32"], None)]);
    w.build();
    let mut t = |s: &str| -> u32 { w.ty(s).0 };
    let tys = Tys {
        unit: unit.0,
        u8: t("u8"),
        u32: t("u32"),
        usize: t("usize"),
        bool: t("bool"),
        char: t("char"),
        str_ref: t("&str"),
        pair: t("(u32, bool)"),
        p: t("P"),
        r: t("&u32"),
        e: t("E"),
        isize: t("isize"),
        arr4: t("[u32; 4]"),
    };
    (Tw { w, bodies: StdVec::new() }, tys)
}

impl Tw {
    fn with(mut self, item: ItemKey, text: &str) -> Tw {
        let body = blaze_body::parse(text, heap().alloc()).unwrap_or_else(|e| panic!("{e:?} in:\n{text}"));
        if let Err(e) = verify(&body) {
            panic!("verify refused {e:?}:\n{text}");
        }
        self.bodies.push((item, body));
        self
    }
}

impl LayoutFacts for Tw {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.adt(key)
    }
    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, Missing> {
        self.w.repr(key)
    }
    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, Missing> {
        self.w.discriminant(key, variant)
    }
    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, Missing> {
        self.w.valid_range(key)
    }
}

impl ConstWorld for Tw {
    fn body(&self, item: ItemKey) -> Fetch<Option<blaze_consteval::Typed<'_>>> {
        self.w.body(item)
    }
    /// The table's bodies are `const fn`s; the rest as the fixture says.
    fn fn_kind(&self, item: ItemKey) -> FnKind {
        if self.bodies.iter().any(|(k, _)| *k == item) { FnKind::Const } else { self.w.fn_kind(item) }
    }
    fn resolve(&self, item: ItemKey, args: List, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Option<(ItemKey, List)> {
        self.w.resolve(item, args, ground, scratch)
    }
    fn code_id(&self, item: ItemKey) -> ContentId {
        self.w.code_id(item)
    }
    fn is_static(&self, item: ItemKey) -> bool {
        self.w.is_static(item)
    }
    fn record(&self, key: RecordKey) -> Fetch<&ConstRecord<Alloc<'_>>> {
        self.w.record(key)
    }
    fn explicit_discriminant(&self, adt: ItemKey, variant: u32) -> Option<ItemKey> {
        self.w.explicit_discriminant(adt, variant)
    }
}

impl BodyWorld for Tw {
    fn verified(&self, item: ItemKey) -> Fetch<Option<Bodied<'_>>> {
        Ok(self.bodies.iter().find(|(k, _)| *k == item).map(|(k, b)| Bodied {
            item: *k,
            body: verify(b).expect("verified when added"),
            types: self.w.fx.t.view(),
        }))
    }
    /// No test drops a value with glue.
    fn drop_glue(&self, _ty: PTy, _ground: &mut PortableTys<'_>, _scratch: Alloc<'_>) -> Result<DropGlue, Refusal<'static>> {
        Ok(DropGlue::Nothing)
    }
}

fn run(tw: &Tw, limits: Limits) -> Result<DataRecord<Alloc<'static>>, EvalError<'static>> {
    let alloc = heap().alloc();
    let mut layouts = Layouts::new_in(alloc);
    let mut ground = PortableTys::new_in(alloc);
    match eval_body(tw, &mut layouts, &mut ground, ROOT, List::EMPTY, limits, alloc)? {
        Answer::Done(r) => Ok(r),
        Answer::Need(n) => panic!("the table world lacks {n:?}"),
    }
}

/// The value's first four bytes as a `u32`, printed.
fn u32_value(tw: &Tw) -> u32 {
    let r = run(tw, Limits::DEFAULT).unwrap_or_else(|e| panic!("refused: {e}"));
    let b = &r.allocs[0].bytes;
    let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    println!("{v}");
    v
}

fn refused(tw: &Tw, limits: Limits) -> EvalError<'static> {
    match run(tw, limits) {
        Ok(_) => panic!("evaluated"),
        Err(e) => {
            println!("{e}");
            e
        }
    }
}

fn checked_add(t: &Tys, a: &str, b: &str) -> String {
    format!(
        "\
local _0: t{u} return
local _1: t{pair} temp e0 value
const c0 = scalar {a}: t{u}
const c1 = scalar {b}: t{u}
bb0:
  _1 = checked add(const c0, const c1)
  assert copy _1.1:t{bool} == false, overflow_add(const c0, const c1) then bb1 unwind continue
bb1:
  _0 = copy _1.0:t{u}
  return
",
        u = t.u32,
        pair = t.pair,
        bool = t.bool,
    )
}

/// `2 + 3` through `checked add` and its overflow assert. Should print: `5`
#[test]
fn checked_add_passes() {
    let (tw, t) = world();
    let tw = tw.with(ROOT, &checked_add(&t, "0x2", "0x3"));
    assert_eq!(u32_value(&tw), 5);
}

/// `u32::MAX + 1`: the assert fails with the overflow refusal at its row. Should print:
/// `constant 0xb0d1 (body 0xb0d1, row 1, bytes 0..0): assert Overflow(Add) failed: Overflow { op: Add }`
#[test]
fn checked_add_overflows() {
    let (tw, t) = world();
    let tw = tw.with(ROOT, &checked_add(&t, "0xffffffff", "0x1"));
    let e = refused(&tw, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Assert { kind: AssertKind::Overflow(BinOp::Add), refusal: Refusal::Overflow { op: Op::Add } }), "{e:?}");
    assert_eq!(e.at.map(|r| (r.body, r.stmt)), Some((ROOT, Stmt(1))));
}

/// The sum of `0..10` through a switch and a back edge. Should print: `45`
#[test]
fn loop_sums() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return mut
local _1: t{u} temp e0 value mut
local _2: t{b} temp e1 value
const c0 = scalar 0x0: t{u}
const c1 = scalar 0x1: t{u}
const c2 = scalar 0xa: t{u}
bb0:
  _0 = const c0
  _1 = const c0
  goto bb1
bb1:
  _2 = lt(copy _1, const c2)
  switch copy _2 [0 -> bb3] else bb2
bb2:
  _0 = add(copy _0, copy _1)
  _1 = add(copy _1, const c1)
  goto bb1
bb3:
  return
",
        u = t.u32,
        b = t.bool,
    );
    let tw = tw.with(ROOT, &text);
    assert_eq!(u32_value(&tw), 45);
}

/// A call to a second body (`x + 7`) with `35`, its return value the constant's. Should
/// print: `42`
#[test]
fn call_returns() {
    let (tw, t) = world();
    let callee = format!(
        "\
local _0: t{u} return
local _1: t{u} arg 0
const c0 = scalar 0x7: t{u}
bb0:
  _0 = add(copy _1, const c0)
  return
",
        u = t.u32,
    );
    let root = format!(
        "\
local _0: t{u} return
const c0 = scalar 0x23: t{u}
bb0:
  call fn k{callee:x}<0,0>(const c0) -> _0 then bb1 unwind continue
bb1:
  return
",
        u = t.u32,
        callee = CALLEE.0,
    );
    let tw = tw.with(CALLEE, &callee).with(ROOT, &root);
    assert_eq!(u32_value(&tw), 42);
}

/// `P { a: 5, b: 9 }` built field by field; `a` read back plus `b` read through a shared
/// reference. Should print: `14`
#[test]
fn struct_fields_and_reference() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return
local _1: t{p} temp e0 value mut
local _2: t{r} temp e1 borrow
local _3: t{u} temp e2 value
const c0 = scalar 0x5: t{u}
const c1 = scalar 0x9: t{u}
origin 'o0
bb0:
  _1.0:t{u} = const c0
  _1.1:t{u} = const c1
  _2 = borrow shared _1.1:t{u} L0 'o0
  _3 = copy _2.*
  _0 = add(copy _1.0:t{u}, copy _3)
  return
",
        u = t.u32,
        p = t.p,
        r = t.r,
    );
    let tw = tw.with(ROOT, &text);
    assert_eq!(u32_value(&tw), 14);
}

/// `E::B(4)` built, its discriminant switched on, the payload read through a downcast.
/// Should print: `4`
#[test]
fn switch_on_discriminant() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return
local _1: t{e} temp e0 value
local _2: t{i} temp e1 value
const c0 = scalar 0x4: t{u}
const c1 = scalar 0x1: t{u}
bb0:
  _1 = adt t{e}#1(const c0)
  _2 = discr _1
  switch copy _2 [0 -> bb1, 1 -> bb2] else bb3
bb1:
  _0 = const c1
  return
bb2:
  _0 = copy _1#1.0:t{u}
  return
bb3:
  unreachable
",
        u = t.u32,
        e = t.e,
        i = t.isize,
    );
    let tw = tw.with(ROOT, &text);
    assert_eq!(u32_value(&tw), 4);
}

/// `E::B(4)` built as MIR builds an enum field by field: `deinit`, the payload through a
/// downcast, then `setdiscr` writes the tag; the discriminant read back selects the payload.
/// Should print: `4`
#[test]
fn set_discriminant_after_fields() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return
local _1: t{e} temp e0 value mut
local _2: t{i} temp e1 value
const c0 = scalar 0x4: t{u}
const c1 = scalar 0x1: t{u}
bb0:
  deinit _1
  _1#1.0:t{u} = const c0
  setdiscr _1 variant 1
  _2 = discr _1
  switch copy _2 [0 -> bb1, 1 -> bb2] else bb3
bb1:
  _0 = const c1
  return
bb2:
  _0 = copy _1#1.0:t{u}
  return
bb3:
  unreachable
",
        u = t.u32,
        e = t.e,
        i = t.isize,
    );
    let tw = tw.with(ROOT, &text);
    assert_eq!(u32_value(&tw), 4);
}

/// A local written, then `deinit`: its bytes are unwritten again, so reading it is refused.
/// Should print: `constant 0xb0d1 (body 0xb0d1, row 2, bytes 0..0): Uninit { offset: 0 }`
#[test]
fn deinit_unwrites_the_place() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return
local _1: t{u} temp e0 value mut
const c0 = scalar 0x4: t{u}
bb0:
  _1 = const c0
  deinit _1
  _0 = add(copy _1, const c0)
  return
",
        u = t.u32,
    );
    let tw = tw.with(ROOT, &text);
    let e = refused(&tw, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::Uninit { offset: 0 })), "{e:?}");
    assert_eq!(e.at.map(|r| r.stmt), Some(Stmt(2)));
}

/// An `unreachable` row reached: undefined behaviour, refused at its row. Should print:
/// `constant 0xb0d1 (body 0xb0d1, row 0, bytes 0..0): Unreachable`
#[test]
fn unreachable_is_refused() {
    let (tw, t) = world();
    let tw = tw.with(ROOT, &format!("local _0: t{u} return\nbb0:\n  unreachable\n", u = t.u32));
    let e = refused(&tw, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::Unreachable)), "{e:?}");
    assert_eq!(e.at.map(|r| r.stmt), Some(Stmt(0)));
}

/// `loop {}`: the step bound stops it by name. Should print:
/// `constant 0xb0d1 (body 0xb0d1, row 0, bytes 0..0): StepLimit { limit: 1000 }`
#[test]
fn step_limit_stops_a_loop() {
    let (tw, t) = world();
    let tw = tw.with(ROOT, &format!("local _0: t{u} return\nbb0:\n  goto bb0\n", u = t.u32));
    let e = refused(&tw, Limits { steps: 1000, ..Limits::DEFAULT });
    assert!(matches!(e.fault, Fault::Refused(Refusal::StepLimit { limit: 1000 })), "{e:?}");
}

/// `Intrinsic::Assume(false)` is UB and must be refused with `Refusal::Unreachable`, while
/// `Intrinsic::Assume(true)` succeeds.
#[test]
fn assume_false_is_refused_and_true_passes() {
    let (tw, t) = world();
    let assume = tw.w.key("assume").0;
    let body_false = format!(
        "\
local _0: t{u} return
local _1: t{unit} temp e0 value
const c0 = scalar 0x0: t{b}
const c1 = scalar 0x7: t{u}
bb0:
  call intrinsic k{assume:x}<0,0>(const c0) -> _1 then bb1 unwind continue
bb1:
  _0 = const c1
  return
",
        u = t.u32,
        unit = t.unit,
        b = t.bool,
    );
    let tw_false = tw.with(ROOT, &body_false);
    let e = refused(&tw_false, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::Unreachable)), "{e:?}");

    let (tw, t) = world();
    let assume = tw.w.key("assume").0;
    let body_true = format!(
        "\
local _0: t{u} return
local _1: t{unit} temp e0 value
const c0 = scalar 0x1: t{b}
const c1 = scalar 0x7: t{u}
bb0:
  call intrinsic k{assume:x}<0,0>(const c0) -> _1 then bb1 unwind continue
bb1:
  _0 = const c1
  return
",
        u = t.u32,
        unit = t.unit,
        b = t.bool,
    );
    let tw_true = tw.with(ROOT, &body_true);
    assert_eq!(u32_value(&tw_true), 7);
}

/// `UnOp::PtrMetadata` on a `&str` bytes constant (`ConstKind::Bytes`) must return its length
/// and not leak `Origin::Data` allocations in a loop.
#[test]
fn ptr_metadata_on_bytes_const_and_loop_no_leak() {
    let (tw, t) = world();
    // 64 bytes ("aa" * 64 = 128 hex chars), looped 200 times under a 1024-byte memory limit
    // (would allocate 12.8 KiB and fail with MemoryLimit if ConstKind::Bytes leaked).
    let hex = "61".repeat(64);
    let text = format!(
        "\
local _0: t{u} return mut
local _1: t{u} temp e0 value mut
local _2: t{b} temp e1 value
local _3: t{us} temp e2 value
const c0 = scalar 0x0: t{u}
const c1 = scalar 0x1: t{u}
const c2 = scalar 0xc8: t{u}
const c3 = bytes \"{hex}\": t{s}
bb0:
  _0 = const c0
  _1 = const c0
  goto bb1
bb1:
  _2 = lt(copy _1, const c2)
  switch copy _2 [0 -> bb3] else bb2
bb2:
  _3 = meta(const c3)
  _0 = cast int_to_int(copy _3) as t{u}
  _1 = add(copy _1, const c1)
  goto bb1
bb3:
  return
",
        u = t.u32,
        b = t.bool,
        us = t.usize,
        s = t.str_ref,
    );
    let tw = tw.with(ROOT, &text);
    let r = run(&tw, Limits { memory: 1024, ..Limits::DEFAULT }).unwrap_or_else(|e| panic!("refused: {e}"));
    let bytes = &r.allocs[0].bytes;
    assert_eq!(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]), 64);
}

/// Invalid `bool` (> 1) and invalid `char` (surrogate `0xD800`) values must be refused on
/// scalar read / transmute.
#[test]
fn invalid_bool_and_char_are_refused() {
    let (tw, t) = world();
    let bad_bool = format!(
        "\
local _0: t{b} return
const c0 = scalar 0x2: t{u8}
bb0:
  _0 = cast transmute(const c0) as t{b}
  return
",
        b = t.bool,
        u8 = t.u8,
    );
    let tw_b = tw.with(ROOT, &bad_bool);
    let e = refused(&tw_b, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::InvalidBool { value: 2 })), "{e:?}");

    let (tw, t) = world();
    let bad_char = format!(
        "\
local _0: t{ch} return
const c0 = scalar 0xd800: t{u}
bb0:
  _0 = cast transmute(const c0) as t{ch}
  return
",
        ch = t.char,
        u = t.u32,
    );
    let tw_c = tw.with(ROOT, &bad_char);
    let e = refused(&tw_c, Limits::DEFAULT);
    assert!(matches!(e.fault, Fault::Refused(Refusal::InvalidChar { value: 0xD800 })), "{e:?}");
}

/// `Value::Repeat` evaluates its operand once and writes all elements.
#[test]
fn repeat_hoisted_builds_array() {
    let (tw, t) = world();
    let text = format!(
        "\
local _0: t{u} return
local _1: t{arr} temp e0 value
const c0 = scalar 0x9: t{u}
const c1 = scalar 0x4: t{us}
bb0:
  _1 = repeat(const c0, c1)
  _0 = add(copy _1[0 of 4], copy _1[3 of 4])
  return
",
        u = t.u32,
        arr = t.arr4,
        us = t.usize,
    );
    let tw = tw.with(ROOT, &text);
    assert_eq!(u32_value(&tw), 18);
}
