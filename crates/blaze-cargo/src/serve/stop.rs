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

#![forbid(unsafe_code)]

use std::{io::Read as _, os::fd::AsFd as _, sync::Arc, thread::JoinHandle, time::Duration};

use blaze_core::exit::Exit;
use blaze_profile::{Watchdog, watchdog::Progress};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use super::{
    pulse::{Look, LoopWatch, Pulse},
    signals,
};

/// How long the guard waits for the scheduler's lock to read its waits: a deadlock may hold it.
const WAITS_READ: Duration = Duration::from_secs(2);

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

/// How often the guard looks: ten looks a bound, between 10 ms and 1 s apart.
fn look_every(bound: Duration) -> Duration {
    (bound / 10).clamp(Duration::from_millis(10), Duration::from_secs(1))
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
    loop {
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
    loop {
        if !matches!(over.try_recv(), Err(crossbeam_channel::TryRecvError::Empty)) {
            return false;
        }
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
    format!("{}; {}; {}", blaze_profile::watchdog::running(), waits(WAITS_READ), children()) // alloc: once a report
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
