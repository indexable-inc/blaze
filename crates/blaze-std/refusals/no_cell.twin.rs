//! twin of no_cell.rs: the cell removed, so it compiles.
use bstd::own::{AnyCell, none};

pub struct Key {
    pub id: u64,
}

const _: () = assert!(none::<Key, AnyCell>());
