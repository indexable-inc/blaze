//@ expect: reject E0277
//@ edition: 2021
// Mutually recursive structs where only `Branch` holds a `Cell`: `Tree: Sync` fails through it.
use std::cell::Cell;

struct Tree {
    label: u8,
    branches: Vec<Branch>,
}

struct Branch {
    hits: Cell<u32>,
    sub: Option<Box<Tree>>,
}

fn is_sync<T: Sync>() {}

fn main() {
    is_sync::<Tree>();
}
