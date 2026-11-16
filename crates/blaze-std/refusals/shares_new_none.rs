//! refused: Shares::new: D is at least 1
// A value split into no parts.
use bstd::{cell::Token, own::Shares};

pub fn run() {
    Token::scope(|token| {
        let none: Shares<u32, _, 0, 0> = Shares::new(token, 7);
        let _ = none.whole();
    });
}
