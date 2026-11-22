//! `blaze run`'s parent loop (docs/v2/plan/design/run.md, "Pieces" item 6; bottom.md section
//! 11): the program started in a process of its own under the launcher's run mode
//! ([`run_program`]), each first call answered through the [`Resolve`] seam
//! ([`crate::run_parent::Lazy`]: one step of the record walk, the record filed before it is
//! published) ([`drive`]), until it ends; its status is `blaze run`'s ([`status_of`]).
//!
//! What the parent forwards: nothing but the answers. The program's standard input, output and
//! error are the command's own ([`Streams::INHERIT`]: `blaze run | consumer` sees the
//! program's output with nothing between); signals from the terminal reach the program as
//! under any shell (it stays in the command's process group: `own_group: false`).
//!
//! The loop, each turn: read the channel up to [`TICK`] for a whole message; serve every whole
//! message there ([`Needs::serve`]) and write the answers by [`WRITE_BOUND`]; then look at the
//! child (at once while the channel is open, up to [`TICK`] once it closed). The run's own
//! deadline (`Bounds::until`) passing kills the child and is [`LoopError::Deadline`]; `blaze
//! run`'s has none (D103: [`Bounds::run`], the one wait rule 18 exempts, named by its span in
//! own_process.rs), and `blaze test` will give each test one.
//!
//! Signals (D103): from just after the start until [`run_program`] returns, the parent ignores
//! SIGINT and SIGQUIT (they reach the program from the terminal: it is in the command's group)
//! and catches SIGTERM ([`Held`]; its drop puts the dispositions back on every path). Each
//! turn first asks the [`ParentSignals`] for a SIGTERM; the first is forwarded to the program
//! once ([`RunChild::terminate`]), its end is then waited for [`Bounds::grace`] (every wait
//! capped by [`Relay::bound`], so the kill is on time), and a program still running then is
//! killed: its status is the kill's (137). A child
//! that closes its channel inside a message (died mid-message) is read to its end and its
//! status returned, with the tear in [`Ran::torn`], never dropped. A write that finds the
//! child's end closed stops the serving and waits for its end.
//!
//! A child never outlives this process: [`ParentDeath::STRONGEST`] (Linux: the kernel's
//! `SIGKILL` when the starting thread ends; macOS: the launcher ends when its descriptor 3
//! hangs up, at its next first call (`ASK_BOUND`): a program that never calls again lives on,
//! the launcher's to close), and the native child's drop kills and reaps it on every return
//! and unwind.

use core::alloc::Allocator;
use core::fmt;
use core::time::Duration;

use blaze_launch::wire::LONGEST_FRAME;
use blaze_own::Alloc;
use bstd::Cx;
use bstd::Deadline;
use blaze_screen::Step;
use bstd::procs::{ChannelError, Exit, GRACE, Held, Message, ParentDeath, ParentSignals, Relay, RelayAct, RunChild, RunError, RunFault, RunSpec, SignalError, Spawner, Started, Streams, Waited};

use crate::run_parent::{Needs, ParentError, Resolve, TICK, WRITE_BOUND, status_of};

/// The loop's bounds: how long one read waits before the child is looked at, how long a write
/// of answers may take, the run's own deadline, and how long a program asked to end by a
/// forwarded SIGTERM has before it is killed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    pub tick: Duration,
    pub write: Duration,
    pub until: Deadline,
    pub grace: Duration,
}

impl Bounds {
    /// `blaze run`'s: [`TICK`], [`WRITE_BOUND`], NO deadline on the program (D103: its
    /// lifetime is the user's; every step of the parent's own keeps its bound), and [`GRACE`].
    pub(crate) fn run() -> Bounds {
        Bounds { tick: TICK, write: WRITE_BOUND, until: Deadline::NEVER, grace: GRACE }
    }
}

/// How a served run ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Ran {
    /// `blaze run`'s status: the program's exit status, or 128 + the signal that ended it.
    pub status: u8,
    /// Whole messages served.
    pub served: u64,
    /// Where the child closed its channel inside a message: (bytes that came, bytes the
    /// message has).
    pub torn: Option<(usize, usize)>,
    /// A SIGTERM sent to the parent was forwarded to the program (D103): its end, by the
    /// signal, its own handling, or the kill after the grace, is `status`.
    pub terminated: bool,
}

/// What the launcher is started as: the launcher's path, the program's `argv[0]` (the binary
/// target's name), its arguments, its environment ([`crate::run_parent::launch_env`]) and its
/// directory.
pub(crate) struct Launch<'a> {
    pub launcher: &'a str,
    pub arg0: &'a str,
    pub args: &'a [&'a str],
    pub env: &'a [(&'a str, &'a str)],
    pub dir: &'a str,
}

/// Why the loop stopped without the program's status.
#[derive(Debug)]
pub enum LoopError {
    /// The child could not be started (the launcher's spawn): own_process.rs names the launcher
    /// it tried and whether the file is there.
    Start(RunError),
    /// A step on the started child failed.
    Child(RunError),
    /// A need could not be answered (a bad message, a slot never declared, the table full).
    Parent(ParentError),
    /// The run's deadline passed at `at`; the child was killed and ended as `killed` (a kill
    /// reaps: always an exit or a signal).
    Deadline { at: Duration, killed: Exit },
    /// The parent's own signal dispositions could not be set (D103); the child was killed.
    Signals(SignalError),
}

impl fmt::Display for LoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoopError::Start(error) => write!(f, "blaze run: the program's process did not start: {error}"),
            LoopError::Child(error) => write!(f, "blaze run: the program's process: {error}"),
            LoopError::Parent(error) => write!(f, "blaze run: answering the program: {error}"),
            LoopError::Deadline { at, killed } => {
                write!(f, "blaze run: the run's deadline passed at {at:?}; the program was killed ({killed:?})")
            }
            LoopError::Signals(error) => write!(f, "blaze run: {error}; the program was killed"),
        }
    }
}

impl core::error::Error for LoopError {}

/// A bug a test plants in [`drive_with`]: [`Plant::None`] but in the test's control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// After serving, the whole inbox is cleared: the start of a next message that came in the
    /// same read is lost.
    DropPartial,
    /// A SIGTERM kills the program at once instead of forwarding it: a program that would
    /// shut down cleanly is never let.
    KillAtOnce,
    /// The waits after a forward are not capped by the grace's end: the kill comes up to a
    /// tick late.
    GraceUncapped,
}

/// The program started under the launcher as `launch` says, and served until it ends, with no
/// deadline on it (D103: [`Bounds::run`]) and the parent's signals held from just after the
/// start to the return ([`Held`]). Held after the start, not before: an ignored disposition
/// survives `execve` and the start keeps it (blaze-core spawn.rs), so the program would never
/// see its Ctrl-C (blaze-core run_signals.rs, the module's doc, names the window this leaves).
/// `ahead` is written to the program right after the start, before any of its messages is
/// read: today's start list placed (`Needs::place_ahead`), or nothing. Each need's symbol not
/// resolved yet is asked of `resolve`.
pub(crate) fn run_program<'a, R: Resolve + ?Sized>(
    cx: &Cx,
    spawner: &Spawner,
    launch: &Launch<'_>,
    needs: &mut Needs<'a>,
    resolve: &mut R,
    ahead: &[u8],
    alloc: Alloc<'a>,
) -> Result<Ran, LoopError> {
    let spec = RunSpec {
        program: launch.launcher,
        arg0: Some(launch.arg0),
        args: launch.args,
        env: launch.env,
        dir: launch.dir,
        streams: Streams::INHERIT,
        own_group: false,
        parent_death: ParentDeath::STRONGEST,
    };
    // The live line erased before the program can write (blaze-screen run_line.rs).
    resolve.watch().see(Step::Started);
    // Inherited streams: no pipes are given.
    let Started { child, pipes: _ } = spawner.start_served(cx, &spec).map_err(LoopError::Start)?;
    // A hold refused returns here: the child's drop kills and reaps it (bstd `Run`).
    let mut held = Held::hold().map_err(LoopError::Signals)?;
    let mut inbox = std::vec::Vec::new_in(alloc);
    let mut out = std::vec::Vec::new_in(alloc);
    let ran = drive(child, needs, resolve, ahead, &Bounds::run(), &mut held, &mut inbox, &mut out);
    // The dispositions back (on an unwind, the drop does the same), then the program's end.
    drop(held);
    ran
}

/// `child` served from `needs` (asking `resolve`) until it ends, `ahead` written first, the
/// parent's SIGTERM read from `signals`: see the module's doc. The child is taken: its wait
/// and its kill take it in turn, so nothing steps it after its end.
#[expect(clippy::too_many_arguments, reason = "the loop's edge: each argument one handle or buffer of it")]
pub(crate) fn drive<C: RunChild, S: ParentSignals, R: Resolve + ?Sized, A: Allocator>(
    child: C,
    needs: &mut Needs<'_>,
    resolve: &mut R,
    ahead: &[u8],
    bounds: &Bounds,
    signals: &mut S,
    inbox: &mut std::vec::Vec<u8, A>,
    out: &mut std::vec::Vec<u8, A>,
) -> Result<Ran, LoopError> {
    drive_with(child, needs, resolve, ahead, bounds, signals, inbox, out, Plant::None)
}

#[expect(clippy::too_many_arguments, reason = "`drive` and the test's plant")]
fn drive_with<C: RunChild, S: ParentSignals, R: Resolve + ?Sized, A: Allocator>(
    mut child: C,
    needs: &mut Needs<'_>,
    resolve: &mut R,
    ahead: &[u8],
    bounds: &Bounds,
    signals: &mut S,
    inbox: &mut std::vec::Vec<u8, A>,
    out: &mut std::vec::Vec<u8, A>,
    plant: Plant,
) -> Result<Ran, LoopError> {
    let mut open = true;
    let mut served = 0u64;
    let mut torn = None;
    let mut relay = Relay::new(bounds.grace);
    // Today's start list, before any of the program's messages is read (run.md D124).
    if !ahead.is_empty() {
        let by = Deadline::after(child.now(), bounds.write).min(bounds.until);
        match child.send(ahead, by) {
            Ok(()) => {}
            // The child's end closed before it read them: its end is next.
            Err(RunError { fault: RunFault::Channel(ChannelError::Hangup), .. }) => open = false,
            Err(error) => return Err(LoopError::Child(error)),
        }
    }
    loop {
        let now = child.now();
        if bounds.until.passed(now) {
            let killed = child.kill().map_err(LoopError::Child)?;
            return Err(LoopError::Deadline { at: now, killed: killed.exit });
        }
        // D103: a SIGTERM to the parent ends the program first. Forwarded once; then the
        // program's end is waited for the grace (the parent's own bounded step); then a kill,
        // whose end is the run's status. SIGINT and SIGQUIT never show here: ignored while held.
        let terminate = signals.take_terminate(now);
        match (relay.turn(now, terminate), plant) {
            (RelayAct::Nothing, _) => {}
            (RelayAct::Kill, _) | (RelayAct::Forward, Plant::KillAtOnce) => {
                // A kill reaps: its end is always known.
                let killed = child.kill().map_err(LoopError::Child)?;
                return Ok(Ran { status: status_of(killed.exit), served, torn, terminated: true });
            }
            (RelayAct::Forward, _) => child.terminate().map_err(LoopError::Child)?,
        }
        let grace_end = match plant {
            Plant::GraceUncapped => Deadline::NEVER,
            Plant::None | Plant::DropPartial | Plant::KillAtOnce => relay.bound(),
        };
        let tick = Deadline::after(now, bounds.tick).min(bounds.until).min(grace_end);
        if open {
            match child.recv_message(inbox, LONGEST_FRAME, tick) {
                Ok(Message::Whole { .. }) => {
                    // The program is blocked on its need while it is served: the live line may
                    // show, and is erased before the answer frees it.
                    resolve.watch().see(Step::Waits);
                    let taken = needs.serve(resolve, &inbox[..], out).map_err(LoopError::Parent)?;
                    resolve.watch().see(Step::Answers);
                    served += count(&inbox[..taken]);
                    match plant {
                        // The served prefix out, the rest kept in order (`drain` wants the
                        // nightly-only allocator trait of `A`).
                        Plant::None | Plant::KillAtOnce | Plant::GraceUncapped => {
                            let left = inbox.len() - taken;
                            inbox.copy_within(taken.., 0);
                            inbox.truncate(left);
                        }
                        Plant::DropPartial => inbox.clear(),
                    }
                    if !out.is_empty() {
                        let by = Deadline::after(child.now(), bounds.write).min(bounds.until);
                        match child.send(&out[..], by) {
                            Ok(()) => {}
                            // The child's end closed while it was answered: its end is next.
                            Err(RunError { fault: RunFault::Channel(ChannelError::Hangup), .. }) => open = false,
                            Err(error) => return Err(LoopError::Child(error)),
                        }
                        out.clear();
                    }
                    continue;
                }
                Ok(Message::Deadline) => {}
                Ok(Message::Closed) => open = false,
                Err(RunError { fault: RunFault::Channel(ChannelError::Torn { have, want }), .. }) => {
                    torn = Some((have, want));
                    inbox.clear();
                    open = false;
                }
                Err(error) => return Err(LoopError::Child(error)),
            }
        }
        // While the channel is open the read was the wait: one look. Once it closed, the
        // child's end is all that is left: wait for it a tick at a time, by the run's deadline.
        let look = match open {
            true => Deadline::PASSED,
            false => Deadline::after(child.now(), bounds.tick).min(bounds.until).min(grace_end),
        };
        child = match child.wait(look).map_err(LoopError::Child)? {
            Waited::Ended(ended) => return Ok(Ran { status: status_of(ended.exit), served, torn, terminated: relay.forwarded() }),
            Waited::Running(running) => running,
        };
    }
}

/// How many messages `bytes` holds: what `serve` took, whole ones only, each a four-byte
/// big-endian length and that many bytes (`blaze_launch::wire::split`).
fn count(bytes: &[u8]) -> u64 {
    let mut at = 0;
    let mut n = 0;
    while let Some(prefix) = bytes[at..].first_chunk::<4>() {
        at += 4 + u32::from_be_bytes(*prefix) as usize;
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    //! The rules: a Rust program's status is what `std::process::exit` passes "through to the
    //! underlying OS" (library/std/src/process.rs, `exit`), of which the parent sees the low 8
    //! bits (waitpid(2): "WEXITSTATUS(wstatus) returns the exit status of the child. This
    //! consists of the least significant 8 bits of the status argument"); a program ended by a
    //! signal is reported as the shell does (POSIX.1-2017, Shell Command Language 2.8.2: "The
    //! exit status of a command that terminated because it received a signal shall be reported
    //! as greater than 128"; bash and zsh: 128 + its number). Every wait has a deadline (rule
    //! 18): a program that neither asks nor ends is killed at the run's deadline, never waited
    //! for forever.
    //!
    //! The child is the simulation's ([`bstd::procs::SimChild`]): its messages arrive in seeded
    //! pieces at seeded times, so each seed is another interleaving of the loop's reads and
    //! the child's writes.

    use bstd::procs::{Act, Arrived, OnTerminate, ProcId, Scripted, SimChild, TermChild};
    use blaze_launch::Thread;
    use blaze_launch::wire::RecordId;
    use blaze_launch::wire::run::{Needed, Symbol, Up};
    use blaze_ladder::code_records::CodeKey;

    use super::*;
    use crate::run_parent::Resolved;

    const SEEDS: u64 = 64;

    fn need(symbol: u8, id: u64) -> std::vec::Vec<u8> {
        let mut out = std::vec::Vec::new();
        Up::Need { need: Needed::Symbol(Symbol([symbol; 32])), thread: Thread(1) }.encode(id, &mut out).expect("a need encodes");
        out
    }

    /// The program's three functions: symbol `n` (every byte `n`, 1 to 3) has record `10 n`
    /// and declares nothing; any other symbol is unknown. The loop's tests are about the loop,
    /// so the resolver is a table (run_parent.rs's tests cover the lazy one).
    struct Table;

    impl Resolve for Table {
        fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
            let n = symbol.0[0];
            Ok(match (1..=3).contains(&n) && symbol.0 == [n; 32] {
                true => Resolved::Record { key: CodeKey(blaze_ids::ContentId([n; 32])), record: RecordId([n * 10; 32]), declares: &[], data: &[], named: &[], sent: crate::run_bytes::Sent::NONE },
                false => Resolved::Unknown,
            })
        }
    }

    fn index(_alloc: Alloc<'_>) -> Table {
        Table
    }

    fn bounds(until: Duration) -> Bounds {
        Bounds { tick: TICK, write: WRITE_BOUND, until: Deadline::at(until), grace: GRACE }
    }

    fn run(acts: &[Act<'_>], seed: u64, until: Duration, plant: Plant) -> (Result<Ran, LoopError>, u32, bool, u64, Duration) {
        // One frame a case, made here (its memory the OS's), handed down as a handle.
        let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the case's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut needs = Needs::new(alloc);
        let mut index = index(alloc);
        let mut child = SimChild::new(acts, seed, ProcId(42));
        let mut inbox = std::vec::Vec::new();
        let mut out = std::vec::Vec::new();
        let mut signals = Scripted::new(&[]);
        let ran = drive_with(&mut child, &mut needs, &mut index, &[], &bounds(until), &mut signals, &mut inbox, &mut out, plant);
        (ran, child.answers(), child.killed(), child.trace(), child.now())
    }

    /// What one run with signals left to look at.
    struct Seen {
        ran: Result<Ran, LoopError>,
        killed: bool,
        terminations: u32,
        terminated_at: Option<Duration>,
        killed_at: Option<Duration>,
        taken: Option<Duration>,
        ignored: u32,
    }

    /// `acts` under the loop with `bounds`, the parent's signals arriving as `arrivals` say, the
    /// program answering a SIGTERM as `on` says.
    fn run_signalled(acts: &[Act<'_>], seed: u64, bounds: &Bounds, arrivals: &[(Duration, Arrived)], on: OnTerminate, plant: Plant) -> Seen {
        // One frame a case, made here (its memory the OS's), handed down as a handle.
        let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the case's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut needs = Needs::new(alloc);
        let mut index = index(alloc);
        let mut child = TermChild::new(SimChild::new(acts, seed, ProcId(42)), on, ProcId(42));
        let mut signals = Scripted::new(arrivals);
        let mut inbox = std::vec::Vec::new();
        let mut out = std::vec::Vec::new();
        let ran = drive_with(&mut child, &mut needs, &mut index, &[], bounds, &mut signals, &mut inbox, &mut out, plant);
        Seen {
            ran,
            killed: child.inner().killed(),
            terminations: child.terminations(),
            terminated_at: child.terminated_at(),
            killed_at: child.killed_at(),
            taken: signals.terminate_taken(),
            ignored: signals.ignored(),
        }
    }

    /// Prints "run loop: three functions served, exit 7, in N interleavings". Two threads ask
    /// at once (their needs may share a read), then a third; each first call is answered with
    /// its slot's Declare and its Publish (six messages); the status is the exit's. A loop that
    /// dropped the start of a second message read with the first (the planted bug) leaves a
    /// thread unanswered in some interleaving: the planted run must fail at least once.
    #[test]
    fn three_functions_then_exit_7() {
        let (a, b, c) = (need(1, 0), need(2, 1), need(3, 2));
        let ms = Duration::from_millis;
        let acts = [
            Act::Send { after: ms(1), message: &a },
            Act::Send { after: ms(0), message: &b },
            Act::Await { answers: 4 },
            Act::Send { after: ms(2), message: &c },
            Act::Await { answers: 6 },
            Act::Exit { after: ms(1), status: 7 },
        ];
        let mut traces = std::vec::Vec::new();
        let mut planted_failed = 0;
        for seed in 0..SEEDS {
            let (ran, answers, killed, trace, _) = run(&acts, seed, Duration::from_secs(60), Plant::None);
            assert_eq!(ran.expect("served to its end"), Ran { status: 7, served: 3, torn: None, terminated: false }, "seed {seed}");
            assert_eq!(answers, 6, "seed {seed}: three Declares and three Publishes");
            assert!(!killed, "seed {seed}: a child that ends is never killed");
            traces.push(trace);
            let (planted, ..) = run(&acts, seed, Duration::from_secs(60), Plant::DropPartial);
            if !matches!(planted, Ok(Ran { status: 7, .. })) {
                planted_failed += 1;
            }
        }
        traces.sort_unstable();
        traces.dedup();
        assert!(traces.len() > 1, "the seeds explored more than one interleaving ({} distinct)", traces.len());
        assert!(planted_failed > 0, "the planted bug (a partial message dropped) is caught by some seed");
        println!("run loop: three functions served, exit 7, in {} interleavings", traces.len());
    }

    /// Prints "run loop: a silent child is killed at the run's deadline". A loop that waited
    /// for the child without a deadline would never return; one that returned without killing
    /// it would leave it running.
    #[test]
    fn a_child_that_never_asks_nor_ends_meets_the_deadline() {
        let acts = [Act::Hang];
        for seed in 0..SEEDS {
            let (ran, answers, killed, _, now) = run(&acts, seed, Duration::from_secs(2), Plant::None);
            match ran {
                Err(LoopError::Deadline { at, killed: Exit::Signaled(9) }) => assert!(at >= Duration::from_secs(2)),
                other => panic!("seed {seed}: the run's deadline, not {other:?}"),
            }
            assert!(killed, "seed {seed}: the child is killed at the deadline");
            assert_eq!(answers, 0);
            assert!(now < Duration::from_secs(2) + TICK, "seed {seed}: no wait past the deadline by more than a tick");
        }
        println!("run loop: a silent child is killed at the run's deadline");
    }

    /// Prints "run loop: killed by signal 9 is 137, torn mid-message is 137 with the tear named".
    /// A loop that took the signal's number as an exit status, or that dropped a tear silently
    /// (or failed the run on it instead of reporting the program's own end), fails it.
    #[test]
    fn a_child_killed_by_a_signal() {
        let (a, b) = (need(1, 0), need(2, 1));
        let ms = Duration::from_millis;
        let signalled = [Act::Send { after: ms(1), message: &a }, Act::Await { answers: 2 }, Act::Signal { after: ms(5), signal: 9 }];
        let mid = [
            Act::Send { after: ms(1), message: &a },
            Act::Await { answers: 2 },
            Act::DieMidMessage { after: ms(1), message: &b, keep: 5 },
        ];
        for seed in 0..SEEDS {
            let (ran, answers, killed, ..) = run(&signalled, seed, Duration::from_secs(60), Plant::None);
            assert_eq!(ran.expect("its end"), Ran { status: 137, served: 1, torn: None, terminated: false }, "seed {seed}");
            assert_eq!(answers, 2);
            assert!(!killed);
            let (ran, ..) = run(&mid, seed, Duration::from_secs(60), Plant::None);
            assert_eq!(ran.expect("its end"), Ran { status: 137, served: 1, torn: Some((5, b.len())), terminated: false }, "seed {seed}");
        }
        println!("run loop: killed by signal 9 is 137, torn mid-message is 137 with the tear named");
    }

    /// Prints "run loop: no deadline on the program: an hour's run ends with its own status".
    /// D103: `blaze run`'s bounds put no deadline on the program; a loop that kept one (the
    /// command's, or any) would kill it and return `LoopError::Deadline` instead of status 0.
    #[test]
    fn blaze_runs_bounds_put_no_deadline_on_the_program() {
        let a = need(1, 0);
        let ms = Duration::from_millis;
        let acts = [Act::Send { after: ms(1), message: &a }, Act::Await { answers: 2 }, Act::Exit { after: Duration::from_secs(3600), status: 0 }];
        for seed in 0..4 {
            let seen = run_signalled(&acts, seed, &Bounds::run(), &[], OnTerminate::Ignore, Plant::None);
            assert_eq!(seen.ran.expect("its end"), Ran { status: 0, served: 1, torn: None, terminated: false }, "seed {seed}");
            assert!(!seen.killed, "seed {seed}: never killed");
        }
        println!("run loop: no deadline on the program: an hour's run ends with its own status");
    }

    /// Prints "run loop: SIGTERM forwarded once, heeded, 143, taken within a tick of its
    /// arrival mid-wait". D103: "SIGTERM to the parent ends the program first": the program is
    /// sent SIGTERM (once, though two came) and its own end by it is the status, 128 + 15
    /// (POSIX.1-2017 2.8.2; SIGTERM is 15). The SIGTERM arrives while the loop waits (300 ms,
    /// inside a tick): it is taken at the next look, within [`TICK`]. A loop that killed at
    /// once (the planted bug) reports 137 and must fail for every seed.
    #[test]
    fn a_sigterm_is_forwarded_and_the_programs_end_is_the_status() {
        let a = need(1, 0);
        let ms = Duration::from_millis;
        let acts = [Act::Send { after: ms(1), message: &a }, Act::Await { answers: 2 }, Act::Hang];
        let arrivals = [(ms(300), Arrived::Terminate), (ms(320), Arrived::Terminate)];
        let heed = OnTerminate::Heed { after: ms(5) };
        let bounds = bounds(Duration::from_secs(60));
        let mut planted_failed = 0;
        for seed in 0..SEEDS {
            let seen = run_signalled(&acts, seed, &bounds, &arrivals, heed, Plant::None);
            assert_eq!(seen.ran.expect("its end"), Ran { status: 143, served: 1, torn: None, terminated: true }, "seed {seed}");
            assert!(!seen.killed, "seed {seed}: a program that heeds SIGTERM is not killed");
            assert_eq!(seen.terminations, 1, "seed {seed}: forwarded once");
            let taken = seen.taken.expect("the SIGTERM was taken");
            assert!(taken >= ms(300) && taken <= ms(300) + TICK, "seed {seed}: taken at {taken:?}, within a tick of 300 ms");
            let planted = run_signalled(&acts, seed, &bounds, &arrivals, heed, Plant::KillAtOnce);
            if !matches!(planted.ran, Ok(Ran { status: 143, .. })) {
                planted_failed += 1;
            }
        }
        assert_eq!(planted_failed, SEEDS, "the planted kill-at-once is caught by every seed");
        println!("run loop: SIGTERM forwarded once, heeded, 143, taken within a tick of its arrival mid-wait");
    }

    /// Prints "run loop: a program that ignores SIGTERM is killed exactly at the grace's end".
    /// D103: the parent's own step after the forward is bounded; the grace (1.1 s here, not a
    /// whole number of ticks) passes, the program is killed (137) at its end exactly: every wait
    /// after the forward is capped by it. A loop that did not cap them (the planted bug) kills
    /// on the next tick boundary, 1.25 s after the forward, and must fail for every seed.
    #[test]
    fn a_program_that_ignores_sigterm_is_killed_at_the_grace() {
        let a = need(1, 0);
        let ms = Duration::from_millis;
        let acts = [Act::Send { after: ms(1), message: &a }, Act::Await { answers: 2 }, Act::Hang];
        let arrivals = [(ms(300), Arrived::Terminate)];
        let bounds = Bounds { grace: ms(1100), ..bounds(Duration::from_secs(60)) };
        let mut planted_failed = 0;
        for seed in 0..SEEDS {
            let seen = run_signalled(&acts, seed, &bounds, &arrivals, OnTerminate::Ignore, Plant::None);
            assert_eq!(seen.ran.expect("its end"), Ran { status: 137, served: 1, torn: None, terminated: true }, "seed {seed}");
            assert!(seen.killed, "seed {seed}: killed after the grace");
            let (sent, killed) = (seen.terminated_at.expect("forwarded"), seen.killed_at.expect("killed"));
            assert_eq!(killed - sent, ms(1100), "seed {seed}: the kill comes at the grace's end");
            let planted = run_signalled(&acts, seed, &bounds, &arrivals, OnTerminate::Ignore, Plant::GraceUncapped);
            if planted.killed_at.zip(planted.terminated_at).map(|(k, s)| k - s) != Some(ms(1100)) {
                planted_failed += 1;
            }
        }
        assert_eq!(planted_failed, SEEDS, "the planted uncapped wait is caught by every seed");
        println!("run loop: a program that ignores SIGTERM is killed exactly at the grace's end");
    }

    /// Prints "run loop: SIGINT and SIGQUIT during the waits leave the program its own end".
    /// D103: while the program runs the parent ignores SIGINT and SIGQUIT (the terminal sends
    /// them to the program, which is in the foreground group); here three arrive during the
    /// loop's waits and the program ends by itself with 3: the status is 3, nothing was
    /// forwarded, nothing killed. (The native disposition that makes the parent survive them
    /// is proven by blaze-core run_signals's `held_ignores_catches_and_restores_on_every_path`,
    /// which raises them in its own process; the planted bugs of the two tests above are what
    /// this loop could get wrong with a SIGTERM in the same script.)
    #[test]
    fn interrupts_during_the_wait_leave_the_program_alone() {
        let a = need(1, 0);
        let ms = Duration::from_millis;
        let acts = [Act::Send { after: ms(1), message: &a }, Act::Await { answers: 2 }, Act::Exit { after: ms(600), status: 3 }];
        let arrivals = [(ms(100), Arrived::Interrupt), (ms(120), Arrived::Quit), (ms(400), Arrived::Interrupt)];
        for seed in 0..SEEDS {
            let seen = run_signalled(&acts, seed, &bounds(Duration::from_secs(60)), &arrivals, OnTerminate::Heed { after: ms(1) }, Plant::None);
            assert_eq!(seen.ran.expect("its end"), Ran { status: 3, served: 1, torn: None, terminated: false }, "seed {seed}");
            assert_eq!(seen.terminations, 0, "seed {seed}: nothing forwarded");
            assert!(!seen.killed, "seed {seed}: nothing killed");
            assert_eq!(seen.ignored, 3, "seed {seed}: all three came during the run and were ignored");
        }
        println!("run loop: SIGINT and SIGQUIT during the waits leave the program its own end");
    }
}
