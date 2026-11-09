//! Where the time of a build goes.
//!
//! Always on, in every process of blaze: the [`flight`] recorder (each thread's last spans,
//! events, switches and [`Plot`] points in a fixed ring), which this crate's subscriber
//! ([`install`]) fills from every kept `tracing` span and from blaze's typed spans ([`span`]);
//! the [`watchdog`], which turns a build that stopped moving into a [`watchdog::Stall`] error
//! naming every thread's open spans, with [`flight::dump`]'s Chrome trace ([`dump`]) beside
//! it; and in the daemon the CPU [`sampler`].
//!
//! The daemon alone writes the profile history ([`history`]): its own rows, each child
//! process it reaped ([`facts::ProcessRow`]), each test run ([`facts::TestRunRow`]), and the
//! rows a client sends at its request's end ([`history::ClientRows`]). `BLAZE_PROFILE=<dir>`
//! is an export from that history, made by the client: build N's Chrome trace, as `blaze
//! debug time --trace` writes it ([`time::load::trace_of`]), at `<dir>/trace.json`.
//!
//! Until 2026-11-04 `BLAZE_PROFILE` was a profiler of its own across processes: each process
//! kept per-thread costs while it was set, wrote `<dir>/parts/*.json` at exit, and the
//! client put them together into `trace.json`, `phases.tsv`, `functions.tsv` and `passes.tsv`
//! and printed `BLAZE_STATS`'s step table (`note`, `step`, `pass`, `write_part`, `finish`,
//! `BLAZE_PROFILE_CONST`). It is gone: the history holds every span with its own CPU time,
//! so a phase or function table is a DuckDB query over it (docs/profiling.md).
//! docs/profiling.md says how to read all of it.

#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

use std::{path::Path, sync::OnceLock, time::Instant};

pub mod arrow;
pub mod batches;
pub mod clock;
pub mod crash;
pub mod diff;
pub mod dump;
pub mod errors;
pub mod facts;
pub mod flight;
pub mod gate;
pub mod heap;
pub mod history;
pub mod hot;
pub mod identity;
pub mod live;
pub mod locks;
pub mod logs;
pub mod panics;
pub mod perf;
pub mod reading;
pub mod sampler;
pub mod schema;
pub mod slow;
pub mod span;
pub mod sql;
mod stack;
pub mod starting;
mod symbols;
pub mod time;
pub mod views;
pub mod watchdog;

pub use flight::{FiberStack, Plot, fiber_stack, moved, plot};
pub use watchdog::{Stall, Watchdog};

/// When this process began, as near as it can tell: the first call, which `main` makes.
pub fn began() -> Instant {
    static BEGAN: OnceLock<Instant> = OnceLock::new();
    *BEGAN.get_or_init(|| clock::origin().1)
}

/// Has every info-level span of the target `blaze` and every rust-analyzer query kept by the
/// flight recorder, and carries fiber jobs' spans with them.
pub fn install() {
    _ = tracing::subscriber::set_global_default(Recorder);
    carry_with_fiber_jobs();
}

/// Moves each fiber job's open spans and build with it when it resumes on another thread
/// (`blaze_graph::sched::fiber::carry`), records its switches, spans each wait for a key
/// another task runs (`blaze_graph::pump::span_waits`) and each job a pool runs
/// (`blaze_graph::pool::span_jobs`), and makes the profiler the named locks' probe
/// (`blaze_core::lock::install`, [`locks::Recorder`]): their contended waits open the same
/// `wait` span and each is a `lock_waits` row. Once a process, before any driver runs:
/// [`install`] calls it.
fn carry_with_fiber_jobs() {
    use blaze_graph::{pool, pump, sched::fiber};
    static CARRIED: std::sync::Once = std::sync::Once::new();
    CARRIED.call_once(|| {
        fiber::carry(fiber::Carried {
            take: flight::take_open,
            put: flight::put_open,
            end: flight::end_open,
        });
        _ = fiber::observe(observed);
        _ = fiber::observe_idle(idled);
        _ = fiber::observe_spawner(given);
        _ = pump::span_waits(pump::WaitSpans {
            begin: wait_began,
            end: wait_ended,
            held: hold_began,
        });
        _ = pool::span_jobs(pool::JobSpans {
            began: job_began,
            ended: job_ended,
            driving: worker_began,
            drove: worker_ended,
        });
        blaze_core::lock::install(&locks::RECORDER);
    });
}

/// The job running on this thread, the time, the build and the span open, as a pool job is
/// given to the pool: its spawner, the start of its wait for a core, the build it works for,
/// and its creator.
fn given() -> blaze_graph::sched::Given {
    blaze_graph::sched::Given {
        spawner: flight::running_job(),
        at: clock::ticks(),
        build: flight::current_build(),
        creator: flight::giving_span(),
    }
}

/// Opens and enters the span of a pool worker's own work between jobs ([`span::Worker`]),
/// `doing` what it does (`search`, `idle`, `switch`): its id for [`worker_ended`].
fn worker_began(doing: &'static str) -> u64 {
    span::open_raw(span::Worker { doing: span::Name::kept(doing) })
}

/// Closes the span [`worker_began`] opened, on the thread that opened it.
fn worker_ended(span: u64) {
    span::close_raw(span);
}

/// Opens and enters the span of a job of `kind` beginning on this thread ([`span::Job`]), named
/// by the job its switches name and by the span it was given inside: the span of the job it
/// runs inside, for [`job_ended`].
fn job_began(kind: &'static str) -> u64 {
    let job = span::Job {
        kind: span::Name::kept(kind),
        job: flight::running_job(),
        creator: flight::begun_creator(),
    };
    flight::job_began(span::open_raw(job))
}

/// Closes the span of the job that ended on this thread, its last segment if it parked, and
/// goes back to `outer`, the span of the job it ran inside.
fn job_ended(outer: u64) {
    let id = flight::job_ended(outer);
    if id != 0 {
        span::close_raw(id);
    }
}

/// Opens and enters the span `wait` ([`span::Wait`]), its `awaits` (and `function`) what it
/// waits for and its `owner` the task running that: its id. The span outlives this call,
/// closed by [`wait_ended`]. A scope's wait names what the scope is for, the span its caller
/// opened it in: `scope 13 of unit codegen serde v1.0.219 (build script)`.
fn wait_began(kind: blaze_graph::wait::WaitKind, awaited: &str, owner: Option<u64>) -> u64 {
    // alloc: once a scope's wait, which suspends
    let mut named = String::new();
    let awaited = if kind == blaze_graph::wait::WaitKind::Scope {
        named.push_str(awaited);
        named.push_str(" of ");
        if !flight::write_working_on(&mut named) {
            named.push_str("no span");
        }
        named.as_str()
    } else {
        awaited
    };
    let awaited = span::Name::of(awaited);
    span::open_raw(span::Wait { awaits: awaited, function: awaited, owner, on: kind })
}

/// Opens and enters the span `hold` ([`span::Hold`]), its `function` what is held and its
/// `owner` the task holding it (the number a waiter's `owner` names); closed by [`wait_ended`]
/// as a wait's is.
fn hold_began(what: &str, holder: u64) -> u64 {
    span::open_raw(span::Hold { what: span::Name::of(what), holder })
}

/// Leaves and closes the span [`wait_began`] opened, on the thread the wait ended on (where its
/// job's open spans moved, for a job on a fiber).
fn wait_ended(span: u64) {
    span::close_raw(span);
}

/// A worker of `blaze_graph`'s fiber driver going idle or waking, as a switch row of no job.
fn idled(idle: blaze_graph::sched::Idle) {
    use blaze_graph::sched::IdleReason;
    use flight::{Switched, WaitedOn};
    let waited = match idle.reason {
        IdleReason::Nothing => (WaitedOn::Nothing, 0),
        IdleReason::Until(at) => (WaitedOn::Until, at),
    };
    if idle.began {
        // What it folded is written before it sleeps, so a flush has it.
        flight::flush_folds();
    }
    flight::switched(flight::Switch {
        event: if idle.began { Switched::Idle } else { Switched::Woke },
        job: 0,
        fiber: 0,
        core: idle.core.0,
        waited: Some(waited),
    });
}

/// A switch of `blaze_graph`'s fiber driver, as a row of the recorder's.
fn observed(switch: blaze_graph::sched::Switch) {
    use blaze_graph::sched::{SwitchEvent, Wait};
    use flight::{Switched, WaitedOn};
    let (event, waited) = match switch.event {
        SwitchEvent::Began { given } => {
            return flight::job_begins(given.spawner, given.at, given.build, given.creator);
        }
        SwitchEvent::Started => (Switched::Started, None),
        SwitchEvent::Ready => (Switched::Ready, None),
        SwitchEvent::Resumed => (Switched::Resumed, None),
        SwitchEvent::Ended => (Switched::Ended, None),
        SwitchEvent::Ran => (Switched::Ran, None),
        SwitchEvent::Parked(None) => (Switched::Parked, Some((WaitedOn::Moved, 0))),
        SwitchEvent::Parked(Some(on)) => (
            Switched::Parked,
            Some(match on {
                Wait::Job(job) => {
                    (WaitedOn::Job, u64::from(switch.awaited) << 32 | u64::from(job.0))
                }
                Wait::Key(key) => {
                    let [a, b, c, d, e, f, g, h, ..] = key.0;
                    (WaitedOn::Key, u64::from_le_bytes([a, b, c, d, e, f, g, h]))
                }
                Wait::Counter(counter) => (WaitedOn::Counter, u64::from(counter.0)),
                Wait::Until(at) => (WaitedOn::Until, at),
            }),
        ),
    };
    flight::switched(flight::Switch {
        event,
        job: u64::from(switch.generation) << 32 | u64::from(switch.job.0),
        fiber: switch.fiber,
        core: switch.core.0,
        waited,
    });
}

/// What the plane counted of a build that ended: its number and how its nodes settled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Built {
    pub serial: u64,
    /// Nodes the memo settled, running nothing.
    pub hits: u64,
    /// Nodes that ran their task.
    pub misses: u64,
    /// Nodes that took a task another node ran.
    pub joined: u64,
}

/// Notes the end of a build: its counts as [`Plot`]s and a mark on the flight recorder's
/// timeline.
pub fn built(built: Built) {
    plot(Plot::MemoHits, built.hits);
    plot(Plot::MemoMisses, built.misses);
    plot(Plot::Joined, built.joined);
    flight::mark(built.serial);
    flight::flush_folds();
    hot::read::mark();
}

/// The CPU time the calling thread has had, in nanoseconds: `None` where the system can't say.
/// What a driver gives [`flight::count_cpu`].
pub fn thread_cpu() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut at = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `at` is a timespec to write.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut at) } == 0 {
            return Some(at.tv_sec as u64 * 1_000_000_000 + at.tv_nsec as u64);
        }
    }
    None
}

/// The subscriber: a span id is the flight recorder's (the ring and slot holding its open
/// state, [`flight::opened`]), so nothing else is kept per open span.
struct Recorder;

impl Recorder {
    /// A span is kept at [`LEVEL`] or above by [`keeps`]; an event by [`logs::keeps_event`]:
    /// WARN and ERROR of any target, INFO by `keeps`, DEBUG of a `blaze` target that opts in.
    fn wanted(meta: &tracing::Metadata<'_>) -> bool {
        if meta.is_event() {
            logs::keeps_event(meta.target(), meta.name(), *meta.level())
        } else {
            meta.is_span() && *meta.level() <= LEVEL && keeps(meta.target(), meta.name())
        }
    }
}

/// The target a span names to be kept wherever it is written: a span's target is its module's
/// path unless it names one, so a span outside a `blaze*` crate writes `target: "blaze"`.
pub const TARGET: &str = "blaze";

/// The least severe level of a span kept. An event's levels are [`logs::keeps_event`]'s.
pub const LEVEL: tracing::Level = tracing::Level::INFO;

/// Whether a span or event of `target` named `name`, at a level [`keeps_level`] keeps, is kept: a
/// `blaze` one (its target starts with [`TARGET`]) or a rust-analyzer query's span (`*_query`;
/// an event's name, `event <file>:<line>`, is never one). Every other one costs its call and is
/// never seen, as `blaze::unrecorded_span` says where it is written. An event's rule is
/// [`logs::keeps_event`], which asks this one for an INFO event; a kept event is a row of the
/// `events` table.
pub fn keeps(target: &str, name: &str) -> bool {
    target.starts_with(TARGET) || name.ends_with("_query")
}

/// Whether spans of the level `tracing` names `level` (`"info"`, `"DEBUG"`, `"3"`) are kept;
/// false for a name that is no level's.
pub fn keeps_level(level: &str) -> bool {
    level.parse::<tracing::Level>().is_ok_and(|it| it <= LEVEL)
}

/// Whether spans of `meta` are kept; those that are get a site in the flight recorder.
fn wants(meta: &'static tracing::Metadata<'static>) -> bool {
    Recorder::wanted(meta) && flight::register(meta).is_some()
}

/// The id of a span beginning now: [`flight::UNRECORDED`] for a callsite past the flight
/// recorder's table.
fn opened(span: &tracing::span::Attributes<'_>) -> u64 {
    let Some(site) = flight::site(span.metadata()) else { return flight::UNRECORDED };
    flight::opened(span, site)
}

/// Whether the span `id` closed: its last handle was dropped.
pub(crate) fn closed(id: u64) -> bool {
    flight::closed(id).is_some()
}

impl tracing::Subscriber for Recorder {
    fn register_callsite(
        &self,
        meta: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if wants(meta) {
            tracing::subscriber::Interest::always()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        // DEBUG for the events of `blaze` targets that opt in ([`logs::keeps_event`]); each
        // callsite's interest decides the rest once.
        Some(tracing::level_filters::LevelFilter::DEBUG)
    }

    fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
        Self::wanted(meta)
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(opened(span))
    }

    fn clone_span(&self, id: &tracing::span::Id) -> tracing::span::Id {
        flight::cloned(id.into_u64());
        tracing::span::Id::from_u64(id.into_u64())
    }

    fn record(&self, id: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        flight::recorded(id.into_u64(), values);
    }

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if let Some(site) = flight::site(event.metadata()) {
            flight::evented(event, site);
        }
    }

    fn enter(&self, id: &tracing::span::Id) {
        flight::entered(id.into_u64());
    }

    fn exit(&self, id: &tracing::span::Id) {
        flight::exited(id.into_u64());
    }

    fn try_close(&self, id: tracing::span::Id) -> bool {
        closed(id.into_u64())
    }
}

/// The command line of the process, short.
pub(crate) fn command() -> String {
    let mut args = std::env::args();
    let program = args.next().unwrap_or_default();
    let program = Path::new(&program).file_name().map(|it| it.to_string_lossy().into_owned());
    let mut line = program.unwrap_or_default();
    for arg in args {
        if line.len() > 160 {
            line.push_str(" ...");
            break;
        }
        line.push(' ');
        line.push_str(&arg);
    }
    line
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_scopes_wait_names_the_span_its_caller_opened_it_in() {
        use blaze_graph::wait::{WaitOn, wait};
        install();
        let tid = std::thread::spawn(|| {
            let unit = tracing::info_span!(
                target: "blaze",
                "unit codegen",
                krate = "build-script-build",
                unit = "serde v1.0.219 (build script)"
            )
            .entered();
            let par = tracing::info_span!(target: "blaze", "par").entered();
            wait(WaitOn::Scope(13), None, || {});
            drop(par);
            drop(unit);
            wait(WaitOn::Scope(14), None, || {});
            sampler::os_tid() as u32
        })
        .join()
        .unwrap();
        let mut read = flight::Read::default();
        flight::window(Duration::from_secs(60), &mut read);
        let awaits: Vec<_> = read
            .spans
            .iter()
            .filter(|it| {
                it.thread == tid && flight::meta(it.site).map(|it| it.name()) == Some("wait")
            })
            .map(|it| it.get::<span::col::Awaits>().and_then(span::Name::text))
            .collect();
        assert_eq!(
            awaits.iter().map(|it| it.as_deref()).collect::<Vec<_>>(),
            [
                Some("scope 13 of unit codegen serde v1.0.219 (build script)"),
                Some("scope 14 of no span")
            ],
            "the innermost span that names what it is of, past one that names nothing"
        );
    }

    /// What the always-on recorder costs: an info span of the target `blaze` opened, entered,
    /// left and closed, and a plot. The cost of a build is these times the spans and plots it
    /// records (docs/v2/plan.md).
    #[test]
    #[ignore = "a measurement, not a check: run it in release with --ignored --nocapture"]
    fn what_a_span_and_a_plot_cost() {
        const N: u32 = 1_000_000;
        install();
        let before = flight::progress();
        let started = Instant::now();
        for _ in 0..N {
            let _span = tracing::info_span!(target: "blaze", "measured").entered();
        }
        let span_ns = started.elapsed().as_nanos() / u128::from(N);
        assert!(flight::progress() - before >= u64::from(N), "every span was recorded");
        let started = Instant::now();
        for it in 0..N {
            plot(Plot::Queued, u64::from(it));
        }
        let plot_ns = started.elapsed().as_nanos() / u128::from(N);
        println!("a span: {span_ns} ns, a plot: {plot_ns} ns");
    }

    /// What a span with two name fields costs as a kind ([`span`]) and as an ad hoc span whose
    /// fields the visitor matches by name, each opened, entered and closed on one thread; and a
    /// kind whose names were interned once, which is what a site that keeps its `Name`s pays.
    #[test]
    #[ignore = "a measurement, not a check: run it in release with --ignored --nocapture"]
    fn what_a_typed_span_costs_beside_a_visited_one() {
        const N: u32 = 1_000_000;
        install();
        let per = |started: Instant| started.elapsed().as_nanos() / u128::from(N);
        let started = Instant::now();
        for _ in 0..N {
            let _span =
                tracing::info_span!(target: "blaze", "unit bodies", krate = "serde", unit = "serde v1.0.219 (lib)")
                    .entered();
        }
        let visited_ns = per(started);
        let started = Instant::now();
        for _ in 0..N {
            let krate = span::Name::of("serde");
            let _span =
                span::UnitBodies { krate, unit: span::Name::of("serde v1.0.219 (lib)") }.open();
        }
        let typed_ns = per(started);
        let (krate, unit) = (span::Name::of("serde"), span::Name::of("serde v1.0.219 (lib)"));
        let started = Instant::now();
        for _ in 0..N {
            let _span = span::UnitBodies { krate, unit }.open();
        }
        let named_ns = per(started);
        let started = Instant::now();
        for it in 0..N {
            let path = span::Name::of("serde_derive::Serialize");
            span::MacroExpand { path, krate }.open().ended(span::MacroExpanded {
                steps: Some(u64::from(it)),
                peak_bytes: Some(4096),
                outcome: span::Outcome::Ok,
            });
        }
        let ended_ns = per(started);
        println!(
            "a span of two fields, visited: {visited_ns} ns; typed: {typed_ns} ns; typed, names kept: {named_ns} ns; a `macro expand` with its end: {ended_ns} ns"
        );
    }
}
