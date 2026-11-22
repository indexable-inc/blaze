//! A closure that captures nothing, coerced to a fn pointer in a constant (the Reference, "Type
//! coercions": "Non capturing closures to `fn` pointers"), as `thread_local!` writes it:
//! `LocalKey::new(const { |_| .. })`. The value is a fn pointer to the closure's
//! `FnOnce::call_once` at `[closure, (args..)]` (blaze-build's `closure_fn_addr`): one word whose
//! relocation is that instance, a different one per closure.
//!
//! PLANTED BUGS this catches: the old refusal ("a closure in a constant"); a pointer keyed by
//! `call_once` alone, without the closure in its arguments (`A` and `B` would share a target);
//! a world that names no `call_once` answered with a guess instead of the named refusal.

use super::{refusal, world};
use crate::{FnTraitMethod, Refusal};
use crate::record::DataTarget;

fn fn_target(w: &mut crate::fixture::World, name: &str) -> blaze_build::InstanceKey {
    let r = w.eval(name).unwrap_or_else(|e| panic!("`{name}`: {:?}", e.refusal));
    assert_eq!(r.data.allocs.len(), 1, "`{name}`: one word, the pointer");
    assert_eq!(r.data.allocs[0].bytes.len(), 8, "`{name}`: a thin fn pointer");
    assert_eq!(r.data.allocs[0].relocs.len(), 1, "`{name}`: its one relocation");
    match r.data.allocs[0].relocs[0].target {
        DataTarget::Fn(key) => key,
        ref other => panic!("`{name}`: the pointer's target is {other:?}, not a fn instance"),
    }
}

/// Prints `test tests::closure_fn_ptr::a_closure_that_captures_nothing_is_a_fn_pointer ... ok`.
#[test]
fn a_closure_that_captures_nothing_is_a_fn_pointer() {
    let mut w = world();
    let call_once = w.plain_fn("call_once", &[], "()", "{}");
    w.fn_once_is(call_once);
    // No operators in the closures: `x + 1` asks the `Add` lang item (the Reference,
    // "Arithmetic and Logical Binary Operators": `+` is `core::ops::Add::add`), which this world
    // does not declare (the check's `MissingLangItem` at `x + 1`, run of 2026-11-17).
    w.konst("A", "fn(u32) -> u32", "|x| x");
    w.konst("B", "fn(u32) -> u32", "|_| 2");
    w.build();
    let a = fn_target(&mut w, "A");
    let b = fn_target(&mut w, "B");
    assert_ne!(a, b, "two closures are two `call_once` instances: the closure is in the key");
}

/// Prints `test tests::closure_fn_ptr::no_call_once_is_refused_by_name ... ok`.
#[test]
fn no_call_once_is_refused_by_name() {
    let mut w = world();
    w.konst("A", "fn(u32) -> u32", "|x| x");
    w.build();
    assert_eq!(
        refusal(&mut w, "A"),
        std::format!(
            "{:?}",
            Refusal::Unsupported { what: "a closure as a fn pointer in a world that names no `FnOnce::call_once` (ConstWorld::fn_trait_method)" }
        )
    );
}

#[test]
fn pending_call_once_names_the_trait_and_resolves_when_loaded() {
    let mut w = world();
    let trait_ = w.fx.tr("FnOnce", 2, std::vec![], &[]);
    let self_ty = w.fx.param(0);
    let args = w.fx.param(1);
    let output = w.ty("u32");
    let call_once = w.fx.trait_fn(trait_, "call_once", true, &[self_ty, args], output);
    w.konst("A", "fn(u32) -> u32", "|x| x");
    w.build();

    w.fn_once = FnTraitMethod::Pending { trait_ };
    let e = w.eval("A").expect_err("the trait's members are pending");
    assert!(matches!(e.refusal, Refusal::Missing { item, .. } if item == trait_), "{e:?}");

    w.fn_once_is(call_once);
    let record = w.eval("A").expect("the trait's members are now available");
    assert!(matches!(&record.fns[..], [f] if f.instance.item == call_once));
}
