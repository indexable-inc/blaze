// The twin: one `Maybe`.
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly},
    grant::Maybe,
};

#[bstd::main]
pub fn main(_cx: &Cx, data: Maybe<Dir<ReadOnly>>) {
    let _ = data;
}
