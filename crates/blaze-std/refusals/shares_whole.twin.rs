//! twin of shares_whole.rs: the parts joined first, so it compiles.
use bstd::{cell::Token, own::Shares};

pub fn run() {
    Token::scope(|token| {
        let all: Shares<u32, _, 2, 2> = Shares::new(token, 7);
        let (one, other) = all.split::<1, 1>();
        let both: Shares<u32, _, 2, 2> = one.join(other);
        let _ = both.whole();
    });
}
