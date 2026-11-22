//! refused: `Maybe<Maybe<..>>` is refused: a grant is optional once
// An optional grant of an optional grant says nothing more and is refused as the crate
// compiles (bstd::grant::compose `maybe_form`, a const panic at the manifest's static).
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly},
    grant::Maybe,
};

#[bstd::main]
pub fn main(_cx: &Cx, data: Maybe<Maybe<Dir<ReadOnly>>>) {
    let _ = data;
}
