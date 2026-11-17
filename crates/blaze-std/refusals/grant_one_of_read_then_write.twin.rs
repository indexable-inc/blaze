// The twin: write if granted, else read (accepted).
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly, ReadWrite},
    grant::OneOf,
};

#[bstd::main]
pub fn main(_cx: &Cx, store: OneOf<(Dir<ReadWrite>, Dir<ReadOnly>)>) {
    let _ = store;
}
