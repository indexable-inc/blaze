// The twin: the same test names the one capability it uses.
#![feature(macro_attr)]
use bstd::{Cx, clock::Clock};

#[bstd::test]
fn sleeps(_cx: &Cx, clock: Clock) {
    let _ = clock;
}
