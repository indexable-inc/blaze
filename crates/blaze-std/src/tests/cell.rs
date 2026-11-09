//! Token cells: one token reads and writes every cell of its kind, and the token behind one
//! `Lock` guards them all across threads. That two live `get_mut` through one token do not
//! compile is the `compile_fail` fixture in `cell.rs`'s docs (unrun: bstd has
//! `doctest = false`).

use crate::{
    cell::{Token, TokenCell},
    sync::Lock,
};

#[test]
fn one_token_reads_and_writes_every_cell_of_its_kind() {
    Token::scope(|mut token| {
        let a = TokenCell::new(1u32);
        let b = TokenCell::new(vec![1u8]);
        *a.get_mut(&mut token) += 41;
        b.get_mut(&mut token).push(2);
        // Two shared reads at once through one shared token.
        let (x, y) = (a.get(&token), b.get(&token));
        assert_eq!((*x, y.as_slice()), (42, &[1u8, 2][..]));
        let mut a = a;
        *a.value_mut() = 7;
        assert_eq!(a.into_inner(), 7);
    });
}

#[test]
fn a_token_behind_one_lock_guards_every_cell_across_threads() {
    const THREADS: u32 = 4;
    const EACH: u32 = 100;
    let (a, b) = Token::scope(|token| {
        let a = TokenCell::new(0u32);
        let b = TokenCell::new(0u64);
        let lock = Lock::new("bstd.tests.token", token);
        std::thread::scope(|threads| {
            for _ in 0..THREADS {
                threads.spawn(|| {
                    for _ in 0..EACH {
                        let mut token = lock.lock();
                        *a.get_mut(&mut *token) += 1;
                        *b.get_mut(&mut *token) += 2;
                    }
                });
            }
        });
        (a.into_inner(), b.into_inner())
    });
    assert_eq!((a, b), (THREADS * EACH, u64::from(THREADS * EACH) * 2));
}
