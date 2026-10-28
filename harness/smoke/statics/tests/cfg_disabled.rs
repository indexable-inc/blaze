//! A disabled crate still gets an empty libtest harness (#1323, Astral #35).
#![cfg(any())]

#[test]
fn disabled_test() {
    panic!("a test from a cfg-disabled crate ran");
}
