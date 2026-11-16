//! twin of token_two_borrows.rs: one borrow ends before the next, so it compiles.
use bstd::cell::{Token, TokenCell};

pub fn run() {
    Token::scope(|mut token| {
        let a = TokenCell::new(1);
        let b = TokenCell::new(2);
        let second = *b.get_mut(&mut token);
        let first = a.get_mut(&mut token);
        *first += second;
    });
}
