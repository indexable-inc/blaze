//! refused: `without(Crashes)`: no parameter of this test writes to a folder a power cut can tear
// A fault kind left out that no parameter can meet is refused (D109): a test of a folder read
// only meets no crash, so `without(Crashes)` there says nothing and would mislead a reader.
#![feature(macro_attr)]
use bstd::{Cx, fs::{Dir, ReadOnly}};

#[bstd::test(without(Crashes))]
fn reads_a_ledger(_cx: &Cx, data: Dir<ReadOnly>) {
    let _ = data;
}
