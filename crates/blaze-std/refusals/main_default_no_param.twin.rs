// The twin: the default names the parameter.
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly},
};

#[bstd::main(data = dir("."))]
pub fn main(_cx: &Cx, data: Dir<ReadOnly>) {
    let _ = data;
}
