//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `N / (N - N)` on usize: the divisor is computed to zero
#![crate_type = "lib"]
pub const N: usize = 9;
pub const X: usize = N / (N - N);
