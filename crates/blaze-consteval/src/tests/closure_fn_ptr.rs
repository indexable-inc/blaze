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
use crate::Refusal;
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
