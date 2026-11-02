//@ expect: reject E0277
//@ edition: 2021
//@ issue: #1522
// The kind of #1522's second case outside a test: rustc rejects `{:?}` of a type that is not
// `Debug` (E0277).
#![crate_type = "lib"]

pub struct Digest(pub [u8; 4]);

pub fn shown(digest: &Digest) -> String {
    format!("{:?}", digest)
}
