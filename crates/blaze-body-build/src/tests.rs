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

#[test]
fn synthetic_drop_glue_and_shims_build_and_verify() {
    use blaze_typeck::{Adt, AdtKind, CtorKind, Field, FnPtrHeader, FnSig, List, Name, Variant};
    use crate::{build_closure_shim, build_ctor_fn, build_drop_glue, build_fn_shim, build_reify_shim, build_vtable_shim};

    fn mk_sig<'w>(types: blaze_typeck::PTys<'w>, inputs: &'w [PTy], output: PTy) -> FnSig<'w> {
        FnSig {
            parent_count: 0,
            own_count: 0,
            declared_count: 0,
            self_param: false,
            header: FnPtrHeader::RUST,
            c_variadic: false,
            legacy_const_generics: &[],
            inputs,
            output,
            types,
        }
    }

    const DROP_IN_PLACE: ItemKey = ItemKey(0x7001);
    const DROP_IMPL_FN: ItemKey = ItemKey(0x7002);
    const PANIC_CANT_UNWIND: ItemKey = ItemKey(0x7003);
    const FN_CALL: ItemKey = ItemKey(0x7004);
    const FN_MUT_CALL: ItemKey = ItemKey(0x7005);
    const FN_ONCE_CALL: ItemKey = ItemKey(0x7006);
    const ADT_DROP: ItemKey = ItemKey(0x7010);
    const ADT_WRAP: ItemKey = ItemKey(0x7011);
    const ADT_ENUM: ItemKey = ItemKey(0x7012);
    const ADT_PAIR: ItemKey = ItemKey(0x7013);
    const CLOSURE_OWNER: ItemKey = ItemKey(0x7020);
    const TARGET_FN: ItemKey = ItemKey(0x7021);
    const TRACKED_FN: ItemKey = ItemKey(0x7022);
    const LOC_ADT: ItemKey = ItemKey(0x7023);
    const VTABLE_METHOD: ItemKey = ItemKey(0x7024);

    struct ShimWorld {
        adt_tys: PortableTys<'static>,
        drop_fields: [Field; 1],
        drop_variants: [Variant; 1],
        wrap_fields: [Field; 2],
        wrap_variants: [Variant; 1],
        enum_fields: [Field; 1],
        enum_variants: [Variant; 2],
        pair_fields: [Field; 2],
        pair_variants: [Variant; 1],
        loc_fields: [Field; 3],
        loc_variants: [Variant; 1],
        closure_sig: PTy,
        closure_env: PTy,
        panic: Panic,
    }

    impl LayoutFacts for ShimWorld {
        fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
            let types = self.adt_tys.view();
            match key {
                ADT_DROP => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: &self.drop_variants, fields: &self.drop_fields, types }),
                ADT_WRAP => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: &self.wrap_variants, fields: &self.wrap_fields, types }),
                ADT_ENUM => Some(Adt { kind: AdtKind::Enum, generics: 0, variants: &self.enum_variants, fields: &self.enum_fields, types }),
                ADT_PAIR => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: &self.pair_variants, fields: &self.pair_fields, types }),
                LOC_ADT => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: &self.loc_variants, fields: &self.loc_fields, types }),
                _ => None,
            }
        }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
        fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::layout::ClosureFacts<'_>> {
            (owner == CLOSURE_OWNER && ordinal == 0).then(|| blaze_build::layout::ClosureFacts {
                types: self.adt_tys.view(),
                sig: self.closure_sig,
                env: self.closure_env,
                kind: blaze_build::FnTrait::Fn,
            })
        }
    }

    impl Adts for ShimWorld {
        fn adt(&self, key: ItemKey) -> Option<Adt<'_>> { LayoutFacts::adt(self, key) }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn overflow_checks(&self) -> blaze_build::OverflowChecks { blaze_build::OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn panic(&self) -> Panic { self.panic }
        fn abi(&self, _: ItemKey) -> Option<Abi> { Some(Abi::Rust) }
        fn lang_fn(&self, name: &str) -> Option<ItemKey> {
            match name {
                "drop_in_place" => Some(DROP_IN_PLACE),
                "panic_cannot_unwind" => Some(PANIC_CANT_UNWIND),
                _ => None,
            }
        }
        fn fn_trait_method(&self, which: blaze_build::FnTrait) -> Option<ItemKey> {
            Some(match which {
                blaze_build::FnTrait::Fn => FN_CALL,
                blaze_build::FnTrait::FnMut => FN_MUT_CALL,
                blaze_build::FnTrait::FnOnce => FN_ONCE_CALL,
            })
        }
        fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> {
            Ok(match ground.view().node(ty) {
                Ok(PNode::Adt(ADT_DROP, _)) => Some(DROP_IMPL_FN),
                _ => None,
            })
        }
        fn location_adt(&self) -> Option<ItemKey> { Some(LOC_ADT) }
        fn track_caller(&self, inst: blaze_build::Instance, _: &mut PortableTys<'_>) -> Option<bool> {
            Some(inst.item == TRACKED_FN)
        }
        fn def_site(&self, item: ItemKey) -> Result<blaze_build::CallerSite<'_>, blaze_build::SiteGap> {
            Ok(blaze_build::CallerSite { file: "shims.rs", item, offset: 7 })
        }
    }

    let mut at = PortableTys::new_in(heap());
    let u32_t = at.intern(PNode::Uint(blaze_typeck::UintTy::U32)).unwrap();
    let i64_t = at.intern(PNode::Int(IntTy::I64)).unwrap();
    let str_t = at.intern(PNode::Str).unwrap();
    let ref_str = at.intern(PNode::Ref(Mutbl::Not, str_t)).unwrap();
    let empty = at.intern_list(&[]).unwrap();
    let drop_adt_t = at.intern(PNode::Adt(ADT_DROP, empty)).unwrap();
    let csig_list = at.intern_list(&[i64_t, i64_t]).unwrap();
    let closure_sig = at.intern(PNode::FnPtr(csig_list, FnPtrHeader::RUST)).unwrap();
    let cenv_list = at.intern_list(&[drop_adt_t]).unwrap();
    let closure_env = at.intern(PNode::Tuple(cenv_list)).unwrap();

    let w_unwind = ShimWorld {
        adt_tys: at,
        drop_fields: [Field { name: Name::of("x"), ty: u32_t }],
        drop_variants: [Variant { key: ADT_DROP, name: Name::of("Droppable"), fields: List { start: 0, len: 1 }, ctor: CtorKind::Braces }],
        wrap_fields: [Field { name: Name::of("a"), ty: drop_adt_t }, Field { name: Name::of("b"), ty: drop_adt_t }],
        wrap_variants: [Variant { key: ADT_WRAP, name: Name::of("Wrap"), fields: List { start: 0, len: 2 }, ctor: CtorKind::Braces }],
        enum_fields: [Field { name: Name::of("0"), ty: drop_adt_t }],
        enum_variants: [
            Variant { key: ItemKey(0x7090), name: Name::of("Empty"), fields: List { start: 0, len: 0 }, ctor: CtorKind::Unit },
            Variant { key: ItemKey(0x7091), name: Name::of("Full"), fields: List { start: 0, len: 1 }, ctor: CtorKind::Tuple },
        ],
        pair_fields: [Field { name: Name::of("0"), ty: u32_t }, Field { name: Name::of("1"), ty: i64_t }],
        pair_variants: [Variant { key: ADT_PAIR, name: Name::of("Pair"), fields: List { start: 0, len: 2 }, ctor: CtorKind::Tuple }],
        loc_fields: [
            Field { name: Name::of("file"), ty: ref_str },
            Field { name: Name::of("line"), ty: u32_t },
            Field { name: Name::of("col"), ty: u32_t },
        ],
        loc_variants: [Variant { key: LOC_ADT, name: Name::of("Location"), fields: List { start: 0, len: 3 }, ctor: CtorKind::Braces }],
        closure_sig,
        closure_env,
        panic: Panic::Unwind,
    };

    // 1. `build_drop_glue`: struct with two droppable fields (unwind ladder), enum, slice, and plain ZST/Copy.
    {
        let mut g = PortableTys::new_in(heap());
        let g_empty = g.intern_list(&[]).unwrap();
        let g_drop = g.intern(PNode::Adt(ADT_DROP, g_empty)).unwrap();
        let g_wrap = g.intern(PNode::Adt(ADT_WRAP, g_empty)).unwrap();
        let g_enum = g.intern(PNode::Adt(ADT_ENUM, g_empty)).unwrap();
        let g_slice = g.intern(PNode::Slice(g_drop)).unwrap();
        let g_u32 = g.intern(PNode::Uint(blaze_typeck::UintTy::U32)).unwrap();

        let b_plain = build_drop_glue(&mut Subst { args: &[g_u32], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        assert!(b_plain.refs.is_empty(), "plain u32 drop glue should emit no calls");

        let b_drop = build_drop_glue(&mut Subst { args: &[g_drop], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        assert!(b_drop.refs.iter().any(|r| r.item == DROP_IMPL_FN), "ADT_DROP glue must call Drop::drop");

        let b_wrap = build_drop_glue(&mut Subst { args: &[g_wrap], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        let lir_wrap = printed(&b_wrap);
        assert!(lir_wrap.contains("invoke"), "unwinding struct drop glue must build unwind ladder:\n{lir_wrap}");
        assert!(lir_wrap.contains("resume"), "unwinding struct drop glue must resume:\n{lir_wrap}");

        let b_enum = build_drop_glue(&mut Subst { args: &[g_enum], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        let lir_enum = printed(&b_enum);
        assert!(lir_enum.contains("switch"), "enum drop glue must switch on discriminant:\n{lir_enum}");

        let b_slice = build_drop_glue(&mut Subst { args: &[g_slice], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        let lir_slice = printed(&b_slice);
        assert!(lir_slice.contains("invoke"), "slice drop glue must invoke element drop_in_place:\n{lir_slice}");
    }

    // 2. `build_closure_shim`: `Fn` closure called via `FnMut` and `FnOnce` (with env drop).
    {
        let mut g = PortableTys::new_in(heap());
        let g_empty = g.intern_list(&[]).unwrap();
        let clo = g.intern(PNode::Closure { owner: CLOSURE_OWNER, ordinal: 0, args: g_empty }).unwrap();
        let b_mut = build_closure_shim(clo, blaze_build::FnTrait::FnMut, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        assert!(b_mut.refs.iter().any(|r| r.item == FN_CALL), "FnMut shim must forward to Fn::call");
        let b_once = build_closure_shim(clo, blaze_build::FnTrait::FnOnce, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        assert!(b_once.refs.iter().any(|r| r.item == FN_CALL), "FnOnce shim must forward to Fn::call");
        assert!(b_once.refs.iter().any(|r| r.item == DROP_IN_PLACE), "FnOnce shim must drop droppable closure env");
    }

    // 3. `build_fn_shim`: fn item + fn pointer shims.
    {
        let mut g = PortableTys::new_in(heap());
        let g_i64 = g.intern(PNode::Int(IntTy::I64)).unwrap();
        let g_empty = g.intern_list(&[]).unwrap();
        let g_tup_l = g.intern_list(&[g_i64]).unwrap();
        let g_tup = g.intern(PNode::Tuple(g_tup_l)).unwrap();
        let g_fndef = g.intern(PNode::FnDef(TARGET_FN, g_empty)).unwrap();
        let g_fpl = g.intern_list(&[g_i64, g_i64]).unwrap();
        let g_fnptr = g.intern(PNode::FnPtr(g_fpl, FnPtrHeader::RUST)).unwrap();

        let mut st = PortableTys::new_in(heap());
        let s_i64 = st.intern(PNode::Int(IntTy::I64)).unwrap();
        let ins = [s_i64];
        let sig = mk_sig(st.view(), &ins, s_i64);

        let b_item = build_fn_shim(g_fndef, g_tup, blaze_build::FnTrait::FnOnce, Some(&sig), &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        assert!(b_item.refs.iter().any(|r| r.item == TARGET_FN), "FnDef shim must call target fn");

        let b_ptr = build_fn_shim(g_fnptr, g_tup, blaze_build::FnTrait::Fn, None, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        let lir_ptr = printed(&b_ptr);
        assert!(lir_ptr.contains("call_indirect"), "FnPtr shim must emit call_indirect:\n{lir_ptr}");
    }

    // 4. `build_ctor_fn`: tuple struct `Pair(u32, i64)` builds and runs cleanly!
    {
        let mut g = PortableTys::new_in(heap());
        let mut st = PortableTys::new_in(heap());
        let s_u32 = st.intern(PNode::Uint(blaze_typeck::UintTy::U32)).unwrap();
        let s_i64 = st.intern(PNode::Int(IntTy::I64)).unwrap();
        let s_empty = st.intern_list(&[]).unwrap();
        let s_pair = st.intern(PNode::Adt(ADT_PAIR, s_empty)).unwrap();
        let ins = [s_u32, s_i64];
        let sig = mk_sig(st.view(), &ins, s_pair);
        let b_ctor = build_ctor_fn(ADT_PAIR, 0, &sig, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        let lir_ctor = printed(&b_ctor);
        assert!(lir_ctor.contains("store"), "ctor fn must store fields:\n{lir_ctor}");
    }

    // 5. `build_reify_shim` and `build_vtable_shim`.
    {
        let mut g = PortableTys::new_in(heap());
        let mut st = PortableTys::new_in(heap());
        let s_i64 = st.intern(PNode::Int(IntTy::I64)).unwrap();
        let ins = [s_i64];
        let sig = mk_sig(st.view(), &ins, s_i64);
        let b_reify = build_reify_shim(TRACKED_FN, &sig, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        let lir_reify = printed(&b_reify);
        assert!(lir_reify.contains("data_addr"), "reify shim must materialize Location row:\n{lir_reify}");
        assert!(b_reify.refs.iter().any(|r| r.item == TRACKED_FN));

        let v_inst = blaze_build::Instance { item: VTABLE_METHOD, args: List::EMPTY };
        let b_vt = build_vtable_shim(v_inst, Some(&sig), &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w_unwind, heap()).unwrap();
        let lir_vt = printed(&b_vt);
        assert!(lir_vt.contains("load"), "vtable shim must load Self from data_ptr:\n{lir_vt}");
        assert!(b_vt.refs.iter().any(|r| r.item == VTABLE_METHOD));
    }
}

#[test]
fn builtin_clone_from_and_test_report_shims_build_and_verify() {
    use blaze_typeck::{AdtKind, CtorKind, Field, FnPtrHeader, FnSig, List, Name, PTys, Variant};
    use crate::{build_builtin_clone, build_builtin_from, build_test_report_shim};

    const DROP_IN_PLACE: ItemKey = ItemKey(0x7100);
    const PANIC_CANT_UNWIND: ItemKey = ItemKey(0x7101);
    const CLONE_FN: ItemKey = ItemKey(0x7102);
    const FROM_FN: ItemKey = ItemKey(0x7103);
    const ADT_DROP: ItemKey = ItemKey(0x7104);
    const DROP_IMPL_FN: ItemKey = ItemKey(0x7105);
    const TEST_FN: ItemKey = ItemKey(0x7106);
    const TEST_NEVER: ItemKey = ItemKey(0x7107);
    const REPORT_FN: ItemKey = ItemKey(0x7108);
    const EXIT_CODE_ADT: ItemKey = ItemKey(0x7109);

    struct W {
        tys: PortableTys<'static>,
        drop_fields: [Field; 1],
        drop_variants: [Variant; 1],
        exit_fields: [Field; 1],
        exit_variants: [Variant; 1],
    }

    impl LayoutFacts for W {
        fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
            let types = self.tys.view();
            match key {
                ADT_DROP => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: &self.drop_variants, fields: &self.drop_fields, types }),
                EXIT_CODE_ADT => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: &self.exit_variants, fields: &self.exit_fields, types }),
                _ => None,
            }
        }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
    }

    impl Adts for W {
        fn adt(&self, key: ItemKey) -> Option<Adt<'_>> { LayoutFacts::adt(self, key) }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn panic(&self) -> Panic { Panic::Unwind }
        fn abi(&self, _: ItemKey) -> Option<Abi> { Some(Abi::Rust) }
        fn lang_fn(&self, name: &str) -> Option<ItemKey> {
            match name {
                "drop_in_place" => Some(DROP_IN_PLACE),
                "panic_cannot_unwind" => Some(PANIC_CANT_UNWIND),
                _ => None,
            }
        }
        fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> {
            Ok(match ground.view().node(ty) {
                Ok(PNode::Adt(ADT_DROP, _)) => Some(DROP_IMPL_FN),
                _ => None,
            })
        }
    }

    let mut at = PortableTys::new_in(heap());
    let u8_t = at.intern(PNode::Uint(blaze_typeck::UintTy::U8)).unwrap();
    let u32_t = at.intern(PNode::Uint(blaze_typeck::UintTy::U32)).unwrap();
    let w = W {
        tys: at,
        drop_fields: [Field { name: Name::of("x"), ty: u32_t }],
        drop_variants: [Variant { key: ADT_DROP, name: Name::of("D"), fields: List { start: 0, len: 1 }, ctor: CtorKind::Braces }],
        exit_fields: [Field { name: Name::of("code"), ty: u8_t }],
        exit_variants: [Variant { key: EXIT_CODE_ADT, name: Name::of("ExitCode"), fields: List { start: 0, len: 1 }, ctor: CtorKind::Tuple }],
    };

    // 1. `build_builtin_clone`: Copy tuple, non-Copy tuple with unwind drop, structural union, fn ptr.
    {
        let mut g = PortableTys::new_in(heap());
        let g_u32 = g.intern(PNode::Uint(blaze_typeck::UintTy::U32)).unwrap();
        let g_i64 = g.intern(PNode::Int(IntTy::I64)).unwrap();
        let g_empty = g.intern_list(&[]).unwrap();
        let g_drop = g.intern(PNode::Adt(ADT_DROP, g_empty)).unwrap();
        let copy_tup_l = g.intern_list(&[g_u32, g_i64]).unwrap();
        let copy_tup = g.intern(PNode::Tuple(copy_tup_l)).unwrap();
        let drop_tup_l = g.intern_list(&[g_drop, g_drop]).unwrap();
        let drop_tup = g.intern(PNode::Tuple(drop_tup_l)).unwrap();
        let union_l = g.intern_list(&[g_drop, g_u32]).unwrap();
        let union_ty = g.intern(PNode::Union(union_l)).unwrap();
        let fp_l = g.intern_list(&[g_i64]).unwrap();
        let fn_ptr = g.intern(PNode::FnPtr(fp_l, FnPtrHeader::RUST)).unwrap();

        let b_copy = build_builtin_clone(CLONE_FN, true, &mut Subst { args: &[copy_tup], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
        assert!(b_copy.refs.is_empty(), "Copy tuple builtin clone must not call field Clone::clone");

        let b_drop = build_builtin_clone(CLONE_FN, false, &mut Subst { args: &[drop_tup], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
        let lir_drop = printed(&b_drop);
        assert!(b_drop.refs.iter().any(|r| r.item == CLONE_FN), "non-Copy tuple must call field Clone::clone");
        assert!(b_drop.refs.iter().any(|r| r.item == DROP_IN_PLACE), "non-Copy tuple must drop prior field on unwind");
        assert!(lir_drop.contains("invoke"), "non-Copy tuple clone must invoke second field:\n{lir_drop}");
        assert!(lir_drop.contains("resume"), "non-Copy tuple clone must resume after cleanup:\n{lir_drop}");

        let b_union = build_builtin_clone(CLONE_FN, false, &mut Subst { args: &[union_ty], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
        let lir_union = printed(&b_union);
        assert!(lir_union.contains("switch"), "structural union clone must switch on discriminant:\n{lir_union}");
        assert!(b_union.refs.iter().any(|r| r.item == CLONE_FN));

        let b_fp = build_builtin_clone(CLONE_FN, false, &mut Subst { args: &[fn_ptr], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
        assert!(b_fp.refs.is_empty(), "fn ptr clone must copy pointer directly");
    }

    // 2. `build_builtin_from`: single-member injection & multi-member sub-union remapping.
    {
        let mut g = PortableTys::new_in(heap());
        let g_i32 = g.intern(PNode::Int(IntTy::I32)).unwrap();
        let g_i64 = g.intern(PNode::Int(IntTy::I64)).unwrap();
        let g_u8 = g.intern(PNode::Uint(blaze_typeck::UintTy::U8)).unwrap();
        let sub_l = g.intern_list(&[g_i32, g_i64]).unwrap();
        let sub_u = g.intern(PNode::Union(sub_l)).unwrap();
        let sup_l = g.intern_list(&[g_i64, g_u8, g_i32]).unwrap();
        let sup_u = g.intern(PNode::Union(sup_l)).unwrap();

        let b_inj = build_builtin_from(FROM_FN, &mut Subst { args: &[sup_u, g_i32], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
        let lir_inj = printed(&b_inj);
        assert!(lir_inj.contains("store"), "single-member union From::from must write tag and payload:\n{lir_inj}");

        let b_remap = build_builtin_from(FROM_FN, &mut Subst { args: &[sup_u, sub_u], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
        let lir_remap = printed(&b_remap);
        assert!(lir_remap.contains("switch"), "sub-union From::from must switch on source discriminant:\n{lir_remap}");
    }

    // 3. `build_test_report_shim`: normal ExitCode struct return & diverging `!` test.
    {
        let mut g = PortableTys::new_in(heap());
        let mut st = PortableTys::new_in(heap());
        let s_u8 = st.intern(PNode::Uint(blaze_typeck::UintTy::U8)).unwrap();
        let s_empty = st.intern_list(&[]).unwrap();
        let s_exit = st.intern(PNode::Adt(EXIT_CODE_ADT, s_empty)).unwrap();
        let s_never = st.intern(PNode::Never).unwrap();
        let s_param0 = st.intern(PNode::Param(0)).unwrap();

        fn mk_sig<'w>(types: PTys<'w>, inputs: &'w [PTy], output: PTy, own_count: u32) -> FnSig<'w> {
            FnSig {
                parent_count: 0,
                own_count,
                declared_count: own_count,
                self_param: false,
                header: FnPtrHeader::RUST,
                c_variadic: false,
                legacy_const_generics: &[],
                inputs,
                output,
                types,
            }
        }
        let test_sig = mk_sig(st.view(), &[], s_u8, 0);
        let rep_in = [s_param0];
        let report_sig = mk_sig(st.view(), &rep_in, s_exit, 1);

        let b_test = build_test_report_shim(TEST_FN, &test_sig, REPORT_FN, &report_sig, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
        let lir_test = printed(&b_test);
        assert!(b_test.refs.iter().any(|r| r.item == TEST_FN), "test report shim must call test fn");
        assert!(b_test.refs.iter().any(|r| r.item == REPORT_FN), "test report shim must call Termination::report");
        assert!(lir_test.contains("zext"), "1-byte ExitCode must zero-extend to i32:\n{lir_test}");

        let never_sig = mk_sig(st.view(), &[], s_never, 0);
        let b_never = build_test_report_shim(TEST_NEVER, &never_sig, REPORT_FN, &report_sig, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
        let lir_never = printed(&b_never);
        assert!(lir_never.contains("unreachable"), "diverging test must end in unreachable:\n{lir_never}");
    }
}

#[test]
fn custom_dst_struct_unsized_tail_drop_glue_builds_and_verifies() {
    use blaze_typeck::{AdtKind, CtorKind, Field, LangItem, List, Name, Variant};
    use crate::build_drop_glue;

    const DROP_IN_PLACE: ItemKey = ItemKey(0x7200);
    const PANIC_CANT_UNWIND: ItemKey = ItemKey(0x7201);
    const ADT_GUARD: ItemKey = ItemKey(0x7202);
    const DROP_GUARD: ItemKey = ItemKey(0x7203);
    const ADT_SLICE_DST: ItemKey = ItemKey(0x7204);
    const DROP_SLICE_DST: ItemKey = ItemKey(0x7205);
    const ADT_DYN_DST: ItemKey = ItemKey(0x7206);
    const ADT_BOX: ItemKey = ItemKey(0x7207);
    const DROP_BOX: ItemKey = ItemKey(0x7208);
    const TRAIT_TR: ItemKey = ItemKey(0x7209);

    struct DstWorld {
        tys: PortableTys<'static>,
        guard_fields: [Field; 1],
        guard_variants: [Variant; 1],
        slice_dst_fields: [Field; 2],
        slice_dst_variants: [Variant; 1],
        dyn_dst_fields: [Field; 2],
        dyn_dst_variants: [Variant; 1],
        box_fields: [Field; 1],
        box_variants: [Variant; 1],
    }

    impl LayoutFacts for DstWorld {
        fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
            let types = self.tys.view();
            match key {
                ADT_GUARD => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: &self.guard_variants, fields: &self.guard_fields, types }),
                ADT_SLICE_DST => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: &self.slice_dst_variants, fields: &self.slice_dst_fields, types }),
                ADT_DYN_DST => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: &self.dyn_dst_variants, fields: &self.dyn_dst_fields, types }),
                ADT_BOX => Some(Adt { kind: AdtKind::Struct, generics: 1, variants: &self.box_variants, fields: &self.box_fields, types }),
                _ => None,
            }
        }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
    }

    impl Adts for DstWorld {
        fn adt(&self, key: ItemKey) -> Option<Adt<'_>> { LayoutFacts::adt(self, key) }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn panic(&self) -> Panic { Panic::Unwind }
        fn abi(&self, _: ItemKey) -> Option<Abi> { Some(Abi::Rust) }
        fn lang_adt(&self, item: LangItem) -> Option<ItemKey> {
            (item == LangItem::OwnedBox).then_some(ADT_BOX)
        }
        fn lang_fn(&self, name: &str) -> Option<ItemKey> {
            match name {
                "drop_in_place" => Some(DROP_IN_PLACE),
                "panic_cannot_unwind" => Some(PANIC_CANT_UNWIND),
                _ => None,
            }
        }
        fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> {
            Ok(match ground.view().node(ty) {
                Ok(PNode::Adt(ADT_GUARD, _)) => Some(DROP_GUARD),
                Ok(PNode::Adt(ADT_SLICE_DST, _)) => Some(DROP_SLICE_DST),
                Ok(PNode::Adt(ADT_BOX, _)) => Some(DROP_BOX),
                _ => None,
            })
        }
    }

    let mut at = PortableTys::new_in(heap());
    let u32_t = at.intern(PNode::Uint(blaze_typeck::UintTy::U32)).unwrap();
    let empty = at.intern_list(&[]).unwrap();
    let guard_t = at.intern(PNode::Adt(ADT_GUARD, empty)).unwrap();
    let slice_guard_t = at.intern(PNode::Slice(guard_t)).unwrap();
    let ex_tr = at.intern(PNode::ExTrait(TRAIT_TR, empty)).unwrap();
    let dyn_l = at.intern_list(&[ex_tr]).unwrap();
    let dyn_t = at.intern(PNode::Dynamic(dyn_l)).unwrap();
    let param0 = at.intern(PNode::Param(0)).unwrap();
    let ptr_param0 = at.intern(PNode::Ptr(Mutbl::Mut, param0)).unwrap();

    let w = DstWorld {
        tys: at,
        guard_fields: [Field { name: Name::of("id"), ty: u32_t }],
        guard_variants: [Variant { key: ADT_GUARD, name: Name::of("Guard"), fields: List { start: 0, len: 1 }, ctor: CtorKind::Braces }],
        slice_dst_fields: [
            Field { name: Name::of("head"), ty: guard_t },
            Field { name: Name::of("tail"), ty: slice_guard_t },
        ],
        slice_dst_variants: [Variant { key: ADT_SLICE_DST, name: Name::of("SliceDst"), fields: List { start: 0, len: 2 }, ctor: CtorKind::Braces }],
        dyn_dst_fields: [
            Field { name: Name::of("head"), ty: guard_t },
            Field { name: Name::of("tail"), ty: dyn_t },
        ],
        dyn_dst_variants: [Variant { key: ADT_DYN_DST, name: Name::of("DynDst"), fields: List { start: 0, len: 2 }, ctor: CtorKind::Braces }],
        box_fields: [Field { name: Name::of("ptr"), ty: ptr_param0 }],
        box_variants: [Variant { key: ADT_BOX, name: Name::of("Box"), fields: List { start: 0, len: 1 }, ctor: CtorKind::Tuple }],
    };

    let mut g = PortableTys::new_in(heap());
    let g_empty = g.intern_list(&[]).unwrap();
    let g_slice_dst = g.intern(PNode::Adt(ADT_SLICE_DST, g_empty)).unwrap();
    let g_dyn_dst = g.intern(PNode::Adt(ADT_DYN_DST, g_empty)).unwrap();
    let box_slice_args = g.intern_list(&[g_slice_dst]).unwrap();
    let g_box_slice_dst = g.intern(PNode::Adt(ADT_BOX, box_slice_args)).unwrap();
    let box_dyn_args = g.intern_list(&[g_dyn_dst]).unwrap();
    let g_box_dyn_dst = g.intern(PNode::Adt(ADT_BOX, box_dyn_args)).unwrap();

    // 1. Custom slice-tailed DST struct: Drop::drop(data, len), prefix Guard, tail [Guard].
    let b_sd = build_drop_glue(&mut Subst { args: &[g_slice_dst], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
    let lir_sd = printed(&b_sd);
    assert!(b_sd.refs.iter().any(|r| r.item == DROP_SLICE_DST), "SliceDst glue must call Drop::drop");
    assert!(b_sd.refs.iter().filter(|r| r.item == DROP_IN_PLACE).count() >= 2, "SliceDst glue must drop prefix and slice tail");
    assert!(lir_sd.contains("invoke") && lir_sd.contains("resume"), "SliceDst glue must build unwind ladder:\n{lir_sd}");

    // 2. Custom dyn-tailed DST struct: prefix Guard, runtime vtable alignment offset, drop_in_place::<dyn Tr>.
    let b_dd = build_drop_glue(&mut Subst { args: &[g_dyn_dst], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
    let lir_dd = printed(&b_dd);
    assert!(lir_dd.contains("ptr_add"), "DynDst glue must compute runtime tail offset via vtable alignment:\n{lir_dd}");
    assert!(b_dd.refs.iter().filter(|r| r.item == DROP_IN_PLACE).count() >= 2, "DynDst glue must drop prefix and dyn tail");

    // 3. Box<SliceDst> and Box<DynDst> drop contents with wide pointers before Box::drop.
    let b_bsd = build_drop_glue(&mut Subst { args: &[g_box_slice_dst], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
    assert!(b_bsd.refs.iter().any(|r| r.item == DROP_IN_PLACE) && b_bsd.refs.iter().any(|r| r.item == DROP_BOX));
    let b_bdd = build_drop_glue(&mut Subst { args: &[g_box_dyn_dst], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
    assert!(b_bdd.refs.iter().any(|r| r.item == DROP_IN_PLACE) && b_bdd.refs.iter().any(|r| r.item == DROP_BOX));
}

#[test]
fn c_variadic_def_call_and_va_arg_build_and_verify() {
    use blaze_body::{BodyBuilder, Callee, LocalDecl, LocalKind, Operand, Span, StmtKind, Unwind, Value};
    use blaze_typeck::{AdtKind, CtorKind, Field, List, Name, Variant};

    const VA_LIST_ADT: ItemKey = ItemKey(0x7300);
    const VA_DEF_FN: ItemKey = ItemKey(0x7301);
    const VA_ARG_INTR: ItemKey = ItemKey(0x7302);
    const CALLER_FN: ItemKey = ItemKey(0x7303);

    struct VaWorld {
        tys: PortableTys<'static>,
        va_fields: [Field; 1],
        va_variants: [Variant; 1],
    }

    impl LayoutFacts for VaWorld {
        fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
            (key == VA_LIST_ADT).then(|| Adt {
                kind: AdtKind::Struct,
                generics: 0,
                variants: &self.va_variants,
                fields: &self.va_fields,
                types: self.tys.view(),
            })
        }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
    }

    impl Adts for VaWorld {
        fn adt(&self, key: ItemKey) -> Option<Adt<'_>> { LayoutFacts::adt(self, key) }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn panic(&self) -> Panic { Panic::Abort }
        fn abi(&self, item: ItemKey) -> Option<Abi> {
            Some(if item == VA_DEF_FN { Abi::C } else { Abi::Rust })
        }
        fn nounwind(&self, item: ItemKey) -> Option<bool> {
            Some(item == VA_DEF_FN)
        }
        fn c_variadic(&self, item: ItemKey) -> Option<u32> {
            (item == VA_DEF_FN).then_some(1)
        }
        fn intrinsic(&self, item: ItemKey) -> Option<blaze_build::Intrinsic> {
            (item == VA_ARG_INTR).then_some(blaze_build::Intrinsic::Table(blaze_build::intrinsics::Which::VaArg))
        }
        fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> {
            Ok(None)
        }
    }

    let mut at = PortableTys::new_in(heap());
    let u8_t = at.intern(PNode::Uint(blaze_typeck::UintTy::U8)).unwrap();
    let ptr_u8 = at.intern(PNode::Ptr(Mutbl::Mut, u8_t)).unwrap();
    let w = VaWorld {
        tys: at,
        va_fields: [Field { name: Name::of("ptr"), ty: ptr_u8 }],
        va_variants: [Variant { key: VA_LIST_ADT, name: Name::of("VaList"), fields: List { start: 0, len: 1 }, ctor: CtorKind::Tuple }],
    };

    let mut from = PortableTys::new_in(heap());
    let i32_t = from.intern(PNode::Int(IntTy::I32)).unwrap();
    let empty = from.intern_list(&[]).unwrap();
    let va_t = from.intern(PNode::Adt(VA_LIST_ADT, empty)).unwrap();
    let ref_mut_va = from.intern(PNode::Ref(Mutbl::Mut, va_t)).unwrap();
    let va_arg_gen = from.intern_list(&[i32_t]).unwrap();

    // 1. C-variadic definition: `extern "C" fn sum(first: i32, mut ap: ...) -> i32 { first + va_arg::<i32>(&mut ap) }`
    let mut b = BodyBuilder::new_in(heap());
    let l0 = b.new_local(LocalDecl { ty: i32_t, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let l1 = b.new_local(LocalDecl { ty: i32_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let l2 = b.new_local(LocalDecl { ty: va_t, kind: LocalKind::Arg(1), mutbl: Mutbl::Mut });
    let l3 = b.new_local(LocalDecl { ty: ref_mut_va, kind: LocalKind::Temp(blaze_body::body::ExprId(0), blaze_body::Role::Borrow), mutbl: Mutbl::Not });
    let l4 = b.new_local(LocalDecl { ty: i32_t, kind: LocalKind::Temp(blaze_body::body::ExprId(1), blaze_body::Role::Value), mutbl: Mutbl::Mut });
    let p0 = b.local_place(l0);
    let p1 = b.local_place(l1);
    let p2 = b.local_place(l2);
    let p3 = b.local_place(l3);
    let p4 = b.local_place(l4);
    let o0 = b.origin(false);
    let bb0 = b.new_block(blaze_body::BlockKind::Normal);
    let bb1 = b.new_block(blaze_body::BlockKind::Normal);
    b.start(bb0);
    b.borrow(p3, blaze_body::BorrowKind::Mut, p2, o0, Span::default());
    let c_va = b.callee(Callee::Intrinsic { key: VA_ARG_INTR, args: va_arg_gen });
    let a_va = b.operands(&[Operand::Move(p3)]);
    b.terminate(StmtKind::Call { callee: c_va, args: a_va, dest: Some(p4), target: Some(bb1), unwind: Unwind::Continue }, Span::default());
    b.start(bb1);
    b.assign(p0, Value::Binary { op: blaze_body::BinOp::Add, lhs: Operand::Copy(p1), rhs: Operand::Copy(p4) }, Span::default());
    b.terminate(StmtKind::Return, Span::default());
    let body = b.finish().unwrap();
    let ver = blaze_body::verify(&body).unwrap();

    let mut ground = PortableTys::new_in(heap());
    let built_def = build(ver, from.view(), VA_DEF_FN, &mut Subst { args: &[], ground: &mut ground, scratch: heap() }, &w, heap()).unwrap();
    let lir_def = printed(&built_def);
    assert!(lir_def.contains("blaze.va_start"), "C-variadic def must call blaze.va_start:\n{lir_def}");
    assert!(lir_def.contains("variadic"), "C-variadic def signature must be variadic:\n{lir_def}");

    // 2. Caller invoking `sum(10, 32)`: signature has 1 named param + `...`, call passes 2 args!
    let mut cb = BodyBuilder::new_in(heap());
    let cl0 = cb.new_local(LocalDecl { ty: i32_t, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let cp0 = cb.local_place(cl0);
    let k10 = cb.new_const(blaze_body::ConstDecl { kind: blaze_body::ConstKind::Scalar { bits: 10 }, ty: i32_t });
    let k32 = cb.new_const(blaze_body::ConstDecl { kind: blaze_body::ConstKind::Scalar { bits: 32 }, ty: i32_t });
    let cbb0 = cb.new_block(blaze_body::BlockKind::Normal);
    let cbb1 = cb.new_block(blaze_body::BlockKind::Normal);
    cb.start(cbb0);
    let c_fn = cb.callee(Callee::FnItem { key: VA_DEF_FN, args: empty });
    let c_args = cb.operands(&[Operand::Const(k10), Operand::Const(k32)]);
    cb.terminate(StmtKind::Call { callee: c_fn, args: c_args, dest: Some(cp0), target: Some(cbb1), unwind: Unwind::Continue }, Span::default());
    cb.start(cbb1);
    cb.terminate(StmtKind::Return, Span::default());
    let cbody = cb.finish().unwrap();
    let cver = blaze_body::verify(&cbody).unwrap();

    let built_call = build(cver, from.view(), CALLER_FN, &mut Subst { args: &[], ground: &mut ground, scratch: heap() }, &w, heap()).unwrap();
    let lir_call = printed(&built_call);
    assert!(lir_call.contains("variadic"), "C-variadic call signature must be variadic with named-only params:\n{lir_call}");
}

#[test]
fn become_explicit_tail_call_emits_tail_call_and_reuses_ref_params() {
    use blaze_body::{BodyBuilder, Callee, LocalDecl, LocalKind, Operand, Span, StmtKind};
    use blaze_typeck::{AdtKind, CtorKind, Field, FnPtrHeader, List, Name, Variant};

    const BIG_ADT: ItemKey = ItemKey(0x7400);
    const TAIL_DIRECT: ItemKey = ItemKey(0x7401);
    const TAIL_INDIRECT: ItemKey = ItemKey(0x7402);
    const TAIL_BIG: ItemKey = ItemKey(0x7403);

    struct TailWorld {
        tys: PortableTys<'static>,
        big_fields: [Field; 4],
        big_variants: [Variant; 1],
    }

    impl LayoutFacts for TailWorld {
        fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
            (key == BIG_ADT).then(|| Adt {
                kind: AdtKind::Struct,
                generics: 0,
                variants: &self.big_variants,
                fields: &self.big_fields,
                types: self.tys.view(),
            })
        }
        fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> { Ok(ReprAttrs::RUST) }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
    }

    impl Adts for TailWorld {
        fn adt(&self, key: ItemKey) -> Option<Adt<'_>> { LayoutFacts::adt(self, key) }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::Off }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn panic(&self) -> Panic { Panic::Abort }
        fn abi(&self, _: ItemKey) -> Option<Abi> { Some(Abi::Rust) }
        fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> {
            Ok(None)
        }
    }

    let mut at = PortableTys::new_in(heap());
    let i64_t = at.intern(PNode::Int(IntTy::I64)).unwrap();
    let w = TailWorld {
        tys: at,
        big_fields: [
            Field { name: Name::of("a"), ty: i64_t },
            Field { name: Name::of("b"), ty: i64_t },
            Field { name: Name::of("c"), ty: i64_t },
            Field { name: Name::of("d"), ty: i64_t },
        ],
        big_variants: [Variant { key: BIG_ADT, name: Name::of("Big"), fields: List { start: 0, len: 4 }, ctor: CtorKind::Braces }],
    };

    let mut from = PortableTys::new_in(heap());
    let f_i64 = from.intern(PNode::Int(IntTy::I64)).unwrap();
    let empty = from.intern_list(&[]).unwrap();
    let f_big = from.intern(PNode::Adt(BIG_ADT, empty)).unwrap();
    let fp_list = from.intern_list(&[f_i64, f_i64]).unwrap();
    let f_fnptr = from.intern(PNode::FnPtr(fp_list, FnPtrHeader::RUST)).unwrap();

    // 1. Direct `tail_call` emits `Op::TailCall`.
    let mut b = BodyBuilder::new_in(heap());
    let _l0 = b.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let l1 = b.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let p1 = b.local_place(l1);
    let bb0 = b.new_block(blaze_body::BlockKind::Normal);
    b.start(bb0);
    let c = b.callee(Callee::FnItem { key: CALLEE, args: empty });
    let args = b.operands(&[Operand::Copy(p1)]);
    b.terminate(StmtKind::TailCall { callee: c, args }, Span::default());
    let body = b.finish().unwrap();
    let ver = blaze_body::verify(&body).unwrap();
    let mut g = PortableTys::new_in(heap());
    let built = build(ver, from.view(), TAIL_DIRECT, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
    let lir = printed(&built);
    assert!(lir.contains("tail_call "), "direct StmtKind::TailCall must emit Op::TailCall:\n{lir}");

    // 2. Indirect `tail_call` emits `Op::TailCallIndirect`.
    let mut ib = BodyBuilder::new_in(heap());
    let _il0 = ib.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let il1 = ib.new_local(LocalDecl { ty: f_fnptr, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let il2 = ib.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
    let ip1 = ib.local_place(il1);
    let ip2 = ib.local_place(il2);
    let ibb0 = ib.new_block(blaze_body::BlockKind::Normal);
    ib.start(ibb0);
    let ic = ib.callee(Callee::Value(Operand::Copy(ip1)));
    let iargs = ib.operands(&[Operand::Copy(ip2)]);
    ib.terminate(StmtKind::TailCall { callee: ic, args: iargs }, Span::default());
    let ibody = ib.finish().unwrap();
    let iver = blaze_body::verify(&ibody).unwrap();
    let ibuilt = build(iver, from.view(), TAIL_INDIRECT, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
    let ilir = printed(&ibuilt);
    assert!(ilir.contains("tail_call_indirect "), "indirect StmtKind::TailCall must emit Op::TailCallIndirect:\n{ilir}");

    // 3. Large `Pass::Ref` struct parameter buffer reuse in `tail_call`.
    let mut bb = BodyBuilder::new_in(heap());
    let _bl0 = bb.new_local(LocalDecl { ty: f_big, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let bl1 = bb.new_local(LocalDecl { ty: f_big, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let bp1 = bb.local_place(bl1);
    let bbb0 = bb.new_block(blaze_body::BlockKind::Normal);
    bb.start(bbb0);
    let bc = bb.callee(Callee::FnItem { key: TAIL_BIG, args: empty });
    let bargs = bb.operands(&[Operand::Move(bp1)]);
    bb.terminate(StmtKind::TailCall { callee: bc, args: bargs }, Span::default());
    let bbody = bb.finish().unwrap();
    let bver = blaze_body::verify(&bbody).unwrap();
    let bbuilt = build(bver, from.view(), TAIL_BIG, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
    let blir = printed(&bbuilt);
    assert!(blir.contains("tail_call "), "Pass::Ref buffer reuse must emit real Op::TailCall:\n{blir}");
}

#[test]
fn inline_asm_aarch64_in_out_inout_const_sym_label_builds_and_verifies() {
    use blaze_body::{
        AsmArch, AsmOpInput, AsmOpKind, AsmPieceInput, AsmRegKind, BodyBuilder, ConstDecl,
        ConstKind, LocalDecl, LocalKind, Operand, Span, StmtKind, Unwind, Value,
    };

    let mut from = PortableTys::new_in(heap());
    let i64_t = from.intern(PNode::Int(IntTy::I64)).unwrap();
    let empty = from.intern_list(&[]).unwrap();
    let fn_sym_t = from.intern(PNode::FnDef(CALLEE, empty)).unwrap();
    let w = World { panic: Panic::Abort };

    // 1. `asm!("add {out}, {inp}, #{c}", "bl {f}", inp = in(reg) x, out = out(reg) y, c = const 42, f = sym CALLEE, out("x9") _, clobber_abi("C"))`
    let mut b = BodyBuilder::new_in(heap());
    let l0 = b.new_local(LocalDecl { ty: i64_t, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let l1 = b.new_local(LocalDecl { ty: i64_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let p0 = b.local_place(l0);
    let p1 = b.local_place(l1);
    let k42 = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 42 }, ty: i64_t });
    let ksym = b.new_const(ConstDecl { kind: ConstKind::FnItem { key: CALLEE, args: empty }, ty: fn_sym_t });
    let bb0 = b.new_block(blaze_body::BlockKind::Normal);
    let bb1 = b.new_block(blaze_body::BlockKind::Normal);
    let asm_id = b.new_asm(
        AsmArch::Aarch64,
        0,
        "C",
        &[
            AsmPieceInput::Text("add "),
            AsmPieceInput::Operand { index: 1, modifier: None },
            AsmPieceInput::Text(", "),
            AsmPieceInput::Operand { index: 0, modifier: None },
            AsmPieceInput::Text(", #"),
            AsmPieceInput::Operand { index: 2, modifier: None },
            AsmPieceInput::Line,
            AsmPieceInput::Text("bl "),
            AsmPieceInput::Operand { index: 3, modifier: None },
        ],
        &[
            AsmOpInput { kind: AsmOpKind::In, reg_kind: AsmRegKind::Class, reg: "reg", in_op: Some(0), out_place: None, label: None, discard_out: false },
            AsmOpInput { kind: AsmOpKind::Out, reg_kind: AsmRegKind::Class, reg: "reg", in_op: None, out_place: Some(p0), label: None, discard_out: false },
            AsmOpInput { kind: AsmOpKind::Const, reg_kind: AsmRegKind::None, reg: "", in_op: Some(1), out_place: None, label: None, discard_out: false },
            AsmOpInput { kind: AsmOpKind::Sym, reg_kind: AsmRegKind::None, reg: "", in_op: Some(2), out_place: None, label: None, discard_out: false },
            AsmOpInput { kind: AsmOpKind::Out, reg_kind: AsmRegKind::Explicit, reg: "x9", in_op: None, out_place: None, label: None, discard_out: true },
        ],
    );
    let ops = b.operands(&[Operand::Copy(p1), Operand::Const(k42), Operand::Const(ksym)]);
    b.start(bb0);
    b.terminate(StmtKind::InlineAsm { asm: asm_id, operands: ops, target: Some(bb1), unwind: Unwind::Continue }, Span::default());
    b.start(bb1);
    b.terminate(StmtKind::Return, Span::default());
    let body = b.finish().unwrap();
    // Round-trip through binary record encoding to verify VERSION=4 preserves all asm tables!
    let encoded = blaze_body::try_encode(&body.view(), heap()).unwrap();
    let reopened = blaze_body::open(&encoded).unwrap();
    let ver = blaze_body::verify_ref(reopened).unwrap();

    let mut g = PortableTys::new_in(heap());
    let built = build(ver, from.view(), ITEM, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
    let lir = printed(&built);
    assert!(lir.contains("asm "), "InlineAsm must emit LIR asm instruction:\n{lir}");
    assert!(lir.contains("#42"), "const operand must be formatted into template:\n{lir}");
    assert!(built.refs.iter().any(|r| r.item == CALLEE), "sym operand must register fn ref");

    // 2. `asm!` with `label` operand emits `asm_br` and valid SSA bridge block!
    let mut lb = BodyBuilder::new_in(heap());
    let ll0 = lb.new_local(LocalDecl { ty: i64_t, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let ll1 = lb.new_local(LocalDecl { ty: i64_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let lp0 = lb.local_place(ll0);
    let lp1 = lb.local_place(ll1);
    let k1 = lb.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 1 }, ty: i64_t });
    let k2 = lb.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 2 }, ty: i64_t });
    let lbb0 = lb.new_block(blaze_body::BlockKind::Normal);
    let lbb_fall = lb.new_block(blaze_body::BlockKind::Normal);
    let lbb_lbl = lb.new_block(blaze_body::BlockKind::Normal);
    let lbb_join = lb.new_block(blaze_body::BlockKind::Normal);
    let lasm = lb.new_asm(
        AsmArch::Aarch64,
        0,
        "",
        &[AsmPieceInput::Text("cbnz "), AsmPieceInput::Operand { index: 0, modifier: None }, AsmPieceInput::Text(", "), AsmPieceInput::Operand { index: 1, modifier: None }],
        &[
            AsmOpInput { kind: AsmOpKind::In, reg_kind: AsmRegKind::Class, reg: "reg", in_op: Some(0), out_place: None, label: None, discard_out: false },
            AsmOpInput { kind: AsmOpKind::Label, reg_kind: AsmRegKind::None, reg: "", in_op: None, out_place: None, label: Some(lbb_lbl), discard_out: false },
        ],
    );
    let lops = lb.operands(&[Operand::Copy(lp1)]);
    lb.start(lbb0);
    lb.terminate(StmtKind::InlineAsm { asm: lasm, operands: lops, target: Some(lbb_fall), unwind: Unwind::Continue }, Span::default());
    lb.start(lbb_fall);
    lb.assign(lp0, Value::Use(Operand::Const(k1)), Span::default());
    lb.terminate(StmtKind::Goto(lbb_join), Span::default());
    lb.start(lbb_lbl);
    lb.assign(lp0, Value::Use(Operand::Const(k2)), Span::default());
    lb.terminate(StmtKind::Goto(lbb_join), Span::default());
    lb.start(lbb_join);
    lb.terminate(StmtKind::Return, Span::default());
    let lbody = lb.finish().unwrap();
    let lver = blaze_body::verify(&lbody).unwrap();
    let lbuilt = build(lver, from.view(), ITEM, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &w, heap()).unwrap();
    let llir = printed(&lbuilt);
    assert!(llir.contains("asm_br "), "InlineAsm with label must emit LIR asm_br:\n{llir}");
}

/// End-to-end coroutine build + LIR execution tests in `blaze-body-build`:
/// 1. Multi-await coroutine with captured upvar (`AggregateKind::Coroutine`) + non-`Copy` droppable
///    local live across `.await`: executes `poll` -> `Pending` -> `Pending` -> `Ready(sum)` AND
///    executes cancel drop while suspended (`BuiltCoroutine::drop`), verifying exact drop counts!
/// 2. `Poll<!>` diverging await coroutine builds, verifies, and suspends cleanly.
/// 3. `AssertKind::ResumedAfterReturn` and `AssertKind::ResumedAfterPanic` emit calls to the
///    corresponding `panic_const_async_fn_resumed*` lang fns, and `build_drop_glue` refuses
///    `PNode::Coroutine` cleanly.
#[test]
fn coroutine_state_machine_executes_poll_resume_cancel_and_panics() {
    use blaze_body::{
        BlockKind as BodyBlockKind, BodyBuilder, Callee, ConstDecl, ConstKind, LocalDecl, LocalKind,
        Operand, Proj, Role, Span, StmtKind, Unwind, Value,
    };
    use blaze_build::{AsyncItems, CoroutineFacts, StateShape};
    use blaze_lir::eval::{Bodies, BodyId, Val};
    use blaze_lir::func::RefId;
    use blaze_typeck::{AdtKind, CtorKind, ExprId, Field, List, Name, PatId, UintTy, Variant};

    const CORO_OWNER: ItemKey = ItemKey(0xc001);
    const NEVER_OWNER: ItemKey = ItemKey(0xc002);
    const POLL_ADT: ItemKey = ItemKey(0xc010);
    const PIN_ADT: ItemKey = ItemKey(0xc011);
    const GUARD_ADT: ItemKey = ItemKey(0xc012);
    const SUB_FUT_ADT: ItemKey = ItemKey(0xc013);
    const INTO_FUT_FN: ItemKey = ItemKey(0xc020);
    const FUTURE_POLL_FN: ItemKey = ItemKey(0xc021);
    const RESUME_FN: ItemKey = ItemKey(0xc022);
    const DROP_IN_PLACE_FN: ItemKey = ItemKey(0xc023);
    const GUARD_DROP_FN: ItemKey = ItemKey(0xc024);
    const RESUMED_PANIC_FN: ItemKey = ItemKey(0xc025);
    const RESUMED_AFTER_PANIC_FN: ItemKey = ItemKey(0xc026);
    const CREATOR_FN: ItemKey = ItemKey(0xc027);

    // Build shared ADT/coroutine type table.
    let mut adt_tys = PortableTys::new_in(heap());
    let p0 = adt_tys.intern(PNode::Param(0)).unwrap();
    let empty_a = adt_tys.intern_list(&[]).unwrap();
    let i64_a = adt_tys.intern(PNode::Int(IntTy::I64)).unwrap();
    let ptr_i64_a = adt_tys.intern(PNode::Ptr(Mutbl::Mut, i64_a)).unwrap();
    let guard_a = adt_tys.intern(PNode::Adt(GUARD_ADT, empty_a)).unwrap();
    let never_a = adt_tys.intern(PNode::Never).unwrap();
    let env_list = adt_tys.intern_list(&[i64_a, ptr_i64_a]).unwrap();
    let env_tuple_a = adt_tys.intern(PNode::Tuple(env_list)).unwrap();
    let never_env_a = adt_tys.intern(PNode::Tuple(empty_a)).unwrap();

    let poll_variants = [
        Variant { key: ItemKey(0xc101), name: Name::of("Ready"), ctor: CtorKind::Tuple, fields: List { start: 0, len: 1 } },
        Variant { key: ItemKey(0xc102), name: Name::of("Pending"), ctor: CtorKind::Unit, fields: List { start: 1, len: 0 } },
    ];
    let poll_fields = [Field { name: Name::of("0"), ty: p0 }];
    let pin_variants = [
        Variant { key: ItemKey(0xc103), name: Name::of("Pin"), ctor: CtorKind::Braces, fields: List { start: 0, len: 1 } },
    ];
    let pin_fields = [Field { name: Name::of("pointer"), ty: p0 }];
    // `struct Guard(*mut i64)` — dropping increments the counter at `*self.0`.
    let guard_variants = [
        Variant { key: ItemKey(0xc104), name: Name::of("Guard"), ctor: CtorKind::Tuple, fields: List { start: 0, len: 1 } },
    ];
    let guard_fields = [Field { name: Name::of("0"), ty: ptr_i64_a }];
    // `struct SubFut(i64)` — dummy sub-future storing step id.
    let sub_variants = [
        Variant { key: ItemKey(0xc105), name: Name::of("SubFut"), ctor: CtorKind::Tuple, fields: List { start: 0, len: 1 } },
    ];
    let sub_fields = [Field { name: Name::of("0"), ty: i64_a }];

    struct CoroWorld<'a> {
        adt_tys: &'a PortableTys<'static>,
        poll_variants: &'a [Variant],
        poll_fields: &'a [Field],
        pin_variants: &'a [Variant],
        pin_fields: &'a [Field],
        guard_variants: &'a [Variant],
        guard_fields: &'a [Field],
        sub_variants: &'a [Variant],
        sub_fields: &'a [Field],
        env_tuple: PTy,
        output_i64: PTy,
        never_env: PTy,
        output_never: PTy,
        state_shape: Option<StateShape>,
    }

    impl LayoutFacts for CoroWorld<'_> {
        fn adt(&self, k: ItemKey) -> Option<Adt<'_>> {
            match k {
                POLL_ADT => Some(Adt { kind: AdtKind::Enum, generics: 1, variants: self.poll_variants, fields: self.poll_fields, types: self.adt_tys.view() }),
                PIN_ADT => Some(Adt { kind: AdtKind::Struct, generics: 1, variants: self.pin_variants, fields: self.pin_fields, types: self.adt_tys.view() }),
                GUARD_ADT => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: self.guard_variants, fields: self.guard_fields, types: self.adt_tys.view() }),
                SUB_FUT_ADT => Some(Adt { kind: AdtKind::Struct, generics: 0, variants: self.sub_variants, fields: self.sub_fields, types: self.adt_tys.view() }),
                _ => None,
            }
        }
        fn repr(&self, k: ItemKey) -> Result<ReprAttrs, Missing> {
            if k == PIN_ADT {
                let mut r = ReprAttrs::RUST;
                r.transparent = true;
                return Ok(r);
            }
            Ok(ReprAttrs::RUST)
        }
        fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> { Ok(None) }
        fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> { Ok(ValidRange::default()) }
        fn coroutine(&self, owner: ItemKey, ordinal: u32) -> Option<CoroutineFacts<'_>> {
            if owner == CORO_OWNER && ordinal == 0 {
                Some(CoroutineFacts { types: self.adt_tys.view(), env: self.env_tuple, output: self.output_i64 })
            } else if owner == NEVER_OWNER && ordinal == 0 {
                Some(CoroutineFacts { types: self.adt_tys.view(), env: self.never_env, output: self.output_never })
            } else {
                None
            }
        }
        fn coroutine_state(&self, owner: ItemKey, ordinal: u32, _: &[PTy], _: blaze_typeck::PTys<'_>) -> Result<Option<StateShape>, Missing> {
            if owner == CORO_OWNER && ordinal == 0 {
                Ok(self.state_shape)
            } else {
                Ok(None)
            }
        }
    }

    impl Adts for CoroWorld<'_> {
        fn adt(&self, k: ItemKey) -> Option<Adt<'_>> { LayoutFacts::adt(self, k) }
        fn facts(&self) -> &dyn LayoutFacts { self }
        fn panic(&self) -> Panic { Panic::Abort }
        fn overflow_checks(&self) -> OverflowChecks { OverflowChecks::On }
        fn ub_checks(&self) -> blaze_build::UbChecks { blaze_build::UbChecks::Off }
        fn drop_impl(&self, ty: PTy, g: &mut PortableTys<'_>) -> Result<Option<ItemKey>, blaze_build::BuildError> {
            Ok(match g.view().node(ty) {
                Ok(PNode::Adt(GUARD_ADT, _)) => Some(GUARD_DROP_FN),
                _ => None,
            })
        }
        fn abi(&self, _: ItemKey) -> Option<Abi> { Some(Abi::Rust) }
        fn lang_fn(&self, name: &str) -> Option<ItemKey> {
            match name {
                "drop_in_place" => Some(DROP_IN_PLACE_FN),
                "panic_const_async_fn_resumed" => Some(RESUMED_PANIC_FN),
                "panic_const_async_fn_resumed_panic" => Some(RESUMED_AFTER_PANIC_FN),
                "panic_cannot_unwind" => Some(CANNOT_UNWIND),
                _ => None,
            }
        }
        fn future_poll(&self) -> Option<ItemKey> { Some(FUTURE_POLL_FN) }
        fn async_items(&self) -> Option<AsyncItems> {
            Some(AsyncItems {
                into_future: INTO_FUT_FN,
                poll: POLL_ADT,
                ready: 0,
                pending: 1,
                pin: PIN_ADT,
                resume: RESUME_FN,
            })
        }
    }

    let mut cw = CoroWorld {
        adt_tys: &adt_tys,
        poll_variants: &poll_variants,
        poll_fields: &poll_fields,
        pin_variants: &pin_variants,
        pin_fields: &pin_fields,
        guard_variants: &guard_variants,
        guard_fields: &guard_fields,
        sub_variants: &sub_variants,
        sub_fields: &sub_fields,
        env_tuple: env_tuple_a,
        output_i64: i64_a,
        never_env: never_env_a,
        output_never: never_a,
        state_shape: None,
    };

    // Build a 2-await coroutine body:
    //   upvars: _1 = base: i64, _2 = drop_counter_ptr: *mut i64
    //   local _3 = guard: Guard(_2)  (live across BOTH awaits, dropped on normal exit AND cancel!)
    //   await 1 -> a: i64 (stored in _4, live across await 2!)
    //   await 2 -> b: i64 (stored in _5)
    //   drop _3; _0 = _1 + _4 + _5; return
    let mut from = PortableTys::new_in(heap());
    let f_empty = from.intern_list(&[]).unwrap();
    let f_i64 = from.intern(PNode::Int(IntTy::I64)).unwrap();
    let f_u8 = from.intern(PNode::Uint(UintTy::U8)).unwrap();
    let f_ptr_i64 = from.intern(PNode::Ptr(Mutbl::Mut, f_i64)).unwrap();
    let f_guard = from.intern(PNode::Adt(GUARD_ADT, f_empty)).unwrap();
    let f_sub = from.intern(PNode::Adt(SUB_FUT_ADT, f_empty)).unwrap();
    let f_sub_l = from.intern_list(&[f_sub]).unwrap();
    let f_mut_sub = from.intern(PNode::Ref(Mutbl::Mut, f_sub)).unwrap();
    let f_pin_l = from.intern_list(&[f_mut_sub]).unwrap();
    let f_pin = from.intern(PNode::Adt(PIN_ADT, f_pin_l)).unwrap();
    let f_poll_l = from.intern_list(&[f_i64]).unwrap();
    let f_poll_i64 = from.intern(PNode::Adt(POLL_ADT, f_poll_l)).unwrap();
    let f_coro = from.intern(PNode::Coroutine { owner: CORO_OWNER, ordinal: 0, args: f_empty }).unwrap();
    let f_never = from.intern(PNode::Never).unwrap();
    let f_poll_never_l = from.intern_list(&[f_never]).unwrap();
    let f_poll_never = from.intern(PNode::Adt(POLL_ADT, f_poll_never_l)).unwrap();

    let mut b = BodyBuilder::new_in(heap());
    let l0 = b.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let l1 = b.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let l2 = b.new_local(LocalDecl { ty: f_ptr_i64, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
    let l_guard = b.new_local(LocalDecl { ty: f_guard, kind: LocalKind::Binding(PatId(0)), mutbl: Mutbl::Mut });
    let l_a = b.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Binding(PatId(1)), mutbl: Mutbl::Mut });
    let l_b = b.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Binding(PatId(2)), mutbl: Mutbl::Mut });
    let l_sum1 = b.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Temp(ExprId(9), Role::Value), mutbl: Mutbl::Mut });

    let (p0, p1, p2) = (b.local_place(l0), b.local_place(l1), b.local_place(l2));
    let (pg, pa, pb, psum1) = (b.local_place(l_guard), b.local_place(l_a), b.local_place(l_b), b.local_place(l_sum1));

    // Helper to emit one `.await` of `SubFut(step_id)` into `dest_place`, dropping `pg` on cancel.
    let emit_await = |b: &mut BodyBuilder<Alloc<'static>>, step_id: u128, e_id: u32, dest_place: blaze_body::Place| {
        let k_step = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: step_id }, ty: f_i64 });
        let fut = b.new_local(LocalDecl { ty: f_sub, kind: LocalKind::Temp(ExprId(e_id), Role::Awaitee), mutbl: Mutbl::Mut });
        let pin_ref = b.new_local(LocalDecl { ty: f_mut_sub, kind: LocalKind::Temp(ExprId(e_id), Role::Borrow), mutbl: Mutbl::Mut });
        let pin = b.new_local(LocalDecl { ty: f_pin, kind: LocalKind::Temp(ExprId(e_id), Role::Pin), mutbl: Mutbl::Mut });
        let poll_res = b.new_local(LocalDecl { ty: f_poll_i64, kind: LocalKind::Temp(ExprId(e_id), Role::Result), mutbl: Mutbl::Mut });
        let discr = b.new_local(LocalDecl { ty: f_u8, kind: LocalKind::Temp(ExprId(e_id), Role::Value), mutbl: Mutbl::Mut });
        let (pfut, ppref, ppin, ppol, pdiscr) = (
            b.local_place(fut), b.local_place(pin_ref), b.local_place(pin), b.local_place(poll_res), b.local_place(discr),
        );
        let head = b.new_block(BodyBlockKind::Normal);
        let check = b.new_block(BodyBlockKind::Normal);
        let pending = b.new_block(BodyBlockKind::Normal);
        let cancel = b.new_block(BodyBlockKind::Normal);
        let cancel_done = b.new_block(BodyBlockKind::Normal);
        let ready = b.new_block(BodyBlockKind::Normal);

        b.push(StmtKind::StorageLive(fut), Span::default());
        let sf_ops = b.operands(&[Operand::Const(k_step)]);
        b.assign(pfut, Value::Aggregate { kind: blaze_body::AggregateKind::Adt(0), ty: f_sub, operands: sf_ops }, Span::default());
        b.terminate(StmtKind::Goto(head), Span::default());

        b.start(head);
        b.push(StmtKind::StorageLive(pin_ref), Span::default());
        let o = b.origin(false);
        b.borrow(ppref, blaze_body::BorrowKind::Mut, pfut, o, Span::default());
        b.push(StmtKind::StorageLive(pin), Span::default());
        let pin_ops = b.operands(&[Operand::Move(ppref)]);
        b.assign(ppin, Value::Aggregate { kind: blaze_body::AggregateKind::Adt(0), ty: f_pin, operands: pin_ops }, Span::default());
        b.push(StmtKind::StorageDead(pin_ref), Span::default());
        b.push(StmtKind::StorageLive(poll_res), Span::default());
        let poll_c = b.callee(Callee::Method { key: FUTURE_POLL_FN, args: f_sub_l });
        let poll_args = b.operands(&[Operand::Move(ppin)]);
        b.terminate(StmtKind::Call { callee: poll_c, args: poll_args, dest: Some(ppol), target: Some(check), unwind: Unwind::Continue }, Span::default());

        b.start(check);
        b.push(StmtKind::StorageDead(pin), Span::default());
        b.push(StmtKind::StorageLive(discr), Span::default());
        b.assign(pdiscr, Value::Discriminant(ppol), Span::default());
        let cases = b.cases(&[(0, ready)]);
        b.terminate(StmtKind::Switch { on: Operand::Copy(pdiscr), cases, otherwise: pending }, Span::default());

        b.start(pending);
        b.push(StmtKind::StorageDead(discr), Span::default());
        b.push(StmtKind::StorageDead(poll_res), Span::default());
        b.terminate(StmtKind::Suspend { resume: head, cancel }, Span::default());

        b.start(cancel);
        b.push(StmtKind::StorageDead(fut), Span::default());
        b.terminate(StmtKind::Drop { place: pg, target: cancel_done, unwind: Unwind::Continue }, Span::default());
        b.start(cancel_done);
        b.push(StmtKind::StorageDead(l_guard), Span::default());
        b.terminate(StmtKind::Cancelled, Span::default());

        b.start(ready);
        b.push(StmtKind::StorageDead(discr), Span::default());
        let dc = b.project(ppol, Proj::Downcast(0));
        let fld = b.project(dc, Proj::Field { index: 0, ty: f_i64 });
        b.assign(dest_place, Value::Use(Operand::Copy(fld)), Span::default());
        b.push(StmtKind::StorageDead(poll_res), Span::default());
        b.push(StmtKind::StorageDead(fut), Span::default());
    };

    let bb0 = b.new_block(BodyBlockKind::Normal);
    b.start(bb0);
    b.push(StmtKind::StorageLive(l_guard), Span::default());
    let gops = b.operands(&[Operand::Copy(p2)]);
    b.assign(pg, Value::Aggregate { kind: blaze_body::AggregateKind::Adt(0), ty: f_guard, operands: gops }, Span::default());
    b.push(StmtKind::StorageLive(l_a), Span::default());
    emit_await(&mut b, 1, 10, pa);
    b.push(StmtKind::StorageLive(l_b), Span::default());
    emit_await(&mut b, 2, 20, pb);

    let after_drop = b.new_block(BodyBlockKind::Normal);
    b.assign(psum1, Value::Binary { op: blaze_body::BinOp::Add, lhs: Operand::Copy(p1), rhs: Operand::Copy(pa) }, Span::default());
    b.assign(p0, Value::Binary { op: blaze_body::BinOp::Add, lhs: Operand::Copy(psum1), rhs: Operand::Copy(pb) }, Span::default());
    b.push(StmtKind::StorageDead(l_b), Span::default());
    b.push(StmtKind::StorageDead(l_a), Span::default());
    b.terminate(StmtKind::Drop { place: pg, target: after_drop, unwind: Unwind::Continue }, Span::default());
    b.start(after_drop);
    b.push(StmtKind::StorageDead(l_guard), Span::default());
    b.terminate(StmtKind::Return, Span::default());

    let coro_body = b.finish().expect("build coro body");
    let coro_ver = blaze_body::verify(&coro_body).expect("verify coro body");
    let mut g = PortableTys::new_in(heap());
    let built_co = crate::build_coroutine(
        coro_ver,
        from.view(),
        CORO_OWNER,
        0,
        &mut Subst { args: &[], ground: &mut g, scratch: heap() },
        &cw,
        None,
        heap(),
    )
    .expect("build_coroutine succeeds");

    // Verify `resume` has finished-tag panic calls wired to `RESUMED_PANIC_FN` and `RESUMED_AFTER_PANIC_FN`!
    assert!(built_co.resume.refs.iter().any(|r| r.item == RESUMED_PANIC_FN), "resume must wire panic_const_async_fn_resumed");
    assert!(built_co.resume.refs.iter().any(|r| r.item == RESUMED_AFTER_PANIC_FN), "resume must wire panic_const_async_fn_resumed_panic");

    // Now populate `cw.state_shape` and build the creator function using `AggregateKind::Coroutine`!
    cw.state_shape = Some(built_co.state);
    let mut cb = BodyBuilder::new_in(heap());
    let cl0 = cb.new_local(LocalDecl { ty: f_coro, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let cl1 = cb.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
    let cl2 = cb.new_local(LocalDecl { ty: f_ptr_i64, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
    let (cp0, cp1, cp2) = (cb.local_place(cl0), cb.local_place(cl1), cb.local_place(cl2));
    let cbb0 = cb.new_block(BodyBlockKind::Normal);
    cb.start(cbb0);
    let cops = cb.operands(&[Operand::Copy(cp1), Operand::Copy(cp2)]);
    cb.assign(cp0, Value::Aggregate { kind: blaze_body::AggregateKind::Coroutine, ty: f_coro, operands: cops }, Span::default());
    cb.terminate(StmtKind::Return, Span::default());
    let creator_body = cb.finish().expect("creator body");
    let creator_ver = blaze_body::verify(&creator_body).expect("verify creator");
    let built_creator = build(
        creator_ver,
        from.view(),
        CREATOR_FN,
        &mut Subst { args: &[], ground: &mut g, scratch: heap() },
        &cw,
        heap(),
    )
    .expect("creator builds with AggregateKind::Coroutine");

    // Build `drop_in_place::<Guard>` shim + `Guard::drop` LIR body that increments `*(guard.0)`!
    let g_empty = g.intern_list(&[]).unwrap();
    let guard_g = g.intern(PNode::Adt(GUARD_ADT, g_empty)).unwrap();
    let built_guard_dip = crate::build_drop_glue(&mut Subst { args: &[guard_g], ground: &mut g, scratch: heap() }, &cw, heap())
        .expect("Guard drop_in_place builds");
    let guard_drop_lir = {
        let mut lb: Builder<Alloc<'static>, Full> = Builder::new_in(64, heap());
        let sig = lb.sig(Conv::C, Variadic::No, &[blaze_lir::builder::Param::of(blaze_lir::Ty::PTR)], &[]);
        lb.set_signature(sig);
        let bb = lb.create_block(BlockKind::Normal);
        lb.begin(bb);
        let self_ptr = lb.param(blaze_lir::Ty::PTR);
        let ctr_ptr = lb.load(blaze_lir::Ty::PTR, blaze_lir::Flags::NONE, self_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        let cur = lb.load(blaze_lir::Ty::I64, blaze_lir::Flags::NONE, ctr_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        let one = lb.iconst(blaze_lir::Ty::I64, 1);
        let next = lb.binary(blaze_lir::Op::Add, blaze_lir::Flags::NONE, cur, one);
        lb.store(blaze_lir::Flags::NONE, next, ctr_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        lb.ret(&[]);
        lb.finish_checked().unwrap()
    };

    // Build `SubFut::poll(pin: ptr, cx: ptr) -> Poll<i64>`:
    // `cx` points to a `[poll_count: i64, ready_on: i64]` control block!
    // Each call increments `*cx`; if `new_count >= ready_on`, returns `Poll::Ready(step_id * 10)`,
    // else returns `Poll::Pending`!
    // Poll<i64> layout: 16 bytes (tag at 0, payload at 8) -> passed via sret `result: ptr` (param 0)!
    let sub_poll_lir = {
        let mut lb: Builder<Alloc<'static>, Full> = Builder::new_in(64, heap());
        let sig = lb.sig(Conv::C, Variadic::No, &[blaze_lir::builder::Param::of(blaze_lir::Ty::PTR); 3], &[]);
        lb.set_signature(sig);
        let entry = lb.create_block(BlockKind::Normal);
        let b_ready = lb.create_block(BlockKind::Normal);
        let b_pend = lb.create_block(BlockKind::Normal);
        lb.begin(entry);
        let res = lb.param(blaze_lir::Ty::PTR);
        let fut_ptr = lb.param(blaze_lir::Ty::PTR);
        let cx_ptr = lb.param(blaze_lir::Ty::PTR);
        let step_id = lb.load(blaze_lir::Ty::I64, blaze_lir::Flags::NONE, fut_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        let cnt = lb.load(blaze_lir::Ty::I64, blaze_lir::Flags::NONE, cx_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        let one = lb.iconst(blaze_lir::Ty::I64, 1);
        let cnt1 = lb.binary(blaze_lir::Op::Add, blaze_lir::Flags::NONE, cnt, one);
        lb.store(blaze_lir::Flags::NONE, cnt1, cx_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        let need = lb.load(blaze_lir::Ty::I64, blaze_lir::Flags::NONE, cx_ptr, 8, blaze_lir::builder::MemAttrs::aligned(3));
        let is_ready = lb.icmp(blaze_lir::IntCond::Sge, cnt1, need);
        lb.br(blaze_lir::Flags::NONE, is_ready, (b_ready, &[]), (b_pend, &[]));

        lb.begin(b_ready);
        let ten = lb.iconst(blaze_lir::Ty::I64, 10);
        let val = lb.binary(blaze_lir::Op::Mul, blaze_lir::Flags::NONE, step_id, ten);
        let tag0 = lb.iconst(blaze_lir::Ty::I64, 0);
        lb.store(blaze_lir::Flags::NONE, tag0, res, 0, blaze_lir::builder::MemAttrs::aligned(3));
        lb.store(blaze_lir::Flags::NONE, val, res, 8, blaze_lir::builder::MemAttrs::aligned(3));
        lb.ret(&[]);

        lb.begin(b_pend);
        let tag1 = lb.iconst(blaze_lir::Ty::I64, 1);
        lb.store(blaze_lir::Flags::NONE, tag1, res, 0, blaze_lir::builder::MemAttrs::aligned(3));
        lb.ret(&[]);
        lb.finish_checked().unwrap()
    };

    // Build a harness LIR driver function that runs BOTH:
    //   Run A (normal completion): create(100, &ctr_a), poll x3 (Pending, Pending, Ready(130)), drop (no-op)
    //   Run B (cancel while suspended): create(50, &ctr_b), poll x1 (Pending), drop (cancel drops Guard -> ctr_b == 1!)
    // Returns `(ready_val << 16) | (ctr_a << 8) | ctr_b`!
    const ID_DRIVER: BodyId = BodyId([0; 32]);
    const ID_CREATOR: BodyId = BodyId([1; 32]);
    const ID_POLL: BodyId = BodyId([2; 32]);
    const ID_RESUME: BodyId = BodyId([3; 32]);
    const ID_DROP: BodyId = BodyId([4; 32]);
    const ID_SUB_POLL: BodyId = BodyId([5; 32]);
    const ID_GUARD_DIP: BodyId = BodyId([6; 32]);
    const ID_GUARD_DROP: BodyId = BodyId([7; 32]);
    const ID_TRAP: BodyId = BodyId([8; 32]);

    let trap_lir = trapping();
    let driver_lir = {
        let mut lb: Builder<Alloc<'static>, Full> = Builder::new_in(64, heap());
        let sig = lb.sig(Conv::C, Variadic::No, &[], &[blaze_lir::builder::Param::of(blaze_lir::Ty::I64)]);
        lb.set_signature(sig);
        let bb = lb.create_block(BlockKind::Normal);
        lb.begin(bb);
        let r_creator = lb.reference(blaze_lir::func::RefKind::Func, 0);
        let r_poll = lb.reference(blaze_lir::func::RefKind::Func, 1);
        let r_drop = lb.reference(blaze_lir::func::RefKind::Func, 2);

        let st_a = lb.slot(built_co.state.size, built_co.state.align, blaze_lir::func::SlotKind::Temp);
        let st_a_ptr = lb.slot_addr(st_a);
        let st_b = lb.slot(built_co.state.size, built_co.state.align, blaze_lir::func::SlotKind::Temp);
        let st_b_ptr = lb.slot_addr(st_b);
        let ctr_a_slot = lb.slot(8, 3, blaze_lir::func::SlotKind::Temp);
        let ctr_a_ptr = lb.slot_addr(ctr_a_slot);
        let ctr_b_slot = lb.slot(8, 3, blaze_lir::func::SlotKind::Temp);
        let ctr_b_ptr = lb.slot_addr(ctr_b_slot);
        let cx_slot = lb.slot(16, 3, blaze_lir::func::SlotKind::Temp);
        let cx_ptr = lb.slot_addr(cx_slot);
        let poll_out = lb.slot(16, 3, blaze_lir::func::SlotKind::Temp);
        let poll_out_ptr = lb.slot_addr(poll_out);

        let z = lb.iconst(blaze_lir::Ty::I64, 0);
        lb.store(blaze_lir::Flags::NONE, z, ctr_a_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        lb.store(blaze_lir::Flags::NONE, z, ctr_b_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));

        // Run A: create(100, &ctr_a); cx = [0, 2] -> poll 1 Pending (cnt=1), poll 2 Ready(10) + await2 Pending (cnt=1 after reset? No: set need=2 for await1 then cnt=2 >= 2 so await1 ready on poll 2; let's set need=2 -> poll1: cnt=1 Pending; poll2: cnt=2 Ready(10) then await2 cnt=3 >= 2 Ready(20) -> Ready(130)!)
        // Wait: set need=3 -> poll1: cnt=1 (await1 Pending); poll2: cnt=2 (await1 Pending); poll3: cnt=3 (await1 Ready(10)), await2 cnt=4 (Ready(20)) -> Ready(130)!
        let sig_creator = lb.sig(
            Conv::C,
            Variadic::No,
            &[
                blaze_lir::builder::Param::of(blaze_lir::Ty::PTR),
                blaze_lir::builder::Param::of(blaze_lir::Ty::I64),
                blaze_lir::builder::Param::of(blaze_lir::Ty::PTR),
            ],
            &[],
        );
        let sig_poll = lb.sig(Conv::C, Variadic::No, &[blaze_lir::builder::Param::of(blaze_lir::Ty::PTR); 3], &[]);
        let sig_drop = lb.sig(Conv::C, Variadic::No, &[blaze_lir::builder::Param::of(blaze_lir::Ty::PTR)], &[]);

        let c100 = lb.iconst(blaze_lir::Ty::I64, 100);
        lb.call(blaze_lir::Flags::NONE, r_creator, sig_creator, &[st_a_ptr, c100, ctr_a_ptr]);
        let need3 = lb.iconst(blaze_lir::Ty::I64, 3);
        lb.store(blaze_lir::Flags::NONE, z, cx_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        lb.store(blaze_lir::Flags::NONE, need3, cx_ptr, 8, blaze_lir::builder::MemAttrs::aligned(3));

        lb.call(blaze_lir::Flags::NONE, r_poll, sig_poll, &[poll_out_ptr, st_a_ptr, cx_ptr]);
        lb.call(blaze_lir::Flags::NONE, r_poll, sig_poll, &[poll_out_ptr, st_a_ptr, cx_ptr]);
        lb.call(blaze_lir::Flags::NONE, r_poll, sig_poll, &[poll_out_ptr, st_a_ptr, cx_ptr]);
        let ready_val = lb.load(blaze_lir::Ty::I64, blaze_lir::Flags::NONE, poll_out_ptr, 8, blaze_lir::builder::MemAttrs::aligned(3));
        // Drop after completion: must be a no-op (Guard already dropped once!).
        lb.call(blaze_lir::Flags::NONE, r_drop, sig_drop, &[st_a_ptr]);

        // Run B: create(50, &ctr_b); poll once -> Pending; drop while suspended -> cancel drops Guard!
        let c50 = lb.iconst(blaze_lir::Ty::I64, 50);
        lb.call(blaze_lir::Flags::NONE, r_creator, sig_creator, &[st_b_ptr, c50, ctr_b_ptr]);
        lb.store(blaze_lir::Flags::NONE, z, cx_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        lb.store(blaze_lir::Flags::NONE, need3, cx_ptr, 8, blaze_lir::builder::MemAttrs::aligned(3));
        lb.call(blaze_lir::Flags::NONE, r_poll, sig_poll, &[poll_out_ptr, st_b_ptr, cx_ptr]);
        lb.call(blaze_lir::Flags::NONE, r_drop, sig_drop, &[st_b_ptr]);

        let ctr_a = lb.load(blaze_lir::Ty::I64, blaze_lir::Flags::NONE, ctr_a_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        let ctr_b = lb.load(blaze_lir::Ty::I64, blaze_lir::Flags::NONE, ctr_b_ptr, 0, blaze_lir::builder::MemAttrs::aligned(3));
        let s16 = lb.iconst(blaze_lir::Ty::I64, 16);
        let s8 = lb.iconst(blaze_lir::Ty::I64, 8);
        let p_hi = lb.binary(blaze_lir::Op::Shl, blaze_lir::Flags::NONE, ready_val, s16);
        let p_mid = lb.binary(blaze_lir::Op::Shl, blaze_lir::Flags::NONE, ctr_a, s8);
        let p1 = lb.binary(blaze_lir::Op::Or, blaze_lir::Flags::NONE, p_hi, p_mid);
        let packed = lb.binary(blaze_lir::Op::Or, blaze_lir::Flags::NONE, p1, ctr_b);
        lb.ret(&[packed]);
        lb.finish_checked().unwrap()
    };

    struct CoroBodies<'a> {
        driver: View<'a>,
        creator: &'a Built<Alloc<'static>>,
        poll: &'a Built<Alloc<'static>>,
        resume: &'a Built<Alloc<'static>>,
        drop: &'a Built<Alloc<'static>>,
        sub_poll: View<'a>,
        guard_dip: &'a Built<Alloc<'static>>,
        guard_drop: View<'a>,
        trap: View<'a>,
    }
    impl<'a> CoroBodies<'a> {
        fn resolve_inst(&self, inst: blaze_build::Instance) -> Option<(BodyId, View<'a>)> {
            match inst.item {
                RESUME_FN => Some((ID_RESUME, self.resume.func.view())),
                FUTURE_POLL_FN => Some((ID_SUB_POLL, self.sub_poll)),
                DROP_IN_PLACE_FN => Some((ID_GUARD_DIP, self.guard_dip.func.view())),
                GUARD_DROP_FN => Some((ID_GUARD_DROP, self.guard_drop)),
                RESUMED_PANIC_FN | RESUMED_AFTER_PANIC_FN => Some((ID_TRAP, self.trap)),
                _ => None,
            }
        }
    }
    impl Bodies for CoroBodies<'_> {
        fn callee(&self, caller: BodyId, r: RefId) -> Option<(BodyId, View<'_>)> {
            let idx = r.0 as usize;
            if caller == ID_DRIVER {
                return match idx {
                    0 => Some((ID_CREATOR, self.creator.func.view())),
                    1 => Some((ID_POLL, self.poll.func.view())),
                    2 => Some((ID_DROP, self.drop.func.view())),
                    _ => None,
                };
            }
            let refs = match caller {
                ID_CREATOR => &self.creator.refs,
                ID_POLL => &self.poll.refs,
                ID_RESUME => &self.resume.refs,
                ID_DROP => &self.drop.refs,
                ID_GUARD_DIP => &self.guard_dip.refs,
                _ => return None,
            };
            self.resolve_inst(*refs.get(idx)?)
        }
        fn target(&self, caller: BodyId, r: RefId) -> Option<blaze_lir::body::Target> {
            self.callee(caller, r).map(|(id, _)| blaze_lir::body::Target::Func(id))
        }
        fn body(&self, id: BodyId) -> Option<View<'_>> {
            match id {
                ID_DRIVER => Some(self.driver),
                ID_CREATOR => Some(self.creator.func.view()),
                ID_POLL => Some(self.poll.func.view()),
                ID_RESUME => Some(self.resume.func.view()),
                ID_DROP => Some(self.drop.func.view()),
                ID_SUB_POLL => Some(self.sub_poll),
                ID_GUARD_DIP => Some(self.guard_dip.func.view()),
                ID_GUARD_DROP => Some(self.guard_drop),
                ID_TRAP => Some(self.trap),
                _ => None,
            }
        }
    }

    let bodies = CoroBodies {
        driver: driver_lir.view(),
        creator: &built_creator,
        poll: &built_co.poll,
        resume: &built_co.resume,
        drop: &built_co.drop,
        sub_poll: sub_poll_lir.view(),
        guard_dip: &built_guard_dip,
        guard_drop: guard_drop_lir.view(),
        trap: trap_lir.view(),
    };
    let out = blaze_lir::eval::run_calls_in(driver_lir.view(), ID_DRIVER, &[], 100_000, &bodies, heap()).expect("driver runs cleanly");
    let vals = returned(out);
    assert_eq!(vals.len(), 1);
    let packed = match vals[0] {
        Val::Int(n) => n,
        other => panic!("expected packed int result, got {other:?}"),
    };
    let ready_val = packed >> 16;
    let ctr_a = (packed >> 8) & 0xff;
    let ctr_b = packed & 0xff;
    assert_eq!(ready_val, 130, "expected 100 + 10 + 20 == 130 across 2 awaits");
    assert_eq!(ctr_a, 1, "Guard must drop exactly once on normal completion (and not again on post-Return drop)");
    assert_eq!(ctr_b, 1, "Guard must drop exactly once on cancel while suspended");

    // 2. `Poll<!>` diverging coroutine builds and verifies cleanly with zero-sized `Poll<!>`.
    let mut nb = BodyBuilder::new_in(heap());
    let _nl0 = nb.new_local(LocalDecl { ty: f_never, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let _npoll = nb.new_local(LocalDecl { ty: f_poll_never, kind: LocalKind::Temp(ExprId(0), Role::Result), mutbl: Mutbl::Mut });
    let nbb0 = nb.new_block(BodyBlockKind::Normal);
    let ncancel = nb.new_block(BodyBlockKind::Normal);
    nb.start(nbb0);
    nb.terminate(StmtKind::Suspend { resume: nbb0, cancel: ncancel }, Span::default());
    nb.start(ncancel);
    nb.terminate(StmtKind::Cancelled, Span::default());
    let never_body = nb.finish().expect("never coro body");
    let never_ver = blaze_body::verify(&never_body).expect("verify never coro");
    let built_never = crate::build_coroutine(
        never_ver,
        from.view(),
        NEVER_OWNER,
        0,
        &mut Subst { args: &[], ground: &mut g, scratch: heap() },
        &cw,
        None,
        heap(),
    )
    .expect("Poll<!> coroutine builds cleanly");
    assert!(printed(&built_never.resume).contains("switch "), "split Poll<!> resume must switch on state tag");
    assert!(printed(&built_never.drop).contains("switch "), "split Poll<!> drop must switch on state tag");

    // 3. `AssertKind::ResumedAfterReturn` and `AssertKind::ResumedAfterPanic` emit the right lang calls!
    for (kind, want_item) in [
        (blaze_body::AssertKind::ResumedAfterReturn, RESUMED_PANIC_FN),
        (blaze_body::AssertKind::ResumedAfterPanic, RESUMED_AFTER_PANIC_FN),
    ] {
        let mut ab = BodyBuilder::new_in(heap());
        let al0 = ab.new_local(LocalDecl { ty: f_i64, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let al1 = ab.new_local(LocalDecl { ty: f_u8, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let (ap0, ap1) = (ab.local_place(al0), ab.local_place(al1));
        let k0 = ab.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 0 }, ty: f_i64 });
        let abb0 = ab.new_block(BodyBlockKind::Normal);
        let abb1 = ab.new_block(BodyBlockKind::Normal);
        let empty_msg = ab.operands(&[]);
        ab.start(abb0);
        ab.terminate(
            StmtKind::Assert { cond: Operand::Copy(ap1), expected: true, kind, msg: empty_msg, target: abb1, unwind: Unwind::Continue },
            Span::default(),
        );
        ab.start(abb1);
        ab.assign(ap0, Value::Use(Operand::Const(k0)), Span::default());
        ab.terminate(StmtKind::Return, Span::default());
        let abody = ab.finish().unwrap();
        let aver = blaze_body::verify(&abody).unwrap();
        let abuilt = build(aver, from.view(), ITEM, &mut Subst { args: &[], ground: &mut g, scratch: heap() }, &cw, heap()).unwrap();
        assert!(abuilt.refs.iter().any(|r| r.item == want_item), "assert {kind:?} must call {want_item:?}");
    }

    // 4. `build_drop_glue` explicitly refuses `PNode::Coroutine` (its drop glue is `BuiltCoroutine::drop`).
    let coro_g = g.intern(PNode::Coroutine { owner: CORO_OWNER, ordinal: 0, args: g_empty }).unwrap();
    let drop_err = crate::build_drop_glue(&mut Subst { args: &[coro_g], ground: &mut g, scratch: heap() }, &cw, heap());
    assert!(drop_err.is_err(), "build_drop_glue must refuse PNode::Coroutine rather than silently emitting a no-op");
    let _ = guard_a;
}
