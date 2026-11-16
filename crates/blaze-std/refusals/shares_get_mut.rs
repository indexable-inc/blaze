//! refused: Shares::get_mut: only the handle of all D parts
// The value changed through one part of two.
use bstd::{cell::Token, own::Shares};

pub fn run() {
    Token::scope(|token| {
        let all: Shares<u32, _, 2, 2> = Shares::new(token, 7);
        let (mut one, other) = all.split::<1, 1>();
        *one.get_mut() += 1;
        let _ = other;
    });
}
