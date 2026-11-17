//! refused: bstd::main: a default's spelling does not fit its parameter
// `data` is a folder, and `connect(..)` spells an endpoint (bstd::grant::defaults `Shape::fits`).
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly},
};

#[bstd::main(data = connect("example.com:443"))]
pub fn main(_cx: &Cx, data: Dir<ReadOnly>) {
    let _ = data;
}
