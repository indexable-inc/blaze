//! refused: a lock is taken only with a token of a lower rank
// A lock of rank 10 taken with the token of a rank-20 guard.
use bstd::sync::{Order, Rank, RankedLock};

pub enum Store {}
impl Rank for Store {
    const LEVEL: u32 = 10;
    const NAME: &'static str = "store";
}

pub enum Index {}
impl Rank for Index {
    const LEVEL: u32 = 20;
    const NAME: &'static str = "index";
}

pub fn run(store: &RankedLock<u32, Store>, order: &mut Order<'_, Index>) {
    let _ = store.try_lock(order).is_some();
}
