//! refused: bstd::main: a default names no parameter of `main`
// `cache` is no parameter of `main`, so its default is refused as the crate compiles
// (bstd::grant::defaults `Manifest::with_defaults`).
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly},
};

#[bstd::main(cache = dir("."))]
pub fn main(_cx: &Cx, data: Dir<ReadOnly>) {
    let _ = data;
}
