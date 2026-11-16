//! twin of shares_new_none.rs: one part, so it compiles.
use bstd::{cell::Token, own::Shares};

pub fn run() {
    Token::scope(|token| {
        let one: Shares<u32, _, 1, 1> = Shares::new(token, 7);
        let _ = one.whole();
    });
}
