//! A second disabled target joins the same worker, with its cfg supplied by cfg_attr.
#![cfg_attr(all(), cfg(any()))]

#[test]
fn disabled_test() {
    panic!("a test from a cfg_attr-disabled crate ran");
}
