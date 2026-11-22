// The twin: many folders.
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly},
    grant::Many,
};

#[bstd::main]
pub fn main(_cx: &Cx, inputs: Many<Dir<ReadOnly>>) {
    let _ = inputs;
}
