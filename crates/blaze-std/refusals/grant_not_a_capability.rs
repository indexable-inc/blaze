//! refused: `main` cannot take a `u32`
// A parameter of `main` whose type is no capability (bstd::grant::Grant) is refused at that
// parameter: the error names the type and lists the kinds.
#![feature(macro_attr)]

use bstd::{Cx, out::Out};

#[bstd::main]
pub fn main(_cx: &Cx, out: Out, count: u32) {
    let _ = (out, count);
}
