//! refused: E0499
// Two live `get_mut` through one token (`cell.rs`'s fixture).
use bstd::cell::{Token, TokenCell};

pub fn run() {
    Token::scope(|mut token| {
        let a = TokenCell::new(1);
        let b = TokenCell::new(2);
        let first = a.get_mut(&mut token);
        let second = b.get_mut(&mut token);
        *first += *second;
    });
}
