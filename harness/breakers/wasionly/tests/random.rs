//! getrandom 0.2 built for wasm32-wasip1 links `wasi`, a dependency of that target alone, which
//! the host's unit graph has not: a proc macro's module that linked getrandom did not build.

#[test]
fn getrandom_ran_in_the_macro() {
    assert!(wasionly::random_filled());
}
