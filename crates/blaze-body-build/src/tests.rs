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

fn printed(b: &Built<Alloc<'static>>) -> String {
    blaze_lir::print(b.func.view())
}

fn returned(outcome: Outcome<Alloc<'static>>) -> Vec<Val> {
    match outcome {
        Outcome::Returned(vals) => vals.iter().copied().collect(),
        _ => panic!("the run did not return"),
    }
}

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

/// A row this builder does not translate is refused by name, here a promoted constant.
/// Should give `BuildError::Unsupported` at row 0.
#[test]
fn unsupported_row_is_named() {
    let text = "\
local _0: t3 return
const c0 = promoted 0: t3
bb0:
  _0 = const c0
  return
";
    match built(text, Panic::Abort) {
        Err(BuildError::Unsupported { stmt, what }) => {
            assert_eq!(stmt.0, 0);
            assert!(what.contains("promoted"), "{what}");
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
