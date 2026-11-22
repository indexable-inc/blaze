//! twin of rank_order.rs: the rank-20 lock taken with the rank-10 token, so it compiles.
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

pub fn run(index: &RankedLock<u32, Index>, order: &mut Order<'_, Store>) {
    let _ = index.try_lock(order).is_some();
}
