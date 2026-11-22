//! refused: reaches
// A handle to a type that can come to own itself: a counted pointer behind a cell
// (`Shared::holding`'s `Acyclic` assertion, the `Cycle` walk).
use std::cell::RefCell;

use bstd::own::Shared;

pub struct Node {
    pub value: u32,
    pub next: RefCell<Option<Shared<Node>>>,
}

pub fn run() {
    let node = Shared::holding(Node { value: 1, next: RefCell::new(None) });
    let _ = node.release();
}
