//! `std::hint`: `spin_loop` is a yield in a model, so a thread that spins waiting for another
//! lets it run and the execution ends.

use std::panic::Location;

pub use std::hint::{assert_unchecked, black_box, unreachable_unchecked};

use crate::rt;

/// `std::hint::spin_loop`.
#[track_caller]
pub fn spin_loop() {
    if rt::yield_now(Location::caller()).is_none() {
        std::hint::spin_loop();
    }
}
