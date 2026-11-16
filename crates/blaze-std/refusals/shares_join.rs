//! refused: Shares::join: the result holds N + M parts
// One part and one part joined into a handle said to hold one.
use bstd::{cell::Token, own::Shares};

pub fn run() {
    Token::scope(|token| {
        let all: Shares<u32, _, 2, 2> = Shares::new(token, 7);
        let (one, other) = all.split::<1, 1>();
        let wrong: Shares<u32, _, 1, 2> = one.join(other);
        let _ = wrong;
    });
}
