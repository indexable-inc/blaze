//! Where an expansion's instance runs: on a thread of its own, never a fiber's
//! (no-wait-across-thread-locals). A Wasmtime activation lives in its thread's locals, and a
//! job on a fiber can resume on another thread after any wait, so a fiber must never be inside
//! a wasm call when it waits. The job hands the expansion to an expansion thread and waits on
//! an [`Event`], holding no thread. What only the database can say of a span (its text, line,
//! parent or edition) the expansion thread asks back over the [`Link`], blocking itself, and
//! the job answers it outside any wasm call, where its query may wait and move threads as any
//! query does.
//!
//! An expansion thread takes nothing borrowed: the expansion is given what it owns (its
//! server, a copy of the calling crate's variables, its input's bytes) and gives back what it
//! made. Threads are kept for the next expansion, at most [`cap`] of them: an expansion handed
//! in while that many run waits, holding no thread, for the first to end. The waiting job
//! parked on its [`Event`] frees its worker, so the pool starts more jobs and more expansions:
//! with no cap, the threads grew with every crate expanded at once, the many programs of
//! `blaze test --workspace` together past macOS's per-process thread limit. The one expansion
//! never queued is one asked for while its job answers another's question ([`answering`]): the
//! thread of that other expansion waits for the answer, so making it wait for a thread could
//! leave every thread of the cap waiting on the queue. Such a nested expansion gets a thread
//! past the cap, which ends with it, up to a [`ceiling`] of [`NESTING`] times the cap: past
//! that, the expansion is refused with an error naming the counts (bounded-progress), never a
//! thread more, so no chain of expansions can take the process to its thread limit.

use std::{
    cell::Cell,
    collections::VecDeque,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, mpsc},
    thread,
};

use base_db::Db;
use blaze_graph::sched::Event;
use blaze_lower::{SubRequest, SubResponse};
use blaze_core::lock::{Lock, Signal};
use span::{Edition, SyntaxContext};

use crate::{guest::Stop, server::Answer};

/// An expansion thread's stack: past Wasmtime's 4 MiB for wasm (`engine.rs`), with the host's
/// shims and the server under it.
const STACK: usize = 16 << 20;

enum Question {
    Span(SubRequest),
    Edition(SyntaxContext),
}

enum Reply {
    Span(SubResponse),
    Edition(Edition),
}

/// What the job and its expansion thread pass each other.
struct Board<T> {
    question: Option<Question>,
    reply: Option<Reply>,
    /// What the expansion gave, or why its thread panicked.
    done: Option<Result<T, Stop>>,
    /// The job unwound while it waited: no reply comes.
    gone: bool,
    /// The job's wait, which the expansion thread ends once it posts a question or is done.
    wake: Option<Event>,
}

/// One expansion's two sides. The expansion thread asks through it as its [`Answer`].
pub(crate) struct Link<T> {
    board: Lock<Board<T>>,
    replied: Signal,
}

impl<T> Link<T> {
    fn new() -> Link<T> {
        Link {
            board: Lock::new("macro.thread.board", Board {
                question: None,
                reply: None,
                done: None,
                gone: false,
                wake: None,
            }),
            replied: Signal::new("macro.thread.replied"),
        }
    }

    /// Posts on the board, ending the job's wait.
    fn post(&self, post: impl FnOnce(&mut Board<T>)) {
        let mut board = self.board.lock();
        post(&mut board);
        let wake = board.wake.take();
        drop(board);
        if let Some(wake) = wake {
            wake.fire();
        }
    }

    /// The job's reply to `question`, the expansion thread blocked until it comes.
    fn ask(&self, question: Question) -> Result<Reply, Stop> {
        self.post(|board| board.question = Some(question));
        let mut board = self.board.lock();
        // wakes: the job's reply and the link's end (`gone`) each notify `replied`
        self.replied.wait_while(&mut board, |it| it.reply.is_none() && !it.gone);
        board.reply.take().ok_or_else(|| {
            Stop("asked the database of a span after the job that ran it was gone".to_owned()) // alloc: error path
        })
    }
}

impl<T> Answer for Link<T> {
    fn answer(&self, req: SubRequest) -> Result<SubResponse, Stop> {
        match self.ask(Question::Span(req))? {
            Reply::Span(it) => Ok(it),
            Reply::Edition(_) => {
                Err(Stop("was told an edition when it asked of a span".to_owned()))
            } // alloc: error path
        }
    }

    fn edition(&self, ctx: SyntaxContext) -> Result<Edition, Stop> {
        match self.ask(Question::Edition(ctx))? {
            Reply::Edition(it) => Ok(it),
            Reply::Span(_) => Err(Stop("was told of a span when it asked an edition".to_owned())), // alloc: error path
        }
    }
}

/// Tells the expansion thread the job is gone if it unwinds while it waits.
struct Gone<'l, T>(&'l Link<T>);

impl<T> Drop for Gone<'_, T> {
    fn drop(&mut self) {
        self.0.board.lock().gone = true;
        self.0.replied.notify_all();
    }
}

/// Runs `expansion` on an expansion thread, answering what it asks of the database from
/// `db` on this one: what it gave, or why it could not run.
pub(crate) fn run<T: Send + 'static>(
    db: &Db,
    expansion: impl FnOnce(&Link<T>) -> T + Send + 'static,
) -> Result<T, Stop> {
    let link = Arc::new(Link::new());
    let theirs = Arc::clone(&link);
    let nested = answering() > 0;
    // The expansion's span (`proc macro expansion`, naming the macro and its input) holds the
    // thread's time too, so a profile says which macro ran on it.
    let parent = tracing::Span::current();
    submit(
        nested,
        Box::new(move || {
            let _running =
                tracing::info_span!(target: "blaze", parent: &parent, "wasm expansion run")
                    .entered();
            let done =
                panic::catch_unwind(AssertUnwindSafe(|| expansion(&theirs))).map_err(|payload| {
                    let said = payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("a payload that is not a string");
                    Stop(format!("panicked on its wasm expansion thread: {said}")) // alloc: error path
                });
            theirs.post(|board| board.done = Some(done));
        }),
    )?;
    let _gone = Gone(&link);
    loop {
        let mut board = link.board.lock();
        if let Some(done) = board.done.take() {
            return done;
        }
        if let Some(question) = board.question.take() {
            drop(board);
            let answering = Answering::enter();
            let reply = match question {
                Question::Span(req) => Reply::Span(db.answer(req)?),
                Question::Edition(ctx) => Reply::Edition(db.edition(ctx)?),
            };
            drop(answering);
            link.board.lock().reply = Some(reply);
            link.replied.notify_all();
            continue;
        }
        // Made under the board's lock, which the expansion thread lets go of before it fires
        // one: the scheduler's lock is never taken the other way round.
        let event = Event::new();
        let waiter = event.waiter();
        board.wake = Some(event);
        drop(board);
        waiter.wait().map_err(|cycle| {
            Stop(format!("waited for its wasm expansion thread in a cycle of waits: {cycle:?}")) // alloc: error path
        })?;
    }
}

type Job = Box<dyn FnOnce() + Send>; // dyn: an expansion handed to a kept thread, the edge

/// The expansion threads, and the expansions waiting for one.
struct Threads {
    /// The threads waiting for an expansion.
    idle: Vec<mpsc::Sender<Job>>,
    /// Expansions handed in while [`cap`] threads ran others, oldest first: the first thread
    /// to end its expansion takes the next.
    queued: VecDeque<Job>,
    /// The threads alive: at most [`cap`] of them, but for those started for a nested
    /// expansion, which end with it.
    started: usize,
}

static THREADS: Lock<Threads> =
    Lock::new("macro.thread.pool", Threads { idle: Vec::new(), queued: VecDeque::new(), started: 0 }); // alloc: none: empty in a const

/// The expansion threads alive now, idle ones included: the proc-macro lane's width the build
/// view shows (`proc macros 3 of 32 expanding`).
pub fn alive() -> usize {
    THREADS.lock().started
}

/// The most expansion threads kept: twice the process's workers, so every worker's job can
/// have an expansion running and one more whose job answers its questions. Fixed for the
/// process, however many crates and programs a build expands at once.
pub(crate) fn cap() -> usize {
    2 * blaze_graph::exec::shared().workers().max(1)
}

/// How many times [`cap`] the expansion threads alive may reach with nested expansions: each
/// level of expansions asked for while answering another's question takes at most a cap's
/// worth more, and no real crate nests this deep.
const NESTING: usize = 8;

/// The most expansion threads alive at once, nested ones included: past it a nested expansion
/// is refused ([`submit`]).
pub(crate) fn ceiling() -> usize {
    NESTING * cap()
}

thread_local! {
    /// How many expansions' questions the job on this thread is answering now: carried with
    /// the job across its waits ([`blaze_query::carry`]), so an expansion its answer asks for
    /// is known as nested wherever the job resumes.
    static ANSWERING: Cell<usize> = const { Cell::new(0) };
}

// Each access to `ANSWERING` is a call of its own, never inlined: an answer that waits may
// resume on another thread (no-wait-across-thread-locals).

/// How many expansions' questions this job is answering.
#[inline(never)]
fn answering() -> usize {
    carry_answering();
    ANSWERING.get()
}

#[inline(never)]
fn take_answering() -> Option<std::num::NonZeroUsize> {
    std::num::NonZeroUsize::new(ANSWERING.replace(0))
}

#[inline(never)]
fn put_answering(taken: std::num::NonZeroUsize) {
    ANSWERING.set(taken.get());
}

fn carry_answering() {
    static CARRIED: std::sync::Once = std::sync::Once::new();
    CARRIED.call_once(|| {
        blaze_query::carry(blaze_query::Carried::word(take_answering, put_answering))
    });
}

/// The job answers an expansion's question while it is held; ended on unwinding too.
struct Answering;

impl Answering {
    #[inline(never)]
    fn enter() -> Answering {
        carry_answering();
        ANSWERING.set(ANSWERING.get() + 1);
        Answering
    }
}

impl Drop for Answering {
    #[inline(never)]
    fn drop(&mut self) {
        ANSWERING.set(ANSWERING.get().saturating_sub(1));
    }
}

/// Gives `job` to an idle expansion thread, or to a new one while fewer than [`cap`] are alive
/// (or, for a `nested` one, fewer than [`ceiling`]), else queues it for the first thread to end
/// its expansion. A nested one past the ceiling is refused, naming the counts.
fn submit(nested: bool, mut job: Job) -> Result<(), Stop> {
    let cap = cap();
    let mut threads = THREADS.lock();
    while let Some(idle) = threads.idle.pop() {
        match idle.send(job) {
            Ok(()) => return Ok(()),
            // Its thread is gone; the job comes back.
            Err(mpsc::SendError(back)) => {
                threads.started = threads.started.saturating_sub(1);
                job = back;
            }
        }
    }
    if !nested && threads.started >= cap {
        threads.queued.push_back(job);
        return Ok(());
    }
    let ceiling = ceiling();
    if threads.started >= ceiling {
        let started = threads.started;
        drop(threads);
        // alloc: error path
        return Err(Stop(format!(
            "refused a nested wasm expansion: {started} expansion threads are alive, the ceiling \
             of {ceiling} ({NESTING} levels of nesting times the cap of {cap}, twice the \
             workers) is reached (bounded-progress: no expansion takes a thread past it)"
        )));
    }
    threads.started += 1;
    drop(threads);
    let (jobs, taken) = mpsc::channel::<Job>();
    let mine = jobs.clone();
    if jobs.send(job).is_err() {
        THREADS.lock().started -= 1;
        return Err(Stop("could not hand its expansion to a new thread".to_owned())); // alloc: error path
    }
    let spawned = thread::Builder::new()
        .name("blaze wasm expansion".to_owned()) // alloc: once per thread started
        .stack_size(STACK)
        .spawn(move || {
            while let Ok(job) = taken.recv() {
                let mut next = Some(job);
                while let Some(job) = next.take() {
                    job();
                    let mut threads = THREADS.lock();
                    if threads.started > cap {
                        // Started past the cap for a nested expansion: it ends with it.
                        threads.started -= 1;
                        return;
                    }
                    match threads.queued.pop_front() {
                        Some(queued) => next = Some(queued),
                        // clone: this thread's own handle, put back for the next expansion
                        None => threads.idle.push(mine.clone()),
                    }
                }
            }
        });
    spawned.map(drop).map_err(|error| {
        THREADS.lock().started -= 1;
        // alloc: error path
        Stop(format!("could not start a thread to run its wasm module on: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use blaze_core::lock::{Lock, Signal};

    use super::{cap, ceiling, submit};

    /// Held by each test: they share the process's expansion threads, and one holding the
    /// ceiling would refuse another's nested expansion.
    static SERIAL: Lock<()> = Lock::new("macro.thread.test.serial", ());

    /// A gate the test's expansions wait at until it opens, and how many of them run now, ran
    /// at most at once, and are done.
    struct Gate {
        open: Lock<bool>,
        opened: Signal,
        running: AtomicUsize,
        most: AtomicUsize,
        done: AtomicUsize,
    }

    impl Default for Gate {
        fn default() -> Gate {
            Gate {
                open: Lock::new("macro.thread.test.gate", false),
                opened: Signal::new("macro.thread.test.opened"),
                running: AtomicUsize::new(0),
                most: AtomicUsize::new(0),
                done: AtomicUsize::new(0),
            }
        }
    }

    impl Gate {
        fn job(self: &Arc<Gate>) -> super::Job {
            let gate = Arc::clone(self);
            Box::new(move || {
                let now = gate.running.fetch_add(1, Ordering::AcqRel) + 1;
                gate.most.fetch_max(now, Ordering::AcqRel);
                let mut open = gate.open.lock();
                let deadline = Instant::now() + Duration::from_secs(60);
                while !*open && !gate.opened.wait_until(&mut open, deadline) {}
                drop(open);
                gate.running.fetch_sub(1, Ordering::AcqRel);
                gate.done.fetch_add(1, Ordering::AcqRel);
            })
        }

        fn open(&self) {
            *self.open.lock() = true;
            self.opened.notify_all();
        }

        /// Waits, at most `within`, until `until` holds: whether it did.
        fn reaches(&self, within: Duration, until: impl Fn(&Gate) -> bool) -> bool {
            let deadline = Instant::now() + within;
            while !until(self) {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::yield_now();
            }
            true
        }
    }

    /// However many expansions are handed in at once, at most [`cap`] run at once, each on a
    /// thread, and every one runs: the threads are fixed by the machine, not by how many
    /// crates or programs a build expands, and the rest wait for a thread, not for each other.
    #[test]
    fn expansions_past_the_cap_wait_for_a_thread_and_all_run() {
        let _serial = SERIAL.lock();
        let cap = cap();
        let gate = Arc::new(Gate::default());
        let count = 3 * cap;
        for _ in 0..count {
            submit(false, gate.job()).expect("an expansion is handed in");
        }
        // The first `cap` are on threads (fewer if another test's expansions hold some).
        gate.reaches(Duration::from_secs(10), |it| it.running.load(Ordering::Acquire) >= cap);
        assert!(gate.most.load(Ordering::Acquire) <= cap, "at most {cap} expansions ran at once");
        gate.open();
        let all =
            gate.reaches(Duration::from_secs(60), |it| it.done.load(Ordering::Acquire) == count);
        assert!(all, "{} of {count} expansions ran", gate.done.load(Ordering::Acquire));
        assert!(gate.most.load(Ordering::Acquire) <= cap, "at most {cap} expansions ran at once");
    }

    /// An expansion asked for while its job answers another's question gets a thread even
    /// with every thread of the cap busy: that other expansion's thread waits for the answer,
    /// so queueing it could leave the cap waiting on itself.
    #[test]
    fn a_nested_expansion_runs_while_the_cap_is_busy() {
        let _serial = SERIAL.lock();
        let cap = cap();
        let gate = Arc::new(Gate::default());
        for _ in 0..cap {
            submit(false, gate.job()).expect("an expansion is handed in");
        }
        let nested = Arc::new(AtomicUsize::new(0));
        let ran = Arc::clone(&nested);
        submit(
            true,
            Box::new(move || {
                ran.fetch_add(1, Ordering::AcqRel);
            }),
        )
        .expect("a nested expansion is handed in");
        let ran = gate.reaches(Duration::from_secs(10), |_| nested.load(Ordering::Acquire) == 1);
        gate.open();
        assert!(ran, "the nested expansion ran while the cap's threads were busy");
        assert!(gate.reaches(Duration::from_secs(60), |it| it.done.load(Ordering::Acquire) == cap));
    }

    /// Nested expansions take threads past the cap only up to the ceiling: one more is refused
    /// with an error naming the counts, and the threads alive never pass it.
    #[test]
    fn nested_expansions_stop_at_the_ceiling() {
        let _serial = SERIAL.lock();
        let ceiling = ceiling();
        let gate = Arc::new(Gate::default());
        let mut handed = 0;
        let refused = loop {
            match submit(true, gate.job()) {
                Ok(()) => handed += 1,
                Err(stop) => break stop,
            }
            assert!(handed <= ceiling, "{handed} nested expansions got threads, past {ceiling}");
        };
        assert!(refused.0.contains("ceiling"), "the refusal names the ceiling: {}", refused.0);
        gate.open();
        let all =
            gate.reaches(Duration::from_secs(60), |it| it.done.load(Ordering::Acquire) == handed);
        assert!(all, "{} of {handed} nested expansions ran", gate.done.load(Ordering::Acquire));
    }
}
