//! refused: is interior mutability
// A key type asserted to be immutable through `&`, which holds a cell (`none::<_, AnyCell>`).
use std::cell::Cell;

use bstd::own::{AnyCell, none};

pub struct Key {
    pub id: u64,
    pub hits: Cell<u32>,
}

const _: () = assert!(none::<Key, AnyCell>());
