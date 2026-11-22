//! refused: Shares::whole: only the handle of all D parts
// The value taken whole through one part of two.
use bstd::{cell::Token, own::Shares};

pub fn run() {
    Token::scope(|token| {
        let all: Shares<u32, _, 2, 2> = Shares::new(token, 7);
        let (one, other) = all.split::<1, 1>();
        let _ = one.whole();
        let _ = other;
    });
}
