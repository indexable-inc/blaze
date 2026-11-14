//! `blaze test` on a terminal: its test executables run while the build goes on, each as soon
//! as it is linked, and the screen (docs/terminal.md) shows both. The screen is blaze-screen's
//! one core behind its driver (`cli::show`); this file is only the threads around it, which
//! do IO and decide nothing of what is shown:
//!
//! - the caller's thread talks to the daemon: each response is the printer's, which makes the
//!   screen's events of it, and each test executable linked ([`Response::TestReady`]) is the
//!   runner's;
//! - the runner starts each test executable, a few at a time, reads libtest's JSON and says
//!   each test as the screen's events ([`blaze_screen::Test`]);
//! - the screen's thread (`show::Threaded`) takes the events of both in the order they came.
//!
//! An interrupt puts the terminal back from `show::watch_interrupts`'s thread; the test
//! executables share the terminal's process group, so they get the signal too.
//!
//! Off a terminal, or with libtest's own output asked for ([`PLAIN_ARGS`]), none of this
//! runs: [`wanted`] says no and `cli::drive` runs the executables after the build, each
//! writing for itself. The exit code is cargo's either way: 0, the code of the first failing
//! executable in cargo's order under `--fail-fast`, else 101 when anything failed; and
//! blaze's own for a run that ran no test, which is refused (`test::refusal`,
//! `Exit::NoTests`): the runner counts what each executable said it ran ([`Run::Counted`]).
//!
//! [`Response::TestReady`]: blaze_api::Response::TestReady

use std::{
    alloc::Global,
    ffi::OsString,
    io::{self, BufRead as _, BufReader, Read as _},
    path::PathBuf,
    process::{Child, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use blaze_api::{Artifact, ArtifactKind, Request, ResponseRef, TargetKind};
use blaze_core::exit::Exit;
use blaze_core::lock::Lock;
use blaze_hash::HashSet;
use blaze_screen::{BinaryEnd, Caps, Event, Tally, Test, Verdict};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};

use super::{
    Then,
    args::{Command, Invocation},
    daemon::{self, Api},
    place::Place,
    print::{Printer, Shown},
    run::Exec,
    say,
    show::{Msg, Sink},
    test::{self, DRAIN, Kind, Programs as _},
};

/// libtest's arguments that change what it prints, which the screen can't read: with any of
/// them the executables write for themselves.
const PLAIN_ARGS: [&str; 9] = [
    "--format",
    "--list",
    "--nocapture",
    "--no-capture",
    "--logfile",
    "-q",
    "--quiet",
    "-h",
    "--help",
];

/// Whether the command's tests are run under the screen: a `test` on a terminal the screen
/// redraws (`caps`), printed for a person (not `-q` or `-v`), that runs its tests (not
/// `--no-run`, nor in the daemon, which runs every `test` but `--doc`, `--no-run` and `--ub`
/// ([`super::in_daemon`]), nor `blaze lock`) with libtest's own output left as the screen
/// reads it.
pub fn wanted(asked: &Invocation, shown: Shown, caps: Caps) -> bool {
    if asked.command() != Command::Test || asked.lock || !caps.live || shown.quiet || shown.verbose > 0 {
        return false;
    }
    let reads_args = asked.program.iter().all(|arg| {
        let arg = arg.to_string_lossy();
        !PLAIN_ARGS.iter().any(|plain| arg == *plain || arg.starts_with(&format!("{plain}=")))
    });
    !asked.flags.no_run && !super::in_daemon(asked) && reads_args
}

/// What the runner is sent.
enum Run {
    /// A test executable is linked and placed.
    Ready(Artifact),
    /// One's suite ended: libtest's counts of it, sent before its [`Run::Drained`].
    Counted { binary: u32, counts: Tally },
    /// One's stdout and stderr closed: all it printed is in its [`Captured`].
    Drained { binary: u32 },
    /// One's process exited, and is not yet reaped (the runner reaps it): its output is read
    /// for at most [`DRAIN`] more. `Err` says why its exit could not be waited for.
    Exited { binary: u32, waited: Result<(), String> },
    /// The build is over: whether it passed, and everything it placed (the executables that
    /// came [`Run::Ready`] among them; the rest, the doc tests and any the daemon did not
    /// stream, are queued now).
    BuildOver { ok: bool, artifacts: Vec<Artifact> },
}

/// The build and its tests under the screen: what `cli::cargo` does for a `test` on a
/// terminal.
pub fn drive<A: Api>(
    api: &mut A,
    asked: &Invocation,
    place: &Place,
    request: Request,
    shown: Shown,
    started: Instant,
    mut to_screen: Sender<Msg>,
) -> io::Result<Then> {
    let (to_runner, runner_gets) = unbounded::<Run>();
    // The workspace's member count comes with `Built`, after the first executables run, so a
    // rerun always names the package: `-p calc --lib` reruns it in any workspace.
    let cores = blaze_core::cores::get().map_err(io::Error::other)?;
    let runner = Runner::new(asked, cores, to_screen.clone(), to_runner.clone(), true, shown.color);
    let runner = thread::Builder::new()
        .name("blaze test runner".into())
        .spawn(move || runner.run(&runner_gets))?;

    let mut out = io::stdout();
    let mut for_printer = to_screen.clone();
    let mut printer = Printer::new(shown, &place.root, &mut out, &mut for_printer);
    printer.place(
        // The user's home, which the printer shortens paths under to `~`; with none, or one
        // that is not UTF-8, paths are printed whole.
        blaze_core::home::user().ok().and_then(|it| it.into_os_string().into_string().ok()),
        super::observe::last_failure(),
    );
    let mut unwritten = None;
    let called = api.call(request, |response, more| {
        if let ResponseRef::Work(worked) = &response {
            super::client_rows::heard(worked.build_id, worked.misses);
            super::client_rows::summarized(worked.build_id, worked.why.as_deref());
        }
        let printed = match response {
            ResponseRef::TestReady(artifact) => {
                let _ = to_runner.send(Run::Ready(artifact));
                Ok(())
            }
            response => printer.on(response),
        };
        let written = printed.and_then(|()| match more {
            true => Ok(()),
            false => printer.draw(),
        });
        if let Err(error) = written {
            unwritten.get_or_insert(error);
        }
    });
    let ok = match called {
        Ok(ok) => ok && !printer.refused() && printer.left_unchecked() == 0,
        Err(error) => {
            // alloc: the error a build ends with, once
            let what = match daemon::hung_up(&error).then(|| api.died()).flatten() {
                Some(Ok(died)) => format!("{error}: {died}"),
                Some(Err(why)) => {
                    format!("{error}, and what came of the daemon is not known: {why:#}")
                }
                None => error.to_string(),
            };
            use std::io::Write as _;
            if let Err(error) = printer.err().write_all(say::error(&what, shown.color).as_bytes()) {
                unwritten.get_or_insert(error);
            }
            false
        }
    };
    // Unless `--fail-fast`, the tests of every unit that built still run when another failed.
    let tests = ok || !asked.flags.fail_fast;
    let why = match ok {
        true => super::why_recomputed(&mut printer, shown, true).unwrap_or(None),
        false => None,
    };
    // Nothing returns before the runner and the screen are told the build is over.
    if let Err(error) = printer.finish(ok, started.elapsed(), why, tests) {
        unwritten.get_or_insert(error);
    }
    let artifacts = printer.take_artifacts();
    let unbuilt = printer.take_unbuilt();
    drop(printer);
    for it in unbuilt {
        to_screen.take(Event::Test(Test::Unbuilt { unit: it.unit, behind: it.behind, why: it.why }));
    }
    let _ = to_runner.send(Run::BuildOver { ok, artifacts });
    drop(to_runner);
    let code = runner.join().unwrap_or(Exit::Failed.code());
    to_screen.take(Event::Test(Test::Over));
    to_screen.draw()?;
    if let Some(error) = unwritten {
        return Err(error);
    }
    Ok(Then::Exit(if ok { code } else { code.max(Exit::Failed.code()) }))
}

/// libtest's variable for how many threads it runs tests on.
const RUST_TEST_THREADS: &str = "RUST_TEST_THREADS";

/// One test executable the runner knows.
struct Binary {
    artifact: Artifact,
    /// cargo's order of it, as text: the summary sorts by it, and the first failing in it
    /// gives the exit code.
    order: String,
    label: String,
    /// libtest's JSON is read from it; otherwise it is one test, its exit status the verdict.
    json: bool,
    began: Option<Instant>,
    /// What its suites said they ran, summed ([`Run::Counted`]); `None` until one ended.
    counts: Option<Tally>,
}

/// One running.
struct Running {
    binary: u32,
    child: Child,
    pid: u32,
    /// What it printed, as read so far.
    captured: Arc<Lock<Captured>>,
    /// When its process exited, once [`Run::Exited`] said so: its output is read until
    /// [`Runner::drain`] past this.
    exited: Option<Instant>,
    /// Stopped by `fail-fast`: its end is no failure of its own.
    killed: bool,
}

/// What one executable printed outside libtest's JSON, and its stderr, as read so far: its
/// readers fill it, and the runner takes it once both pipes closed, or once the drain's
/// deadline passed with a pipe still open.
#[derive(Default)]
struct Captured {
    out: String,
    err: Vec<u8>,
}

impl Captured {
    /// Its stdout's lines, then its stderr.
    fn text(&self) -> String {
        let mut text = self.out.clone(); // alloc: one executable's output, once it ended
        text.push_str(&String::from_utf8_lossy(&self.err));
        text
    }
}

/// Starts each test executable as soon as it is ready, a few at a time, and says how each
/// went; its result is the command's exit code.
struct Runner {
    args: Vec<OsString>,
    fail_fast: bool,
    spec_flag: bool,
    to_screen: Sender<Msg>,
    to_runner: Sender<Run>,
    binaries: Vec<Binary>,
    seen: HashSet<PathBuf, Global>,
    queued: Vec<u32>,
    running: Vec<Running>,
    slots: usize,
    /// The `RUST_TEST_THREADS` each executable is started with, a share of blaze's core budget;
    /// `None` when the command set `RUST_TEST_THREADS` itself, which then reaches each as set.
    threads: Option<usize>,
    /// The failures, as (order, exit code): the first in cargo's order is the exit code.
    failed: Vec<(String, Option<i32>)>,
    stopping: bool,
    build: Option<bool>,
    /// Whether the warnings it prints are colored.
    color: bool,
    /// How long an executable's output is read after it exited: [`DRAIN`].
    drain: Duration,
    /// What the command selects, for the rule on a run of none.
    selection: test::Selection,
    /// What the executables that ended ran, summed.
    tally: Tally,
}

impl Runner {
    fn new(
        asked: &Invocation,
        cores: blaze_core::cores::Cores,
        to_screen: Sender<Msg>,
        to_runner: Sender<Run>,
        spec_flag: bool,
        color: bool,
    ) -> Runner {
        let told = std::env::var_os(RUST_TEST_THREADS).is_some();
        Runner::of(
            asked.test_args(),
            asked.flags.fail_fast,
            cores,
            told,
            to_screen,
            to_runner,
            spec_flag,
            color,
            test::Selection::of(asked),
        )
    }

    /// `told`: whether the command's environment sets `RUST_TEST_THREADS` itself.
    ///
    /// The budget is split two ways at once, so the threads running stay near it: a quarter of
    /// it in executables at once ([`blaze_core::cores::NESTED`]), each with libtest told the
    /// share that leaves it (`RUST_TEST_THREADS`), where libtest alone would start a thread per
    /// core in each. Executables are not run one at a time with the whole budget: they start as
    /// each is linked, while the build goes on, and most are small; a few at once keeps the
    /// cores busy while one long suite runs. A command's own `RUST_TEST_THREADS` or
    /// `--test-threads` wins.
    #[expect(clippy::too_many_arguments, reason = "the runner's whole setup, made once a command")]
    fn of(
        args: Vec<OsString>,
        fail_fast: bool,
        cores: blaze_core::cores::Cores,
        told: bool,
        to_screen: Sender<Msg>,
        to_runner: Sender<Run>,
        spec_flag: bool,
        color: bool,
        selection: test::Selection,
    ) -> Runner {
        let slots = cores.split(blaze_core::cores::NESTED).get();
        Runner {
            args,
            fail_fast,
            spec_flag,
            to_screen,
            to_runner,
            binaries: Vec::new(),     // alloc: one a command
            seen: HashSet::default(), // alloc: one a command
            queued: Vec::new(),       // alloc: one a command
            running: Vec::new(),      // alloc: one a command
            slots,
            threads: (!told).then(|| cores.split(slots).get()),
            failed: Vec::new(), // alloc: one a command
            stopping: false,
            build: None,
            color,
            drain: DRAIN,
            selection,
            tally: Tally::default(),
        }
    }

    fn say(&self, test: Test) {
        let _ = self.to_screen.send(Msg::Event(Event::Test(test)));
    }

    /// Words of the runner's own, a line that stays.
    fn warn(&self, what: &str) {
        let _ = self.to_screen.send(Msg::Event(Event::Said(say::warning(what, self.color))));
    }

    fn run(mut self, gets: &Receiver<Run>) -> i32 {
        loop {
            if self.build.is_some() && self.queued.is_empty() && self.running.is_empty() {
                return self.finish();
            }
            // The first exited executable whose output is still open: its drain's deadline.
            let deadline = self
                .running
                .iter()
                .filter_map(|it| it.exited)
                .min()
                .map(|exited| exited + self.drain);
            let got = {
                let _waiting = tracing::info_span!(
                    target: "blaze",
                    "test.runner.wait",
                    running = self.running.len(),
                    queued = self.queued.len(),
                    build_over = self.build.is_some(),
                )
                .entered();
                match deadline {
                    Some(deadline) => gets.recv_deadline(deadline),
                    None => gets.recv().map_err(|_| RecvTimeoutError::Disconnected),
                }
            };
            let run = match got {
                Ok(run) => run,
                Err(RecvTimeoutError::Timeout) => {
                    self.undrained();
                    self.start();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // Every sender gone with work left: the caller's thread ended early.
                    self.stop_all();
                    return Exit::Failed.code();
                }
            };
            match run {
                Run::Ready(artifact) => self.ready(artifact),
                Run::Counted { binary, counts } => {
                    if let Some(it) = self.binaries.get_mut(binary as usize) {
                        it.counts.get_or_insert_with(Tally::default).add(&counts);
                    }
                }
                Run::Drained { binary } => self.ended(binary, false),
                Run::Exited { binary, waited } => self.exited(binary, waited),
                Run::BuildOver { ok, artifacts } => {
                    let (tests, docs) = test::executables(&artifacts);
                    for artifact in tests.into_iter().chain(docs) {
                        // clone: the artifact outlives the build's list
                        self.ready(artifact.clone());
                    }
                    self.build = Some(ok);
                }
            }
            self.start();
        }
    }

    /// The command's exit code once every executable ended: [`Runner::code`], or for a run in
    /// which nothing failed and no test ran, the refusal said and [`Exit::NoTests`]
    /// (`test::refusal`).
    fn finish(&mut self) -> i32 {
        let code = self.code();
        if code != 0 {
            return code;
        }
        let Some(refused) = test::refusal(&self.selection, &self.tally, || self.names()) else {
            return code;
        };
        self.say(Test::Refused { what: refused.what, help: refused.help });
        Exit::NoTests.code()
    }

    /// The tests of every libtest executable the run started, each asked for its list: a
    /// refusal's help names the nearest.
    fn names(&self) -> test::Names {
        let mut names = test::Names::default();
        for it in self.binaries.iter().filter(|it| it.json) {
            let executable =
                PathBuf::from(it.artifact.executable.as_deref().unwrap_or(&it.artifact.path));
            let root = std::path::Path::new(&it.artifact.manifest_path)
                .parent()
                .map_or_else(PathBuf::new, std::path::Path::to_owned);
            let exec = Exec::of(&it.artifact, executable, test::LIST.map(OsString::from), root);
            match test::Spawned.names(&exec) {
                Ok(listed) => names.names.extend(listed),
                Err(why) => names.unlisted.push(format!("{}: {why}", exec.shown())), // alloc: a refusal's help, once
            }
        }
        names
    }

    /// 0, the first failing executable's code in cargo's order (fail-fast), or 101.
    fn code(&mut self) -> i32 {
        if self.build == Some(false) {
            return Exit::Failed.code();
        }
        if self.failed.is_empty() {
            return 0;
        }
        if !self.fail_fast {
            return Exit::Failed.code();
        }
        self.failed.sort();
        self.failed[0].1.unwrap_or(Exit::Failed.code())
    }

    fn ready(&mut self, artifact: Artifact) {
        let Some(executable) = artifact.executable.as_deref() else { return };
        if !self.seen.insert(PathBuf::from(executable)) {
            return;
        }
        let doc = artifact.kind == ArtifactKind::Doctest;
        let kind = if doc { Kind::Doctest } else { Kind::Test };
        let (name, version) =
            (say::package_name(&artifact.package), say::package_version(&artifact.package));
        let rank = match artifact.target.kind {
            TargetKind::Lib => 0,
            TargetKind::Bin => 1,
            TargetKind::Test => 2,
            TargetKind::Bench => 3,
            TargetKind::Example => 4,
            TargetKind::CustomBuild => 5,
        };
        // cargo's: every unit test executable by package and target, then the doc tests.
        // alloc: one key and label an executable
        let order = format!(
            "{} {name} {version} {} {rank} {}",
            u8::from(doc),
            artifact.package,
            artifact.target.name
        );
        let label = match (doc, artifact.target.kind) {
            (true, _) => format!("{name} doctests"),
            (false, TargetKind::Lib) => format!("{name} lib"),
            (false, _) => format!("{name} {}", artifact.target.name),
        };
        let rerun = test::rerun(&artifact, kind, self.spec_flag);
        let binary = self.binaries.len() as u32;
        self.say(Test::Queued { binary, label: label.clone(), rerun: format!("blaze test {rerun}") }); // alloc: once a test executable
        let json = artifact.harness && !doc;
        self.binaries.push(Binary { artifact, order, label, json, began: None, counts: None });
        if self.stopping {
            self.say(Test::Ended { binary, end: BinaryEnd::Stopped, output: String::new() });
            return;
        }
        self.queued.push(binary);
    }

    /// Starts the queued, first in cargo's order, while there are slots.
    fn start(&mut self) {
        while !self.stopping && self.running.len() < self.slots && !self.queued.is_empty() {
            let at = (0..self.queued.len())
                .min_by(|a, b| {
                    self.binaries[self.queued[*a] as usize]
                        .order
                        .cmp(&self.binaries[self.queued[*b] as usize].order)
                })
                .unwrap_or(0);
            let binary = self.queued.remove(at);
            self.spawn(binary);
        }
    }

    fn spawn(&mut self, binary: u32) {
        let it = &self.binaries[binary as usize];
        let mut args = self.args.clone();
        if it.json {
            args.extend(
                ["--format", "json", "-Z", "unstable-options", "--report-time"].map(OsString::from),
            );
        }
        let executable =
            PathBuf::from(it.artifact.executable.as_deref().unwrap_or(&it.artifact.path));
        let root = std::path::Path::new(&it.artifact.manifest_path)
            .parent()
            .map_or_else(PathBuf::new, std::path::Path::to_owned);
        let exec = Exec::of(&it.artifact, executable, args, root);
        let mut command = exec.command();
        if let Some(threads) = self.threads {
            command.env(RUST_TEST_THREADS, threads.to_string()); // alloc: one an executable run
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let json = it.json;
        let name = it.label.clone();
        self.say(Test::Started { binary });
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let why =
                    format!("could not execute process {} (never executed): {error}", exec.shown());
                let order = self.binaries[binary as usize].order.clone();
                self.say(Test::Ended {
                    binary,
                    end: BinaryEnd::NeverRan { why },
                    output: String::new(),
                });
                self.failed(order, None, binary);
                return;
            }
        };
        self.binaries[binary as usize].began = Some(Instant::now());
        if !json {
            self.say(Test::Total { binary, tests: 1 });
            self.say(Test::TestStarted { binary, name });
        }
        let mut child = child;
        let pid = child.id();
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let captured = Arc::new(Lock::new("live.run.captured", Captured::default())); // alloc: one an executable run
        let read = {
            let (to_screen, to_runner, captured) =
                (self.to_screen.clone(), self.to_runner.clone(), captured.clone());
            let label = self.binaries[binary as usize].label.clone();
            thread::Builder::new().name("blaze test output".into()).spawn(move || {
                read(binary, &label, json, stdout, stderr, &captured, &to_screen, &to_runner)
            })
        };
        // Its exit is waited for apart from its output: a process it started may hold its
        // pipes open long after it exited.
        let wait = read.and_then(|_| {
            let to_runner = self.to_runner.clone();
            let label = self.binaries[binary as usize].label.clone();
            thread::Builder::new().name("blaze test wait".into()).spawn(move || {
                let _waiting =
                    tracing::info_span!(target: "blaze", "test.wait_exit", test = %label, pid)
                        .entered();
                let waited =
                    crate::raw::process::wait_exited(pid).map_err(|error| error.to_string());
                let _ = to_runner.send(Run::Exited { binary, waited });
            })
        });
        if let Err(error) = wait {
            let _ = child.kill();
            let _ = child.wait();
            let why =
                format!("could not read the output of {} or wait for it: {error}", exec.shown());
            let order = self.binaries[binary as usize].order.clone();
            self.say(Test::Ended {
                binary,
                end: BinaryEnd::NeverRan { why },
                output: String::new(),
            });
            self.failed(order, None, binary);
            return;
        }
        self.running.push(Running { binary, child, pid, captured, exited: None, killed: false });
    }

    /// One's process exited: its output is read for at most [`Runner::drain`] more.
    fn exited(&mut self, binary: u32, waited: Result<(), String>) {
        // Gone: its output was drained first, and it is reaped.
        let Some(running) = self.running.iter_mut().find(|it| it.binary == binary) else { return };
        match waited {
            Ok(()) => {
                running.exited.get_or_insert_with(Instant::now);
            }
            Err(why) => {
                // Its output is read to its end, with no deadline: only its pipes' closing
                // ends it now.
                let label = &self.binaries[binary as usize].label;
                let what = format!(
                    "could not wait for {label} (pid {}) to exit: {why}; its end is now when its stdout and \
                     stderr close, which a process it started and left running can hold off",
                    running.pid
                );
                self.warn(&what);
            }
        }
    }

    /// Ends each executable that exited [`Runner::drain`] ago with a pipe still open, with
    /// what it printed until now, and says so.
    fn undrained(&mut self) {
        let now = Instant::now();
        let drain = self.drain;
        // alloc: the few executables whose drain ended at once
        let late: Vec<u32> = self
            .running
            .iter()
            .filter(|it| it.exited.is_some_and(|exited| now >= exited + drain))
            .map(|it| it.binary)
            .collect();
        for binary in late {
            self.ended(binary, true);
        }
    }

    /// One's output is drained, or `held` past its drain's deadline: its status, then what it
    /// ended as.
    fn ended(&mut self, binary: u32, held: bool) {
        let Some(at) = self.running.iter().position(|it| it.binary == binary) else { return };
        let mut running = self.running.swap_remove(at);
        let output = running.captured.lock().text();
        if held {
            let label = &self.binaries[binary as usize].label;
            let what = format!(
                "{label} (pid {}) exited, but a process it started still holds its stdout or stderr open; \
                 its output was read for {} ms after it exited, and what that process prints from now on \
                 is not shown",
                running.pid,
                self.drain.as_millis()
            );
            self.warn(&what);
        }
        let ended = running.child.wait().map(say::Ended::of);
        let it = &self.binaries[binary as usize];
        let took = it.began.map(|began| began.elapsed());
        let (order, json, name) = (it.order.clone(), it.json, it.label.clone());
        if running.killed {
            self.say(Test::Ended { binary, end: BinaryEnd::Stopped, output });
            return;
        }
        let ended = match ended {
            Ok(ended) => ended,
            Err(error) => {
                let why = format!("could not wait for {name}: {error}");
                self.say(Test::Ended { binary, end: BinaryEnd::NeverRan { why }, output });
                self.failed(order, None, binary);
                return;
            }
        };
        let ok = ended.success();
        let counts = test::counted(self.binaries[binary as usize].counts.take(), ok, json);
        self.tally.add(&counts);
        if !json {
            let verdict = if ok { Verdict::Passed } else { Verdict::Failed };
            // clone: the box and the binary's end both show it
            self.say(Test::TestEnded {
                binary,
                name,
                verdict,
                took_ms: took.map(|it| u64::try_from(it.as_millis()).unwrap_or(u64::MAX)),
                output: output.clone(), // clone: one test's output, once it ended
            });
        }
        if ok {
            self.say(Test::Ended { binary, end: BinaryEnd::Passed, output });
            return;
        }
        let code = match ended {
            say::Ended::Code(code) => Some(code),
            say::Ended::Signal(_) => None,
        };
        self.say(Test::Ended {
            binary,
            end: BinaryEnd::Failed { status: say::ended(ended) },
            output,
        });
        self.failed(order, code, binary);
    }

    /// One failed: kept for the exit code, and under fail-fast the rest stopped.
    fn failed(&mut self, order: String, code: Option<i32>, binary: u32) {
        self.failed.push((order, code));
        if !self.fail_fast || self.stopping {
            return;
        }
        self.stopping = true;
        let label = &self.binaries[binary as usize].label;
        let why = format!("{label} failed; stopping the rest (`--fail-fast`; without it they run)");
        self.warn(&why);
        self.stop_all();
    }

    fn stop_all(&mut self) {
        for binary in std::mem::take(&mut self.queued) {
            self.say(Test::Ended { binary, end: BinaryEnd::Stopped, output: String::new() });
        }
        for running in &mut self.running {
            running.killed = true;
            let _ = running.child.kill();
        }
    }
}

/// Reads one executable's stdout (libtest's JSON when `json`) into the view's events, and its
/// stderr beside it, each into `captured` as it comes, then tells the runner both closed.
#[expect(clippy::too_many_arguments, reason = "one thread's whole input, given once")]
fn read(
    binary: u32,
    label: &str,
    json: bool,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
    captured: &Arc<Lock<Captured>>,
    to_screen: &Sender<Msg>,
    to_runner: &Sender<Run>,
) {
    let _reading =
        tracing::info_span!(target: "blaze", "test.read_output", test = %label).entered();
    let errors = stderr.map(|mut stderr| {
        let (captured, into) = (captured.clone(), captured.clone());
        let read = thread::Builder::new().name("blaze test stderr".into()).spawn(move || {
            let captured = into;
            let mut chunk = [0; 8192];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => return,
                    Ok(n) => captured.lock().err.extend_from_slice(&chunk[..n]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        let said =
                            format!("\nblaze stopped reading this executable's stderr: {error}\n");
                        captured.lock().err.extend_from_slice(said.as_bytes());
                        return;
                    }
                }
            }
        });
        (read, captured)
    });
    if let Some(stdout) = stdout {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            // A suite's end: its counts are the runner's, for the rule on a run of none.
            if json && let Some(counts) = test::counts_of(&line) {
                let _ = to_runner.send(Run::Counted { binary, counts });
            }
            match json.then(|| said(binary, &line)).flatten() {
                Some(event) => {
                    let _ = to_screen.send(Msg::Event(Event::Test(event)));
                }
                None if json && line.trim_start().starts_with('{') => {}
                None => {
                    let mut captured = captured.lock();
                    captured.out.push_str(&line);
                    captured.out.push('\n');
                }
            }
        }
    }
    match errors {
        Some((Ok(read), _)) => {
            let _ = read.join();
        }
        Some((Err(error), captured)) => {
            let said = format!("blaze could not read this executable's stderr: {error}\n");
            captured.lock().err.extend_from_slice(said.as_bytes());
        }
        None => {}
    }
    let _ = to_runner.send(Run::Drained { binary });
}

/// One line of libtest's JSON as the view's event: a suite's start, a test's start or end, and
/// a suite's end as how many tests its filter left out. Anything else is `None`.
fn said(binary: u32, line: &str) -> Option<Test> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = value.get("type")?.as_str()?;
    let event = value.get("event")?.as_str()?;
    let name = || value.get("name").and_then(|it| it.as_str()).map(str::to_owned);
    match (kind, event) {
        ("suite", "started") => {
            let tests = value.get("test_count").and_then(serde_json::Value::as_u64).unwrap_or(0);
            Some(Test::Total { binary, tests: u32::try_from(tests).unwrap_or(u32::MAX) })
        }
        ("suite", "ok" | "failed") => {
            let tests = value.get("filtered_out")?.as_u64()?;
            Some(Test::Filtered { binary, tests: u32::try_from(tests).unwrap_or(u32::MAX) })
        }
        ("test", "started") => Some(Test::TestStarted { binary, name: name()? }),
        ("test", "ok" | "failed" | "ignored") => {
            let verdict = match event {
                "ok" => Verdict::Passed,
                "failed" => Verdict::Failed,
                _ => Verdict::Ignored,
            };
            let took = value
                .get("exec_time")
                .and_then(serde_json::Value::as_f64)
                .and_then(|it| Duration::try_from_secs_f64(it).ok());
            let mut output =
                value.get("stdout").and_then(|it| it.as_str()).unwrap_or_default().to_owned();
            if let Some(message) = value.get("message").and_then(|it| it.as_str())
                && verdict == Verdict::Failed
            {
                output.push_str(message);
                output.push('\n');
            }
            Some(Test::TestEnded {
                binary,
                name: name()?,
                verdict,
                took_ms: took.map(|it: Duration| u64::try_from(it.as_millis()).unwrap_or(u64::MAX)),
                output,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use blaze_screen::{BinaryEnd, Event, Test, Verdict};
    use crossbeam_channel::unbounded;

    use super::{Msg, Run, Runner, said};

    /// A test executable that starts a process which inherits its stdout and outlives it ends
    /// when it exits, its output read until then: the runner does not wait for the pipe to
    /// close, which happens only when that process ends, and says that it stopped reading.
    #[cfg(unix)]
    #[test]
    fn a_test_whose_child_outlives_it_ends_when_it_exits() {
        let dir = tempfile::tempdir().unwrap();
        let mut artifact =
            crate::cli::run::tests::artifact("calc", "test", "it", true, Some("deps/it-0"));
        artifact.harness = false;
        artifact.manifest_path = dir.path().join("Cargo.toml").to_string_lossy().into_owned();
        // The executable is `sh`'s `$0`: the script starts a process holding its stdout, says
        // its pid, and exits 0.
        artifact.runner =
            ["/bin/sh", "-c", "sleep 60 & echo \"child $!\""].map(str::to_owned).to_vec();
        let (to_screen, screen_gets) = unbounded::<Msg>();
        let (to_runner, runner_gets) = unbounded::<Run>();
        let one = std::num::NonZero::<usize>::MIN;
        let cores =
            blaze_core::cores::Cores::resolve(blaze_core::cores::Setting::Exactly(one), one);
        let mut runner =
            Runner::of(
                Vec::new(),
                true,
                cores,
                true,
                to_screen,
                to_runner.clone(),
                true,
                false,
                crate::cli::test::Selection::default(),
            );
        runner.drain = Duration::from_millis(300);
        to_runner.send(Run::BuildOver { ok: true, artifacts: vec![artifact] }).unwrap();
        let (done, finished) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || done.send(runner.run(&runner_gets)));
        let code = finished.recv_timeout(Duration::from_secs(20));

        let events: Vec<Event> = screen_gets
            .try_iter()
            .filter_map(|msg| match msg {
                Msg::Event(event) => Some(event),
                Msg::Draw | Msg::Sync(_) => None,
            })
            .collect();
        let output = events.iter().find_map(|event| match event {
            Event::Test(Test::Ended { end: BinaryEnd::Passed, output, .. }) => Some(output.clone()),
            _ => None,
        });
        // The process left running is ended here, whatever the runner did.
        if let Some(pid) = output.as_deref().and_then(|it| it.trim().strip_prefix("child ")) {
            let _ = std::process::Command::new("kill").arg(pid).status();
        }
        assert_eq!(code, Ok(0), "the runner did not end within 20 s: {events:#?}");
        assert!(output.as_deref().is_some_and(|it| it.starts_with("child ")), "{events:#?}");
        let warned = events.iter().any(|event| matches!(event, Event::Said(text) if text.contains("calc it (pid ") && text.contains("still holds its stdout or stderr open")));
        assert!(warned, "{events:#?}");
    }

    #[test]
    fn libtest_json_becomes_the_views_events() {
        let started = said(3, r#"{ "type": "suite", "event": "started", "test_count": 2 }"#);
        assert!(matches!(started, Some(Test::Total { binary: 3, tests: 2 })));
        let failed = said(
            3,
            r#"{ "type": "test", "name": "a::b", "event": "failed", "exec_time": 0.25, "stdout": "boom\n" }"#,
        );
        let Some(Test::TestEnded { name, verdict, took_ms, output, .. }) = failed else {
            panic!("{failed:?}")
        };
        assert_eq!((name.as_str(), verdict, output.as_str()), ("a::b", Verdict::Failed, "boom\n"));
        assert_eq!(took_ms, Some(250));
        assert!(said(3, r#"{ "type": "suite", "event": "ok", "passed": 2 }"#).is_none());
        assert!(said(3, "running 2 tests").is_none());
        // A suite's end, whole: how many tests its filter left out.
        let ended = said(
            3,
            r#"{ "type": "suite", "event": "ok", "passed": 2, "failed": 0, "ignored": 0, "measured": 0, "filtered_out": 5, "exec_time": 0.1 }"#,
        );
        assert!(matches!(ended, Some(Test::Filtered { binary: 3, tests: 5 })), "{ended:?}");
    }

    /// One libtest executable, stood in for by a shell script that says what libtest's JSON
    /// says of a suite whose filter left out all 7 of its tests, and exits 0: the runner's
    /// exit code and the screen's events for `selection`.
    #[cfg(unix)]
    fn a_run_of_none(selection: crate::cli::test::Selection) -> (i32, Vec<Event>) {
        let dir = tempfile::tempdir().unwrap();
        let mut artifact =
            crate::cli::run::tests::artifact("calc", "lib", "calc", true, Some("deps/calc-0"));
        artifact.manifest_path = dir.path().join("Cargo.toml").to_string_lossy().into_owned();
        let suite = r#"echo '{ "type": "suite", "event": "started", "test_count": 0 }'; echo '{ "type": "suite", "event": "ok", "passed": 0, "failed": 0, "ignored": 0, "measured": 0, "filtered_out": 7, "exec_time": 0.0 }'"#;
        artifact.runner = ["/bin/sh", "-c", suite].map(str::to_owned).to_vec();
        let (to_screen, screen_gets) = unbounded::<Msg>();
        let (to_runner, runner_gets) = unbounded::<Run>();
        let one = std::num::NonZero::<usize>::MIN;
        let cores =
            blaze_core::cores::Cores::resolve(blaze_core::cores::Setting::Exactly(one), one);
        let runner = Runner::of(
            vec!["nope".into()],
            false,
            cores,
            true,
            to_screen,
            to_runner.clone(),
            true,
            false,
            selection,
        );
        to_runner.send(Run::BuildOver { ok: true, artifacts: vec![artifact] }).unwrap();
        let (done, finished) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || done.send(runner.run(&runner_gets)));
        let code = finished.recv_timeout(Duration::from_secs(20)).expect("the runner ends within 20 s");
        let events = screen_gets
            .try_iter()
            .filter_map(|msg| match msg {
                Msg::Event(event) => Some(event),
                Msg::Draw | Msg::Sync(_) => None,
            })
            .collect();
        (code, events)
    }

    /// On a terminal too, a run whose filter matched no test is refused (agents.json A39): the
    /// runner counts what each executable's suite said it ran, exits with blaze's code for a
    /// run of none, and tells the screen, which says it and makes the summary a failure. The
    /// control: `--no-tests pass` is exit 0 with no refusal.
    #[cfg(unix)]
    #[test]
    fn a_run_of_none_under_the_screen_is_refused() {
        let selection = crate::cli::test::Selection {
            typed: "blaze test nope".to_owned(),
            filters: vec!["nope".to_owned()],
            ..crate::cli::test::Selection::default()
        };
        let (code, events) = a_run_of_none(selection.clone());
        assert_eq!(code, blaze_core::exit::Exit::NoTests.code(), "{events:#?}");
        assert!(
            events.iter().any(|it| matches!(it, Event::Test(Test::Filtered { tests: 7, .. }))),
            "{events:#?}"
        );
        let refused = events.iter().find_map(|it| match it {
            Event::Test(Test::Refused { what, help }) => Some((what.as_str(), help.len())),
            _ => None,
        });
        assert_eq!(
            refused,
            Some(("no test ran: `blaze test nope` matched none of the 7 tests in 1 test executable", 1)),
            "{events:#?}"
        );

        let (code, events) = a_run_of_none(crate::cli::test::Selection { pass: true, ..selection });
        assert_eq!(code, 0, "{events:#?}");
        assert!(!events.iter().any(|it| matches!(it, Event::Test(Test::Refused { .. }))), "{events:#?}");
    }
}
