//! A test's native world (`bstd::test::native`, plan port27/bstd-capabilities.md P3): made with
//! a test's root of real-OS authority, `blaze_core::testing::os()`, so a native test needs
//! neither `Io::native` (which reaches the process's root itself and goes in batch B1) nor
//! `Os::process()` (the program's entry's). Behind the `testing` feature (and bstd's own tests).

use core::{num::NonZeroU32, panic::AssertUnwindSafe, time::Duration};

use blaze_core::wait::Deadline;

use crate::{
    cx::Cx,
    io::{Io, Limits, NativeWorld, OpenError},
    mem::{Frame, TASK_FRAME, Tag},
    task::threads::{Plants, Topology},
};

use super::{RootPath, catch_unwind, pool, resume_unwind};

impl Io {
    /// Runs a test in this world on a flat pool of `workers` threads. On return or unwind,
    /// the pool has five seconds on its own monotonic clock to stop.
    pub fn with_test_workers<R>(&self, workers: NonZeroU32, run: impl FnOnce(&Cx) -> R) -> R {
        let frame = Frame::with_reserve(TASK_FRAME, Tag::Frames).expect("reserve the test pool's root frame");
        let deadline = Deadline::after(self.now(), crate::test::BUDGET);
        let running = pool::start(&Topology::flat(workers.get()), Plants::NONE).expect("start the test worker pool");
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let cx = Cx::root_on(deadline, frame, self.clone(), running.pool().clone());
            run(&cx)
        }));
        if let Err(hung) = running.stop(Duration::from_secs(5)) {
            panic!("the test worker pool did not stop: {hung}");
        }
        match outcome {
            Ok(value) => value,
            Err(panic) => resume_unwind(panic),
        }
    }
}

#[cfg(all(test, not(blaze_check)))]
#[test]
fn test_workers_exit_before_the_callback_panic_resumes() {
    let io = Io::simulated(crate::io::SimWorld::new(17, Limits::USUAL));
    let workers = NonZeroU32::new(2).expect("two workers");
    let mut observed = None;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        io.with_test_workers(workers, |cx| {
            observed = cx.pool().cloned();
            panic!("fixture callback");
        });
    }));
    let payload = outcome.expect_err("the callback panic must reach its caller");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"fixture callback"));
    let pool = observed.expect("the callback ran on a pool");
    for at in 0..workers.get() {
        assert!(pool.exited(at), "worker {at} was still running when the panic resumed");
    }
}

/// A native world over `dir`: one file thread, an empty environment and command line, the
/// host's name empty, the seed of `Io::rng` fixed at 0 (a test says its seed), no folder above
/// `dir`, and [`Limits::USUAL`]. `dir` must exist; the driver's refusal is the error, naming it.
pub fn native(dir: &RootPath) -> Result<Io, OpenError> {
    Io::native_in(
        blaze_core::testing::os(),
        NativeWorld {
            root: dir,
            threads: 1,
            // alloc: none: an empty environment and command line.
            env: crate::env::Vars::given(Default::default()),
            args: crate::args::ArgList::given(Default::default()),
            // alloc: once per test world, the host's name (empty).
            host_name: "".into(),
            seed: Some(0),
            above: 0,
            limits: Limits::USUAL,
        },
    )
}
