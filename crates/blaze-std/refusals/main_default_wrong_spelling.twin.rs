// The twin: a folder spelled as a folder, and an endpoint as an endpoint.
#![feature(macro_attr)]

use bstd::{
    Cx,
    fs::{Dir, ReadOnly},
    net::Net,
};

#[bstd::main(data = dir("."), api = connect("example.com:443"))]
pub fn main(_cx: &Cx, data: Dir<ReadOnly>, api: Net) {
    let _ = (data, api);
}
