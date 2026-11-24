//! Bodies in blaze-body's text form, built at a table-backed facts world, then run in
//! `blaze_lir::eval` and compared with the expected value. Type `tN` is row N of the world's
//! `from` table: t0 `i64`, t1 `bool`, t2 `(i64, i64)`, t3 `*mut (i64, i64)`, t4 `(i64, bool)`.

use blaze_build::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
use blaze_build::{Abi, Adts, Built, OverflowChecks, Panic, Subst};
use blaze_lir::eval::{BodyId, Bodies, Outcome, Val, run_calls_in, run_in};
use blaze_lir::func::{BlockKind, Conv, RefId};
use blaze_lir::{Builder, Full, View};
use blaze_lir::builder::Variadic;
use blaze_own::Alloc;
use blaze_typeck::{Adt, IntTy, ItemKey, Mutbl, PNode, PTy, PortableTys};

use crate::{BuildError, build};

/// The one fn item every body here is built as.
const ITEM: ItemKey = ItemKey(1);
/// The callee of the call tests.
const CALLEE: ItemKey = ItemKey(7);
/// Every `panic_const_*` entry, and `panic_cannot_unwind`.
const PANIC: ItemKey = ItemKey(100);
const CANNOT_UNWIND: ItemKey = ItemKey(101);

struct World {
    panic: Panic,
}

impl LayoutFacts for World {
    fn adt(&self, _: ItemKey) -> Option<Adt<'_>> {
        None
    }

    fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> {
        Ok(ReprAttrs::RUST)
    }

    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }

    fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> {
        Ok(ValidRange::default())
    }
}

impl Adts for World {
    fn adt(&self, _: ItemKey) -> Option<Adt<'_>> {
        None
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self
    }

    fn panic(&self) -> Panic {
        self.panic
    }

    fn overflow_checks(&self) -> OverflowChecks {
        OverflowChecks::On
    }

    fn ub_checks(&self) -> blaze_build::UbChecks {
        blaze_build::UbChecks::Off
    }

    fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> {
        Ok(None)
    }

    fn abi(&self, _: ItemKey) -> Option<Abi> {
        Some(Abi::Rust)
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        match name {
            "panic_cannot_unwind" => Some(CANNOT_UNWIND),
            n if blaze_build::PanicConst::ALL.iter().any(|p| p.lang() == n) => Some(PANIC),
            _ => None,
        }
    }
}

/// The `from` table of the module doc, rows in order.
fn from_table() -> PortableTys<'static> {
    let mut t = PortableTys::new_in(Alloc::global());
    let i64_ = t.intern(PNode::Int(IntTy::I64)).unwrap();
    let bool_ = t.intern(PNode::Bool).unwrap();
    let both = t.intern_list(&[i64_, i64_]).unwrap();
    let pair = t.intern(PNode::Tuple(both)).unwrap();
    t.intern(PNode::Ptr(Mutbl::Mut, pair)).unwrap();
    let flagged = t.intern_list(&[i64_, bool_]).unwrap();
    t.intern(PNode::Tuple(flagged)).unwrap();
    t
}

/// `text` parsed, verified and built as `ITEM` at no arguments, under `panic`.
fn built(text: &str, panic: Panic) -> Result<Built<Alloc<'static>>, BuildError> {
    let from = from_table();
    let body = blaze_body::text::parse(text, Alloc::global()).unwrap();
    let verified = blaze_body::verify(&body).unwrap();
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    build(verified, from.view(), ITEM, &mut subst, &World { panic }, Alloc::global())
}

fn printed(b: &Built<Alloc<'static>>) -> impl core::ops::Deref<Target = str> + core::fmt::Display {
    blaze_lir::print(b.func.view())
}

fn returned(outcome: Outcome<Alloc<'static>>) -> blaze_own::Vec<Val, Alloc<'static>> {
    match outcome {
        Outcome::Returned(vals) => {
            let mut out = blaze_own::Vec::with_capacity_in(vals.len(), heap());
            out.extend(vals.iter().copied());
            out
        }
        _ => panic!("the run did not return"),
    }
}

trait DefaultAlloc {
    type A: core::alloc::Allocator;
}
impl<A: core::alloc::Allocator> DefaultAlloc for Vec<u8, A> {
    type A = A;
}
type PromotedRecord = blaze_build::data::DataRecord<<Vec<u8> as DefaultAlloc>::A>;

/// A body of one block that traps: what every panic entry is here.
fn trapping() -> blaze_lir::Func<Alloc<'static>> {
    let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, Alloc::global());
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    b.trap(0);
    b.finish_checked().unwrap()
}

/// Every reference of the root is the one body `callee`.
struct One<'v> {
    callee: View<'v>,
}

impl Bodies for One<'_> {
    fn callee(&self, _: BodyId, _: RefId) -> Option<(BodyId, View<'_>)> {
        Some((BodyId([1; 32]), self.callee))
    }
    fn target(&self, _: BodyId, _: RefId) -> Option<blaze_lir::body::Target> {
        Some(blaze_lir::body::Target::Func(BodyId([1; 32])))
    }
    fn body(&self, id: BodyId) -> Option<View<'_>> {
        if id == BodyId([1; 32]) { Some(self.callee) } else { None }
    }
}

const ADD: &str = "\
local _0: t0 return
local _1: t0 arg 0
local _2: t0 arg 1
bb0:
  _0 = add(copy _1, copy _2)
  return
";

/// `_0 = _1 + _2` on two value locals: one block of parameters, an `add` and a `ret`. Should
/// print a line `= add` and return 42 for 40 and 2.
#[test]
fn add_two_integers() {
    let b = built(ADD, Panic::Abort).unwrap();
    let text = printed(&b);
    assert!(text.contains("add"), "{text}");
    assert!(!text.contains("slot"), "value locals took a slot:\n{text}");
    let out = run_in(b.func.view(), &[Val::Int(40), Val::Int(2)], 1000, Alloc::global()).unwrap();
    assert_eq!(returned(out), [Val::Int(42)]);
}

const OVERFLOW: &str = "\
local _0: t0 return
local _1: t0 arg 0
local _2: t4 temp e0 value
const c0 = scalar 0x1: t0
bb0:
  _2 = checked add(copy _1, const c0)
  assert copy _2.1:t1 == false, overflow_add(copy _1, const c0) then bb1 unwind continue
bb1:
  _0 = copy _2.0:t0
  return
";

/// `_1 + 1` with its overflow assert: 6 for 5 (not taken); a trap for `i64::MAX` (taken: the
/// cold block calls `panic_const_add_overflow`, which traps here). Should print a `cold` block.
#[test]
fn overflow_assert_taken_and_not() {
    let b = built(OVERFLOW, Panic::Abort).unwrap();
    let text = printed(&b);
    assert!(text.contains("cold"), "{text}");
    let panic = trapping();
    let bodies = One { callee: panic.view() };
    let ok = run_calls_in(b.func.view(), BodyId([0; 32]), &[Val::Int(5)], 1000, &bodies, Alloc::global()).unwrap();
    assert_eq!(returned(ok), [Val::Int(6)]);
    let max = i64::MAX as u128;
    let taken = run_calls_in(b.func.view(), BodyId([0; 32]), &[Val::Int(max)], 1000, &bodies, Alloc::global()).unwrap();
    assert!(matches!(taken, Outcome::Trapped { .. }), "the overflow did not reach the panic entry");
}

const LOOP: &str = "\
local _0: t0 return
local _1: t0 temp e0 value
local _2: t1 temp e1 value
const c0 = scalar 0x0: t0
const c1 = scalar 0xa: t0
const c2 = scalar 0x1: t0
bb0:
  _0 = const c0
  _1 = const c0
  goto bb1
bb1:
  _2 = lt(copy _1, const c1)
  switch copy _2 [0 -> bb3] else bb2
bb2:
  _0 = add(copy _0, copy _1)
  _1 = add(copy _1, const c2)
  goto bb1
bb3:
  return
";

/// The sum of 0..10, the sum and the counter carried around the loop as block parameters (no
/// slot): returns 45.
#[test]
fn loop_sums_through_block_parameters() {
    let b = built(LOOP, Panic::Abort).unwrap();
    let lir = printed(&b);
    assert!(!lir.contains("slot"), "the loop's values went to memory:\n{lir}");
    let out = run_in(b.func.view(), &[], 1000, Alloc::global()).unwrap();
    assert_eq!(returned(out), [Val::Int(45)]);
}

const SLOT: &str = "\
local _0: t0 return
local _1: t2 temp e0 value
local _2: t3 temp e1 value
const c0 = scalar 0x7: t0
const c1 = scalar 0x5: t0
bb0:
  _2 = raw mut _1
  _1.0:t0 = const c0
  _1.1:t0 = const c1
  _0 = copy _1.1:t0
  return
";

/// A pair whose address is taken lives in a slot: both fields stored, the second read back.
/// Should print `slot` and return 5 (a wrong field offset returns 7).
#[test]
fn struct_in_a_slot() {
    let b = built(SLOT, Panic::Abort).unwrap();
    let text = printed(&b);
    assert!(text.contains("slot"), "{text}");
    let out = run_in(b.func.view(), &[], 1000, Alloc::global()).unwrap();
    assert_eq!(returned(out), [Val::Int(5)]);
}

const CALLER: &str = "\
local _0: t0 return
const c0 = scalar 0x29: t0
bb0:
  call fn k7<0,0>(const c0) -> _0 then bb1 unwind continue
bb1:
  return
";

const ADD_ONE: &str = "\
local _0: t0 return
local _1: t0 arg 0
const c0 = scalar 0x1: t0
bb0:
  _0 = add(copy _1, const c0)
  return
";

/// The caller passes 41 to `k7`, built from `ADD_ONE`, and returns what it returns: 42. Should
/// print a `call`.
#[test]
fn call_and_return() {
    let caller = built(CALLER, Panic::Abort).unwrap();
    let callee = built(ADD_ONE, Panic::Abort).unwrap();
    assert_eq!(caller.refs.len(), 1);
    assert_eq!(caller.refs[0].item, CALLEE);
    let text = printed(&caller);
    assert!(text.contains("call"), "{text}");
    let bodies = One { callee: callee.func.view() };
    let out = run_calls_in(caller.func.view(), BodyId([0; 32]), &[], 1000, &bodies, Alloc::global()).unwrap();
    assert_eq!(returned(out), [Val::Int(42)]);
}

const UNWINDING: &str = "\
local _0: t0 return
local _1: t0 arg 0
bb0:
  call fn k7<0,0>(copy _1) -> _0 then bb1 unwind bb2
bb1:
  return
bb2 cleanup:
  resume
";

/// Under `Panic::Unwind` a call naming a cleanup block is an `invoke` whose landing block (a
/// `cleanup` block with the exception parameter) jumps to the body's cleanup block, which
/// resumes. The evaluator refuses `invoke`, so the printed text is checked: `invoke`,
/// `cleanup` and `resume`. Under `Panic::Abort` the same body prints a plain `call` and no
/// `invoke`.
#[test]
fn call_with_cleanup_is_an_invoke() {
    let b = built(UNWINDING, Panic::Unwind).unwrap();
    let text = printed(&b);
    assert!(text.contains("invoke"), "{text}");
    assert!(text.contains("cleanup"), "{text}");
    assert!(text.contains("resume"), "{text}");
    let plain = built(UNWINDING, Panic::Abort).unwrap();
    let text = printed(&plain);
    assert!(!text.contains("invoke") && text.contains("call"), "{text}");
}

/// A row this builder does not translate is refused by name, here an inline `asm!` block.
/// Should give `BuildError::Unsupported` at row 0.
#[test]
fn unsupported_row_is_named() {
    let text = "\
local _0: t0 return
bb0:
  asm 0() unwind continue
";
    match built(text, Panic::Abort) {
        Err(BuildError::Unsupported { stmt, what }) => {
            assert_eq!(stmt.0, 0);
            assert!(what.contains("asm"), "{what}");
        }
        other => panic!("not refused by name: {:?}", other.err()),
    }
}

/// `ConstKind::StaticAddr` and `BodyValue::ThreadLocalAddr` lower to `data_addr` / `tls_addr`
/// and populate `Built.statics` / `Built.thread_locals` deduplicated by `ItemKey`.
#[test]
fn static_and_thread_local_addresses_emit_refs() {
    let static_text = "\
local _0: t3 return
local _1: t3 temp e0 value
const c0 = static k42: t3
bb0:
  _1 = const c0
  _0 = const c0
  return
";
    let b = built(static_text, Panic::Abort).expect("static addr lowers cleanly");
    assert_eq!(b.statics.as_slice(), &[ItemKey(0x42)]);
    let lir = printed(&b);
    assert!(lir.contains("data_addr"), "{lir}");

    let tls_text = "\
local _0: t3 return
const c0 = static k77: t3
bb0:
  _0 = tls c0
  return
";
    let b_tls = built(tls_text, Panic::Abort).expect("tls addr lowers cleanly");
    assert_eq!(b_tls.thread_locals.as_slice(), &[ItemKey(0x77)]);
    let lir_tls = printed(&b_tls);
    assert!(lir_tls.contains("tls_addr"), "{lir_tls}");
}

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// Configurable facts world for exercising C ABI, `rust-call`, intrinsics, overflow checks,
/// unions, and `dyn Trait` vtables without touching the global allocator.
struct RichWorld<'a> {
    abi: Abi,
    callee_abi: Abi,
    overflow: OverflowChecks,
    intrinsic_item: Option<(ItemKey, blaze_build::Intrinsic)>,
    union_key: Option<ItemKey>,
    union_types: Option<PortableTys<'static>>,
    union_fields: &'a [blaze_typeck::Field],
    union_variants: &'a [blaze_typeck::Variant],
    trait_key: Option<ItemKey>,
    trait_methods: &'a [blaze_build::vtable::ObjectMethod],
}

impl<'a> RichWorld<'a> {
    fn base() -> Self {
        Self {
            abi: Abi::Rust,
            callee_abi: Abi::Rust,
            overflow: OverflowChecks::On,
            intrinsic_item: None,
            union_key: None,
            union_types: None,
            union_fields: &[],
            union_variants: &[],
            trait_key: None,
            trait_methods: &[],
        }
    }
}

impl LayoutFacts for RichWorld<'_> {
    fn adt(&self, k: ItemKey) -> Option<Adt<'_>> {
        if self.union_key == Some(k) {
            let tys = self.union_types.as_ref()?;
            return Some(Adt {
                kind: blaze_typeck::AdtKind::Union,
                generics: 0,
                types: tys.view(),
                variants: self.union_variants,
                fields: self.union_fields,
            });
        }
        None
    }
    fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> {
        Ok(ReprAttrs::RUST)
    }
    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }
    fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> {
        Ok(ValidRange::default())
    }
}

impl Adts for RichWorld<'_> {
    fn adt(&self, k: ItemKey) -> Option<Adt<'_>> {
        LayoutFacts::adt(self, k)
    }
    fn facts(&self) -> &dyn LayoutFacts {
        self
    }
    fn panic(&self) -> Panic {
        Panic::Abort
    }
    fn overflow_checks(&self) -> OverflowChecks {
        self.overflow
    }
    fn ub_checks(&self) -> blaze_build::UbChecks {
        blaze_build::UbChecks::Off
    }
    fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> {
        Ok(None)
    }
    fn abi(&self, k: ItemKey) -> Option<Abi> {
        if k == CALLEE { Some(self.callee_abi) } else { Some(self.abi) }
    }
    fn intrinsic(&self, k: ItemKey) -> Option<blaze_build::Intrinsic> {
        self.intrinsic_item.and_then(|(ik, intr)| (ik == k).then_some(intr))
    }
    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        if self.trait_key == Some(t) {
            return Some(blaze_build::vtable::ObjectTrait {
                supers: &[],
                tys: blaze_typeck::PTys::EMPTY,
                methods: self.trait_methods,
                shape: blaze_ids::ContentId([7; 32]),
            });
        }
        None
    }
    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        match name {
            "panic_cannot_unwind" => Some(CANNOT_UNWIND),
            n if blaze_build::PanicConst::ALL.iter().any(|p| p.lang() == n) => Some(PANIC),
            _ => None,
        }
    }
}

/// With `OverflowChecks::Off`, arithmetic overflow asserts (`add`, `sub`, `mul`, `shl`, `shr`, `neg`)
/// are elided so `i64::MAX + 1` wraps to `i64::MIN` with no `cold` panic block.
#[test]
fn overflow_checks_off_elides_add_overflow_assert() {
    let from = from_table();
    let body = blaze_body::text::parse(OVERFLOW, heap()).unwrap();
    let verified = blaze_body::verify(&body).unwrap();
    let mut ground = PortableTys::new_in(heap());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
    let mut w = RichWorld::base();
    w.overflow = OverflowChecks::Off;
    let b = build(verified, from.view(), ITEM, &mut subst, &w, heap()).unwrap();
    let text = printed(&b);
    assert!(!text.contains("cold"), "overflow check should be elided:\n{text}");
    let out = run_in(b.func.view(), &[Val::Int(u128::from(i64::MAX.cast_unsigned()))], 1000, heap()).unwrap();
    assert_eq!(returned(out), [Val::Int(u128::from(i64::MIN.cast_unsigned()))]);
}

/// `extern "C"` functions and calls handle both small composites (`Pass::Words` in 1-2 `i64`
/// registers) and >16-byte composites (`Pass::Memory` `byval`/`sret`).
#[test]
fn c_abi_words_and_memory_composites_execute() {
    let mut t = PortableTys::new_in(heap());
    let i64_ = t.intern(PNode::Int(IntTy::I64)).unwrap(); // t0
    let i32_ = t.intern(PNode::Int(IntTy::I32)).unwrap(); // t1
    let l12 = t.intern_list(&[i32_, i32_, i32_]).unwrap();
    let _c12 = t.intern(PNode::Tuple(l12)).unwrap(); // t2: 12-byte (no padding) C Words composite
    let l24 = t.intern_list(&[i64_, i64_, i64_]).unwrap();
    let _c24 = t.intern(PNode::Tuple(l24)).unwrap(); // t3: 24-byte C Memory (byval/sret) composite

    // Callee: extern "C" fn(p: (i32, i32, i32)) -> (i32, i32, i32) { (p.0 + 1, p.1, p.2) }
    let callee_text = "\
local _0: t2 return
local _1: t2 arg 0
local _2: t1 temp e0 value
const c0 = scalar 0x1: t1
bb0:
  _2 = add(copy _1.0:t1, const c0)
  _0 = tuple t2(copy _2, copy _1.1:t1, copy _1.2:t1)
  return
";
    // Caller: extern "C" fn() -> i64 { let r = k7((10, 20, 12)); (r.0 + r.1 + r.2) as i64 }
    let caller_text = "\
local _0: t0 return
local _1: t2 temp e0 value
local _2: t2 temp e1 value
local _3: t1 temp e2 value
local _4: t1 temp e3 value
const c0 = scalar 0xa: t1
const c1 = scalar 0x14: t1
const c2 = scalar 0xc: t1
bb0:
  _1 = tuple t2(const c0, const c1, const c2)
  call fn k7<0,0>(move _1) -> _2 then bb1 unwind continue
bb1:
  _3 = add(copy _2.0:t1, copy _2.1:t1)
  _4 = add(copy _3, copy _2.2:t1)
  _0 = cast int_to_int(copy _4) as t0
  return
";
    let mut w = RichWorld::base();
    w.abi = Abi::C;
    w.callee_abi = Abi::C;
    let build_one = |text: &str, item: ItemKey| {
        let body = blaze_body::text::parse(text, heap()).unwrap();
        let verified = blaze_body::verify(&body).unwrap();
        let mut ground = PortableTys::new_in(heap());
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
        build(verified, t.view(), item, &mut subst, &w, heap()).unwrap()
    };
    let callee = build_one(callee_text, CALLEE);
    let caller = build_one(caller_text, ITEM);
    let bodies = One { callee: callee.func.view() };
    let out = run_calls_in(caller.func.view(), BodyId([0; 32]), &[], 1000, &bodies, heap()).unwrap();
    assert_eq!(returned(out), [Val::Int(43)]);

    // 24-byte `Pass::Memory` (`byval` + `sret`): extern "C" fn(t3) -> t3, called from caller.
    let callee24 = "\
local _0: t3 return
local _1: t3 arg 0
local _2: t0 temp e0 value
bb0:
  _2 = add(copy _1.0:t0, copy _1.1:t0)
  _0 = tuple t3(copy _2, copy _1.1:t0, copy _1.2:t0)
  return
";
    let caller24 = "\
local _0: t0 return
local _1: t3 temp e0 value
local _2: t3 temp e1 value
const c0 = scalar 0xa: t0
const c1 = scalar 0x14: t0
const c2 = scalar 0xc: t0
bb0:
  _1 = tuple t3(const c0, const c1, const c2)
  call fn k7<0,0>(move _1) -> _2 then bb1 unwind continue
bb1:
  _0 = add(copy _2.0:t0, copy _2.2:t0)
  return
";
    let c24_b = build_one(callee24, CALLEE);
    let r24_b = build_one(caller24, ITEM);
    let bodies24 = One { callee: c24_b.func.view() };
    let out24 = run_calls_in(r24_b.func.view(), BodyId([0; 32]), &[], 1000, &bodies24, heap()).unwrap();
    assert_eq!(returned(out24), [Val::Int(42)]);
}

/// `extern "rust-call"` spreads the trailing tuple argument across individual Rust ABI parameters
/// on both caller and callee sides.
#[test]
fn rust_call_abi_spreads_tuple_argument() {
    let from = from_table(); // t0 = i64, t2 = (i64, i64)
    let callee_text = "\
local _0: t0 return
local _1: t0 arg 0
local _2: t2 arg 1
local _3: t0 temp e0 value
bb0:
  _3 = add(copy _1, copy _2.0:t0)
  _0 = add(copy _3, copy _2.1:t0)
  return
";
    let caller_text = "\
local _0: t0 return
local _1: t2 temp e0 value
const c0 = scalar 0x5: t0
const c1 = scalar 0xa: t0
const c2 = scalar 0x1b: t0
bb0:
  _1 = tuple t2(const c1, const c2)
  call fn k7<0,0>(const c0, move _1) -> _0 then bb1 unwind continue
bb1:
  return
";
    let mut w = RichWorld::base();
    w.abi = Abi::Rust;
    w.callee_abi = Abi::RustCall;
    let build_one = |text: &str, item: ItemKey| {
        let body = blaze_body::text::parse(text, heap()).unwrap();
        let verified = blaze_body::verify(&body).unwrap();
        let mut ground = PortableTys::new_in(heap());
        let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
        build(verified, from.view(), item, &mut subst, &w, heap()).unwrap()
    };
    let callee = build_one(callee_text, CALLEE);
    let caller = build_one(caller_text, ITEM);
    let bodies = One { callee: callee.func.view() };
    let out = run_calls_in(caller.func.view(), BodyId([0; 32]), &[], 1000, &bodies, heap()).unwrap();
    assert_eq!(returned(out), [Val::Int(42)]);
}

/// `Intrinsic::Table` calls (`ctpop`) lower directly through `blaze_build::intrinsics::build`
/// and execute with zero call overhead.
#[test]
fn table_intrinsics_lower_and_execute() {
    let mut t = PortableTys::new_in(heap());
    let u64_ = t.intern(PNode::Uint(blaze_typeck::UintTy::U64)).unwrap(); // t0
    let _u32_ = t.intern(PNode::Uint(blaze_typeck::UintTy::U32)).unwrap(); // t1 (`ctpop` returns u32)
    let _g = t.intern_list(&[u64_]).unwrap(); // list 0..1 = [u64]
    let text = "\
local _0: t1 return
local _1: t0 arg 0
bb0:
  call intrinsic k7<0,1>(copy _1) -> _0 then bb1 unwind continue
bb1:
  return
";
    let mut w = RichWorld::base();
    w.intrinsic_item = Some((CALLEE, blaze_build::Intrinsic::Table(blaze_build::intrinsics::Which::Ctpop)));
    let body = blaze_body::text::parse(text, heap()).unwrap();
    let verified = blaze_body::verify(&body).unwrap();
    let mut ground = PortableTys::new_in(heap());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
    let b = build(verified, t.view(), ITEM, &mut subst, &w, heap()).unwrap();
    assert!(b.refs.is_empty(), "intrinsic lowered inline, no function ref");
    let out = run_in(b.func.view(), &[Val::Int(0xF0F0)], 1000, heap()).unwrap();
    assert_eq!(returned(out), [Val::Int(8)]);
}

/// `CastKind::Unsize` to `&dyn Trait` emits a `Vtable` reference into `Built::vtables`, and
/// `Callee::Method` with `Self = dyn Trait` dispatches through the vtable slot via `call_indirect`.
#[test]
fn dyn_vtable_unsize_and_method_dispatch() {
    const TRAIT: ItemKey = ItemKey(55);
    let mut t = PortableTys::new_in(heap());
    let i64_ = t.intern(PNode::Int(IntTy::I64)).unwrap(); // t0
    let ref_i64 = t.intern(PNode::Ref(Mutbl::Not, i64_)).unwrap(); // t1
    let empty = t.intern_list(&[]).unwrap();
    let ex = t.intern(PNode::ExTrait(TRAIT, empty)).unwrap(); // t2
    let preds = t.intern_list(&[ex]).unwrap();
    let dyn_tr = t.intern(PNode::Dynamic(preds)).unwrap(); // t3
    let _ref_dyn = t.intern(PNode::Ref(Mutbl::Not, dyn_tr)).unwrap(); // t4
    let dyn_args = t.intern_list(&[dyn_tr]).unwrap(); // list 1..2 = [dyn Tr]
    assert_eq!((dyn_args.start, dyn_args.len), (1, 1));
    let _ = ref_i64;
    let text = "\
local _0: t0 return
local _1: t1 arg 0
local _2: t4 temp e0 value
bb0:
  _2 = cast unsize(copy _1) as t4
  call method k7<1,1>(copy _2) -> _0 then bb1 unwind continue
bb1:
  return
";
    let methods = [blaze_build::vtable::ObjectMethod {
        item: CALLEE,
        dispatch: blaze_build::vtable::MethodDispatch::Direct,
    }];
    let mut w = RichWorld::base();
    w.trait_key = Some(TRAIT);
    w.trait_methods = &methods;
    let body = blaze_body::text::parse(text, heap()).unwrap();
    let verified = blaze_body::verify(&body).unwrap();
    let mut ground = PortableTys::new_in(heap());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
    let b = build(verified, t.view(), ITEM, &mut subst, &w, heap()).unwrap();
    assert_eq!(b.vtables.len(), 1);
    assert_eq!(b.vtables[0].principal, TRAIT);
    let lir = printed(&b);
    assert!(lir.contains("call_indirect"), "dyn method should dispatch via vtable:\n{lir}");
}

/// `ConstKind::Param` and large `Value::Repeat` (> 64 elements, lowered as a counted LIR loop)
/// build and execute accurately; missing `ConstKind::Item`/`Anon` report typed errors.
#[test]
fn const_param_item_anon_and_large_repeat_execute() {
    let mut t = PortableTys::new_in(heap());
    let i64_ = t.intern(PNode::Int(IntTy::I64)).unwrap(); // t0
    let usize_ = t.intern(PNode::Uint(blaze_typeck::UintTy::Usize)).unwrap(); // t1
    let c100 = t.intern(PNode::ConstValue { ty: usize_, bits: 100 }).unwrap(); // t2
    let _arr100 = t.intern(PNode::Array(i64_, c100)).unwrap(); // t3

    let text = "\
local _0: t0 return
local _1: t3 temp e0 value
local _2: t0 temp e1 value
local _3: t0 temp e2 value
const c0 = param 0: t0
const c1 = scalar 0x64: t1
bb0:
  _1 = repeat(const c0, c1)
  _2 = copy _1[99 of 100]
  _3 = copy _1[0 of 100]
  _0 = add(copy _2, copy _3)
  return
";
    let w = RichWorld::base();
    let body = blaze_body::text::parse(text, heap()).unwrap();
    let verified = blaze_body::verify(&body).unwrap();
    let mut ground = PortableTys::new_in(heap());
    let g_i64 = ground.intern(PNode::Int(IntTy::I64)).unwrap();
    let g_p0 = ground.intern(PNode::ConstValue { ty: g_i64, bits: 21 }).unwrap();
    let mut subst = Subst { args: &[g_p0], ground: &mut ground, scratch: heap() };
    let b = build(verified, t.view(), ITEM, &mut subst, &w, heap()).unwrap();
    // 21 (arr[99]) + 21 (arr[0]) = 42
    let out = run_in(b.func.view(), &[], 10000, heap()).unwrap();
    assert_eq!(returned(out), [Val::Int(42)]);

    // Missing const item / const block surface typed errors rather than Unsupported.
    let item_text = "local _0: t0 return\nconst c0 = item k99<0,0>: t0\nbb0:\n  _0 = const c0\n  return\n";
    let ib = blaze_body::text::parse(item_text, heap()).unwrap();
    let iv = blaze_body::verify(&ib).unwrap();
    let mut g2 = PortableTys::new_in(heap());
    let mut s2 = Subst { args: &[], ground: &mut g2, scratch: heap() };
    assert!(matches!(build(iv, t.view(), ITEM, &mut s2, &w, heap()), Err(BuildError::NoConstValue { .. })));

    let anon_text = "local _0: t0 return\nconst c0 = anon e5: t0\nbb0:\n  _0 = const c0\n  return\n";
    let ab = blaze_body::text::parse(anon_text, heap()).unwrap();
    let av = blaze_body::verify(&ab).unwrap();
    let mut g3 = PortableTys::new_in(heap());
    let mut s3 = Subst { args: &[], ground: &mut g3, scratch: heap() };
    assert!(matches!(build(av, t.view(), ITEM, &mut s3, &w, heap()), Err(BuildError::NoConstBlock { .. })));
}

/// `Proj::ConstIndex` from the end of a slice, `Proj::Subslice`, `CastKind::Transmute` of a
/// pair, and `AdtKind::Union` aggregates all build and execute accurately.
#[test]
fn slice_from_end_subslice_transmute_pair_and_union_execute() {
    const UNION_K: ItemKey = ItemKey(88);
    let mut t = PortableTys::new_in(heap());
    let i64_ = t.intern(PNode::Int(IntTy::I64)).unwrap(); // t0
    let u64_ = t.intern(PNode::Uint(blaze_typeck::UintTy::U64)).unwrap(); // t1
    let usize_ = t.intern(PNode::Uint(blaze_typeck::UintTy::Usize)).unwrap(); // t2
    let c4 = t.intern(PNode::ConstValue { ty: usize_, bits: 4 }).unwrap(); // t3
    let arr4 = t.intern(PNode::Array(i64_, c4)).unwrap(); // t4
    let _ref_arr4 = t.intern(PNode::Ref(Mutbl::Not, arr4)).unwrap(); // t5
    let slice = t.intern(PNode::Slice(i64_)).unwrap(); // t6
    let _ref_slice = t.intern(PNode::Ref(Mutbl::Not, slice)).unwrap(); // t7
    let pi = t.intern_list(&[i64_, i64_]).unwrap();
    let _pair_i = t.intern(PNode::Tuple(pi)).unwrap(); // t8
    let pu = t.intern_list(&[u64_, u64_]).unwrap();
    let _pair_u = t.intern(PNode::Tuple(pu)).unwrap(); // t9
    let empty = t.intern_list(&[]).unwrap();
    let _union_ty = t.intern(PNode::Adt(UNION_K, empty)).unwrap(); // t10

    let text = "\
local _0: t0 return
local _1: t4 temp e0 value
local _2: t5 temp e1 value
local _3: t7 temp e2 value
local _4: t7 temp e3 value
local _5: t8 temp e4 value
local _6: t9 temp e5 value
local _7: t10 temp e6 value
origin 'o0
origin 'o1
const c0 = scalar 0xa: t0
const c1 = scalar 0x14: t0
const c2 = scalar 0x1e: t0
const c3 = scalar 0x28: t0
bb0:
  _1 = array t4(const c0, const c1, const c2, const c3)
  _2 = borrow shared _1 L0 'o0
  _3 = cast unsize(copy _2) as t7
  _4 = borrow shared _3.*[1..-1] L1 'o1
  _5 = tuple t8(copy _4.*[-1 of 1], copy _4.*[0 of 1])
  _6 = cast transmute(move _5) as t9
  _7 = adt t10 #0(copy _6.0:t1)
  _0 = add(copy _7.1:t0, copy _6.1:t1)
  return
";
    let mut ut = PortableTys::new_in(heap());
    let u_u64 = ut.intern(PNode::Uint(blaze_typeck::UintTy::U64)).unwrap();
    let u_i64 = ut.intern(PNode::Int(IntTy::I64)).unwrap();
    let union_fields = [
        blaze_typeck::Field { name: blaze_typeck::Name::of("a"), ty: u_u64 },
        blaze_typeck::Field { name: blaze_typeck::Name::of("b"), ty: u_i64 },
    ];
    let union_variants = [blaze_typeck::Variant {
        key: UNION_K,
        name: blaze_typeck::Name::of("U"),
        fields: blaze_typeck::List { start: 0, len: 2 },
        ctor: blaze_typeck::CtorKind::Braces,
    }];
    let mut w = RichWorld::base();
    w.union_key = Some(UNION_K);
    w.union_types = Some(ut);
    w.union_fields = &union_fields;
    w.union_variants = &union_variants;

    let body = blaze_body::text::parse(text, heap()).unwrap();
    let verified = blaze_body::verify(&body).unwrap();
    let mut ground = PortableTys::new_in(heap());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
    let b = build(verified, t.view(), ITEM, &mut subst, &w, heap()).unwrap();
    // arr = [10, 20, 30, 40]; subslice [1..-1] = [20, 30]; [-1] = 30, [0] = 20 -> 30 + 20 = 50!
    let out = run_in(b.func.view(), &[], 1000, heap()).unwrap();
    assert_eq!(returned(out), [Val::Int(50)]);
}

/// `ConstKind::Promoted(expr_id)` loads its evaluated `DataRecord` via `Adts::promoted_value`
/// and returns its address; a missing record is refused with `BuildError::NoPromotedValue`.
#[test]
fn promoted_const_loads_data_row_and_executes() {
    struct PromotedWorld {
        rec: Option<PromotedRecord>,
    }
    impl LayoutFacts for PromotedWorld {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> { None }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
    }
    impl Adts for PromotedWorld {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> { None }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> { Ok(None) }
        fn abi(&self, _: ItemKey) -> Option<Abi> { Some(Abi::Rust) }
        fn promoted_value(&self, owner: ItemKey, borrow: blaze_typeck::ExprId, _: &[PTy]) -> Option<&PromotedRecord> {
            if owner == ITEM && borrow == blaze_typeck::ExprId(5) { self.rec.as_ref() } else { None }
        }
    }

    let mut t = PortableTys::new_in(heap());
    let i64_ = t.intern(PNode::Int(IntTy::I64)).unwrap(); // t0
    let _ref_i64 = t.intern(PNode::Ref(Mutbl::Not, i64_)).unwrap(); // t1
    let text = "\
local _0: t0 return
local _1: t1 temp e5 value
const c0 = promoted 5: t1
bb0:
  _1 = const c0
  _0 = copy _1.*
  return
";
    let body = blaze_body::text::parse(text, heap()).unwrap();
    let verified = blaze_body::verify(&body).unwrap();

    let bytes = 42i64.to_le_bytes().to_vec();
    let allocs = core::iter::once(blaze_build::data::DataAlloc { bytes, align: 3, mutable: false, relocs: Default::default() }).collect();
    let w = PromotedWorld { rec: Some(blaze_build::data::DataRecord { allocs }) };
    let mut ground = PortableTys::new_in(heap());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
    let b = build(verified, t.view(), ITEM, &mut subst, &w, heap()).unwrap();
    let lir = printed(&b);
    assert!(lir.contains("data_addr"), "{lir}");
    let out = run_in(b.func.view(), &[], 1000, heap()).unwrap();
    assert_eq!(returned(out), [Val::Int(42)]);

    let missing = PromotedWorld { rec: None };
    let mut g2 = PortableTys::new_in(heap());
    let mut s2 = Subst { args: &[], ground: &mut g2, scratch: heap() };
    assert!(matches!(
        build(verified, t.view(), ITEM, &mut s2, &missing, heap()),
        Err(BuildError::NoPromotedValue { stmt, expr }) if stmt.0 == 0 && expr == blaze_typeck::ExprId(5)
    ));
}

/// `CastKind::ClosureFnPointer` reifies a non-capturing closure to its `FnOnce::call_once`
/// instance address and executes through the resulting function pointer.
#[test]
fn closure_fn_pointer_cast_emits_call_once_ref_and_executes() {
    const CALL_ONCE: ItemKey = ItemKey(77);
    struct ClosureWorld {
        ctys: PortableTys<'static>,
        csig: PTy,
        cenv: PTy,
    }
    impl LayoutFacts for ClosureWorld {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> { None }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
        fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::layout::ClosureFacts<'_>> {
            if owner == ITEM && ordinal == 0 {
                Some(blaze_build::layout::ClosureFacts {
                    types: self.ctys.view(),
                    sig: self.csig,
                    env: self.cenv,
                    kind: blaze_build::FnTrait::Fn,
                })
            } else {
                None
            }
        }
    }
    impl Adts for ClosureWorld {
        fn adt(&self, _: ItemKey) -> Option<Adt<'_>> { None }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> { Ok(None) }
        fn abi(&self, k: ItemKey) -> Option<Abi> {
            if k == CALL_ONCE { Some(Abi::RustCall) } else { Some(Abi::Rust) }
        }
        fn fn_trait_method(&self, which: blaze_build::FnTrait) -> Option<ItemKey> {
            if which == blaze_build::FnTrait::FnOnce { Some(CALL_ONCE) } else { None }
        }
    }

    let mut ctys = PortableTys::new_in(heap());
    let ci64 = ctys.intern(PNode::Int(IntTy::I64)).unwrap();
    let sig_list = ctys.intern_list(&[ci64, ci64]).unwrap();
    let csig = ctys.intern(PNode::FnPtr(sig_list, blaze_typeck::FnPtrHeader::RUST)).unwrap();
    let empty_env = ctys.intern_list(&[]).unwrap();
    let cenv = ctys.intern(PNode::Tuple(empty_env)).unwrap();
    let w = ClosureWorld { ctys, csig, cenv };

    let mut t = PortableTys::new_in(heap());
    let i64_ = t.intern(PNode::Int(IntTy::I64)).unwrap(); // t0
    let empty = t.intern_list(&[]).unwrap();
    let _cty = t.intern(PNode::Closure { owner: ITEM, ordinal: 0, args: empty }).unwrap(); // t1
    let fpl = t.intern_list(&[i64_, i64_]).unwrap();
    let _fpty = t.intern(PNode::FnPtr(fpl, blaze_typeck::FnPtrHeader::RUST)).unwrap(); // t2

    let text = "\
local _0: t0 return
local _1: t0 arg 0
local _2: t1 temp e0 value
local _3: t2 temp e1 value
bb0:
  _2 = closure t1()
  _3 = cast closure_fn_pointer(move _2) as t2
  call value copy _3(copy _1) -> _0 then bb1 unwind continue
bb1:
  return
";
    let body = blaze_body::text::parse(text, heap()).unwrap();
    let verified = blaze_body::verify(&body).unwrap();
    let mut ground = PortableTys::new_in(heap());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: heap() };
    let b = build(verified, t.view(), ITEM, &mut subst, &w, heap()).unwrap();
    assert_eq!(b.refs.len(), 1);
    assert_eq!(b.refs[0].item, CALL_ONCE);

    // Build a simple `|x| x + 10` callee body and execute through the function pointer!
    let mut cb: Builder<Alloc<'static>, Full> = Builder::new_in(64, heap());
    let p = blaze_lir::builder::Param::of(blaze_lir::Ty::I64);
    let sig = cb.sig(Conv::C, Variadic::No, &[p], &[p]);
    cb.set_signature(sig);
    let ent = cb.create_block(BlockKind::Normal);
    cb.begin(ent);
    let x = cb.param(blaze_lir::Ty::I64);
    let ten = cb.iconst(blaze_lir::Ty::I64, 10);
    let sum = cb.binary(blaze_lir::Op::Add, blaze_lir::Flags::NONE, x, ten);
    cb.ret(&[sum]);
    let callee_func = cb.finish_checked().unwrap();

    let bodies = One { callee: callee_func.view() };
    let out = run_calls_in(b.func.view(), BodyId([0; 32]), &[Val::Int(32)], 1000, &bodies, heap()).unwrap();
    assert_eq!(returned(out), [Val::Int(42)]);
}

/// `#[track_caller]`: a tracked fn takes a hidden last `Location` ptr and forwards it to a tracked
/// callee (and returns it from `caller_location`), while an untracked caller materializes a
/// `Location` data row from `def_site` + statement span, and `ReifyFnPointer` uses `REIFY_SHIM_ITEM`.
#[test]
fn track_caller_receives_forwards_and_materializes_location() {
    const TRACKED_OUTER: ItemKey = ItemKey(0x201);
    const TRACKED_INNER: ItemKey = ItemKey(0x202);
    const UNTRACKED_CALLER: ItemKey = ItemKey(0x203);
    const LOC_ADT: ItemKey = ItemKey(0x204);
    const CALLER_LOC_INTR: ItemKey = ItemKey(0x205);

    struct TrackWorld {
        loc_tys: PortableTys<'static>,
        loc_fields: [blaze_typeck::Field; 3],
        loc_variants: [blaze_typeck::Variant; 1],
    }
    impl LayoutFacts for TrackWorld {
        fn adt(&self, k: ItemKey) -> Option<Adt<'_>> {
            if k == LOC_ADT {
                Some(Adt {
                    kind: blaze_typeck::AdtKind::Struct,
                    generics: 0,
                    types: self.loc_tys.view(),
                    variants: &self.loc_variants,
                    fields: &self.loc_fields,
                })
            } else {
                None
            }
        }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
    }
    impl Adts for TrackWorld {
        fn adt(&self, k: ItemKey) -> Option<Adt<'_>> { LayoutFacts::adt(self, k) }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> { Ok(None) }
        fn abi(&self, _: ItemKey) -> Option<Abi> { Some(Abi::Rust) }
        fn intrinsic(&self, k: ItemKey) -> Option<blaze_build::Intrinsic> {
            if k == CALLER_LOC_INTR {
                Some(blaze_build::Intrinsic::Table(blaze_build::intrinsics::Which::CallerLocation))
            } else {
                None
            }
        }
        fn location_adt(&self) -> Option<ItemKey> { Some(LOC_ADT) }
        fn track_caller(&self, inst: blaze_build::Instance, _: &mut PortableTys<'_>) -> Option<bool> {
            Some(inst.item == TRACKED_OUTER || inst.item == TRACKED_INNER)
        }
        fn def_site(&self, item: ItemKey) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> {
            Ok(blaze_build::CallerSite { file: "test.rs", item, offset: 10 })
        }
    }

    let mut lt = PortableTys::new_in(heap());
    let str_t = lt.intern(PNode::Str).unwrap();
    let ref_str = lt.intern(PNode::Ref(Mutbl::Not, str_t)).unwrap();
    let u32_t = lt.intern(PNode::Uint(blaze_typeck::UintTy::U32)).unwrap();
    let loc_fields = [
        blaze_typeck::Field { name: blaze_typeck::Name::of("file"), ty: ref_str },
        blaze_typeck::Field { name: blaze_typeck::Name::of("line"), ty: u32_t },
        blaze_typeck::Field { name: blaze_typeck::Name::of("col"), ty: u32_t },
    ];
    let loc_variants = [blaze_typeck::Variant {
        key: LOC_ADT,
        name: blaze_typeck::Name::of("Location"),
        fields: blaze_typeck::List { start: 0, len: 3 },
        ctor: blaze_typeck::CtorKind::Braces,
    }];
    let w = TrackWorld { loc_tys: lt, loc_fields, loc_variants };

    let mut t = PortableTys::new_in(heap());
    let i64_ = t.intern(PNode::Int(IntTy::I64)).unwrap(); // t0
    let empty = t.intern_list(&[]).unwrap();
    let loc_t = t.intern(PNode::Adt(LOC_ADT, empty)).unwrap(); // t1
    let _ref_loc = t.intern(PNode::Ref(Mutbl::Not, loc_t)).unwrap(); // t2
    let _fn_def = t.intern(PNode::FnDef(TRACKED_INNER, empty)).unwrap(); // t3
    let fpl = t.intern_list(&[i64_, i64_]).unwrap();
    let _fn_ptr = t.intern(PNode::FnPtr(fpl, blaze_typeck::FnPtrHeader::RUST)).unwrap(); // t4

    // 1. Tracked fn forwarding hidden location to `TRACKED_INNER` and reading `caller_location`:
    let fwd_text = "\
local _0: t2 return
local _1: t0 arg 0
local _2: t0 temp e0 value
bb0:
  call fn k202<0,0>(copy _1) -> _2 then bb1 unwind continue @12..20
bb1:
  call fn k205<0,0>() -> _0 then bb2 unwind continue @22..30
bb2:
  return
";
    let fb = blaze_body::text::parse(fwd_text, heap()).unwrap();
    let fv = blaze_body::verify(&fb).unwrap();
    let mut g1 = PortableTys::new_in(heap());
    let mut s1 = Subst { args: &[], ground: &mut g1, scratch: heap() };
    let b1 = build(fv, t.view(), TRACKED_OUTER, &mut s1, &w, heap()).unwrap();
    let lir1 = printed(&b1);
    // Entry takes `(i64, ptr)` (extra hidden location arg!) and passes it to `k202`!
    assert!(lir1.contains("i64, ptr"), "expected hidden location param:\n{lir1}");
    assert!(!lir1.contains("data_addr"), "forwarded location must not materialize new row:\n{lir1}");

    // 2. Untracked caller calling `TRACKED_INNER` + reifying `TRACKED_INNER` as fn ptr:
    let call_text = "\
local _0: t4 return
local _1: t0 arg 0
local _2: t0 temp e0 value
const c0 = fn k202<0,0>: t3
bb0:
  call fn k202<0,0>(copy _1) -> _2 then bb1 unwind continue @42..50
bb1:
  _0 = cast reify_fn_pointer(const c0) as t4
  return
";
    let cb = blaze_body::text::parse(call_text, heap()).unwrap();
    let cv = blaze_body::verify(&cb).unwrap();
    let mut g2 = PortableTys::new_in(heap());
    let mut s2 = Subst { args: &[], ground: &mut g2, scratch: heap() };
    let b2 = build(cv, t.view(), UNTRACKED_CALLER, &mut s2, &w, heap()).unwrap();
    let lir2 = printed(&b2);
    assert!(lir2.contains("data_addr"), "untracked caller must materialize Location data row:\n{lir2}");
    assert!(
        b2.refs.iter().any(|r| r.item == blaze_build::REIFY_SHIM_ITEM),
        "reifying tracked fn must target REIFY_SHIM_ITEM: {:?}",
        b2.refs
    );
}
