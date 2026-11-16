//! twin of acyclic_shared.rs: the counted pointer behind a cell removed, so it compiles.
use bstd::own::Shared;

pub struct Node {
    pub value: u32,
}

pub fn run() {
    let node = Shared::holding(Node { value: 1 });
    let _ = node.release();
}
