//! twin of acyclic_shares.rs: the counted pointer behind a cell removed, so it compiles.
use bstd::{cell::Token, own::Shares};

pub struct Node {
    pub value: u32,
}

pub fn run() {
    Token::scope(|token| {
        let all: Shares<Node, _, 1, 1> = Shares::new(token, Node { value: 1 });
        let _ = all.whole();
    });
}
