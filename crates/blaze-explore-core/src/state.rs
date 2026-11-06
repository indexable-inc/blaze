//! The live state, folded from hello, the live frames and answers: each thread's open span
//! chain, which threads are the pool's workers, the recent closed spans, the counters' latest
//! values, the live tasks folded through `blaze_protocol::now::Fold` ([`crate::tasks`]); and
//! the two models Running now shows: [`Session::look`], the fold's [`Now`] (phase, blockers,
//! downloads, memory, slow or stalled), and [`Session::running`], each thread's whole chain.
//!
//! Everything kept is bounded: [`RECENT`] closed spans, [`MAX_THREADS`] threads, [`MAX_COUNTERS`]
//! counters, and the frame splitter's buffer at a frame and a chunk.
//!
//! Which threads are workers, under the `blaze` layer: the pool's threads, named
//! `blaze-work-<n>` (blaze-graph's `pool`) and `blaze-fiber-<n>` (its `sched::fiber`), and any
//! thread whose outermost open span is the pool's own `worker` span (target `blaze`; blaze-graph's
//! `pool::Driving`, function `search`, `idle` or `switch`), which a worker is inside exactly when
//! it runs no job: `DriverSpan` is never held across a job's run, and every moment of a worker is
//! inside a job's span or that one. So a worker is **busy** when it is inside spans and the
//! outermost is not `worker`, and **idle** otherwise. Every other thread inside a span is the
//! daemon's own machinery (its server, the live tick, the watchdog, IO), shown apart as
//! **internal** so "busy of workers" counts only the build's workers. A source without the
//! `blaze` layer names no pool, so there every thread is a worker, busy while inside a span.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::collections::{BTreeMap, VecDeque};

use arrow_array::RecordBatch;

use blaze_protocol::now::{self, Memory, Now, Worker};

use crate::{
    Error,
    live::{self, CounterRow, Decoder, EdgeRow, OpenRow, SpanRow, Stream, TaskRow},
    tasks::Tasks,
    wire::{self, Frames, Hello, HEAD, Live, LiveHead, Refusal},
};

/// The counter holding a process's footprint, what [`Now::memory`] says it holds.
pub const FOOTPRINT: &str = "footprint bytes";

/// Closed spans kept, newest last.
pub const RECENT: usize = 256;

/// Threads kept: past this, a new thread takes the place of the one seen longest ago with no
/// open span.
pub const MAX_THREADS: usize = 4096;

/// Counters kept, each its latest value: past this, a new counter takes the place of the one
/// updated longest ago.
pub const MAX_COUNTERS: usize = 4096;

/// The layer whose workers, `worker` span and spans' targets the rule above reads.
pub const BLAZE: &str = "blaze";

/// A thread: its process and its id there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadKey {
    pub process: u64,
    pub thread: u64,
}

/// A thread as the frames showed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub name: String,
    /// Named as a pool's thread, or seen inside the pool's `worker` span.
    pub pool_worker: bool,
    /// The spans it is inside, outermost first (innermost last).
    pub open: Vec<OpenRow>,
    /// The last frame that showed it.
    pub seen_ns: i64,
    /// Spans it closed, since the session began.
    pub closed: u64,
}

/// Whether `name` is a pool thread's: `blaze-work-<n>` or `blaze-fiber-<n>`.
pub fn pool_worker_name(name: &str) -> bool {
    ["blaze-work-", "blaze-fiber-"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Whether `span` is the pool's own `worker` span: a worker between jobs.
pub fn worker_span(span: &OpenRow) -> bool {
    span.name == "worker" && span.target.as_deref() == Some(BLAZE)
}

/// What the live stream said beside rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Live frames read.
    pub frames: u64,
    /// Rows the daemon wrote over before this session read them, summed over frames.
    pub skipped: u64,
    /// Spans the daemon did not record since its process began, the latest frame's.
    pub lost: u64,
    /// Frames of tables this core does not keep.
    pub other_frames: u64,
    /// Ticks whose frame the daemon could not encode.
    pub refused: u64,
}

/// The certificate the live stream said was made anew.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cert {
    pub certhash: String,
    pub not_after_unix_s: u64,
}

/// The last answer fed.
#[derive(Clone, Debug, PartialEq)]
pub struct AnswerSeen {
    pub op: String,
    pub rows: Option<u64>,
    pub elapsed_ns: Option<u64>,
    pub refusal: Option<Refusal>,
}

/// The tables the live state keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Table {
    Open,
    Spans,
    Counters,
    Tasks,
    Edges,
}

/// One session with the daemon: what it said, folded. Its IPC is read through `S`, the driver's
/// decoder ([`crate::ipc`]).
#[derive(Debug)]
pub struct Session<S> {
    pub(crate) hello: Option<Hello>,
    open: Decoder<S>,
    spans: Decoder<S>,
    counters: Decoder<S>,
    tasks_in: Decoder<S>,
    edges_in: Decoder<S>,
    frames: Frames,
    // Scratch, reused frame to frame.
    frame: Vec<u8>,
    batches: Vec<RecordBatch>,
    open_rows: Vec<OpenRow>,
    span_rows: Vec<SpanRow>,
    counter_rows: Vec<CounterRow>,
    task_rows: Vec<TaskRow>,
    edge_rows: Vec<EdgeRow>,
    pub(crate) threads: BTreeMap<ThreadKey, Thread>,
    /// The live tasks and their fold.
    pub(crate) tasks: Tasks,
    /// Closed spans, newest last.
    pub(crate) recent: VecDeque<SpanRow>,
    pub(crate) counters_now: BTreeMap<(u64, String), CounterRow>,
    /// The latest frame's `at_ns`.
    pub(crate) at_ns: Option<i64>,
    /// When a span last closed (its start and duration), from the `spans` frames.
    pub(crate) last_close_ns: Option<i64>,
    pub(crate) stats: Stats,
    pub(crate) cert: Option<Cert>,
    pub(crate) refusal: Option<Refusal>,
    pub(crate) answer: Option<AnswerSeen>,
}

impl<S: Stream> Default for Session<S> {
    fn default() -> Session<S> {
        Session::new()
    }
}

/// A thread that is inside spans, as Running now shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Busy<'s> {
    pub key: ThreadKey,
    pub thread_name: &'s str,
    /// Its open spans, outermost first, the innermost last.
    pub chain: &'s [OpenRow],
    /// How long it has been inside its outermost span.
    pub for_ns: u64,
}

/// What is running now: the generic model the view shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Running<'s> {
    /// The latest frame's time (hello's before any frame).
    pub at_ns: i64,
    /// The time the durations are measured to: the page's reading of the daemon's clock, never
    /// before `at_ns`.
    pub now_ns: i64,
    /// Whether the source has the `blaze` layer, so workers are the pool's.
    pub layered: bool,
    pub workers: u64,
    /// Busy workers, longest first, then by thread.
    pub busy: Vec<Busy<'s>>,
    pub idle: u64,
    /// The daemon's own threads inside spans, apart from the workers.
    pub internal: Vec<Busy<'s>>,
    /// How long since a span last closed, once one has.
    pub quiet_ns: Option<u64>,
}

/// `now - since`, 0 when `since` is later.
pub(crate) fn elapsed(now: i64, since: i64) -> u64 {
    u64::try_from(i128::from(now) - i128::from(since)).unwrap_or(0)
}

impl<S: Stream> Session<S> {
    pub fn new() -> Session<S> {
        Session {
            hello: None,
            open: Decoder::new("open"),
            spans: Decoder::new("spans"),
            counters: Decoder::new("counters"),
            tasks_in: Decoder::new("tasks"),
            edges_in: Decoder::new("task_edges"),
            frames: Frames::new(),
            frame: Vec::new(),
            batches: Vec::new(),
            open_rows: Vec::new(),
            span_rows: Vec::new(),
            counter_rows: Vec::new(),
            task_rows: Vec::new(),
            edge_rows: Vec::new(),
            threads: BTreeMap::new(),
            tasks: Tasks::default(),
            recent: VecDeque::new(),
            counters_now: BTreeMap::new(),
            at_ns: None,
            last_close_ns: None,
            stats: Stats::default(),
            cert: None,
            refusal: None,
            answer: None,
        }
    }

    pub fn hello(&self) -> Option<&Hello> {
        self.hello.as_ref()
    }

    pub fn threads(&self) -> &BTreeMap<ThreadKey, Thread> {
        &self.threads
    }

    /// Closed spans, newest last.
    pub fn recent(&self) -> &VecDeque<SpanRow> {
        &self.recent
    }

    /// Each counter's latest value, by process and name.
    pub fn counters(&self) -> &BTreeMap<(u64, String), CounterRow> {
        &self.counters_now
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The live tasks.
    pub fn tasks(&self) -> &Tasks {
        &self.tasks
    }

    /// Streams begun of `open`, `spans`, `counters`, `tasks` and `task_edges`.
    pub fn streams(&self) -> [u64; 5] {
        [
            self.open.streams(),
            self.spans.streams(),
            self.counters.streams(),
            self.tasks_in.streams(),
            self.edges_in.streams(),
        ]
    }

    /// The daemon's hello, the answer to the session's first request; once a session.
    pub fn feed_hello(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if let Some(had) = &self.hello {
            return Err(Error::new(
                "hello",
                "",
                // alloc: an error's text
                format!(
                    "a second hello, and this session has daemon {}'s: a new session reads a new \
                     hello",
                    had.daemon
                ),
            ));
        }
        self.hello = Some(wire::hello(bytes)?);
        Ok(())
    }

    /// Bytes of the live stream as they arrive, split anywhere; each whole frame is folded in.
    /// Every whole frame is read even when one fails; the first failure is returned.
    pub fn feed_live_stream(&mut self, chunk: &[u8]) -> Result<(), Error> {
        self.frames.push(chunk);
        let mut first: Option<Error> = None;
        let mut frame = std::mem::take(&mut self.frame);
        // Each frame takes at least its length's bytes.
        let mut fuel = self.frames.pending() / HEAD + 1;
        loop {
            if fuel == 0 {
                first.get_or_insert_with(|| {
                    Error::new(
                        "the live stream",
                        "",
                        "more frames came out than its bytes hold: the splitter is out of step",
                    )
                });
                break;
            }
            fuel -= 1;
            match self.frames.next_into(&mut frame) {
                Ok(true) => {
                    if let Err(why) = self.feed_live_frame(&frame) {
                        first.get_or_insert(why);
                    }
                }
                Ok(false) => break,
                Err(why) => {
                    first.get_or_insert(why);
                    break;
                }
            }
        }
        self.frame = frame;
        first.map_or(Ok(()), Err)
    }

    /// One live frame: the bytes after its `u32` length.
    pub fn feed_live_frame(&mut self, frame: &[u8]) -> Result<(), Error> {
        if self.hello.is_none() {
            return Err(Error::new(
                "live frame",
                "",
                "came before hello: a session reads the daemon's hello first",
            ));
        }
        let live = wire::live(frame)?;
        self.stats.frames += 1;
        match live {
            Live::Cert { certhash, not_after_unix_s } => {
                self.cert = Some(Cert { certhash, not_after_unix_s });
                Ok(())
            }
            Live::Refused(refusal) => {
                self.stats.refused += 1;
                self.refusal = Some(refusal);
                Ok(())
            }
            Live::Rows(head) => self.rows(&head),
        }
    }

    fn rows(&mut self, head: &LiveHead<'_>) -> Result<(), Error> {
        let table = match head.table.as_str() {
            "open" => Table::Open,
            "spans" => Table::Spans,
            "counters" => Table::Counters,
            "tasks" => Table::Tasks,
            "task_edges" => Table::Edges,
            _ => {
                self.stats.other_frames += 1;
                return Ok(());
            }
        };
        self.at_ns = Some(self.at_ns.map_or(head.at_ns, |it| it.max(head.at_ns)));
        self.stats.skipped = self.stats.skipped.saturating_add(head.skipped);
        self.stats.lost = self.stats.lost.max(head.lost);
        // alloc: once a frame, the message its errors name
        let message = format!("live frame for table `{}`", head.table);
        let decoder = match table {
            Table::Open => &mut self.open,
            Table::Spans => &mut self.spans,
            Table::Counters => &mut self.counters,
            Table::Tasks => &mut self.tasks_in,
            Table::Edges => &mut self.edges_in,
        };
        decoder.decode(head.ipc, &mut self.batches)?;
        let got = match table {
            Table::Open => {
                self.open_rows.clear();
                for batch in &self.batches {
                    live::open_rows(batch, &message, &mut self.open_rows)?;
                }
                self.open_rows.len()
            }
            Table::Spans => {
                self.span_rows.clear();
                for batch in &self.batches {
                    live::span_rows(batch, &message, &mut self.span_rows)?;
                }
                self.span_rows.len()
            }
            Table::Counters => {
                self.counter_rows.clear();
                for batch in &self.batches {
                    live::counter_rows(batch, &message, &mut self.counter_rows)?;
                }
                self.counter_rows.len()
            }
            Table::Tasks => {
                self.task_rows.clear();
                for batch in &self.batches {
                    live::task_rows(batch, &message, &mut self.task_rows)?;
                }
                self.task_rows.len()
            }
            Table::Edges => {
                self.edge_rows.clear();
                for batch in &self.batches {
                    live::edge_rows(batch, &message, &mut self.edge_rows)?;
                }
                self.edge_rows.len()
            }
        };
        if got as u64 != head.rows {
            return Err(Error::new(
                message,
                "field `rows`",
                // alloc: an error's text
                format!("says {}, and its IPC holds {got}", head.rows),
            ));
        }
        let folded = match table {
            Table::Open => self.fold_open(head.at_ns, &message),
            Table::Spans => {
                self.fold_spans(head.at_ns);
                Ok(())
            }
            Table::Counters => {
                self.fold_counters();
                Ok(())
            }
            Table::Tasks => {
                self.tasks.upsert(&mut self.task_rows, head.at_ns);
                Ok(())
            }
            Table::Edges => {
                self.tasks.needs(&mut self.edge_rows, head.at_ns);
                Ok(())
            }
        };
        self.tasks.evict(head.at_ns);
        folded
    }

    /// An `open` frame is the whole open set at `at_ns`: every chain is replaced; a frame with
    /// no rows says the set emptied.
    fn fold_open(&mut self, at_ns: i64, message: &str) -> Result<(), Error> {
        for thread in self.threads.values_mut() {
            thread.open.clear();
        }
        let mut rows = std::mem::take(&mut self.open_rows);
        let mut kept = Ok(());
        for row in rows.drain(..) {
            let key = ThreadKey { process: row.process, thread: row.thread };
            match self.thread(key, &row.thread_name, at_ns, message) {
                // alloc: a span of a thread's chain, the chain's buffer reused frame to frame
                Ok(thread) => thread.open.push(row),
                Err(why) => {
                    kept = Err(why);
                    break;
                }
            }
        }
        self.open_rows = rows;
        for thread in self.threads.values_mut() {
            thread.open.sort_by_key(|span| span.depth);
            if thread.open.first().is_some_and(worker_span) {
                thread.pool_worker = true;
            }
        }
        kept
    }

    /// The thread at `key`, made when new (its name read for the pool's), its name and time
    /// brought up to date.
    fn thread(
        &mut self,
        key: ThreadKey,
        name: &str,
        at_ns: i64,
        message: &str,
    ) -> Result<&mut Thread, Error> {
        if !self.threads.contains_key(&key) && self.threads.len() >= MAX_THREADS {
            let oldest = self
                .threads
                .iter()
                .filter(|(_, it)| it.open.is_empty())
                .min_by_key(|(at, it)| (it.seen_ns, **at))
                .map(|(at, _)| *at);
            let Some(oldest) = oldest else {
                return Err(Error::new(
                    message,
                    "",
                    // alloc: an error's text
                    format!("more than {MAX_THREADS} threads inside spans at once"),
                ));
            };
            self.threads.remove(&oldest);
        }
        let thread = self.threads.entry(key).or_insert_with(|| Thread {
            // alloc: once a thread
            name: name.to_owned(),
            pool_worker: pool_worker_name(name),
            open: Vec::new(),
            seen_ns: at_ns,
            closed: 0,
        });
        if thread.name != name {
            name.clone_into(&mut thread.name);
            thread.pool_worker |= pool_worker_name(name);
        }
        thread.seen_ns = thread.seen_ns.max(at_ns);
        Ok(thread)
    }

    fn fold_spans(&mut self, at_ns: i64) {
        for row in self.span_rows.drain(..) {
            let end = row.start_ns.saturating_add(row.dur_ns);
            self.last_close_ns = Some(self.last_close_ns.map_or(end, |it| it.max(end)));
            let key = ThreadKey { process: row.process, thread: row.thread };
            if let Some(thread) = self.threads.get_mut(&key) {
                thread.closed += 1;
                thread.seen_ns = thread.seen_ns.max(at_ns);
            }
            if self.recent.len() == RECENT {
                self.recent.pop_front();
            }
            // alloc: a closed span, the ring's buffer at most RECENT
            self.recent.push_back(row);
        }
    }

    fn fold_counters(&mut self) {
        for row in self.counter_rows.drain(..) {
            // clone: a counter's key, once a frame's row
            let key = (row.process, row.name.clone());
            match self.counters_now.get_mut(&key) {
                Some(had) => {
                    if row.t_ns >= had.t_ns {
                        *had = row;
                    }
                }
                None => {
                    if self.counters_now.len() >= MAX_COUNTERS {
                        let oldest = self
                            .counters_now
                            .iter()
                            .min_by(|a, b| a.1.t_ns.cmp(&b.1.t_ns).then_with(|| a.0.cmp(b.0)))
                            // clone: the evicted counter's key
                            .map(|(at, _)| at.clone());
                        if let Some(oldest) = oldest {
                            self.counters_now.remove(&oldest);
                        }
                    }
                    self.counters_now.insert(key, row);
                }
            }
        }
    }

    /// An answer to a request other than hello: its table read whole, its rows counted.
    pub fn feed_answer(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let answer = match wire::answer(bytes)? {
            Err(refusal) => {
                let op = refusal.detail.get("op").and_then(|it| it.as_str()).unwrap_or("");
                self.answer = Some(AnswerSeen {
                    // alloc: once an answer
                    op: op.to_owned(),
                    rows: None,
                    elapsed_ns: None,
                    refusal: Some(refusal),
                });
                return Ok(());
            }
            Ok(answer) => answer,
        };
        // alloc: once an answer, the message its errors name
        let message = format!("answer to `{}`", answer.op);
        let batches = live::whole::<S>(answer.ipc, &message)?;
        let got: u64 = batches.iter().map(|it| it.num_rows() as u64).sum();
        if let Some(rows) = answer.rows
            && rows != got
        {
            return Err(Error::new(
                message,
                "field `rows`",
                // alloc: an error's text
                format!("says {rows}, and its IPC holds {got}"),
            ));
        }
        self.answer = Some(AnswerSeen {
            op: answer.op,
            rows: answer.rows,
            elapsed_ns: answer.elapsed_ns,
            refusal: None,
        });
        Ok(())
    }

    /// hello, the latest frame's time (hello's before any frame) and the time a view measures
    /// to: `now_ns`, the page's reading of the daemon's clock, never before the frame's.
    /// `view` names the view in the error when there is no hello yet.
    pub fn clock(&self, view: &str, now_ns: i64) -> Result<(&Hello, i64, i64), Error> {
        let Some(hello) = &self.hello else {
            return Err(Error::new(view, "", "no hello yet: a session reads it first"));
        };
        let at_ns = match self.at_ns {
            Some(it) => it,
            None => i64::try_from(hello.now_ns).map_err(|_| {
                Error::new(
                    "hello",
                    "field `now_ns`",
                    // alloc: an error's text
                    format!("holds {}, past an i64 the frames' `at_ns` count on", hello.now_ns),
                )
            })?,
        };
        Ok((hello, at_ns, now_ns.max(at_ns)))
    }

    /// Whether `thread` is a worker: every thread without the `blaze` layer, the pool's with
    /// it.
    fn worker(layered: bool, thread: &Thread) -> bool {
        !layered || thread.pool_worker
    }

    /// The program at `now_ns`, the page's reading of the daemon's clock, as the one model
    /// says it ([`now::Fold::look`] over the live tasks): each worker with its open spans (an
    /// idle pool worker, inside only the pool's `worker` span, with none) and the task its
    /// innermost span carries (`open.task`), the downloads, the spans closed since the last
    /// look, and the memory the processes' [`FOOTPRINT`] counters say.
    pub fn look(&mut self, now_ns: i64) -> Result<Now, Error> {
        let (hello, _, now) = self.clock("running now", now_ns)?;
        let layered = hello.layers.iter().any(|it| it == BLAZE);
        let mut workers = 0u32;
        // alloc: once a look, a worker each
        let mut threads = Vec::new();
        for (key, thread) in &self.threads {
            if !Self::worker(layered, thread) {
                continue;
            }
            workers = workers.saturating_add(1);
            let idle = thread.open.first().is_none_or(|outer| layered && worker_span(outer));
            let task = thread.open.last().and_then(|span| span.task);
            threads.push(Worker {
                // alloc: a worker's name in the look, `process:thread`, which the view joins on
                thread: format!("{}:{}", key.process, key.thread),
                task: task.and_then(|task| self.tasks.fold_id((key.process, task))),
                open: if idle {
                    Vec::new()
                } else {
                    thread.open.iter().map(|span| now_span(span, now)).collect()
                },
            });
        }
        let held = self
            .counters_now
            .iter()
            .filter(|((_, name), _)| name == FOOTPRINT)
            .map(|(_, row)| row.value.max(0.0) as u64)
            .fold(0u64, u64::saturating_add);
        let memory = Memory { held, ..Memory::default() };
        Ok(self.tasks.look(now, workers, &threads, &memory))
    }

    /// What is running at `now_ns`, the page's reading of the daemon's clock (hello's `clock`):
    /// each thread inside spans with its whole chain, the workers busy or idle by the rule
    /// above and the daemon's own threads apart. [`Session::look`] is the model of the same
    /// moment; Running now shows both, joined by thread.
    pub fn running(&self, now_ns: i64) -> Result<Running<'_>, Error> {
        let (hello, at_ns, now) = self.clock("running now", now_ns)?;
        let layered = hello.layers.iter().any(|it| it == BLAZE);
        let mut workers = 0u64;
        // alloc: once a view, at most a thread each
        let mut busy = Vec::new();
        // alloc: once a view, at most a thread each
        let mut internal = Vec::new();
        for (key, thread) in &self.threads {
            let worker = Self::worker(layered, thread);
            workers += u64::from(worker);
            let Some(outer) = thread.open.first() else { continue };
            if worker && layered && worker_span(outer) {
                continue;
            }
            let row = Busy {
                key: *key,
                thread_name: &thread.name,
                chain: &thread.open,
                for_ns: elapsed(now, outer.start_ns),
            };
            if worker { busy.push(row) } else { internal.push(row) }
        }
        let order = |a: &Busy<'_>, b: &Busy<'_>| b.for_ns.cmp(&a.for_ns).then_with(|| a.key.cmp(&b.key));
        busy.sort_by(order);
        internal.sort_by(order);
        Ok(Running {
            at_ns,
            now_ns: now,
            layered,
            workers,
            idle: workers - busy.len() as u64,
            busy,
            internal,
            quiet_ns: self.last_close_ns.map(|it| elapsed(now, it)),
        })
    }
}

/// An open span as the one model names it: its name, the function it is of as its item, its
/// crate, what it waits on and for how long it has been open at `now`.
fn now_span(span: &OpenRow, now: i64) -> now::Span {
    now::Span {
        // clone: the look's spans own their strings
        name: span.name.clone(),
        item: span.function.clone(),
        krate: span.krate.clone(),
        awaits: span.awaits.clone(),
        for_ms: elapsed(now, span.start_ns) / 1_000_000,
    }
}
