//! refused: Shares::split: A + B == N, each at least 1
// Two parts split into parts that do not add up to two.
use bstd::{cell::Token, own::Shares};

pub fn run() {
    Token::scope(|token| {
        let all: Shares<u32, _, 2, 2> = Shares::new(token, 7);
        let (one, other) = all.split::<1, 2>();
        let _ = (one, other);
    });
}
