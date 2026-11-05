//! The waits of a target with no kernel waits of its own beside a sleep (WASI, the seed of
//! docs/future/bootstrap.md).

use std::{convert::Infallible, io};

use super::{Clock, Deadline, Once, WaitError, Waited, until};

/// Sleeps until `deadline` on `clock`, each call a sleep of what is left; one that ends before
/// the deadline on `clock` sleeps again for what is left.
pub fn sleep_until(
    what: &'static str,
    deadline: Deadline,
    clock: &impl Clock,
) -> Result<(), WaitError> {
    let waited = until(what, clock, Some(deadline), |left| -> io::Result<Once<Infallible>> {
        sleep(left.unwrap_or_default());
        Ok(Once::TimedOut)
    })?;
    match waited {
        Waited::Ready(never) => match never {},
        Waited::Deadline => Ok(()),
    }
}

/// One sleep: WASI's one wait is a sleep on its clock.
#[expect(
    clippy::disallowed_methods,
    reason = "the one sleep of `wait::sleep_until`, its length recomputed from the deadline"
)]
fn sleep(left: std::time::Duration) {
    std::thread::sleep(left);
}
