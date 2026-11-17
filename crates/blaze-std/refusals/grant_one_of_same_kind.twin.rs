// The twin: the repeated alternative gone.
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly},
    grant::OneOf,
    net::Net,
};

#[bstd::main]
pub fn main(_cx: &Cx, store: OneOf<(Dir<ReadOnly>, Net)>) {
    let _ = store;
}
