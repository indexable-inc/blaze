// The twin: the same test leaves out a fault kind its folder can meet, a failed effect.
#![feature(macro_attr)]
use bstd::{Cx, fs::{Dir, ReadOnly}};

#[bstd::test(without(IoFailures))]
fn reads_a_ledger(_cx: &Cx, data: Dir<ReadOnly>) {
    let _ = data;
}
