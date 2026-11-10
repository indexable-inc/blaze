//! The bound on a stop: a daemon whose stop makes no progress for its stall bound (its workers
//! deadlocked, the loop stuck inside the image it writes), or that is sent a second SIGTERM or
//! SIGINT, writes what each thread is inside and what each wait waits on, then ends
//! ([`Exit::StopStalled`]), rather than ignore the SIGTERM that asked it to stop. The stall watchdog's core ([`Watchdog`]) judges the progress,
//! as it does for a build ([`super::daemon`]'s `watch`); this is its driver on a thread of its
//! own, as the loop that drives the other may be the one that is stuck.
//!
//! Before any stop it watches the loop's turns ([`LoopWatch`]): a turn that lasts half the
//! stall bound is reported (what each thread is inside, each wait, and each child not reaped
//! with what waits for it), and a loop that has not turned while no job ended and no query
//! completed for the whole bound ends the daemon ([`Exit::LoopStalled`]). The loop is the
//! one thread that reaps the daemon's children: stuck inside a turn (lane reaphang,
//! 2026-11-03), it left them zombies and its clients waiting, and nothing said so.
//!
//! At each look, every [`super::ceiling::EVERY`] at least, before a stop and during one, it
//! reads the daemon's footprint against its memory ceiling ([`super::ceiling`], judged by
//! [`CeilingWatch`]): past it, the daemon writes what it held by tag and what each thread was
//! inside, and ends ([`Exit::MemoryCeiling`]). A look allocates nothing and takes no lock.
//!
//! At each look too, before a stop and during one, it names the stalls in the wait-for graph
//! of named locks, signals and kernel waits ([`Stalls`], blaze-core's `lock::graph`): once a
//! wait is older than [`STALL_THRESHOLD`], each deadlock and each convoy is said once as a
//! `blaze::stall` event while it lasts, and the top one's line goes in each build's live model
//! (`Now::stall`, through [`Pulse::set_stall`]), so `blaze status` and the build view say it
//! long before the loop's own bound ends the daemon.

#![forbid(unsafe_code)]

use std::{io::Read as _, os::fd::AsFd as _, sync::Arc, thread::JoinHandle, time::Duration};

use blaze_core::{
    exit::Exit,
    lock::{
        graph,
        stall::{Stall, WaitRow},
    },
};
use blaze_profile::{Watchdog, watchdog::Progress};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use super::{
    ceiling::{EASED, Gib, NEAR, STOPPED},
    pulse::{CeilingLook, CeilingWatch, Look, LoopWatch, Pulse, StallWatch},
    signals,
};

/// How long a wait in the wait-for graph (a named lock, a signal, a kernel wait) lasts before
/// the guard looks for the stall behind it. Far under the loop's own bounds (a stuck turn is
/// reported at half the stall bound, 60 s by default, and the daemon ends at the whole of it,
/// [`Exit::LoopStalled`]), so a convoy or a deadlock is named, with who holds what, before the
/// loop watchdog says only that nothing moved. 2 s: longer than any lock should be held (the
/// flight-recorder convoy of 2026-11-05 held 60 threads for minutes; a healthy hold is
/// microseconds) and than a fair queue behind one long hold, short enough that a person
/// watching the build view sees the stall while it is the reason. A daemon run with a stall
/// bound under 8 s takes a quarter of it instead, to stay under its report.
const STALL_THRESHOLD: Duration = Duration::from_secs(2);

/// How long the guard waits for the scheduler's lock to read its waits: a deadlock may hold it.
const WAITS_READ: Duration = Duration::from_secs(2);

/// How long a daemon past its memory ceiling waits for the scheduler's lock to name its waits:
/// its workers go on taking memory meanwhile.
const WAITS_READ_AT_CEILING: Duration = Duration::from_millis(250);

/// What a stop that ended so says first: `blaze serve: pid 7 stop stalled: ...`.
pub const STALLED: &str = " stop stalled: ";

/// What a report of a loop stuck inside a turn says first: `blaze serve: pid 7 loop stuck: ...`.
pub const STUCK: &str = " loop stuck: ";

/// The guard's thread, waiting for a stop, then watching it ([`arm`]).
pub struct Guard {
    done: Sender<()>,
    thread: JoinHandle<()>,
}

impl Guard {
    /// The stop is over: the guard ends, unless the bound ended the process first.
    pub fn disarm(self) {
        let Guard { done, thread } = self;
        drop(done);
        // A guard that panicked has nothing more to say than its panic, which it printed.
        _ = thread.join();
    }
}

/// The guard of the daemon's loop and stop. Until a stop it watches the loop's turns
/// ([`watch_turns`]). Then it waits for SIGTERM or SIGINT, or for the daemon to begin a stop of
/// its own ([`signals::stop_begun`]), and ends the process once neither the worker pool's jobs,
/// the queries nor the loop's progress ([`Pulse::moved`]) moved for `bound`.
pub fn arm(bound: Duration, pulse: Arc<Pulse>) -> anyhow::Result<Guard> {
    use anyhow::Context as _;
    let pipe = signals::stop_pipe().context("making the pipe the stop's guard waits on")?;
    let (done, over) = crossbeam_channel::bounded(1);
    let thread = std::thread::Builder::new()
        .name("blaze-stop-guard".to_owned()) // alloc: once a daemon
        .spawn(move || guard(pipe, bound, &pulse, &over))
        .context("starting the stop's guard")?;
    Ok(Guard { done, thread })
}

/// How often the guard looks: ten looks a bound, between 10 ms and 1 s apart, and at least
/// every [`super::ceiling::EVERY`], the footprint's watch.
fn look_every(bound: Duration) -> Duration {
    (bound / 10).clamp(Duration::from_millis(10), Duration::from_secs(1)).min(super::ceiling::EVERY)
}

fn now_ns() -> u64 {
    u64::try_from(blaze_profile::clock::mono_ns()).unwrap_or(0)
}

fn guard(pipe: std::fs::File, bound: Duration, pulse: &Pulse, over: &Receiver<()>) {
    if !watch_turns(pipe, bound, pulse, over) {
        return;
    }
    let mut watchdog = Watchdog::new(bound);
    let pool = blaze_graph::exec::shared();
    let every = look_every(bound);
    let mut memory = Memory::default();
    let mut stalls = Stalls::new(bound);
    let pid = std::process::id();
    loop {
        // A stop that writes its image takes memory too: held to the ceiling as before it.
        memory.look(pid);
        // And may stall on a lock: named as before it.
        stalls.look(pid, pulse);
        let progress = Progress {
            jobs: pool.ended(),
            queries: blaze_query::db::completed(),
            steps: pulse.moved(),
            working: 0,
        };
        let now = now_ns();
        let quiet = watchdog.tick(now, progress, true);
        let again = signals::stops_asked() >= 2;
        if quiet.is_some() || again {
            let why = match quiet {
                Some(quiet) => format!(
                    "nothing moved for {} s, its bound ({})",
                    quiet.for_ns / 1_000_000_000,
                    quiet.at
                ), // alloc: once, as the process ends
                None => format!("asked to stop again ({progress})"), // alloc: as above
            };
            let said = stalled(std::process::id(), &why);
            tracing::error!(target: "blaze", "{said}");
            eprintln!("{said}");
            signals::exit_now(Exit::StopStalled);
        }
        let _wait = tracing::info_span!(target: "blaze", "watch the stop", awaits = "the daemon's stop to end").entered();
        match over.recv_timeout(every) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Watches the loop's turns until a stop: SIGTERM or SIGINT, or the daemon's own stop, as a
/// byte on `pipe`. A turn that lasts half of `bound` is reported once ([`LoopWatch`]); a loop
/// that has not turned while the worker pool's jobs and the queries stood still too, for the
/// whole `bound` ([`Watchdog`]), is deadlocked, and the daemon ends ([`Exit::LoopStalled`])
/// rather than leave its children unreaped and its clients waiting for ever. `true` once a stop
/// began; `false` once the guard is disarmed ([`Guard::disarm`]) with no stop, or when the pipe
/// can't be read, so no stop is bounded.
fn watch_turns(
    mut pipe: std::fs::File,
    bound: Duration,
    pulse: &Pulse,
    over: &Receiver<()>,
) -> bool {
    let mut turning = LoopWatch::new(bound / 2);
    let mut watchdog = Watchdog::new(bound);
    let pool = blaze_graph::exec::shared();
    let every = look_every(bound);
    let pid = std::process::id();
    let mut memory = Memory::default();
    let mut stalls = Stalls::new(bound);
    loop {
        if !matches!(over.try_recv(), Err(crossbeam_channel::TryRecvError::Empty)) {
            return false;
        }
        memory.look(pid);
        stalls.look(pid, pulse);
        let now = now_ns();
        let turns = pulse.turns();
        let look = turning.look(now, turns);
        if let Look::Stuck { for_ns, turns, .. } = look {
            let said = stuck(pid, for_ns, turns);
            tracing::error!(target: "blaze", "{said}");
            eprintln!("{said}");
        }
        let progress = Progress {
            jobs: pool.ended(),
            queries: blaze_query::db::completed(),
            steps: turns,
            working: 0,
        };
        if let Some(quiet) = watchdog.tick(now, progress, look != Look::Turning) {
            // alloc: once, as the process ends
            let why = format!(
                "the loop has not finished a turn and nothing moved for {} s, its bound ({}): it ends with exit status {}",
                quiet.for_ns / 1_000_000_000,
                quiet.at,
                Exit::LoopStalled
            );
            let said = stuck_line(pid, &why);
            tracing::error!(target: "blaze", "{said}");
            eprintln!("{said}");
            signals::exit_now(Exit::LoopStalled);
        }
        let _wait = tracing::info_span!(
            target: "blaze",
            "wait for a stop",
            awaits = "SIGTERM, SIGINT or the daemon's own stop, or the next look at the loop's turns"
        )
        .entered();
        match crate::raw::process::readable_within(pipe.as_fd(), every) {
            Ok(false) => {}
            Ok(true) => {
                // Readable: a byte is there (or the write end closed, which it never is), so
                // the read does not wait.
                let mut byte = [0u8; 1];
                match pipe.read(&mut byte) {
                    Ok(_) => return true,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return unread(&error),
                }
            }
            Err(error) => return unread(&error),
        }
    }
}

/// The guard's watch of the wait-for graph: the driver of [`StallWatch`]. Each look reads the
/// oldest wait in progress (one pass over the registry, under its lock, allocating nothing);
/// only past the threshold does it take the graph's snapshot and analysis
/// ([`graph::stalls`]), say each stall new since the last look ([`said`]) and put the top
/// one's line in the pulse for the loop's live models.
#[derive(Debug)]
struct Stalls {
    watch: StallWatch,
    /// [`STALL_THRESHOLD`], or a quarter of a shorter stall bound.
    threshold_ns: u64,
    /// The pulse holds a line from an earlier look, to clear once no stall lasts.
    named: bool,
    /// No lock probe is installed, and the guard said so once.
    unprobed: bool,
}

impl Stalls {
    fn new(bound: Duration) -> Stalls {
        let threshold = STALL_THRESHOLD.min(bound / 4);
        Stalls {
            watch: StallWatch::default(),
            threshold_ns: u64::try_from(threshold.as_nanos()).unwrap_or(u64::MAX),
            named: false,
            unprobed: false,
        }
    }

    /// One look. The graph's times are on the lock probe's clock (blaze-profile installs it in
    /// a daemon), so it is read from the probe; without one no wait is timed, and the guard
    /// says once that it names no stall rather than guess at a clock.
    fn look(&mut self, pid: u32, pulse: &Pulse) {
        let Some(probe) = blaze_core::lock::installed() else {
            if !self.unprobed {
                self.unprobed = true;
                tracing::error!(target: "blaze", "blaze serve: pid {pid}: {UNPROBED}");
                eprintln!("blaze serve: pid {pid}: {UNPROBED}");
            }
            return;
        };
        let now = probe.now_ns();
        let past = graph::oldest_since_ns()
            .is_some_and(|since| now.saturating_sub(since) >= self.threshold_ns);
        // `Vec::new` allocates nothing: a look with no wait past the threshold stays free.
        let stalls = if past { graph::stalls(now, self.threshold_ns) } else { Vec::new() };
        for at in self.watch.look(&stalls) {
            said(pid, &stalls[at], now);
        }
        match stalls.first() {
            Some(top) => {
                pulse.set_stall(Some(top.line(now)));
                self.named = true;
            }
            None if self.named => {
                pulse.set_stall(None);
                self.named = false;
            }
            None => {}
        }
    }
}

/// What a guard with no lock probe to time waits by says, once.
const UNPROBED: &str = "no lock probe is installed (blaze-profile installs it), so the stop's guard \
     times no wait in the wait-for graph and names no deadlock or convoy";

/// Says one stall as the `blaze::stall` event the profile's `stalls` view reads, and on the
/// daemon's stderr: `kind` (`deadlock` or `convoy`), its `line` ([`Stall::line`]), the convoy's
/// `head` thread (0 for a deadlock), how many `waiters` (a convoy's waits behind its head, a
/// deadlock's threads in its cycle), the `threads` by mark, comma-separated (a deadlock's
/// cycle in its order; a convoy's head, then its oldest waiter) and `oldest_ns`, how long the
/// oldest of its waits has waited, on the probe's clock.
fn said(pid: u32, stall: &Stall, now_ns: u64) {
    use std::fmt::Write as _;
    let age = |row: &WaitRow| now_ns.saturating_sub(row.since_ns);
    let mut threads = String::new(); // alloc: a stall's report, once a stall
    let (kind, head, waiters, oldest_ns) = match stall {
        Stall::Deadlock { cycle } => {
            for (at, row) in cycle.iter().enumerate() {
                let comma = if at == 0 { "" } else { "," };
                _ = write!(threads, "{comma}{}", row.thread);
            }
            let oldest = cycle.iter().map(age).max().unwrap_or(0);
            ("deadlock", 0, cycle.len() as u64, oldest)
        }
        Stall::Convoy { head, waiters, oldest, .. } => {
            _ = write!(threads, "{head},{}", oldest.thread);
            ("convoy", *head, *waiters as u64, age(oldest))
        }
    };
    let line = stall.line(now_ns);
    tracing::warn!(
        target: "blaze::stall",
        kind,
        line = %line,
        head,
        waiters,
        threads = %threads,
        oldest_ns,
        "stall"
    );
    eprintln!("blaze serve: pid {pid} stall: {line}");
}

/// What a guard that can't read the daemon's footprint says, once until it reads it again.
const UNREAD: &str = "its footprint is unread, so its own watch holds it to no memory ceiling until \
     a read comes back (the kernel's limit, where its ceiling's line names one, still holds it)";

/// The guard's watch of the daemon's footprint against its memory ceiling: the driver of
/// [`CeilingWatch`], which reads the ceiling ([`super::ceiling::ceiling`]) and the footprint as
/// the system counts it ([`crate::raw::memory::own_footprint`], as `blaze_mem::tag::resident_by`
/// reads it for admission), says a footprint near the ceiling or eased from it once, and ends
/// the daemon past it ([`passed`]).
#[derive(Debug, Default)]
struct Memory {
    watch: CeilingWatch,
    /// The last look could not read the footprint, and said so.
    unread: bool,
}

impl Memory {
    /// One look. Allocates nothing and takes no lock on a look that finds the footprint under
    /// the ceiling and no nearer or further than before (an atomic's load and one read of the
    /// kernel's: `proc_pid_rusage` on macOS; on Linux the daemon's cgroup's `memory.current`
    /// and `memory.stat`, or `/proc/self/statm` where it has none, into buffers of the stack);
    /// a line is said only at a crossing.
    fn look(&mut self, pid: u32) {
        let ceiling = super::ceiling::ceiling();
        if ceiling == 0 {
            return;
        }
        let Some(footprint) = crate::raw::memory::own_footprint() else {
            if !self.unread {
                self.unread = true;
                tracing::error!(target: "blaze", "blaze serve: pid {pid}: {UNREAD}");
                eprintln!("blaze serve: pid {pid}: {UNREAD}");
            }
            return;
        };
        self.unread = false;
        let mut look = self.watch.look(footprint, ceiling);
        let mut footprint = footprint;
        if look == CeilingLook::Purge {
            // Past the ceiling: what the allocators keep freed goes back first, and only a
            // footprint still past it after that ends the daemon. The purge reads it once every
            // worker gave back its heaps' free pages (or its wait passed), on this thread, the
            // guard's, never the loop's: a reading at once still held what they kept.
            let purged = crate::raw::memory::purge();
            footprint = purged.after.unwrap_or(footprint);
            look = self.watch.look(footprint, ceiling);
            tracing::warn!(
                target: "blaze",
                "blaze serve: pid {pid} passed its memory ceiling of {ceiling} bytes and purged what its allocators kept freed: {footprint} bytes after, {} returned, workers' give-back {:?}",
                purged.returned(),
                purged.settle,
            );
        }
        let (at, of) = (Gib(footprint), Gib(ceiling));
        let line = match look {
            CeilingLook::Under => return,
            CeilingLook::Near => NEAR,
            CeilingLook::Eased => EASED,
            // A purge gave back too little: the second look ends it.
            CeilingLook::Purge | CeilingLook::Passed => passed(pid, footprint, ceiling),
        };
        // A crossing's line, once a crossing: written straight to stderr, which buffers nothing.
        tracing::warn!(
            target: "blaze",
            "blaze serve: pid {pid}{line}{footprint} bytes of {ceiling} bytes ({at} of {of} GiB)"
        );
        eprintln!("blaze serve: pid {pid}{line}{footprint} bytes of {ceiling} bytes ({at} of {of} GiB)");
    }
}

/// The daemon's footprint, `footprint` bytes, passed its ceiling, `ceiling`: it writes its
/// postmortem to its log first (what it holds by tag, as its memory line says it, then the
/// ceiling, the footprint, what each thread is inside, the scheduler's waits and the children
/// not reaped), then ends with [`Exit::MemoryCeiling`], which the client a request hung up on
/// reads from that line and says as a failure naming the ceiling (`cli::daemon::Died`).
fn passed(pid: u32, footprint: u64, ceiling: u64) -> ! {
    super::daemon::log_memory(&blaze_mem::tag::snapshot());
    // alloc: once, as the process ends
    let said = format!(
        "blaze serve: pid {pid}{STOPPED}{footprint} bytes passed {ceiling} bytes ({} of {} GiB): it ends with exit status {}; {}",
        Gib(footprint),
        Gib(ceiling),
        Exit::MemoryCeiling,
        inside_within(WAITS_READ_AT_CEILING),
    );
    tracing::error!(target: "blaze", "{said}");
    eprintln!("{said}");
    signals::exit_now(Exit::MemoryCeiling);
}

/// Without its pipe the guard hears no stop: said, and the stop runs unbounded.
fn unread(error: &std::io::Error) -> bool {
    tracing::error!(target: "blaze", "the stop's guard can't read its pipe, so no stop of this daemon is bounded: {error}");
    eprintln!(
        "blaze serve: the stop's guard can't read its pipe, so no stop of this daemon is bounded: {error}"
    );
    false
}

/// The report of a turn that has lasted `for_ns`, the loop having finished `turns` turns.
fn stuck(pid: u32, for_ns: u64, turns: u64) -> String {
    // alloc: once a stuck turn
    let why = format!(
        "the loop has not finished a turn for {} s (after {turns} turns), so nothing reaps its children or hands on what completed",
        for_ns / 1_000_000_000
    );
    stuck_line(pid, &why)
}

/// `blaze serve: pid 7 loop stuck: why; inside`.
fn stuck_line(pid: u32, why: &str) -> String {
    format!("blaze serve: pid {pid}{STUCK}{why}; {}", inside()) // alloc: once a stuck turn
}

/// The line a stop that did not end says as it ends the daemon: `why` (how long nothing moved
/// and where the counts stayed, or a second signal), then [`inside`].
fn stalled(pid: u32, why: &str) -> String {
    // alloc: once, as the process ends
    format!(
        "blaze serve: pid {pid}{STALLED}{why}: it ends with exit status {} without finishing its stop; {}",
        Exit::StopStalled,
        inside(),
    )
}

/// What a stuck daemon is inside: what each thread is inside, what each of the scheduler's
/// waits waits on, and each child not reaped with what waits for it ([`children`]).
fn inside() -> String {
    inside_within(WAITS_READ)
}

/// [`inside`], the scheduler's waits read within `within`.
fn inside_within(within: Duration) -> String {
    format!("{}; {}; {}", blaze_profile::watchdog::running(), waits(within), children()) // alloc: once a report
}

/// Each child started and not reaped, its state and what waits for it: a zombie whose waiter is
/// a spawn effect says the loop that polls its driver is stuck.
fn children() -> String {
    let unreaped = blaze_core::children::unreaped();
    if unreaped.is_empty() {
        return "children not reaped: none".to_owned(); // alloc: once a report
    }
    let mut said = format!("{} children not reaped:", unreaped.len()); // alloc: once a report
    for (at, child) in unreaped.iter().enumerate() {
        let sep = if at == 0 { " " } else { "; " };
        said.push_str(sep);
        said.push_str(&child.to_string()); // alloc: once a report
    }
    said
}

/// The scheduler's waits, read on a thread of their own: [`blaze_graph::sched::waits_now`] takes
/// the scheduler's lock, which a deadlock may hold, so at most `within` is waited for it.
fn waits(within: Duration) -> String {
    let (said, heard) = crossbeam_channel::bounded(1);
    let started = std::thread::Builder::new()
        .name("blaze-stop-waits".to_owned()) // alloc: once, as the process ends
        .spawn(move || drop(said.send(blaze_graph::sched::waits_now().to_string())));
    if let Err(error) = started {
        return format!("the scheduler's waits: unread, as their thread did not start: {error}"); // alloc: as above
    }
    let _wait = tracing::info_span!(target: "blaze", "read the scheduler's waits", awaits = "the scheduler's lock").entered();
    match heard.recv_timeout(within) {
        Ok(waits) => waits,
        Err(_) => {
            format!("the scheduler's waits: unread, its lock not had in {} s", within.as_secs())
        } // alloc: as above
    }
}

#[cfg(test)]
mod tests {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    use super::*;

    const CHILD: &str = "BLAZE_TEST_STOP_GUARD";
    const NAME: &str = "serve::stop::tests::a_deadlocked_daemon_ends_on_sigterm_within_its_bound_naming_what_it_waits_on";

    /// The daemon of the test, in a child process: its guard armed with the bound `CHILD` says
    /// (in ms), the loop turning with its progress never moving, and a worker inside a span it
    /// never leaves, as the macro sessions' deadlock held 18 workers (selfbuild4 group 1); it
    /// says it is ready on stdout and waits to be signalled.
    fn deadlocked(bound_ms: u64) -> ! {
        blaze_profile::install();
        signals::catch();
        let pulse = Arc::new(Pulse::default()); // alloc: once, the test's
        let guard = arm(Duration::from_millis(bound_ms), Arc::clone(&pulse)).unwrap(); // clone: a handle
        let entered = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("worker deadlocked".to_owned())
                .spawn_scoped(scope, || {
                    let _held = tracing::info_span!(target: "blaze", "hold the test's lock", awaits = "the lock no one lets go").entered();
                    entered.wait();
                    loop {
                        std::thread::park();
                    }
                })
                .unwrap();
            entered.wait();
            println!("ready");
            // The loop turns, idle, its progress never moving: the stop's bound alone judges.
            loop {
                pulse.turned(0);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        guard.disarm();
        unreachable!("the deadlocked daemon's threads never end");
    }

    /// Runs the child with `bound_ms`, sends it `signals` SIGTERMs, and gives what it said on
    /// stderr and its exit code, waiting at most `within` from the first signal.
    fn signalled(
        bound_ms: u64,
        signals: usize,
        within: Duration,
    ) -> (String, Option<i32>, Duration) {
        use std::io::BufRead as _;
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--nocapture", "--test-threads", "1"])
            .env(CHILD, bound_ms.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while !line.contains("ready") {
            line.clear();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "the child ended before it was ready"
            );
        }
        let pid = child.id().to_string();
        let sent = Instant::now();
        for _ in 0..signals {
            let killed = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
            assert!(killed.success(), "kill -TERM {pid}: {killed}");
            std::thread::sleep(Duration::from_millis(50));
        }
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if sent.elapsed() > within {
                _ = child.kill();
                panic!("the deadlocked daemon did not end within {within:?} of SIGTERM");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let took = sent.elapsed();
        let mut said = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut said).unwrap();
        (said, status.code(), took)
    }

    /// A daemon whose workers are deadlocked ignored SIGTERM and was SIGKILLed with no report
    /// (selfbuild4 group 15). Its stop's guard ends it once nothing moved for its bound, or at
    /// a second signal at once, and says first which thread is inside what. Remove the guard's
    /// exit, or its wake on the signal, and the child outlives the test's wait.
    #[test]
    fn a_deadlocked_daemon_ends_on_sigterm_within_its_bound_naming_what_it_waits_on() {
        if let Some(bound) = std::env::var_os(CHILD) {
            deadlocked(bound.to_str().unwrap().parse().unwrap());
        }
        let (said, code, took) = signalled(500, 1, Duration::from_secs(30));
        assert_eq!(code, Some(Exit::StopStalled.code()), "{said}");
        assert!(took >= Duration::from_millis(500), "it waited for its bound: {took:?}");
        for named in [
            STALLED,
            "nothing moved for",
            "worker deadlocked",
            "hold the test's lock",
            "wait graph",
        ] {
            assert!(said.contains(named), "it says {named:?}:\n{said}");
        }
        // A bound past the test's wait: the second signal alone ends it.
        let (said, code, took) = signalled(600_000, 2, Duration::from_secs(30));
        assert_eq!(code, Some(Exit::StopStalled.code()), "{said}");
        assert!(took < Duration::from_secs(30), "{took:?}");
        assert!(
            said.contains("asked to stop again") && said.contains("worker deadlocked"),
            "{said}"
        );
    }

    const STUCK_CHILD: &str = "BLAZE_TEST_LOOP_STUCK";
    const STUCK_NAME: &str = "serve::stop::tests::a_loop_stuck_in_a_turn_is_reported_with_its_unreaped_children_and_ends";

    /// The daemon of the stuck-loop test, in a child process: its driver started a child (a
    /// spawn effect tagged 0x5ca1ab1e, as a build script or sandbox launcher is) and is never
    /// polled again, as the daemon's loop parked inside a write's maintain was (lane reaphang,
    /// 2026-11-03), so the child exits and stays a zombie; the loop's turns never move.
    fn stuck(bound_ms: u64) -> ! {
        use blaze_core::Driver as _;
        blaze_profile::install();
        let root = tempfile::tempdir().unwrap();
        let mut driver = blaze_core::native(root.path(), 1).unwrap();
        let command = Box::new(blaze_core::Command {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            env: Vec::new(),
            dir: "/".into(),
            stdin: Vec::new(),
            sandbox: blaze_core::Sandbox::Off,
            memory: None,
        });
        driver.submit(&mut vec![blaze_core::Effect::Spawn {
            command,
            tag: blaze_core::Tag(0x5ca1_ab1e),
        }]);
        let _guard = arm(Duration::from_millis(bound_ms), Arc::default()).unwrap();
        println!("ready");
        // The turn that never ends: the driver, held, is never polled.
        loop {
            std::thread::park();
        }
    }

    /// A loop stuck inside a turn left its children zombies and its clients at "Compiling",
    /// and said nothing: its watchdog, memory log and dump all run between turns. The guard,
    /// on its own thread, says at half the bound that the loop has not turned, naming each
    /// child not reaped and the effect that waits for it, and ends the daemon at the bound as
    /// nothing moved. Make the guard's look never judge the loop stuck (`LoopWatch::look`
    /// never `Stuck`, or `watch_turns` a plain read of the pipe, as before) and the child
    /// outlives the test's wait, its zombie unnamed.
    #[test]
    fn a_loop_stuck_in_a_turn_is_reported_with_its_unreaped_children_and_ends() {
        use std::io::BufRead as _;
        if let Some(bound) = std::env::var_os(STUCK_CHILD) {
            stuck(bound.to_str().unwrap().parse().unwrap());
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([STUCK_NAME, "--exact", "--nocapture", "--test-threads", "1"])
            .env(STUCK_CHILD, "1000")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        while !line.contains("ready") {
            line.clear();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "the child ended before it was ready"
            );
        }
        let ready = Instant::now();
        let within = Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if ready.elapsed() > within {
                _ = child.kill();
                panic!("the daemon whose loop is stuck did not end within {within:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut said = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut said).unwrap();
        assert_eq!(status.code(), Some(Exit::LoopStalled.code()), "{said}");
        for named in [
            STUCK,
            "has not finished a turn for",
            "nothing moved for",
            "/bin/sh",
            "exited and not reaped",
            "0x5ca1ab1e",
        ] {
            assert!(said.contains(named), "it says {named:?}:\n{said}");
        }
    }
}
