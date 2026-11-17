// The twin: the same test taking a capability in the type's place, which compiles with or
// without `sim-draws`.
#![feature(macro_attr)]
use bstd::{Cx, clock::Clock};

#[bstd::test]
fn keeps_what_it_committed(_cx: &Cx, clock: Clock) {
    let _ = clock;
}
