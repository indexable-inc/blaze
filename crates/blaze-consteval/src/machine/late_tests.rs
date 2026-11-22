//! A refusal met while compiling a body that names something a later step supplies is
//! PENDING (machine/late.rs): the body's code is not complete, and the next evaluation that
//! asks for it compiles that body again, with no reset from outside; every other body is
//! compiled once.
//!
//! The case: `const C: i32 = mk() + one();` where `const fn mk` builds a `P { x: 3, y: 4 }`.
//! The first evaluation runs in a world that does not hold `P`'s definition yet (as the front
//! end's store before it makes the row: blaze-ladder front2/consts.rs `adt_row`): compiling
//! `mk` meets `Refusal::Missing { what: "ADT definition", item: P }`, which is compiled in and
//! reached. The second evaluation, on the SAME `Codes`, runs in the world that holds `P`: it
//! must give 3 * 10 + 4 + 1 = 35 (the Reference, "Constant evaluation": a `const fn` call in a
//! constant is evaluated at compile time; "Struct expressions" for `P { .. }`).
//!
//! The control flips the old behaviour (`Plant::KeepPendingCode`: a pending refusal kept as if
//! final, so `mk`'s code stays "complete"); the second evaluation then raises the OLD refusal,
//! `Missing { what: "ADT definition", item: P }`, and `mk` is compiled once. To see the main
//! test fail that way, pass `Plant::KeepPendingCode` in its `retry` call.
//!
//! Run: `cargo test -p blaze-consteval --lib late_tests`; each test prints
//! `test machine::late_tests::<name> ... ok`, and with `--nocapture` the main one prints
//! `late: second evaluation 35; compiles mk 2, C 1, one 1`.

use std::format;
use std::string::String;

use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_typeck::{Adt, Fetch, ItemKey, PortableTys};

use super::{Answer, Codes, Plant, value_of_planted};
use crate::fixture::{World, int_of};
use crate::record::{ConstRecord, RecordKey};
use crate::{ConstError, ConstWorld, FnKind, LayoutFacts, Layouts, ReprAttrs, Typed, ValidRange};

/// `World` with one ADT's definition not made yet.
struct Withheld<'w> {
    w: &'w World,
    adt: ItemKey,
}

impl LayoutFacts for Withheld<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        if key == self.adt { None } else { self.w.adt(key) }
    }

    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, blaze_build::layout::Missing> {
        self.w.repr(key)
    }

    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, blaze_build::layout::Missing> {
        self.w.discriminant(key, variant)
    }

    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, blaze_build::layout::Missing> {
        self.w.valid_range(key)
    }
}

impl ConstWorld for Withheld<'_> {
    fn body(&self, item: ItemKey) -> Fetch<Option<Typed<'_>>> {
        self.w.body(item)
    }

    fn fn_kind(&self, item: ItemKey) -> FnKind {
        self.w.fn_kind(item)
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

/// What the two evaluations gave, and each body's compile count after them.
struct Retried {
    first: Result<u128, String>,
    waits_after_first: usize,
    second: Result<u128, String>,
    waits_after_second: usize,
    compiles: (u32, u32, u32),
    p: ItemKey,
}

fn answer(out: Result<super::Outcome<'_>, ConstError<'_>>) -> Result<u128, String> {
    match out {
        Ok(Answer::Done(r)) => Ok(int_of(&r)),
        Ok(Answer::Need(n)) => Err(format!("need {n:?}")),
        Err(ConstError { refusal, .. }) => Err(format!("{refusal:?}")),
    }
}

/// `C` evaluated twice on one `Codes`: first without `P`'s definition, then with it.
fn retry(plant: Plant) -> Retried {
    let mut w = World::new();
    w.strukt("P", &[("x", "i32"), ("y", "i32")]);
    w.const_fn("mk", &[], "i32", "{ let p = P { x: 3, y: 4 }; p.x * 10 + p.y }");
    w.const_fn("one", &[], "i32", "{ 1 }");
    let c = w.konst("C", "i32", "mk() + one()");
    w.build();
    let (p, mk, one) = (w.key("P"), w.key("mk"), w.key("one"));
    let alloc = blaze_own::testing::heap().alloc();
    let mut ground = PortableTys::new_in(alloc);
    let mut layout = Layouts::new_in(alloc);
    let mut codes = Codes::new_in(alloc);
    let withheld = Withheld { w: &w, adt: p };
    let first = answer(value_of_planted(&withheld, &mut layout, &mut codes, c, &mut ground, plant, alloc));
    let waits_after_first = codes.waits(mk);
    // No reset of `codes`, `layout` or `ground` here: the evaluator alone decides what to
    // compile again.
    let second = answer(value_of_planted(&w, &mut layout, &mut codes, c, &mut ground, plant, alloc));
    let waits_after_second = codes.waits(mk);
    let compiles = (codes.compiles(mk), codes.compiles(c), codes.compiles(one));
    Retried { first, waits_after_first, second, waits_after_second, compiles, p }
}

fn missing_p(p: ItemKey) -> String {
    format!("{:?}", crate::Refusal::Missing { what: "ADT definition", item: p })
}

#[test]
fn pending_refusal_is_compiled_again_without_a_reset() {
    let r = retry(Plant::None);
    assert_eq!(r.first, Err(missing_p(r.p)), "the first evaluation reaches the refusal compiled into `mk`");
    assert!(r.waits_after_first >= 1, "`mk`'s code lists what it waited for");
    assert_eq!(r.second, Ok(35), "the second evaluation compiles `mk` again against the world that holds `P`");
    assert_eq!(r.waits_after_second, 0, "`mk`'s new code waits for nothing: it is complete");
    assert_eq!(r.compiles, (2, 1, 1), "(mk, C, one): only the body that waited is compiled twice");
    std::eprintln!("late: second evaluation 35; compiles mk {}, C {}, one {}", r.compiles.0, r.compiles.1, r.compiles.2);
}

/// The old behaviour (a pending refusal kept as final) is caught: the second evaluation
/// raises the first one's refusal from the kept code, and `mk` is compiled only once.
#[test]
fn control_pending_refusal_kept_as_final_is_caught() {
    let r = retry(Plant::KeepPendingCode);
    assert_eq!(r.first, Err(missing_p(r.p)));
    assert_eq!(r.second, Err(missing_p(r.p)), "the planted bug re-raises the old refusal");
    assert_eq!(r.compiles, (1, 1, 0), "(mk, C, one): the kept code is never compiled again");
}

/// Which refusals are pending is said by the type (machine/late.rs `Wait::of`): a projection
/// no impl answered and a missing definition wait; the program's own fault and an unhandled
/// form do not.
#[test]
fn pending_and_final_refusals_are_told_apart() {
    use super::late::Wait;
    use crate::Refusal;
    use blaze_typeck::Name;
    let (t, n) = (ItemKey(0x77), Name(5));
    assert!(Wait::of(&Refusal::Projection { trait_: t, name: n }).is_some());
    assert!(Wait::of(&Refusal::Missing { what: "impl for an associated item", item: t }).is_some());
    assert!(Wait::of(&Refusal::FieldGround { adt: t, field: 1, name: n, projection: Some((t, n)) }).is_some());
    assert!(Wait::of(&Refusal::FieldGround { adt: t, field: 1, name: n, projection: None }).is_none());
    assert!(Wait::of(&Refusal::Unsupported { what: "a type with no ground form (an alias, an opaque type, a closure)" }).is_none());
    assert!(Wait::of(&Refusal::DivByZero).is_none());
}
