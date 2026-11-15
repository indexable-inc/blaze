//! `blaze debug inspect`: what a live daemon is doing, answered by a thread of its own
//! (`blaze-inspect`) on a socket of its own (`<home>/v2.inspect.sock`), never by the loop and
//! never by a pool job, so the answer comes when every work thread is busy and when the loop
//! is stuck inside a turn.
//!
//! One request, one answer, each a `blaze_protocol` frame of deterministic CBOR with this
//! module's [`SCHEMA`] (rule no-mcp): [`Asked`], then [`Answer`]. A request of another schema
//! is refused naming both.
//!
//! Everything it says is read from what other threads already publish, with no word from the
//! loop or a worker:
//! - each thread's open spans, outermost first, from the flight recorder
//!   (`blaze_profile::flight::threads_now`): the task it runs (its `task` span's node), the
//!   span it is in, and for a wait what it awaits, who ends it and for how long;
//! - the named-lock, signal and kernel waits in progress with each lock's holder
//!   (`blaze_core::lock::graph::snapshot`), timed on the lock probe's clock;
//! - the scheduler's waits (`blaze_graph::sched::waits_now`: each parked job and the key,
//!   counter or job it waits on, with the key's claimer), read on a thread of their own for at
//!   most [`WAITS_READ`], since a deadlock may hold the scheduler's lock;
//! - the plane's queued and running tasks from the table the loop publishes for the explorer
//!   (`blaze_explore::Tasks`), taken only if no one holds it ([`Inspected::tasks_unread`] says
//!   when it could not be);
//! - memory: the footprint, the number the kernel enforces, and each subsystem's bytes
//!   (`blaze_mem::tag::snapshot`);
//! - the loop's turns and the pool's workers, so a reader sees a loop that does not turn.
//!
//! What it cannot see, and says nothing false about: a job parked on a fiber keeps its open
//! spans off every thread, so its wait is in [`Inspected::job_waits`] (with no age: the
//! scheduler times no wait) and not in [`Inspected::longest`]; and a queued task's wait for a
//! thread is in its `admission` span only once it starts.
//!
//! With [`Asked::record`] it also writes a flight recording ([`super::flightrec`]) and names
//! the file.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use super::{flightrec, pulse::Pulse};

/// The version of [`Asked`] and [`Answer`]. A change to any type here is a new number.
pub const SCHEMA: u32 = 4;

/// How long the scheduler's lock is waited for.
const WAITS_READ: Duration = Duration::from_secs(1);

/// The waits [`Inspected::longest`] holds at most.
const LONGEST: usize = 32;

/// The socket beside the daemon's own: `v2.sock`'s `v2.inspect.sock`.
pub fn socket_of(daemon: &Path) -> PathBuf {
    daemon.with_extension("inspect.sock")
}

/// The request.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Asked {
    /// Also write a flight recording, and name it.
    pub record: bool,
    /// [`SCHEMA`], as the client knows it.
    pub schema: u32,
}

/// The answer.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum Answer {
    Inspected(Box<Inspected>),
    /// Why there is no document: the request was of another schema.
    Refused(String),
}

/// What the daemon is doing.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Inspected {
    pub pid: u32,
    /// The loop and the pool.
    pub turn: Turn,
    /// The plane's tasks that have not ended, running first, then queued, each longest first.
    pub tasks: Vec<Task>,
    pub memory: Memory,
    /// [`SCHEMA`].
    pub schema: u32,
    /// The waits in progress, longest first, at most 32: a thread's wait span, a lock wait.
    pub longest: Vec<Waiting>,
    /// Every thread inside a span, waiting ones first, each longest first.
    pub threads: Vec<Thread>,
    /// The flight recording written, when one was asked for; its error when it failed.
    pub recorded: Option<Result<String, String>>,
    /// The scheduler's waits in one line, or why they were not read.
    pub job_waits: String,
    /// The lock, signal and kernel waits in progress, longest first.
    pub lock_waits: Vec<LockWait>,
    /// Why [`Inspected::tasks`] is empty though the plane may have tasks: the table was held.
    pub tasks_unread: Option<String>,
    /// Each job's write of its front end that waits, longest first, as the loop last looked
    /// (once a second while a build runs): what it waits with, its readers alive, the tasks
    /// the loop holds for it and the node applying it.
    pub writes: Vec<super::work::WriteWaiting>,
    /// The proc macros' expansion threads; `None` when their lock was held as it was read.
    pub expansion: Option<Expansion>,
}

/// The proc macros' expansion threads (`blaze_macro_eval::expansion_counts`).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Expansion {
    /// Threads alive, idle ones included.
    pub alive: u64,
    /// Of them, waiting for an expansion.
    pub idle: u64,
    /// Expansions waiting for a thread.
    pub queued: u64,
    /// Threads blocked on `macro.thread.replied`, waiting for their job's answer.
    pub asking: u64,
    /// The most threads kept.
    pub cap: u64,
}

/// The loop's turns and the pool's workers.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    /// Workers with nothing to do.
    pub idle: bool,
    /// The loop's own progress count.
    pub moved: u64,
    /// The turns the loop finished: read twice, [`Turn::turned`] apart, so a loop that does
    /// not turn shows.
    pub turns: u64,
    /// Workers parked.
    pub parked: u64,
    /// Turns finished in the 60 ms before the answer; an idle loop turns at least every 50 ms.
    pub turned: u64,
    /// The pool's workers.
    pub workers: u64,
    /// Workers inside a job's code.
    pub working: u64,
    /// Pool jobs that ran to their end.
    pub jobs_ended: u64,
}

/// The daemon's memory.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    /// Each subsystem's bytes in use, largest first.
    pub tags: Vec<(String, u64)>,
    /// The footprint, the number the kernel enforces; `None` when it could not be read.
    pub footprint: Option<u64>,
    /// The global heap by holder, largest first: each tag's and `outside` (no task's).
    pub heap: Vec<Heap>,
    /// The bytes blaze-mem's regions have committed and not purged.
    pub regions: u64,
    /// The footprint less the global heap's live bytes and `regions`: freed and kept, at most.
    pub kept: u64,
}

/// One holder's part of the global heap.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Heap {
    /// A tag's name, or `outside`.
    pub holder: String,
    /// The bytes of its live blocks as asked for, from the counting allocator.
    pub live: u64,
    /// Its pages in the allocator, what they have committed, their live blocks' bytes, and of
    /// the committed bytes what pages with no live block hold and what pages under a quarter
    /// in use hold (`blaze_mem::tag::HeapPages`); all 0 where the allocator does not say.
    pub pages: u64,
    pub committed: u64,
    pub used: u64,
    pub empty: u64,
    pub sparse: u64,
}

/// A plane task that has not ended.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// The plane request's number in its high half, the node in its low.
    pub id: u64,
    /// The unsettled tasks it waits on, by id.
    pub deps: Vec<u64>,
    pub kind: String,
    pub name: String,
    /// `running` or `queued`.
    pub state: String,
    /// What the thread running it waits on now, when it waits: its innermost span's `awaits`.
    pub awaits: Option<String>,
    /// How long it has been in its state; `None` before the loop gave the table its clock.
    pub for_ms: Option<u64>,
    /// The thread running it, when one is inside its `task` span.
    pub thread: Option<String>,
}

/// A thread inside a span.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Thread {
    pub tid: u64,
    pub name: String,
    /// The plane node of the task it runs, when it is inside a `task` span.
    pub node: Option<u64>,
    /// Its spans, outermost first.
    pub open: Vec<flightrec::Open>,
    /// `waiting` when its innermost span awaits something, else `running`.
    pub state: String,
    /// What its innermost span awaits.
    pub awaits: Option<String>,
    /// Spans it is inside past those named.
    pub deeper: u32,
    /// How long it has been in its innermost span.
    pub for_ms: u64,
}

/// A lock, signal or kernel wait in progress.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct LockWait {
    /// What it waits on, by name: a lock's, a signal's, a kernel wait's.
    pub on: String,
    /// `lock`, `signal` or `wait`.
    pub kind: String,
    /// Where it asked, when known.
    pub site: Option<String>,
    /// The thread that holds the lock, by tid, when one does.
    pub holder: Option<u64>,
    /// The waiting thread's tid.
    pub thread: u64,
    pub for_ms: u64,
}

/// One wait, for [`Inspected::longest`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Waiting {
    /// What it waits on.
    pub on: String,
    /// The thread, by name (a span's wait) or tid (a lock wait).
    pub who: String,
    /// `span` (a thread's open wait span) or `lock`, `signal`, `wait` (the lock graph's).
    pub kind: String,
    pub for_ms: u64,
}

/// The threads of `now`, as the document has them: waiting ones first, each longest first.
pub fn threads(now: &[blaze_profile::flight::ThreadNow]) -> Vec<Thread> {
    // alloc: an answer, on demand
    let mut threads: Vec<Thread> = now
        .iter()
        .map(|it| {
            let flightrec::Thread { tid, name, open, deeper } = flightrec::Thread::of(it);
            let innermost = open.last();
            let awaits = innermost.and_then(|it| it.awaits.clone()); // clone: the answer's copy
            Thread {
                tid,
                name,
                node: open.iter().rev().find_map(|it| it.node),
                state: if awaits.is_some() { "waiting" } else { "running" }.to_owned(),
                awaits,
                deeper,
                for_ms: innermost.map_or(0, |it| it.for_ns / 1_000_000),
                open,
            }
        })
        .collect();
    threads.sort_by(|a, b| {
        (b.awaits.is_some(), b.for_ms, &a.name).cmp(&(a.awaits.is_some(), a.for_ms, &b.name))
    });
    threads
}

/// The waits of `threads` and `locks`, longest first, at most [`LONGEST`].
pub fn longest(threads: &[Thread], locks: &[LockWait]) -> Vec<Waiting> {
    // alloc: an answer, on demand
    let mut waits: Vec<Waiting> = threads
        .iter()
        .filter_map(|it| {
            // clone: the answer's copy
            Some(Waiting {
                on: it.awaits.clone()?,
                who: it.name.clone(),
                kind: "span".to_owned(),
                for_ms: it.for_ms,
            })
        })
        .chain(locks.iter().map(|it| Waiting {
            on: it.on.clone(), // clone: as above
            who: format!("thread {}", it.thread),
            kind: it.kind.clone(), // clone: as above
            for_ms: it.for_ms,
        }))
        .collect();
    waits.sort_by(|a, b| (b.for_ms, &a.who, &a.on).cmp(&(a.for_ms, &b.who, &b.on)));
    waits.truncate(LONGEST);
    waits
}

/// The plane's unsettled tasks from the explorer's table, each joined with the thread inside
/// its `task` span: running first, then queued, each longest first. `Err` when the table is
/// held (the loop is writing it, or stuck holding it).
fn tasks(table: &blaze_explore::Tasks, threads: &[Thread]) -> Result<Vec<Task>, String> {
    let Some(table) = table.try_lock() else {
        return Err("the tasks table is held by the loop that writes it: ask again, and if it \
                    stays so the loop is stuck with it (see `turn.turned`)"
            .to_owned());
    };
    let now = blaze_profile::clock::mono_ns();
    let offset = table.offset();
    let mut live = std::collections::BTreeSet::new(); // alloc: an answer, on demand
    table.live(|it| drop(live.insert(it.id)));
    let mut tasks = Vec::new(); // alloc: as above
    table.live(|it| {
        // A plane node is the low half of a task's id, and a `task` span says the node.
        let node = it.id & u64::from(u32::MAX);
        let on = threads.iter().find(|thread| thread.node == Some(node));
        let since = it.running_ns.unwrap_or(it.said_ns);
        let for_ms = offset.map(|offset| {
            let since = i64::try_from(since).unwrap_or(i64::MAX).saturating_add(offset);
            u64::try_from(now.saturating_sub(since)).unwrap_or(0) / 1_000_000
        });
        tasks.push(Task {
            id: it.id,
            deps: it.deps.iter().copied().filter(|dep| live.contains(dep)).collect(),
            kind: it.kind.to_owned(),
            name: it.name.to_owned(),
            state: if it.running_ns.is_some() { "running" } else { "queued" }.to_owned(),
            awaits: on.and_then(|thread| thread.awaits.clone()), // clone: the answer's copy
            for_ms,
            thread: on.map(|thread| thread.name.clone()), // clone: as above
        });
    });
    tasks.sort_by(|a, b| {
        (b.state == "running", b.for_ms, a.id).cmp(&(a.state == "running", a.for_ms, b.id))
    });
    Ok(tasks)
}

/// The lock graph's waits in progress, longest first; none when no lock probe is installed
/// (no wait is timed then).
fn lock_waits() -> Vec<LockWait> {
    let Some(probe) = blaze_core::lock::installed() else { return Vec::new() };
    let now = probe.now_ns();
    // alloc: an answer, on demand
    let mut waits: Vec<LockWait> = blaze_core::lock::graph::snapshot()
        .waits
        .iter()
        .map(|row| {
            let (kind, on) = row.on.said();
            LockWait {
                on: on.to_owned(),
                kind: kind.to_owned(),
                site: row.site.map(|it| it.to_string()),
                holder: row.holder,
                thread: row.thread,
                for_ms: now.saturating_sub(row.since_ns) / 1_000_000,
            }
        })
        .collect();
    waits.sort_by(|a, b| (b.for_ms, a.thread).cmp(&(a.for_ms, b.thread)));
    waits
}

/// The scheduler's waits, read on a thread of their own within [`WAITS_READ`].
fn job_waits() -> String {
    let (said, heard) = crossbeam_channel::bounded(1);
    let started = std::thread::Builder::new()
        .name("blaze-inspect-waits".to_owned()) // alloc: once an answer
        .spawn(move || drop(said.send(blaze_graph::sched::waits_now().to_string())));
    if let Err(error) = started {
        return format!("unread: their thread did not start: {error}"); // alloc: as above
    }
    let _wait = tracing::info_span!(target: "blaze", "inspect: read the scheduler's waits", awaits = "the scheduler's lock").entered();
    match heard.recv_timeout(WAITS_READ) {
        Ok(waits) => waits,
        // alloc: as above
        Err(_) => format!(
            "unread: the scheduler's lock was not had in {} s, so something holds it",
            WAITS_READ.as_secs()
        ),
    }
}

/// What the answering thread reads the daemon by.
pub struct Inspector {
    pub pulse: Arc<Pulse>,
    pub tasks: Arc<blaze_explore::Tasks>,
}

/// The global heap by holder, largest live first.
fn heap(snapshot: &blaze_mem::tag::Snapshot) -> Vec<Heap> {
    let pages = blaze_mem::tag::heap_pages().unwrap_or_default();
    let holders = blaze_mem::Tag::ALL
        .into_iter()
        .map(|tag| (tag.name(), snapshot.get(tag).heap, pages[tag.index()]))
        .chain([("outside", snapshot.heap_outside, pages[blaze_mem::Tag::COUNT])]);
    // alloc: an answer, on demand
    let mut heap: Vec<Heap> = holders
        .filter(|(_, live, pages)| *live != 0 || pages.pages != 0)
        .map(|(holder, live, pages)| Heap {
            holder: holder.to_owned(), // alloc: an answer, on demand
            live,
            pages: pages.pages,
            committed: pages.committed,
            used: pages.used,
            empty: pages.empty,
            sparse: pages.sparse,
        })
        .collect();
    heap.sort_by(|a, b| (b.committed.max(b.live), &a.holder).cmp(&(a.committed.max(a.live), &b.holder)));
    heap
}

impl Inspector {
    /// The document, now. Waits 60 ms between its two readings of the loop's turns.
    pub fn inspect(&self, record: bool) -> Inspected {
        let turns = self.pulse.turns();
        let pool = blaze_graph::exec::shared();
        let threads = threads(&blaze_profile::flight::threads_now());
        let lock_waits = lock_waits();
        let (tasks, tasks_unread) = match tasks(&self.tasks, &threads) {
            Ok(tasks) => (tasks, None),
            Err(why) => (Vec::new(), Some(why)),
        };
        let footprint = crate::raw::memory::own_footprint_now();
        // alloc: an answer, on demand
        let snapshot = blaze_mem::tag::snapshot();
        let mut tags: Vec<(String, u64)> = blaze_mem::Tag::ALL
            .into_iter()
            .map(|tag| (tag.name().to_owned(), snapshot.get(tag).live))
            .filter(|(_, bytes)| *bytes != 0)
            .collect();
        tags.sort_by(|a, b| (b.1, &a.0).cmp(&(a.1, &b.0)));
        let heap = heap(&snapshot);
        let job_waits = job_waits();
        let recorded = record.then(|| {
            flightrec::record(flightrec::Why::Asked, "blaze debug inspect --record", flightrec::WINDOW)
                .map(|path| path.display().to_string())
        });
        {
            let _wait = tracing::info_span!(target: "blaze", "inspect: watch the loop turn", awaits = "60 ms of the loop's turns").entered();
            std::thread::sleep(Duration::from_millis(60));
        }
        Inspected {
            pid: std::process::id(),
            turn: Turn {
                idle: pool.idle(),
                moved: self.pulse.moved(),
                turns: self.pulse.turns(),
                parked: pool.parked() as u64,
                turned: self.pulse.turns().saturating_sub(turns),
                workers: pool.workers() as u64,
                working: pool.working() as u64,
                jobs_ended: pool.ended(),
            },
            tasks,
            memory: Memory {
                tags,
                footprint,
                heap,
                regions: snapshot.resident_committed(),
                kept: footprint
                    .map_or(0, |footprint| blaze_graph::arena::kept_of(&snapshot, footprint)),
            },
            schema: SCHEMA,
            longest: longest(&threads, &lock_waits),
            threads,
            recorded,
            job_waits,
            lock_waits,
            tasks_unread,
            writes: self.pulse.writes(),
            expansion: blaze_macro_eval::expansion_counts().map(|it| Expansion {
                alive: it.alive as u64,
                idle: it.idle as u64,
                queued: it.queued as u64,
                asking: it.asking as u64,
                cap: it.cap as u64,
            }),
        }
    }

    /// The answer to `asked`. An answer path: nothing under it syncs a file
    /// (`blaze::sync_on_answer_path`, docs/v2/plan/design/effect-summary.md).
    #[blaze::answers_request]
    pub fn answer(&self, asked: Asked) -> Answer {
        if asked.schema != SCHEMA {
            // alloc: a refusal, once
            return Answer::Refused(format!(
                "`blaze debug inspect` asked in schema {} and this daemon (pid {}) answers schema \
                 {SCHEMA}: it is another blaze's; ask with the blaze that started it",
                asked.schema,
                std::process::id()
            ));
        }
        Answer::Inspected(Box::new(self.inspect(asked.record))) // alloc: the answer
    }
}

/// Starts the thread that answers on `socket` for as long as the process lives. A socket that
/// cannot be served is an error naming it: the daemon says it and serves without `inspect`.
pub fn serve(socket: &Path, inspector: Inspector) -> Result<(), String> {
    use crate::socket::{NotServed, Served};
    let served = match Served::bind(socket) {
        Ok(served) => served,
        Err(NotServed::Taken) => {
            return Err(format!(
                "`blaze debug inspect` is not served: another process holds {}",
                socket.display()
            ));
        }
        Err(NotServed::Failed(why)) => {
            return Err(format!(
                "`blaze debug inspect` is not served: listening on {}: {why}",
                socket.display()
            ));
        }
    };
    std::thread::Builder::new()
        .name("blaze-inspect".to_owned()) // alloc: once a daemon
        .spawn(move || {
            // One connection at a time, each bounded by the socket's own timeouts: a client
            // that says nothing holds the next up for at most `socket::REQUEST_TIMEOUT`.
            for stream in served.incoming() {
                let Ok((stream, _open)) = stream else { continue };
                if let Err(why) = answer_one(&stream, &inspector) {
                    tracing::warn!(target: "blaze", "`blaze debug inspect` was not answered: {why}");
                }
            }
        })
        .map(drop)
        .map_err(|why| format!("`blaze debug inspect` is not served: starting its thread: {why}"))
}

/// One request read from `stream` and answered on it.
fn answer_one(
    mut stream: &std::os::unix::net::UnixStream,
    inspector: &Inspector,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(crate::socket::REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(crate::socket::REPLY_TIMEOUT))?;
    let Some(asked) = blaze_protocol::receive::<Asked>(&mut stream)? else { return Ok(()) };
    blaze_protocol::send(&mut stream, &inspector.answer(asked))
}

/// The client's side: `asked` sent to the daemon's inspect socket at `socket`, and its answer.
/// An error names the socket and what failed; no daemon is started.
pub fn ask(socket: &Path, asked: Asked) -> Result<Answer, String> {
    let failed = |doing: &str, why: std::io::Error| {
        format!("`blaze debug inspect`: {doing} {}: {why}", socket.display())
    };
    let stream = crate::socket::connect(socket).map_err(|why| match why.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => format!(
            "`blaze debug inspect`: no daemon answers at {} ({why}): none is running in this \
             BLAZE_HOME, or it is a blaze from before `inspect`",
            socket.display()
        ),
        _ => failed("connecting to", why),
    })?;
    // The answer takes the scheduler's read and the turn watch: a second or so, bounded here.
    stream
        .set_read_timeout(Some(crate::socket::REPLY_TIMEOUT))
        .map_err(|why| failed("bounding the read from", why))?;
    blaze_protocol::send(&mut &stream, &asked).map_err(|why| failed("asking", why))?;
    match blaze_protocol::receive::<Answer>(&mut &stream) {
        Ok(Some(answer)) => Ok(answer),
        Ok(None) => Err(format!(
            "`blaze debug inspect`: the daemon at {} closed the connection with no answer",
            socket.display()
        )),
        Err(why) => Err(failed("reading the answer from", why)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_profile::flight::{OpenSpan, ThreadNow};

    fn span(name: &'static str, awaits: Option<&str>, for_ms: u64, node: Option<u64>) -> OpenSpan {
        OpenSpan {
            name,
            for_ns: for_ms * 1_000_000,
            awaits: awaits.map(Into::into),
            node,
            ..OpenSpan::default()
        }
    }

    fn thread(tid: u64, name: &str, open: Vec<OpenSpan>) -> ThreadNow {
        ThreadNow { tid, name: name.to_owned(), open, deeper: 0 }
    }

    /// A thread waits when its innermost span awaits something; the task it runs is its `task`
    /// span's node; waiting threads come first and the longest wait leads `longest`, a lock
    /// wait among the spans' by its age.
    #[test]
    fn waiting_threads_lead_and_the_longest_wait_is_first() {
        let now = [
            thread(1, "blaze-work-1", vec![span("task", None, 9_000, Some(4)), span("unit bodies", None, 800, None)]),
            thread(2, "blaze-work-2", vec![span("task", None, 5_000, Some(7)), span("wait", Some("key infer(Foo)"), 3_000, None)]),
            thread(3, "main", vec![span("wait", Some("channel daemon loop"), 1, None)]),
        ];
        let threads = threads(&now);
        let said: Vec<(&str, &str, Option<u64>, u64)> =
            threads.iter().map(|it| (it.name.as_str(), it.state.as_str(), it.node, it.for_ms)).collect();
        assert_eq!(
            said,
            [
                ("blaze-work-2", "waiting", Some(7), 3_000),
                ("main", "waiting", None, 1),
                ("blaze-work-1", "running", Some(4), 800),
            ]
        );
        let locks = [LockWait {
            on: "store.packs".to_owned(),
            kind: "lock".to_owned(),
            site: None,
            holder: Some(2),
            thread: 9,
            for_ms: 2_000,
        }];
        let longest = longest(&threads, &locks);
        let said: Vec<(&str, &str, u64)> =
            longest.iter().map(|it| (it.who.as_str(), it.on.as_str(), it.for_ms)).collect();
        assert_eq!(
            said,
            [
                ("blaze-work-2", "key infer(Foo)", 3_000),
                ("thread 9", "store.packs", 2_000),
                ("main", "channel daemon loop", 1),
            ]
        );
    }

    /// The plane's tasks are read off the loop: a running one is joined with the thread inside
    /// its `task` span and says what that thread awaits; a queued one names the unsettled
    /// tasks it waits on; and a table someone holds is said, never waited for.
    #[test]
    fn tasks_join_their_threads_and_a_held_table_is_said() {
        use blaze_explore::tasks::task_id;
        use blaze_protocol::now::Record;
        let table = blaze_explore::Tasks::new();
        table.clock(0, blaze_profile::clock::mono_ns());
        {
            let mut rows = table.lock();
            rows.said(3, 0, Record::Task { id: 7, kind: "unit bodies (infer)", name: "reth", deps: &[] });
            rows.said(3, 0, Record::Task { id: 8, kind: "link", name: "reth", deps: &[7] });
        }
        let now = [thread(
            2,
            "blaze-work-2",
            vec![span("task", None, 5_000, Some(7)), span("wait", Some("key infer(Foo)"), 3_000, None)],
        )];
        let threads = threads(&now);
        let tasks = tasks(&table, &threads).unwrap();
        let link = tasks.iter().find(|it| it.kind == "link").unwrap();
        assert_eq!((link.id, link.state.as_str()), (task_id(3, 8), "queued"));
        assert_eq!(link.deps, [task_id(3, 7)]);
        let infer = tasks.iter().find(|it| it.kind == "unit bodies (infer)").unwrap();
        assert_eq!(infer.thread.as_deref(), Some("blaze-work-2"));
        assert_eq!(infer.awaits.as_deref(), Some("key infer(Foo)"));

        let held = table.lock();
        let unread = super::tasks(&table, &threads).unwrap_err();
        assert!(unread.contains("is held by the loop"), "{unread}");
        drop(held);
    }

    /// The document is deterministic CBOR that reads back equal, and a request of another
    /// schema is refused naming both.
    #[test]
    fn the_answer_round_trips_and_another_schema_is_refused_by_number() {
        let inspected = Inspected {
            pid: 7,
            turn: Turn { idle: false, moved: 1, turns: 9, parked: 2, turned: 0, workers: 8, working: 6, jobs_ended: 40 },
            tasks: Vec::new(),
            memory: Memory {
                tags: vec![("Store".to_owned(), 1 << 20)],
                footprint: Some(75 << 30),
                heap: vec![Heap {
                    holder: "query".to_owned(),
                    live: 1 << 30,
                    pages: 40_000,
                    committed: 2 << 30,
                    used: 1 << 30,
                    empty: 1 << 20,
                    sparse: 1 << 29,
                }],
                regions: 2 << 30,
                kept: 1 << 30,
            },
            schema: SCHEMA,
            longest: Vec::new(),
            threads: Vec::new(),
            recorded: Some(Err("no flight recording is written".to_owned())),
            job_waits: "no job waits".to_owned(),
            lock_waits: Vec::new(),
            tasks_unread: None,
            writes: vec![super::super::work::WriteWaiting {
                root: "/ws".to_owned(),
                waited_ms: 12_000,
                wait: super::super::work::WriteWait {
                    runs: 0,
                    macros: 3,
                    msls: 0,
                    readers: 1,
                },
                held: 40,
                applying: None,
            }],
            expansion: Some(Expansion { alive: 4, idle: 0, queued: 0, asking: 4, cap: 256 }),
        };
        let answer = Answer::Inspected(Box::new(inspected));
        let bytes = blaze_protocol::cbor::to_vec(&answer).unwrap();
        blaze_protocol::cbor::check(&bytes).unwrap();
        assert_eq!(blaze_protocol::cbor::from_slice::<Answer>(&bytes).unwrap(), answer);

        let inspector =
            Inspector { pulse: Arc::new(Pulse::default()), tasks: Arc::new(blaze_explore::Tasks::new()) };
        let Answer::Refused(why) = inspector.answer(Asked { record: false, schema: SCHEMA + 1 }) else {
            panic!("a request of another schema was answered");
        };
        let (asked, answers) = (format!("schema {}", SCHEMA + 1), format!("answers schema {SCHEMA}"));
        assert!(why.contains(&asked) && why.contains(&answers), "{why}");
    }
}
