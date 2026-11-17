//! refused: a `Net` parameter needs its endpoint in the attribute
// A test that takes the network names the one endpoint it reaches in its attribute (D109): a
// `Net` with none is refused as the crate compiles, saying what to write.
#![feature(macro_attr)]
use bstd::{Cx, net::Net};

#[bstd::test]
fn reaches_an_api(_cx: &Cx, api: Net) {
    let _ = api;
}
