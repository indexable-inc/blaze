//! refused: a `OneOf` with two alternatives of one kind is refused
// Two alternatives no grant tells apart (bstd::grant::compose `one_of_form`).
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly},
    grant::OneOf,
    net::Net,
};

#[bstd::main]
pub fn main(_cx: &Cx, store: OneOf<(Dir<ReadOnly>, Net, Dir<ReadOnly>)>) {
    let _ = store;
}
