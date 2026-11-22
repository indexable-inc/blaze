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
use blaze_own::{SharedAlloc, Vec};
use bstd::clock::Clock;
use bstd::cx::Cx;
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
    /// One a task of the last phase: in the request's arena (engine_phase_mem.rs), as the pool
    /// lives to the request's end.
    times: Vec<u64, SharedAlloc<'c>>,
    /// What each task of the last phase used on its worker (D169): beside `times`, by task; a
    /// task whose two snapshots were refused (it moved threads between them) says why.
    used: Vec<Result<blaze_meter::Used, blaze_meter::Unpaired>, SharedAlloc<'c>>,
    /// Every task's use over every phase this pool ran, all on its workers' threads, none on
    /// the command's: what the profile's reconciliation counts beside the command's phases.
    used_all: blaze_meter::Spread,
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
    /// The request's arena's handle (engine_phase_mem.rs `RequestMem::shared`): the tasks'
    /// times.
    pub(crate) alloc: SharedAlloc<'c>,
}

impl<'c> Workers<'c> {
    pub(crate) fn new(opts: WorkersOpts<'c>) -> Workers<'c> {
        Workers {
            cx: opts.cx,
            workers: opts.workers,
            clock: opts.clock,
            failed: None,
            times: Vec::new_in(opts.alloc),
            used: Vec::new_in(opts.alloc),
            used_all: blaze_meter::Spread::NONE,
            board: opts.board,
        }
    }

    /// Each task's time on its worker in the last [`Pool::run`], in the order of its tasks.
    pub(crate) fn times(&self) -> &[u64] {
        &self.times
    }

    /// What each task used on its worker in the last [`Pool::run`], in the order of its tasks
    /// (D169): beside [`Workers::times`].
    pub(crate) fn used(&self) -> &[Result<blaze_meter::Used, blaze_meter::Unpaired>] {
        &self.used
    }

    /// Every task's use over every [`Pool::run`] so far: its sum, each counter's most, how many.
    pub(crate) fn used_all(&self) -> blaze_meter::Spread {
        self.used_all
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
        self.used.clear();
        if tasks.is_empty() || buffers.is_empty() || self.failed.is_some() {
            return;
        }
        let cx = self.cx;
        let per = tasks.len().div_ceil(buffers.len());
        self.times.resize(tasks.len(), 0);
        self.used.resize(tasks.len(), Ok(blaze_meter::Used::ZERO));
        let scope = cx.scope();
        scope.narrow_workers(self.workers, Class::Foreground);
        let frame: &Frame = cx.frame();
        let mut runs = Vec::new_in(frame);
        for (((run, buffer), times), used) in tasks.chunks(per).zip(buffers.iter_mut()).zip(self.times.chunks_mut(per)).zip(self.used.chunks_mut(per)) {
            let clock = self.clock.clone();
            let board = self.board;
            // PHASE-MEM-DEBT(Outlines): a run's tasks share its frame, not reset per task (a
            // frame resets by `&mut`, and `cx.frame()` is shared): the Outlines work writes
            // nothing to it today (engine_tasks.rs `Tasks::work` publishes into the buffer's
            // allocator), so it holds nothing; a task that scratches must get its own frame.
            runs.push(scope.may_overlap("engine run", TASK_FRAME, move |cx: &Cx| {
                for ((task, time), used) in run.iter().zip(times.iter_mut()).zip(used.iter_mut()) {
                    // Not run once a stop is asked: the driver stops before the merge.
                    let Some(running) = board.start(Stage::Outlines, || Label::debug(&task.planned.name), cx.frame().used() as u64, board.now(&clock)) else { break };
                    // The meter's two snapshots bracket the clock's two reads: the task's
                    // `Used` is its work and its two clock reads, on this worker (D169).
                    let metered = blaze_meter::Meter::now();
                    let began = clock.now();
                    work(cx, task, buffer);
                    *time = clock.now().since(began).map_or(0, crate::engine_times::ns);
                    *used = metered.used();
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
        // A task not run (a stop) left `Used::ZERO`: counted as nothing used, its time 0 too.
        // A refused pair is counted as refused beside the stretches, never as a zero one.
        for used in self.used.iter() {
            self.used_all.take_since(used);
        }
        self.failed = failed;
    }
}
