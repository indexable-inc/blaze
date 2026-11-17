//! refused: a test cannot take a whole `Io`
// A test names what it takes after `cx` (D109); the whole simulated world is refused at the
// attribute, with the parameters to write instead.
#![feature(macro_attr)]
use bstd::{Cx, Io};

#[bstd::test]
fn sleeps(_cx: &Cx, io: Io) {
    let _ = io;
}
