// The twin: the same test with its endpoint named in the attribute.
#![feature(macro_attr)]
use bstd::{Cx, net::Net};

#[bstd::test(api = connect("10.0.0.2:443"))]
fn reaches_an_api(_cx: &Cx, api: Net) {
    let _ = api;
}
