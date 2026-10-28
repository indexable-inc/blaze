//! What a failed `assert!` says when its condition is written with spacing of its own: rustc
//! prints binary operators with a space on each side and paths without one, and keeps a
//! literal as written. Reduced by Charlie Marsh (astral-experiments/blaze).

use std::hint::black_box;
use std::panic::catch_unwind;

pub fn run() -> String {
    let caught = catch_unwind(|| {
        let value = black_box(9u8);
        assert!(value<=  u8 :: MAX&&"A :: B".is_empty());
    });
    match caught {
        Err(payload) => payload.downcast_ref::<&str>().map_or(String::new(), |it| it.to_string()),
        Ok(()) => String::new(),
    }
}
