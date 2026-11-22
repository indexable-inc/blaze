// The twin: the same `main` with a capability there.
#![feature(macro_attr)]

use bstd::{Cx, clock::Clock, out::Out};

#[bstd::main]
pub fn main(_cx: &Cx, out: Out, count: Clock) {
    let _ = (out, count);
}
