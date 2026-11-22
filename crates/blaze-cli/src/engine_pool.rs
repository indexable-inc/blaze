//! The engine's pool on bstd's workers (engine.md E6, `blaze engine-check`): a phase's tasks cut
//! into chunks by a [`Schedule`], claimed in order from one shared cursor by claimers that each
//! run as a task of one bstd scope and each own one buffer; after the phase the buffers' results
//! are put back in task order ([`in_chunk_order`]), so the merge reads the same list whichever
//! claimer ran which chunk (rule 25). A claimer's tasks share its frame (`cx.frame()`,
//! `TASK_FRAME`), which ends with the claimer.
//!
//! The schedules (rule 78: one seam, each tested equal, `tests/engine_pool_tests.rs`):
//!
//! - [`Static`], today's: four equal chunks a worker (`len / (4 * workers)` tasks each), one
//!   claimer a chunk, the pool's stealing balancing the claimers.
//! - [`Guided`]: guided self-scheduling, each chunk a `1 / (4 * workers)` part of the tasks still
//!   unclaimed, down to one task, claimed by one claimer a worker. A long task in an early chunk
//!   no longer holds the tasks queued behind it in its chunk: the others take the rest one small
//!   chunk at a time (the outside review, thoughts.md 2.3: a skewed phase left N-1 workers idle).
//!
//! Which one runs is [`Chosen`]: the one line that switches the command.
//!
//! What each phase cost is in [`Balance`] (rule 16, rule 67): the wall, its floor (the longer of
//! the busy time spread over the workers and the longest task, below which no schedule that does
//! not split a task goes), their ratio, the idle worker time and the tail from the first claimer
//! out of work to the phase's end.
//!
//! A scope or a claimer that fails (bstd's `TaskError`: a cancel, a deadline, a frame exhausted)
//! is kept and named after the phase ([`Workers::failed`]): `Pool::run` returns nothing, and the
//! merge then stops on the results it lacks before the command reads the failure.

use core::alloc::Allocator;
use core::convert::Infallible;
use core::fmt;
use core::marker::PhantomData;
use core::num::NonZeroU32;
use core::ops::Range;
use core::sync::atomic::{AtomicUsize, Ordering};

use blaze_engine::{Buffer, Pool, Stage, Task};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{SharedAlloc, Vec};
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::mem::{Frame, TASK_FRAME};
use bstd::task::{Class, TaskError};

use crate::engine_status::Label;

#[cfg(test)]
#[path = "tests/engine_pool_tests.rs"]
mod tests;

/// The schedule the command runs: the one line that switches it.
pub(crate) type Chosen = Guided;

/// How a phase's tasks are cut into chunks and how many claimers take them (rule 78).
pub(crate) trait Schedule: Copy + Default + Send + Sync + 'static {
    /// What the profile calls it.
    const NAME: &'static str;

    /// The chunks of `len` tasks on `workers` workers and `buffers` buffers: each chunk's first
    /// task, ascending, then `len`, into `starts` (cleared first). A pure function of its
    /// arguments, so a plan is the same on every run of one phase.
    fn plan<A: Allocator>(len: usize, workers: usize, buffers: usize, starts: &mut Vec<usize, A>);

    /// How many claimers take `chunks` chunks: at most `buffers`, one buffer each.
    fn claimers(workers: usize, buffers: usize, chunks: usize) -> usize;
}

/// Today's: `buffers` equal chunks, one claimer each.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Static;

impl Schedule for Static {
    const NAME: &'static str = "static";

    fn plan<A: Allocator>(len: usize, _workers: usize, buffers: usize, starts: &mut Vec<usize, A>) {
        starts.clear();
        let per = len.div_ceil(buffers.max(1)).max(1);
        let mut at = 0;
        while at < len {
            starts.push(at);
            at = at.saturating_add(per).min(len);
        }
        starts.push(len);
    }

    fn claimers(_workers: usize, buffers: usize, chunks: usize) -> usize {
        chunks.min(buffers)
    }
}

/// Guided self-scheduling: each chunk `ceil(left / (4 * workers))` of the tasks left, so the
/// first is a static chunk's size and the last `4 * workers` are one task each; one claimer a
/// worker. About `4 * workers * (1 + ln(len / (4 * workers)))` chunks, one cursor add each.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Guided;

impl Schedule for Guided {
    const NAME: &'static str = "guided";

    fn plan<A: Allocator>(len: usize, workers: usize, _buffers: usize, starts: &mut Vec<usize, A>) {
        starts.clear();
        let parts = workers.max(1).saturating_mul(4);
        let mut at = 0;
        while at < len {
            starts.push(at);
            at += (len - at).div_ceil(parts).max(1);
        }
        starts.push(len);
    }

    fn claimers(workers: usize, buffers: usize, chunks: usize) -> usize {
        workers.max(1).min(buffers).min(chunks)
    }
}

/// The chunks of one phase and the cursor its claimers take them from, in order: the sans-IO
/// core of the pool's scheduling (its simulation is `tests/engine_pool_tests.rs`).
pub(crate) struct Claims<'p> {
    starts: &'p [usize],
    next: AtomicUsize,
}

impl<'p> Claims<'p> {
    /// The chunks a [`Schedule::plan`] wrote.
    pub(crate) fn new(starts: &'p [usize]) -> Claims<'p> {
        Claims { starts, next: AtomicUsize::new(0) }
    }

    pub(crate) fn chunks(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// The next chunk and its tasks, or `None` once all are claimed. Each chunk is claimed once;
    /// one claimer's chunks ascend.
    pub(crate) fn claim(&self) -> Option<(usize, Range<usize>)> {
        // Relaxed: a claim orders nothing; what a claimer wrote is the scope's join's to order.
        // A claimer stops at its first `None`, so the cursor passes the chunks by at most the
        // claimers' count.
        let at = self.next.fetch_add(1, Ordering::Relaxed);
        (at < self.chunks()).then(|| (at, self.starts[at]..self.starts[at + 1]))
    }
}

/// What one claimer made of one chunk: who claimed it, how many of its tasks ran (all, unless a
/// stop came), and how many results of each list it added to its buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    pub(crate) claimer: usize,
    pub(crate) ran: usize,
    pub(crate) done: usize,
    pub(crate) made: usize,
    pub(crate) owed: usize,
}

impl Piece {
    /// A chunk no claimer took (a stop came first): it added nothing.
    pub(crate) const UNCLAIMED: Piece = Piece { claimer: usize::MAX, ran: 0, done: 0, made: 0, owed: 0 };
}

/// The bugs a control plants in [`in_chunk_order`] (`tests/engine_pool_tests.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PoolPlant {
    /// The code as it is.
    #[cfg_attr(not(test), allow(dead_code, reason = "the clean run of tests/engine_pool_tests.rs's control"))]
    None,
    /// The claimers' buffers left as they are, in claimer order: the merge's input then depends
    /// on which claimer ran which chunk.
    Concatenated,
}

impl Plants for PoolPlant {}

/// The claimers' lists (`parts`, each its chunks' items in the order it claimed them) as one
/// list in chunk order, which is task order: left in `parts[0]`, the others emptied. `pieces` is
/// one a chunk, in chunk order, `count` its items in this list. Nothing moves when the claimers'
/// lists are in chunk order already (each claimer's chunks after the one before's).
pub(crate) fn in_chunk_order<T, A: Allocator + Copy, F: Allocator>(
    parts: &mut [&mut Vec<T, A>],
    pieces: &[Piece],
    count: impl Fn(&Piece) -> usize,
    scratch: F,
    plant: impl Planted<PoolPlant>,
) {
    let Some(first) = parts.first() else { return };
    let mut last = 0;
    let in_order = pieces.iter().filter(|piece| piece.claimer != usize::MAX).all(|piece| {
        let after = piece.claimer >= last;
        last = piece.claimer;
        after
    });
    if in_order || plant.has(PoolPlant::Concatenated) {
        return;
    }
    let alloc = *first.allocator();
    let total = parts.iter().map(|part| part.len()).sum();
    // alloc: the phase's results once more, in the buffers' own allocator (the request's), for
    // the merge that reads them next.
    let mut out = Vec::with_capacity_in(total, alloc);
    let mut taken = Vec::with_capacity_in(parts.len(), scratch);
    for part in parts.iter_mut() {
        taken.push(core::mem::replace(&mut **part, Vec::new_in(alloc)).into_iter());
    }
    for piece in pieces.iter().filter(|piece| piece.claimer != usize::MAX) {
        out.extend(taken[piece.claimer].by_ref().take(count(piece)));
    }
    drop(taken);
    *parts[0] = out;
}

/// One claimer's account of a phase, reused phase to phase: in the request's arena.
struct Log<'c> {
    /// Each chunk it claimed and what it made of it, in claim order.
    pieces: Vec<(usize, Piece), SharedAlloc<'c>>,
    /// Each task it ran, in order: its time and what it used on the worker (D169).
    ran: Vec<(u64, Result<blaze_meter::Used, blaze_meter::Unpaired>), SharedAlloc<'c>>,
    /// Its tasks' times summed, and the longest.
    busy: u64,
    longest: u64,
    /// When it found nothing left, from the phase's start.
    ended: u64,
}

impl<'c> Log<'c> {
    fn new_in(alloc: SharedAlloc<'c>) -> Log<'c> {
        Log { pieces: Vec::new_in(alloc), ran: Vec::new_in(alloc), busy: 0, longest: 0, ended: 0 }
    }

    fn clear(&mut self) {
        self.pieces.clear();
        self.ran.clear();
        self.busy = 0;
        self.longest = 0;
        self.ended = 0;
    }
}

/// How well the pool kept its workers busy, summed over the phases it ran (rule 16, rule 67).
/// Every number is of this run and machine: the profile's, never a key's (rule 25).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Balance {
    pub(crate) phases: u64,
    pub(crate) tasks: u64,
    pub(crate) chunks: u64,
    /// The most claimers that ran at once in any phase: the workers, or fewer claimers.
    pub(crate) workers: u64,
    pub(crate) wall_ns: u64,
    pub(crate) busy_ns: u64,
    /// Each phase's floor: the longer of its busy time over its workers and its longest task.
    pub(crate) floor_ns: u64,
    /// Worker time inside the phases' walls not in a task: `workers * wall - busy`.
    pub(crate) idle_ns: u64,
    /// From the first claimer out of work to the phase's end, summed.
    pub(crate) tail_ns: u64,
    pub(crate) longest_ns: u64,
}

/// One phase's numbers, for [`Balance::add`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PhaseBalance {
    pub(crate) tasks: u64,
    pub(crate) chunks: u64,
    pub(crate) workers: u64,
    pub(crate) wall_ns: u64,
    pub(crate) busy_ns: u64,
    pub(crate) longest_ns: u64,
    /// When the first claimer found nothing left, from the phase's start.
    pub(crate) first_out_ns: u64,
}

impl Balance {
    pub(crate) fn add(&mut self, phase: PhaseBalance) {
        let workers = phase.workers.max(1);
        self.phases += 1;
        self.tasks = self.tasks.saturating_add(phase.tasks);
        self.chunks = self.chunks.saturating_add(phase.chunks);
        self.workers = self.workers.max(phase.workers);
        self.wall_ns = self.wall_ns.saturating_add(phase.wall_ns);
        self.busy_ns = self.busy_ns.saturating_add(phase.busy_ns);
        self.floor_ns = self.floor_ns.saturating_add(phase.busy_ns.div_ceil(workers).max(phase.longest_ns));
        self.idle_ns = self.idle_ns.saturating_add(workers.saturating_mul(phase.wall_ns).saturating_sub(phase.busy_ns));
        self.tail_ns = self.tail_ns.saturating_add(phase.wall_ns.saturating_sub(phase.first_out_ns));
        self.longest_ns = self.longest_ns.max(phase.longest_ns);
    }

    /// The profile's line: `pool (Outlines, guided): ...`.
    pub(crate) fn line(&self, schedule: &str, into: &mut impl fmt::Write) -> fmt::Result {
        let ms = |ns: u64| ns / 1_000_000;
        let ratio = match self.floor_ns {
            0 => 0,
            floor => self.wall_ns.saturating_mul(100) / floor,
        };
        writeln!(
            into,
            "pool (Outlines, {schedule}): {} phases, {} tasks in {} chunks on at most {} workers · wall {} ms against a floor of {} ms (the longer of the busy time over the workers and the longest task), ratio {}.{:02} · idle worker time {} ms · tail {} ms (the first worker out of work to the phase's end) · longest task {} ms",
            self.phases,
            self.tasks,
            self.chunks,
            self.workers,
            ms(self.wall_ns),
            ms(self.floor_ns),
            ratio / 100,
            ratio % 100,
            ms(self.idle_ns),
            ms(self.tail_ns),
            ms(self.longest_ns),
        )
    }
}

/// The pool: the command's context, how many workers it may use, the first failure, and each
/// task's time of the last phase it ran.
pub(crate) struct Workers<'c, S: Schedule = Chosen> {
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
    /// The request's arena: the claimers' logs.
    alloc: SharedAlloc<'c>,
    /// One a claimer, reused phase to phase.
    logs: Vec<Log<'c>, SharedAlloc<'c>>,
    balance: Balance,
    schedule: PhantomData<S>,
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

impl<'c> Workers<'c, Chosen> {
    /// The command's pool, on the [`Chosen`] schedule.
    pub(crate) fn new(opts: WorkersOpts<'c>) -> Workers<'c, Chosen> {
        Workers::scheduled(opts, Chosen::default())
    }
}

impl<'c, S: Schedule> Workers<'c, S> {
    /// A pool on the schedule `S` (its value says which; it holds nothing).
    pub(crate) fn scheduled(opts: WorkersOpts<'c>, _schedule: S) -> Workers<'c, S> {
        Workers {
            cx: opts.cx,
            workers: opts.workers,
            clock: opts.clock,
            failed: None,
            times: Vec::new_in(opts.alloc),
            used: Vec::new_in(opts.alloc),
            used_all: blaze_meter::Spread::NONE,
            board: opts.board,
            alloc: opts.alloc,
            logs: Vec::new_in(opts.alloc),
            balance: Balance::default(),
            schedule: PhantomData,
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

    /// How well the phases so far kept the workers busy.
    #[allow(dead_code, reason = "the profile's pool line (engine_check.rs `checking`, the coordinator's switch)")]
    pub(crate) fn balance(&self) -> Balance {
        self.balance
    }

    /// The profile's pool line, for the phases so far.
    #[allow(dead_code, reason = "the profile's pool line (engine_check.rs `checking`, the coordinator's switch)")]
    pub(crate) fn balance_line(&self, into: &mut impl fmt::Write) -> fmt::Result {
        self.balance.line(S::NAME, into)
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

/// The claimers' buffers' results in task order (each list by [`in_chunk_order`]).
fn regroup(buffers: &mut [Buffer<'_>], pieces: &[Piece], frame: &Frame) {
    let mut done = Vec::with_capacity_in(buffers.len(), frame);
    for buffer in buffers.iter_mut() {
        done.push(&mut buffer.done);
    }
    in_chunk_order(&mut done, pieces, |piece| piece.done, frame, NoPlant);
    drop(done);
    let mut made = Vec::with_capacity_in(buffers.len(), frame);
    for buffer in buffers.iter_mut() {
        made.push(&mut buffer.made);
    }
    in_chunk_order(&mut made, pieces, |piece| piece.made, frame, NoPlant);
    drop(made);
    let mut owed = Vec::with_capacity_in(buffers.len(), frame);
    for buffer in buffers.iter_mut() {
        owed.push(&mut buffer.owed);
    }
    in_chunk_order(&mut owed, pieces, |piece| piece.owed, frame, NoPlant);
}

impl<'a, S: Schedule> Pool<'a> for Workers<'_, S> {
    type Cx = Cx;

    fn run(&mut self, tasks: &[Task], buffers: &mut [Buffer<'a>], work: &(dyn Fn(&Cx, &Task, &mut Buffer<'a>) + Sync)) {
        self.times.clear();
        self.used.clear();
        if tasks.is_empty() || buffers.is_empty() || self.failed.is_some() {
            return;
        }
        let cx = self.cx;
        let frame: &Frame = cx.frame();
        let workers = usize::try_from(self.workers.get()).unwrap_or(usize::MAX);
        let mut starts = Vec::new_in(frame);
        S::plan(tasks.len(), workers, buffers.len(), &mut starts);
        let claims = Claims::new(&starts);
        let claimers = S::claimers(workers, buffers.len(), claims.chunks());
        self.times.resize(tasks.len(), 0);
        self.used.resize(tasks.len(), Ok(blaze_meter::Used::ZERO));
        while self.logs.len() < claimers {
            self.logs.push(Log::new_in(self.alloc));
        }
        for log in self.logs.iter_mut() {
            log.clear();
        }
        let began = self.clock.now();
        let scope = cx.scope();
        scope.narrow_workers(self.workers, Class::Foreground);
        let mut runs = Vec::new_in(frame);
        for (buffer, log) in buffers.iter_mut().zip(self.logs.iter_mut()).take(claimers) {
            let clock = self.clock.clone();
            let board = self.board;
            let claims = &claims;
            // PHASE-MEM-DEBT(Outlines): a claimer's tasks share its frame, not reset per task (a
            // frame resets by `&mut`, and `cx.frame()` is shared): the Outlines work writes
            // nothing to it today (engine_tasks.rs `Tasks::work` publishes into the buffer's
            // allocator), so it holds nothing; a task that scratches must get its own frame.
            runs.push(scope.may_overlap("engine run", TASK_FRAME, move |cx: &Cx| {
                'claims: while let Some((chunk, range)) = claims.claim() {
                    let (done, made, owed) = (buffer.done.len(), buffer.made.len(), buffer.owed.len());
                    let mut ran = 0;
                    let mut stopped = false;
                    for task in &tasks[range] {
                        // Not run once a stop is asked: the driver stops before the merge.
                        let Some(running) = board.start(Stage::Outlines, || Label::debug(&task.planned.name), cx.frame().used() as u64, board.now(&clock)) else {
                            stopped = true;
                            break;
                        };
                        // The meter's two snapshots bracket the clock's two reads: the task's
                        // `Used` is its work and its two clock reads, on this worker (D169).
                        let metered = blaze_meter::Meter::now();
                        let task_began = clock.now();
                        work(cx, task, buffer);
                        let time = clock.now().since(task_began).map_or(0, crate::engine_times::ns);
                        log.ran.push((time, metered.used()));
                        log.busy = log.busy.saturating_add(time);
                        log.longest = log.longest.max(time);
                        ran += 1;
                        running.end(board.now(&clock));
                    }
                    let piece = Piece { claimer: 0, ran, done: buffer.done.len() - done, made: buffer.made.len() - made, owed: buffer.owed.len() - owed };
                    log.pieces.push((chunk, piece));
                    if stopped {
                        break 'claims;
                    }
                }
                log.ended = clock.now().since(began).map_or(0, crate::engine_times::ns);
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
        let wall = self.clock.now().since(began).map_or(0, crate::engine_times::ns);
        // Each chunk's piece, and each task's time and use where it ran.
        let mut pieces = Vec::new_in(frame);
        pieces.resize(claims.chunks(), Piece::UNCLAIMED);
        let mut phase = PhaseBalance { tasks: tasks.len() as u64, chunks: claims.chunks() as u64, workers: claimers.min(workers) as u64, wall_ns: wall, first_out_ns: wall, ..PhaseBalance::default() };
        for (claimer, log) in self.logs.iter().enumerate().take(claimers) {
            let mut at = 0;
            for &(chunk, piece) in log.pieces.iter() {
                pieces[chunk] = Piece { claimer, ..piece };
                let first = starts[chunk];
                for (offset, &(time, used)) in log.ran[at..at + piece.ran].iter().enumerate() {
                    self.times[first + offset] = time;
                    self.used[first + offset] = used;
                }
                at += piece.ran;
            }
            phase.busy_ns = phase.busy_ns.saturating_add(log.busy);
            phase.longest_ns = phase.longest_ns.max(log.longest);
            phase.first_out_ns = phase.first_out_ns.min(log.ended);
        }
        // The results in task order, whichever claimer ran which chunk (rule 25).
        regroup(&mut buffers[..claimers], &pieces, frame);
        self.balance.add(phase);
        // A task not run (a stop) left `Used::ZERO`: counted as nothing used, its time 0 too.
        // A refused pair is counted as refused beside the stretches, never as a zero one.
        for used in self.used.iter() {
            self.used_all.take_since(used);
        }
        self.failed = failed;
    }
}
