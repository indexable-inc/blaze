//! refused: reaches
// `Shares::new` of a type that can come to own itself (its `Cycle` assertion).
use std::cell::RefCell;

use bstd::{cell::Token, own::{Shared, Shares}};

pub struct Node {
    pub value: u32,
    pub next: RefCell<Option<Shared<Node>>>,
}

pub fn run() {
    Token::scope(|token| {
        let all: Shares<Node, _, 1, 1> = Shares::new(token, Node { value: 1, next: RefCell::new(None) });
        let _ = all.whole();
    });
}
