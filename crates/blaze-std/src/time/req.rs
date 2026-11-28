//! Zero-alloc typed linear timer requests (`blaze_core::request::{Sleep, SleepOpts}`).

#![blaze::sans_io]

use core::time::Duration;

pub use blaze_core::request::{Sleep, SleepOpts};

/// Constructs a typed linear [`Sleep`] request lasting `after` on the driver's monotonic or
/// virtual clock.
#[inline]
pub fn sleep(after: Duration) -> Sleep {
    Sleep::new(SleepOpts { after })
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use blaze_core::{
        Driver,
        handle::{ExitKind, TaskId},
        request::SimHandler,
        sim::{Faults, Sim},
    };
    use blaze_own::{testing::heap, with_brand};

    use super::sleep;
    use crate::{
        io::{Io, Limits, SimWorld},
        mem::TASK_FRAME,
    };

    #[test]
    fn cx_run_sleep_over_sim_handler_advances_time_and_reclaims_cleanly() {
        let h = heap();
        let io = Io::simulated(SimWorld::new(51, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");
        let mut sim = Sim::new(51, Faults::default());

        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut sim, TaskId(1), brand, h.alloc());
            let before = handler.driver().now();

            let res = cx.run("bstd.time.sleep", &mut handler, sleep(Duration::from_millis(25)));
            assert_eq!(res, Ok(()));

            let after = handler.driver().now();
            assert!(
                after.saturating_sub(before) >= Duration::from_millis(25),
                "expected simulated clock to advance by at least 25ms (before={before:?}, after={after:?})"
            );
            assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
        });
    }
}
