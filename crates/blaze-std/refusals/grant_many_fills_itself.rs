//! refused: a `Many` of a kind that fills itself
// A clock takes no grant, so there is nothing to have many of (bstd::grant::compose `many_form`).
#![feature(macro_attr)]

use bstd::{Cx, clock::Clock, grant::Many};

#[bstd::main]
pub fn main(_cx: &Cx, clocks: Many<Clock>) {
    let _ = clocks;
}
