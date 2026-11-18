//! The engine's pool on bstd's workers (engine.md E6, `blaze engine-check`): a phase's tasks cut
//! into one run per buffer, each run a task of one bstd scope, so the pool's work stealing
//! balances the runs and each worker owns the buffer of the run it is in. A run's tasks share its
//! frame (`cx.frame()`, `TASK_FRAME`), which ends with the run.
//!
//! A scope or a run that fails (bstd's `TaskError`: a cancel, a deadline, a frame exhausted) is
//! kept and named after the phase ([`Workers::failed`]): `Pool::run` returns nothing, and the
//! merge then stops on the results it lacks before the command reads the failure.

use core::convert::Infallible;
use core::num::NonZeroU32;

use blaze_engine::{Buffer, Pool, Stage, Task};
use blaze_own::Vec;
use bstd::clock::Clock;
use bstd::cx::Cx;
use std::alloc::Global;
use bstd::mem::{Frame, TASK_FRAME};
use bstd::task::{Class, TaskError};

use crate::engine_status::Label;

/// The pool: the command's context, how many workers it may use, the first failure, and each
/// task's time of the last phase it ran.
pub(crate) struct Workers<'c> {
    cx: &'c Cx,
    workers: NonZeroU32,
    clock: &'c Clock,
    failed: Option<TaskError>,
    times: std::vec::Vec<u64, Global>,
    board: &'c crate::engine_status::Board<'c>,
}

/// What a [`Workers`] is made from.
pub(crate) struct WorkersOpts<'c> {
    /// The command's context: its scopes are the phases'.
    pub(crate) cx: &'c Cx,
    /// What each task is timed by on its worker (engine_times.rs).
    pub(crate) clock: &'c Clock,
    /// At most this many runs at once (`--workers N`, else one per core).
    pub(crate) workers: NonZeroU32,
    /// Where each task says it runs (engine_status.rs): its start is the workers' scheduling
    /// point, refused once a stop is asked. The pool runs the `Outlines` phases alone.
    pub(crate) board: &'c crate::engine_status::Board<'c>,
}

impl<'c> Workers<'c> {
    pub(crate) fn new(opts: WorkersOpts<'c>) -> Workers<'c> {
        Workers { cx: opts.cx, workers: opts.workers, clock: opts.clock, failed: None, times: std::vec::Vec::new_in(Global), board: opts.board }
    }

    /// Each task's time on its worker in the last [`Pool::run`], in the order of its tasks.
    pub(crate) fn times(&self) -> &[u64] {
        &self.times
    }

    /// How many buffers a phase is cut into: four runs a worker, so a long run is balanced by
    /// the others being stolen.
    pub(crate) fn runs(&self) -> usize {
        usize::try_from(self.workers.get()).unwrap_or(usize::MAX).saturating_mul(4)
    }

    /// The first failure of any phase's scope or run, taken: a caller that drives its own phases
    /// asks after each.
    pub(crate) fn failed(&mut self) -> Result<(), TaskError> {
        match self.failed.take() {
            None => Ok(()),
            Some(error) => Err(error),
        }
    }
}

impl<'a> Pool<'a> for Workers<'_> {
    type Cx = Cx;

    fn run(&mut self, tasks: &[Task], buffers: &mut [Buffer<'a>], work: &(dyn Fn(&Cx, &Task, &mut Buffer<'a>) + Sync)) {
        self.times.clear();
        if tasks.is_empty() || buffers.is_empty() || self.failed.is_some() {
            return;
        }
        let cx = self.cx;
        let per = tasks.len().div_ceil(buffers.len());
        self.times.resize(tasks.len(), 0);
        let scope = cx.scope();
        scope.narrow_workers(self.workers, Class::Foreground);
        let frame: &Frame = cx.frame();
        let mut runs = Vec::new_in(frame);
        for ((run, buffer), times) in tasks.chunks(per).zip(buffers.iter_mut()).zip(self.times.chunks_mut(per)) {
            let clock = self.clock.clone();
            let board = self.board;
            runs.push(scope.may_overlap("engine run", TASK_FRAME, move |cx: &Cx| {
                for (task, time) in run.iter().zip(times.iter_mut()) {
                    // Not run once a stop is asked: the driver stops before the merge.
                    let Some(running) = board.start(Stage::Outlines, || Label::debug(&task.planned.name), cx.frame().used() as u64, board.now(&clock)) else { break };
                    let began = clock.now();
                    work(cx, task, buffer);
                    *time = clock.now().since(began).map_or(0, crate::engine_times::ns);
                    running.end(board.now(&clock));
                }
                Ok::<(), Infallible>(())
            }));
        }
        let mut failed = None;
        for run in runs {
            if let Err(error) = run.join(cx) {
                failed.get_or_insert(error);
            }
        }
        if let Err(error) = scope.join(cx) {
            failed.get_or_insert(error);
        }
        self.failed = failed;
    }
}
