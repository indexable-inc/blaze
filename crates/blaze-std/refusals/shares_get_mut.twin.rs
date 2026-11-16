//! twin of shares_get_mut.rs: changed through the handle of both parts, so it compiles.
use bstd::{cell::Token, own::Shares};

pub fn run() {
    Token::scope(|token| {
        let mut all: Shares<u32, _, 2, 2> = Shares::new(token, 7);
        *all.get_mut() += 1;
        let _ = all.whole();
    });
}
