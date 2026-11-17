//! refused: a `OneOf` with `Dir<ReadWrite>` after `Dir<ReadOnly>` is refused
// The read-only folder takes every folder grant first, so the read-write one is never chosen
// (bstd::grant::compose `one_of_form`). The twin is the useful order: write if granted, else read.
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly, ReadWrite},
    grant::OneOf,
};

#[bstd::main]
pub fn main(_cx: &Cx, store: OneOf<(Dir<ReadOnly>, Dir<ReadWrite>)>) {
    let _ = store;
}
