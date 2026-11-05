// An integration test whose root is configured out, as fff's `#![cfg(stress)]` ones are
// without `--cfg stress` (#1360): rustc builds it as an empty program of tests, which links,
// runs no test and passes.
#![cfg(any())]

#[test]
fn never_built() {
    panic!("a configured-out crate has no tests");
}
