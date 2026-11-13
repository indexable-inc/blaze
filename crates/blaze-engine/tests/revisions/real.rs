//! The real driver: a test's revisions as real processes. One daemon in a home of its own
//! serves every revision, its incremental state kept between them; then each revision is
//! built from nothing at the same paths (a fresh home, cache and checkout, so the bytes can
//! be compared) and the two are held together (`check.rs`).
//!
//! It is for what only a kernel shows (the file watcher's event order, `kill -9`, a program
//! really run) and as the cross-check of the simulated driver. What it reads of a build is
//! blaze's own account: the `blaze-work` and `build-finished` lines of
//! `--message-format=json`, `blaze why --all --json` (the functions and units made again)
//! and `blaze explain edit_work` (the engine's memos run and verified).
//!
//! Every command runs under one fixed environment (`env_clear` and a short list), in a root
//! off any symlink (`/private/tmp`, never `/tmp`: a home named through a link turns the
//! no-op lane off), with the store check forced on (`BLAZE_VERIFY_STORE=1`), and its exit
//! status and output are read.
//!
//! `blaze` is the thin `blaze` (crates/blaze-thin) with the engine beside it, as an install
//! lays them out ([`install`]): every command goes through the thin's route, and a case of
//! its own path ([`Test::as_typed`]) runs its commands as a person types them, so the thin's
//! own check of a request with nothing changed (blaze-thin's `solo`) is what answers them.

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use super::{
    check::{self, Row},
    spec::{Change, Observed, Ran, Revision, Step, Test},
};

/// Longer than any command here takes with the standard library built cold.
const DEADLINE: Duration = Duration::from_secs(600);

/// The most tests that run at once, each with one daemon alive: a daemon's store pool
/// reserves a large address range, and many at once fail with ENOMEM. `BLAZE_REVISIONS_JOBS`
/// sets another bound.
const JOBS: u32 = 4;

static RUNNING: (Mutex<u32>, Condvar) = (Mutex::new(0), Condvar::new());

struct Slot;

/// The bound on tests at once.
pub fn jobs() -> u32 {
    std::env::var("BLAZE_REVISIONS_JOBS").ok().and_then(|it| it.parse().ok()).unwrap_or(JOBS)
}

impl Slot {
    fn take() -> Slot {
        let most = jobs();
        let mut running = RUNNING.0.lock().unwrap();
        while *running >= most {
            running = RUNNING.1.wait(running).unwrap();
        }
        *running += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        *RUNNING.0.lock().unwrap_or_else(|it| it.into_inner()) -= 1;
        RUNNING.1.notify_one();
    }
}

struct Out {
    ok: bool,
    /// `None`: a signal ended it.
    status: Option<i32>,
    stdout: String,
    stderr: String,
    ms: u128,
}

struct Real {
    blaze: PathBuf,
    root: PathBuf,
    /// The checkout, the daemon's home and the machine's cache: the same paths for the
    /// incremental daemon and for each clean build.
    dir: PathBuf,
    home: PathBuf,
    cache: PathBuf,
    /// Every checkout a `project` step made beside the first, each set aside with it.
    others: Vec<PathBuf>,
    /// How much of the daemon's log was read for panics.
    log_read: u64,
    /// Whether a failure leaves the root in place, for its logs.
    keep: bool,
    /// The program [`Step::Start`] started, while it runs.
    running: Option<Running>,
    /// When the last edit step wrote its files.
    edited: Option<Instant>,
    /// Where the thin `blaze` (`blaze`) and its engine ([`blaze_thin::ENGINE`]) are, this
    /// test's alone ([`install`]).
    bin: PathBuf,
    /// [`Test::as_typed`]: `build`, `run`, `check` and `test` run as a person types them.
    typed: bool,
    /// The number of the last build the driver saw blaze name (a `build-finished` line,
    /// `blaze why`), `Ok(None)` before any; `Err` says why it is not known (a cancelled build
    /// names none). How a command typed with no messages is told to have built nothing.
    last_build: Result<Option<u64>, &'static str>,
    /// The program the last `blaze build` named (`compiler-artifact`'s `executable`): what
    /// `run the file` runs.
    last_executable: Option<String>,
    /// Where in this home's `serve.log` the revision began: the count of
    /// [`Expect::starts`](super::spec::Expect::starts) is of the lines after it.
    starts_from: u64,
}

/// A program started by [`Step::Start`] and left running: `blaze run`'s process (which
/// becomes or waits for the program), and what the program printed, each read of its
/// standard output with when it arrived.
struct Running {
    child: std::process::Child,
    started: Instant,
    printed: std::sync::Arc<std::sync::Mutex<Vec<(Instant, Vec<u8>)>>>,
    said: thread::JoinHandle<String>,
}

impl Running {
    /// Everything printed so far, and for each byte's read when it arrived.
    fn printed(&self) -> (Vec<u8>, Vec<(usize, Instant)>) {
        let reads = self.printed.lock().unwrap_or_else(|it| it.into_inner());
        let (mut all, mut ends) = (Vec::new(), Vec::new());
        for (at, bytes) in reads.iter() {
            all.extend_from_slice(bytes);
            ends.push((all.len(), *at));
        }
        (all, ends)
    }

    /// When `text` was first whole in the output, if it is there. `from` is not a bound on
    /// it: a text printed before `from` is found as printed then (an `await` of what was
    /// there already is met at once).
    fn first_after(&self, text: &str, from: Instant) -> Option<Instant> {
        let (all, ends) = self.printed();
        let at = all.windows(text.len().max(1)).position(|it| it == text.as_bytes())?;
        ends.iter().find(|(end, _)| *end >= at + text.len()).map(|it| it.1.max(from))
    }

    fn end(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.said.join().unwrap_or_default()
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// The thin `blaze` and the engine, installed for the test whose root is named `name` as an
/// install lays them out (harness/v2/integrate.py; the release's archive): the thin as
/// `blaze` and the engine as [`blaze_thin::ENGINE`] in one directory, so the thin finds its
/// engine beside its own file (blaze-thin main.rs, `Engine::beside`: the directory of its
/// resolved `current_exe`). Each is a hard link of the built file (no copy of a large
/// engine a test) in a directory of this test's own (in `CARGO_TARGET_TMPDIR`, on the built
/// files' filesystem), so `replace engine` changes this test's engine alone. The engine is
/// `BLAZE_REVISIONS_BLAZE` where that is set, else the built `blaze-engine`; the thin is
/// `BLAZE_REVISIONS_THIN`, else `blaze-thin` beside the engine, which `cargo test` does not
/// build: `cargo build -p blaze-thin` does. Either one missing fails the test, naming it.
fn install(name: &str) -> PathBuf {
    let engine = std::env::var_os("BLAZE_REVISIONS_BLAZE")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_blaze-engine")), PathBuf::from);
    let thin = std::env::var_os("BLAZE_REVISIONS_THIN").map_or_else(|| engine.with_file_name("blaze-thin"), PathBuf::from);
    for (what, path, how) in [
        ("the engine", &engine, "`cargo build -p blaze-engine`, or BLAZE_REVISIONS_BLAZE naming one"),
        ("the thin `blaze`", &thin, "`cargo build -p blaze-thin` makes it beside the engine, or BLAZE_REVISIONS_THIN names one"),
    ] {
        if let Err(error) = fs::metadata(path) {
            panic!("{what} {} is not there ({error}): {how}", path.display());
        }
    }
    let bin = Path::new(env!("CARGO_TARGET_TMPDIR")).join("revisions-bin").join(name);
    let _ = fs::remove_dir_all(&bin);
    fs::create_dir_all(&bin).unwrap_or_else(|error| panic!("making {}: {error}", bin.display()));
    for (from, to) in [(&thin, bin.join("blaze")), (&engine, bin.join(blaze_thin::ENGINE))] {
        fs::hard_link(from, &to).unwrap_or_else(|error| {
            panic!(
                "linking {} as {} ({error}): the built file and CARGO_TARGET_TMPDIR are on one filesystem, or it is \
                 named on another",
                from.display(),
                to.display()
            )
        });
    }
    bin
}

/// The arguments of a revision before its `--` (blaze's) and after it (the program's).
fn split(revision: &Revision) -> (&[String], &[String]) {
    let at = revision.args.iter().position(|it| it == "--").unwrap_or(revision.args.len());
    (&revision.args[..at], revision.args.get(at + 1..).unwrap_or_default())
}

/// Every run of an action but the first, held to what each run is held to
/// ([`check::OF_THE_ACTION`] is the first's): each failure says which run it is of.
fn of_other_runs(revision: &Revision, others: &[Observed]) -> Vec<check::Failure> {
    let mut failed = Vec::new();
    let n = others.len() + 1;
    for (index, seen) in others.iter().enumerate() {
        for mut failure in check::held(revision, seen, None, check::OF_THE_ACTION) {
            failure.got = format!("run {} of {n} started together: {}", index + 2, failure.got);
            failed.push(failure);
        }
    }
    failed
}

/// No build ran: nothing was made again, and no memo ran; `why` says how that is known.
fn nothing_built(seen: &mut Observed, why: &str) {
    (seen.redone, seen.executed, seen.verified) = (Some(Vec::new()), Some(0), Some(0));
    seen.records = Some(0);
    seen.runs = Some(0);
    // No build ran, so no unit was made, whether or not a `blaze-work` line said so.
    seen.units = Some(seen.units.unwrap_or(0));
    (seen.written, seen.unkept, seen.described) = (Some(0), Some(0), Some(0));
    seen.ran = Some(BTreeMap::new());
    seen.spans = Some(BTreeMap::new());
    seen.noop = Some(true);
    seen.why = why.to_owned();
    // No build ran, so there is no work to account for.
    seen.account = Some(Vec::new());
}

/// A process started and not yet waited for ([`Real::finish`]).
struct Started {
    child: std::process::Child,
    stdout: thread::JoinHandle<String>,
    stderr: thread::JoinHandle<String>,
    began: Instant,
    what: String,
}

fn alive(pid: &str) -> bool {
    Command::new("kill").args(["-0", pid]).stderr(Stdio::null()).status().unwrap().success()
}

impl Real {
    fn new(test: &Test) -> Real {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // The daemon's socket is `<home>/v2.sock`, at most 104 bytes on macOS; and
        // `/private/tmp`, not `/tmp`, which is a link there.
        // `BLAZE_REVISIONS_ROOT`: another directory to work under (short, and off any link).
        let tmp = std::env::var("BLAZE_REVISIONS_ROOT")
            .unwrap_or_else(|_| if Path::new("/private/tmp").is_dir() { "/private/tmp" } else { "/tmp" }.to_owned());
        let root = Path::new(&tmp).join(format!("brv-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("test"), &test.name).unwrap();
        let bin = install(&root.file_name().expect("the root has a name").to_string_lossy());
        Real {
            blaze: bin.join("blaze"),
            bin,
            typed: test.as_typed(),
            last_build: Ok(None),
            last_executable: None,
            starts_from: 0,
            dir: root.join("w"),
            home: root.join("h"),
            cache: root.join("c"),
            others: Vec::new(),
            root,
            log_read: 0,
            keep: true,
            running: None,
            edited: None,
        }
    }

    /// A `build` started and interrupted `after` milliseconds in, and waited for: its
    /// outcome is not read (it may have finished first).
    fn cancel(&self, args: &[&str], env: &BTreeMap<String, String>, after: u64) {
        let mut child = self.command(args, env).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        thread::sleep(Duration::from_millis(after));
        let _ = Command::new("kill").args(["-INT", &child.id().to_string()]).stderr(Stdio::null()).status();
        let began = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if began.elapsed() > DEADLINE {
                child.kill().unwrap();
                child.wait().unwrap();
                self.kill_daemon();
                panic!("a `blaze {}` interrupted {after} ms in did not end in {} s: a hang", args.join(" "), DEADLINE.as_secs());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// `blaze ARGS` in the checkout, in the environment every command of the test has; in a
    /// case of the thin's own path ([`Test::as_typed`]) a `build`, `run`, `check` or `test`
    /// in a person's (the driver's own reads, `why`, `explain`, `stop`, `store`, keep it whole).
    fn command(&self, args: &[&str], env: &BTreeMap<String, String>) -> Command {
        let mut command = Command::new(&self.blaze);
        let typed = self.typed && matches!(args.first().copied(), Some("build" | "run" | "check" | "test"));
        self.environ(&mut command, env, !typed);
        command.args(args);
        command
    }

    /// `command` in the checkout, in the environment every command of the test has: blaze's
    /// and the programs [`Expect::as_built`](super::spec::Expect::as_built) runs alike.
    /// `whole`: with the driver's own settings of blaze (`BLAZE_LOCK`, `BLAZE_VERIFY_STORE`,
    /// `BLAZE_DUCKDB`); without them it is a person's, whose command the thin `blaze` reads
    /// (any `BLAZE_*` but `BLAZE_HOME` sends it to the engine, blaze-thin's `route::env`).
    fn environ(&self, command: &mut Command, env: &BTreeMap<String, String>, whole: bool) {
        command.env_clear();
        let passed: &[&str] = if whole { &["HOME", "BLAZE_DUCKDB"] } else { &["HOME"] };
        for name in passed {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if whole {
            command.env("BLAZE_LOCK", "write").env("BLAZE_VERIFY_STORE", "1");
        }
        command
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("TMPDIR", self.root.join("t"))
            .env("BLAZE_HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("RUST_BACKTRACE", "0")
            .envs(env)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }

    fn blaze(&self, args: &[&str], env: &BTreeMap<String, String>) -> Out {
        let command = self.command(args, env);
        self.wait(command, &format!("blaze {}", args.join(" ")))
    }

    /// `command` run to its end, `what` naming it in a hang's panic.
    fn wait(&self, command: Command, what: &str) -> Out {
        let started = self.start(command, what);
        self.finish(started)
    }

    /// `command` started, its output drained as it comes ([`Self::finish`] waits for it).
    fn start(&self, mut command: Command, what: &str) -> Started {
        let began = Instant::now();
        let mut child = command.spawn().unwrap_or_else(|error| panic!("starting `{what}`: {error}"));
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());
        Started { child, stdout, stderr, began, what: what.to_owned() }
    }

    /// A [`Started`] process waited for, at most [`DEADLINE`] from its start.
    fn finish(&self, started: Started) -> Out {
        let Started { mut child, stdout, stderr, began, what } = started;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if began.elapsed() > DEADLINE {
                child.kill().unwrap();
                child.wait().unwrap();
                self.kill_daemon();
                panic!(
                    "`{what}` in {} ran {} s without finishing: a hang (the daemon's log: {})",
                    self.dir.display(),
                    DEADLINE.as_secs(),
                    self.home.join("v2/serve.log").display()
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        Out {
            ok: status.success(),
            status: status.code(),
            stdout: stdout.join().unwrap(),
            stderr: stderr.join().unwrap(),
            ms: began.elapsed().as_millis(),
        }
    }

    /// The pid of the daemon the home's log last says is listening.
    fn daemon_pid(&self) -> Option<String> {
        let log = fs::read_to_string(self.home.join("v2/serve.log")).ok()?;
        let pid = log.rsplit("listening on ").next()?.split("(pid ").nth(1)?.split(')').next()?;
        (!pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())).then(|| pid.to_owned())
    }

    /// `kill -9` of the daemon, by its pid, waited for until the process is gone: a command
    /// started meanwhile would reach the dying daemon.
    fn kill_daemon(&self) {
        let Some(pid) = self.daemon_pid() else { return };
        let _ = Command::new("kill").args(["-KILL", &pid]).stderr(Stdio::null()).status();
        let began = Instant::now();
        while alive(&pid) {
            assert!(began.elapsed() < Duration::from_secs(30), "pid {pid} outlived its kill -9 by 30 s");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// `blaze stop`, and the daemon's process gone: `stop` answers before the process has
    /// exited, and a daemon started meanwhile at the same home has ended at once (exit 1,
    /// "its command line was refused", 2026-11-11).
    fn stop(&self) {
        let pid = self.daemon_pid();
        let out = self.blaze(&["stop"], &BTreeMap::new());
        assert!(out.ok, "`blaze stop` failed:\n{}", out.stderr);
        let began = Instant::now();
        while pid.as_deref().is_some_and(alive) {
            assert!(began.elapsed() < Duration::from_secs(60), "the daemon (pid {pid:?}) outlived its `blaze stop` by 60 s");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn write(&self, changes: &[Change], how: &Step) {
        for change in changes {
            let path = self.dir.join(&change.path);
            let Some(bytes) = &change.bytes else {
                fs::remove_file(&path).unwrap_or_else(|error| panic!("removing {}: {error}", path.display()));
                continue;
            };
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            match how {
                Step::EditTwice => {
                    if let Some(first) = super::spec::first_save(&change.path, bytes) {
                        fs::write(&path, first).unwrap();
                        thread::sleep(Duration::from_millis(30));
                    }
                    fs::write(&path, bytes).unwrap();
                }
                Step::EditByRename => {
                    let beside = path.with_extension("saved-by-rename");
                    fs::write(&beside, bytes).unwrap();
                    fs::rename(&beside, &path).unwrap();
                }
                Step::EditTruncateWrite => {
                    let mut file = fs::File::create(&path).unwrap();
                    file.sync_all().unwrap();
                    file.write_all(bytes).unwrap();
                }
                _ => fs::write(&path, bytes).unwrap(),
            }
        }
    }

    /// Panics the daemon's log holds past what was read before, and each line of it that
    /// says an unwind of the changed-set walk was contained.
    fn panics(&mut self) -> (u64, String, Vec<String>) {
        let Ok(log) = fs::read(self.home.join("v2/serve.log")) else { return (0, String::new(), Vec::new()) };
        // A new daemon may start its log again.
        let from = if (log.len() as u64) < self.log_read { 0 } else { self.log_read as usize };
        self.log_read = log.len() as u64;
        let new = String::from_utf8_lossy(&log[from..]).into_owned();
        let alarms: Vec<String> =
            new.lines().filter(|line| line.contains("verification of a row unwound")).map(str::to_owned).collect();
        let mut lines = new.lines().filter(|line| line.contains("panicked at"));
        match lines.next() {
            Some(first) => (1 + lines.count() as u64, first.to_owned(), alarms),
            None => (0, String::new(), alarms),
        }
    }

    /// The revision's action run, and what blaze says of it: one [`Observed`] a run (an
    /// incremental action runs `at_once` times, started together; a clean one once). The
    /// first holds what is of the action as a whole ([`check::OF_THE_ACTION`]). `account`:
    /// whether the daemon is asked what it made again and whether a daemon is reached (the
    /// incremental build), or only the outcome is read (the clean one).
    fn act(&mut self, revision: &Revision, env: &BTreeMap<String, String>, account: bool) -> Vec<Observed> {
        let mut runs = self.acted(revision, env, account);
        // After the action's account, so the build is never the one that account is of.
        if revision.expect.as_built {
            let built = self.as_built(revision, env);
            for seen in &mut runs {
                seen.built = Some(built.clone()); // clone: one build's run, held against each run
            }
        }
        runs
    }

    /// Who holds blaze's state now: whether a daemon runs on it, asked of `serve.lock` as
    /// blaze asks it (`blaze_cargo::serve::holder`, a shared try-lock that never waits).
    fn held(&self) -> bool {
        let state = self.home.join("v2");
        match blaze_cargo::serve::holder(&state) {
            Ok(blaze_cargo::serve::Holder::None) => false,
            Ok(blaze_cargo::serve::Holder::Daemon { .. }) => true,
            Err(error) => panic!("who holds {}: {error:#}", state.join("serve.lock").display()),
        }
    }

    /// The daemon's log of this home.
    fn log(&self) -> PathBuf {
        self.home.join("v2/serve.log")
    }

    /// How long this home's `serve.log` is: 0 where no daemon ever wrote one.
    fn log_len(&self) -> u64 {
        match fs::metadata(self.log()) {
            Ok(meta) => meta.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("{}: {error}", self.log().display()),
        }
    }

    /// The daemons that started since `from` bytes into `serve.log`: the lines a daemon writes
    /// as it starts serving (`blaze_cargo::serve::LISTENING`). The log is appended to by every
    /// daemon; one shorter than `from` is not counted but refused, naming it.
    fn starts_since(&self, from: u64) -> u64 {
        let log = match fs::read(self.log()) {
            Ok(log) => log,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && from == 0 => return 0,
            Err(error) => panic!("{}: {error}", self.log().display()),
        };
        assert!(
            log.len() as u64 >= from,
            "{} is {} bytes, shorter than the {from} it was when the revision began: its daemon starts can't be counted",
            self.log().display(),
            log.len()
        );
        String::from_utf8_lossy(&log[from as usize..]).lines().filter(|line| line.contains(blaze_cargo::serve::LISTENING)).count()
            as u64
    }

    /// What blaze's JSON messages in `stdout` say of the builds the driver tells apart: the
    /// last build's number, and, of a `blaze build` (`build`), the program it names.
    fn saw(&mut self, stdout: &str, build: bool) {
        for line in stdout.lines().filter(|line| line.starts_with("{\"reason\"")) {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            match value["reason"].as_str() {
                Some("build-finished") => {
                    if let Some(id) = value["build_id"].as_u64() {
                        self.last_build = Ok(Some(id));
                    }
                }
                Some("compiler-artifact") if build => {
                    if let Some(path) = value["executable"].as_str() {
                        self.last_executable = Some(path.to_owned());
                    }
                }
                _ => {}
            }
        }
    }

    /// `blaze build` with the revision's arguments before a `--`: the program it names, or why
    /// there is none.
    fn built_file(&mut self, revision: &Revision, env: &BTreeMap<String, String>) -> Result<String, String> {
        let mut args = vec!["build", "--message-format=json"];
        args.extend(split(revision).0.iter().map(String::as_str));
        let out = self.blaze(&args, env);
        if !out.ok {
            return Err(format!("`blaze {}` failed:\n{}", args.join(" "), out.stderr));
        }
        self.last_executable = None;
        self.saw(&out.stdout, true);
        // clone: the path, out of the driver's state
        self.last_executable.clone().ok_or_else(|| format!("`blaze {}` named no executable in its messages", args.join(" ")))
    }

    /// [`Expect::as_built`](super::spec::Expect::as_built): `blaze build` with the revision's
    /// arguments before a `--`, then the file it names run with those after it, in the
    /// checkout and the test's environment.
    fn as_built(&mut self, revision: &Revision, env: &BTreeMap<String, String>) -> Result<Ran, String> {
        let executable = self.built_file(revision, env)?;
        let mut command = Command::new(&executable);
        self.environ(&mut command, env, !self.typed);
        command.args(split(revision).1);
        let ran = self.wait(command, &executable);
        Ok(Ran { stdout: ran.stdout, stderr: ran.stderr, status: ran.status })
    }

    /// The action's commands, `runs` of them started together and each waited for. A case of
    /// the thin's own path ([`Test::as_typed`]) types them as a person does; any other asks for
    /// blaze's JSON messages. `run the file` runs the program the last `blaze build` named; from
    /// nothing (`fresh`) that build is made first.
    fn commands(&mut self, revision: &Revision, env: &BTreeMap<String, String>, runs: u64, fresh: bool) -> Vec<Out> {
        let label = &revision.label;
        let mut started = Vec::new();
        if revision.action == super::spec::Action::RunTheFile {
            if fresh && let Err(why) = self.built_file(revision, env) {
                panic!("{label}: `run the file` from nothing: {why}");
            }
            // clone: the path, out of the driver's state
            let Some(program) = self.last_executable.clone() else {
                panic!("{label}: `run the file` with no program made: no `blaze build` named one before it (a `build` step, or an `as_built` revision)");
            };
            for _ in 0..runs {
                let mut command = Command::new(&program);
                self.environ(&mut command, env, !self.typed);
                command.args(split(revision).1);
                started.push(self.start(command, &program));
            }
        } else {
            let mut args = vec![revision.action.word()];
            if !self.typed {
                args.push("--message-format=json");
            }
            args.extend(revision.args.iter().map(String::as_str));
            for _ in 0..runs {
                let command = self.command(&args, env);
                started.push(self.start(command, &format!("blaze {}", args.join(" "))));
            }
        }
        started.into_iter().map(|it| self.finish(it)).collect()
    }

    /// The revision's action run, and what blaze says of it ([`Self::act`]).
    fn acted(&mut self, revision: &Revision, env: &BTreeMap<String, String>, account: bool) -> Vec<Observed> {
        // Whether a daemon is reached, asked before the driver asks anything of its own.
        let before = account.then(|| self.held());
        let at_action = self.log_len();
        let outs = self.commands(revision, env, if account { revision.at_once } else { 1 }, !account);
        let after = account.then(|| self.held());
        let started_by_action = self.starts_since(at_action);
        let starts = account.then(|| self.starts_since(self.starts_from));
        if !self.typed {
            let [out] = <[Out; 1]>::try_from(outs).unwrap_or_else(|_| panic!("{}: one run of a JSON action", revision.label));
            let (mut seen, build) = self.read_messages(revision, out);
            (seen.daemon_after, seen.starts) = (after, starts);
            if account {
                if seen.noop == Some(true) {
                    nothing_built(&mut seen, "answered as the last build was (the no-op lane)");
                } else {
                    self.account_of(revision, &mut seen, build);
                }
            }
            return vec![seen];
        }
        let mut runs: Vec<Observed> = outs
            .into_iter()
            .map(|out| Observed { ok: out.ok, status: out.status, stdout: out.stdout, said: out.stderr, ms: out.ms, ..Observed::default() })
            .collect();
        let first = &mut runs[0];
        (first.panics, first.first_panic, first.alarms) = self.panics();
        (first.daemon_after, first.starts) = (after, starts);
        if !account {
            return runs;
        }
        // A command typed as a person types it says nothing of its build: whether one ran is
        // known from the daemon. No daemon before, none started and none after: none built.
        if before == Some(false) && started_by_action == 0 && after == Some(false) {
            nothing_built(&mut runs[0], "no daemon held blaze's state before the action, none started during it and none held it after: nothing was built");
            return runs;
        }
        if runs.len() > 1 {
            runs[0].account_said = format!(
                "{} runs started together that a daemon served: blaze names the last build alone, so what each made is not \
                 told apart and is not read",
                runs.len()
            );
            return runs;
        }
        let none = BTreeMap::new();
        let why = self.blaze(&["why", "--all", "--json"], &none);
        let named = serde_json::from_str::<serde_json::Value>(why.stdout.trim()).ok().filter(|_| why.ok).map(|it| it["build"].as_u64());
        let mut seen = runs.swap_remove(0);
        match (named, self.last_build) {
            (None, _) => seen.account_said = format!("`blaze why --all --json` said: {}{}", why.stdout, why.stderr),
            (Some(build), Ok(last)) if build == last => nothing_built(
                &mut seen,
                "a daemon answered it with no build of its own: `blaze why` names the build the driver saw before it",
            ),
            (Some(Some(build)), Ok(_)) => {
                self.last_build = Ok(Some(build));
                self.account_of(revision, &mut seen, Some(build));
            }
            (Some(None), Ok(_)) => seen.account_said = "`blaze why --all --json` names no build, after a build the driver saw".to_owned(),
            (Some(_), Err(unknown)) => seen.account_said = format!("whether the action built is not known: {unknown}"),
        }
        vec![seen]
    }

    /// What one command run with `--message-format=json` said: the build's messages read, every
    /// other line of standard output the program's; and the build's number, where it names one.
    fn read_messages(&mut self, revision: &Revision, out: Out) -> (Observed, Option<u64>) {
        let mut seen = Observed { ok: out.ok, status: out.status, said: out.stderr, ms: out.ms, ..Observed::default() };
        // Each line of standard output that is one of blaze's JSON messages is the build's;
        // every other line is the program's.
        let mut build = None;
        let mut executable = None;
        let mut diagnostics = Vec::new();
        for line in out.stdout.split_inclusive('\n') {
            let value = match line.starts_with("{\"reason\"") {
                true => serde_json::from_str::<serde_json::Value>(line).ok(),
                false => None,
            };
            let Some(value) = value else {
                seen.stdout.push_str(line);
                continue;
            };
            match value["reason"].as_str() {
                Some("blaze-work") => {
                    seen.units = value["compiled"].as_u64();
                    seen.link = Some(match value["links"].as_array().and_then(|it| it.last()) {
                        Some(link) => link["how"].as_str().unwrap_or("?").to_owned(),
                        None => "none".to_owned(),
                    });
                }
                Some("build-finished") => {
                    build = value["build_id"].as_u64();
                    seen.noop = Some(value["build_id"].is_null());
                }
                Some("compiler-artifact") => {
                    if let Some(path) = value["executable"].as_str() {
                        executable = Some(path.to_owned());
                    }
                }
                Some("compiler-message") => {
                    let message = &value["message"];
                    if let Some(rendered) = message["rendered"].as_str() {
                        seen.said.push_str(rendered);
                    }
                    let spans = message["spans"].as_array();
                    let span = spans.and_then(|all| all.iter().find(|it| it["is_primary"] == true).or(all.first()));
                    diagnostics.push(format!(
                        "{} {} {:?} {}:{}:{}",
                        message["level"].as_str().unwrap_or("?"),
                        message["code"]["code"].as_str().unwrap_or("-"),
                        message["message"].as_str().unwrap_or("?"),
                        span.and_then(|it| it["file_name"].as_str()).unwrap_or("-"),
                        span.and_then(|it| it["line_start"].as_u64()).unwrap_or(0),
                        span.and_then(|it| it["column_start"].as_u64()).unwrap_or(0),
                    ));
                }
                _ => {}
            }
        }
        diagnostics.sort();
        seen.diagnostics = Some(diagnostics);
        (seen.panics, seen.first_panic, seen.alarms) = self.panics();
        if let Some(build) = build {
            self.last_build = Ok(Some(build));
        }
        if revision.action == super::spec::Action::Build
            && let Some(path) = &executable
        {
            self.last_executable = Some(path.clone()); // clone: the path, kept as the last build's program
        }
        if revision.expect.bytes && seen.ok {
            seen.image = executable.and_then(|path| fs::read(path).ok());
        }
        (seen, build)
    }

    /// What blaze says it made of the build numbered `build` (`blaze why`, `blaze explain`),
    /// into `seen`: what was made again, its account, the engine's memos, its spans and its
    /// queries, each as the revision asks for them.
    fn account_of(&mut self, revision: &Revision, seen: &mut Observed, build: Option<u64>) {
        let none = BTreeMap::new();
        let why = self.blaze(&["why", "--all", "--json"], &none);
        match serde_json::from_str::<serde_json::Value>(why.stdout.trim()) {
            Ok(value) if why.ok && value["build"].as_u64() == build && build.is_some() => {
                let mut redone = Vec::new();
                let mut records = 0;
                let mut unkept = 0;
                let mut runs = 0;
                for group in value["groups"].as_array().into_iter().flatten() {
                    if group["step"].as_str() == Some("unit") {
                        records += group["items"].as_array().map_or(0, |it| it.len() as u64);
                    }
                    if group["step"].as_str() == Some("unit record") {
                        unkept += group["items"].as_array().map_or(0, |it| it.len() as u64);
                    }
                    // `serve/unit/script.rs`, `RUN`: the step a build script's run misses under.
                    if group["step"].as_str() == Some("build script run") {
                        runs += group["items"].as_array().map_or(0, |it| it.len() as u64);
                    }
                    if group["step"].as_str().is_some_and(|it| it.starts_with("codegen")) {
                        redone.extend(group["items"].as_array().into_iter().flatten().filter_map(|it| it.as_str()).map(str::to_owned));
                    }
                }
                redone.sort();
                redone.dedup();
                seen.redone = Some(redone);
                seen.records = Some(records);
                seen.unkept = Some(unkept);
                seen.runs = Some(runs);
            }
            _ => seen.why = format!("`blaze why --all --json` of build {build:?} said: {}{}\n", why.stdout, why.stderr),
        }
        let said = self.blaze(&["why", "--all"], &none);
        for line in said.stdout.lines().chain(said.stderr.lines()).filter(|line| line.contains("no recorded reason")) {
            seen.alarms.push(line.to_owned());
        }
        seen.why.push_str(&said.stdout);
        // The build's account: the standing expectation of every revision (`check::ACCOUNT`).
        match build {
            Some(build) => {
                let account = self.blaze(&["explain", "account", &format!("build={build}"), "--json"], &none);
                let row = account
                    .stdout
                    .lines()
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .find(|row| row["build_id"].as_u64() == Some(build));
                match row {
                    Some(row) if account.ok => {
                        let n = |column: &str| row[column].as_u64().map_or("-".to_owned(), |it| it.to_string());
                        seen.account = Some(match row["alarms"].as_str() {
                            Some(alarms) => alarms.split(", ").map(str::to_owned).collect(),
                            None => Vec::new(),
                        });
                        seen.account_said = format!(
                            "written={} executed={} ran_uncaused={} steps={} changes={} by_unexplained={} by_unstable={} unbalanced={} unkept={}",
                            n("written"), n("executed"), n("ran_uncaused"), n("steps"), n("changes"), n("by_unexplained"), n("by_unstable"), n("unbalanced"), n("unkept")
                        );
                    }
                    _ => seen.account_said = format!("`blaze explain account build={build} --json` said: {}{}", account.stdout, account.stderr),
                }
            }
            None => seen.account_said = "the build's number was not seen in its messages".to_owned(),
        }
        if let Some(build) = build
            && (revision.expect.max_executed.is_some()
                || revision.expect.max_verified.is_some()
                || revision.expect.max_written.is_some()
                || revision.expect.max_described.is_some()
                || !revision.expect.max_ran.is_empty())
        {
            let work = self.blaze(&["explain", "edit_work", &format!("build={build}"), "limit=500", "--json"], &none);
            for line in work.stdout.lines() {
                let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                if row["stage"] == "engine memos" {
                    match row["counter"].as_str() {
                        Some("executed") => seen.executed = row["total"].as_u64(),
                        Some("verified") => seen.verified = row["total"].as_u64(),
                        Some("written") => seen.written = row["total"].as_u64(),
                        _ => {}
                    }
                }
                if row["stage"] == "codegen walk" && row["counter"] == "described" {
                    seen.described = row["total"].as_u64();
                }
            }
            if work.stderr.contains("counted no work in any stage") {
                (seen.executed, seen.verified, seen.written) = (Some(0), Some(0), Some(0));
            }
            // A build whose units were all records or the no-op lane's answer walked nothing.
            if revision.expect.max_described.is_some() && seen.described.is_none() && seen.executed.is_some() {
                seen.described = Some(0);
            }
            if seen.executed.is_none() {
                seen.why.push_str(&format!("`blaze explain edit_work build={build}` said: {}{}\n", work.stdout, work.stderr));
            }
        }
        if let Some(build) = build
            && !revision.expect.spans.is_empty()
        {
            // Each span name the build ran, with how many (`span_sums`, folded spans counted).
            let view = self.blaze(&["explain", "time_by_span_name", &format!("build={build}"), "--json"], &none);
            let mut spans = BTreeMap::new();
            for line in view.stdout.lines() {
                let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                let count = &row["spans"];
                // A sum may come as an integer, a float or (DuckDB's HUGEINT) a string.
                let count = count.as_u64().or_else(|| count.as_f64().map(|it| it as u64)).or_else(|| count.as_str().and_then(|it| it.parse().ok()));
                if let (Some(name), Some(count)) = (row["name"].as_str(), count) {
                    *spans.entry(name.to_owned()).or_insert(0) += count;
                }
            }
            match view.ok && !spans.is_empty() {
                true => seen.spans = Some(spans),
                false => seen.why.push_str(&format!("`blaze explain time_by_span_name build={build}` said: {}{}\n", view.stdout, view.stderr)),
            }
        }
        if let Some(build) = build
            && !revision.expect.max_ran.is_empty()
        {
            // Each query that ran in the build, by name (`query_counts`).
            let counts = self.blaze(&["explain", "counts_by_cause", &format!("build={build}"), "limit=5000", "--json"], &none);
            let mut ran = BTreeMap::new();
            let mut rows = 0;
            for line in counts.stdout.lines() {
                let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                rows += 1;
                if let (Some(query), Some(executed)) = (row["query"].as_str(), row["executed"].as_u64()) {
                    *ran.entry(query.to_owned()).or_insert(0) += executed;
                }
            }
            // A view that failed or said nothing is not a build that ran nothing: only a
            // build whose engine work was read as none (above) has no rows by right.
            match counts.ok && (rows > 0 || seen.executed == Some(0)) {
                true => seen.ran = Some(ran),
                false => seen.why.push_str(&format!("`blaze explain counts_by_cause build={build}` said: {}{}\n", counts.stdout, counts.stderr)),
            }
        }
    }

    /// The running program ended, if one runs: what it and `blaze run` said on standard error.
    fn end_running(&mut self) -> String {
        self.running.take().map(Running::end).unwrap_or_default()
    }

    /// `before`: the project's files as the revision before left them.
    fn step(&mut self, step: &Step, revision: &Revision, env: &BTreeMap<String, String>, before: &BTreeMap<String, Vec<u8>>) {
        let label = &revision.label;
        match step {
            Step::Edit | Step::EditByRename | Step::EditTruncateWrite | Step::EditTwice => {
                self.write(&revision.changes, step);
                self.edited = Some(Instant::now());
            }
            Step::Cancel(after) => {
                let mut args = vec!["build"];
                args.extend(revision.args.iter().map(String::as_str));
                self.cancel(&args, env, *after);
                // It may have made a build, whose number it never said.
                self.last_build = Err("a build was cancelled since the last build blaze named, and may have made one");
            }
            Step::Beside(name) => {
                let first = self.dir.clone(); // clone: the checkout to come back to
                self.enter(Some(name));
                // clone: the files as changes, once a step
                let files: Vec<Change> = before.iter().map(|(path, bytes)| Change { path: path.clone(), bytes: Some(bytes.clone()) }).collect();
                self.write(&files, &Step::Edit);
                let mut args = vec!["build"];
                args.extend(revision.args.iter().map(String::as_str));
                let out = self.blaze(&args, env);
                self.dir = first;
                assert!(out.ok, "{}: the build of the second checkout `{name}` failed:\n{}", revision.label, out.stderr);
                // Its messages were not asked for: whether it made a build is not said.
                self.last_build = Err("a second checkout was built since the last build blaze named, which names no number");
            }
            Step::Start => {
                assert!(self.running.is_none(), "{label}: `start` with a program already running");
                let mut args = vec!["run"];
                args.extend(revision.args.iter().map(String::as_str));
                let mut child = self
                    .command(&args, env)
                    .spawn()
                    .unwrap_or_else(|error| panic!("{label}: starting {}: {error}", self.blaze.display()));
                let printed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let (mut out, into) = (child.stdout.take().unwrap(), std::sync::Arc::clone(&printed));
                // Each read with when it arrived, until the program's output ends.
                thread::spawn(move || {
                    let mut chunk = [0u8; 4096];
                    while let Ok(got @ 1..) = out.read(&mut chunk) {
                        let read = (Instant::now(), chunk[..got].to_vec());
                        into.lock().unwrap_or_else(|it| it.into_inner()).push(read);
                    }
                });
                let said = drain(child.stderr.take().unwrap());
                self.running = Some(Running { child, started: Instant::now(), printed, said });
                self.edited = None;
            }
            Step::Await { within_ms, text } => {
                let Some(running) = &mut self.running else {
                    panic!("{label}: `await` with no program running: `start` one first");
                };
                // From the last save, or from the start with none since.
                let from = self.edited.unwrap_or(running.started);
                let by = from + Duration::from_millis(*within_ms);
                let seen = loop {
                    if let Some(seen) = running.first_after(text, from) {
                        break Some(seen);
                    }
                    let gone = running.child.try_wait().unwrap().is_some();
                    if gone || Instant::now() > by {
                        break None;
                    }
                    thread::sleep(Duration::from_millis(1));
                };
                let took = seen.map(|it| it.saturating_duration_since(from).as_millis());
                if !seen.is_some_and(|it| it <= by) {
                    let (all, _) = running.printed();
                    let tail = String::from_utf8_lossy(&all[all.len().saturating_sub(400)..]).into_owned();
                    let said = self.end_running();
                    panic!(
                        "{label}: the running program did not print `{text}` within {within_ms} ms of {} (it took \
                         {took:?} ms); its output ends:\n{tail}\nand it and `blaze run` said:\n{said}",
                        if self.edited.is_some() { "the save" } else { "its start" },
                    );
                }
                eprintln!("{label}: the running program printed `{text}` {} ms after {}", took.unwrap_or(0), if self.edited.is_some() { "the save" } else { "its start" });
            }
            Step::Count { times, text } => {
                let Some(running) = &self.running else {
                    panic!("{label}: `count` with no program running: `start` one first");
                };
                let (all, _) = running.printed();
                let found = all.windows(text.len().max(1)).filter(|it| *it == text.as_bytes()).count() as u64;
                if found != *times {
                    let said = self.end_running();
                    panic!(
                        "{label}: the running program printed `{text}` {found} times, not {times} (a program \
                         that prints it as it starts started {found} times); it and `blaze run` said:\n{said}"
                    );
                }
            }
            Step::End => {
                assert!(self.running.is_some(), "{label}: `end` with no program running");
                self.end_running();
            }
            Step::Stop => self.stop(),
            Step::Kill => {
                assert!(self.daemon_pid().is_some(), "`kill`: the daemon's log names no pid");
                self.kill_daemon();
            }
            Step::Touch(path) => {
                let file = fs::File::options().write(true).open(self.dir.join(path)).unwrap_or_else(|error| panic!("touch {path}: {error}"));
                file.set_modified(SystemTime::now()).unwrap();
            }
            Step::Wait(ms) => std::thread::sleep(std::time::Duration::from_millis(*ms)),
            Step::Project(name) => self.enter(Some(name)),
            Step::Home(name) => self.rehome(name),
            Step::Packed(name) => self.repack(name, env),
            Step::Do(action) => {
                // The revision's own arguments: a request that differs in them is another
                // request to the no-op lane.
                let mut args = vec![action.word(), "--message-format=json"];
                args.extend(revision.args.iter().map(String::as_str));
                let out = self.blaze(&args, env);
                assert!(out.ok, "{}: the step `{}` failed:\n{}", revision.label, action.word(), out.stderr);
                self.saw(&out.stdout, *action == super::spec::Action::Build);
            }
            Step::ReplaceEngine => self.replace_engine(label),
        }
    }

    /// `replace engine` ([`Step::ReplaceEngine`]): the engine beside the thin `blaze` copied to a
    /// name beside it, its modification time set to now, and renamed over itself. A copy that
    /// kept the file's inode would test nothing, and is refused.
    fn replace_engine(&self, label: &str) {
        use std::os::unix::fs::MetadataExt as _;
        let engine = self.bin.join(blaze_thin::ENGINE);
        let beside = self.bin.join(format!("{}.replacing", blaze_thin::ENGINE));
        let was = fs::metadata(&engine).unwrap_or_else(|error| panic!("{label}: `replace engine`: {}: {error}", engine.display()));
        fs::copy(&engine, &beside).unwrap_or_else(|error| panic!("{label}: `replace engine`: copying {} to {}: {error}", engine.display(), beside.display()));
        let copy = fs::File::options().write(true).open(&beside).unwrap_or_else(|error| panic!("{label}: `replace engine`: {}: {error}", beside.display()));
        copy.set_modified(SystemTime::now()).unwrap_or_else(|error| panic!("{label}: `replace engine`: the time of {}: {error}", beside.display()));
        drop(copy);
        fs::rename(&beside, &engine).unwrap_or_else(|error| panic!("{label}: `replace engine`: renaming {} over {}: {error}", beside.display(), engine.display()));
        let now = fs::metadata(&engine).unwrap_or_else(|error| panic!("{label}: `replace engine`: {}: {error}", engine.display()));
        assert!(
            now.ino() != was.ino() && now.len() == was.len(),
            "{label}: `replace engine`: {} is inode {} of {} bytes, was inode {} of {} bytes: not the same bytes under a new inode",
            engine.display(),
            now.ino(),
            now.len(),
            was.ino(),
            was.len()
        );
    }

    /// Commands run in the checkout of the project `name` from here on (`project NAME`), or
    /// in the first (`None`): a directory beside the first, under the same home and daemon.
    fn enter(&mut self, name: Option<&str>) {
        let dir = self.root.join(match name {
            // Never the first checkout's, the homes' or the scratch directory's name.
            Some(name) => format!("p-{name}"),
            None => "w".to_owned(),
        });
        if name.is_some() && !self.others.contains(&dir) {
            self.others.push(dir.clone()); // clone: a path, once a project
        }
        fs::create_dir_all(&dir).unwrap();
        self.dir = dir;
    }

    /// Commands run under the home `h-<name>` and the machine directory `c-<name>` from here
    /// on (`home NAME`): the daemon stopped, and the new home given a copy of the store it
    /// left, nothing else of the old home (no database image, no build directory, no ledger).
    fn rehome(&mut self, name: &str) {
        self.stop();
        let (home, cache) = (self.root.join(format!("h-{name}")), self.root.join(format!("c-{name}")));
        let store = self.home.join("v2/store");
        assert!(store.is_dir(), "`home {name}`: the first home has no store at {}", store.display());
        copy_dir(&store, &home.join("v2/store"));
        for dir in [&self.home, &self.cache] {
            if !self.others.contains(dir) {
                self.others.push(dir.clone()); // clone: a path, once a home
            }
        }
        (self.home, self.cache) = (home, cache);
        self.log_read = 0;
        self.starts_from = 0;
    }

    /// Commands run under the home `h-<name>` and the machine directory `c-<name>` from here
    /// on (`packed NAME`), as [`Self::rehome`], but the new home's store starts empty and is
    /// given the first home's records as a pack: exported whole from the first home with its
    /// daemon stopped, imported under the new one before any daemon starts there. The pack
    /// is unsigned and imported on this driver's word (`--unsigned`), as a file a user names.
    fn repack(&mut self, name: &str, env: &BTreeMap<String, String>) {
        self.stop();
        let pack = self.root.join(format!("k-{name}"));
        let at = pack.to_str().expect("the driver's paths are UTF-8");
        let out = self.blaze(&["store", "export", "--all", "--pin", "build", "--out", at], env);
        assert!(out.ok, "`packed {name}`: the export failed:\n{}", out.stderr);
        let (home, cache) = (self.root.join(format!("h-{name}")), self.root.join(format!("c-{name}")));
        for dir in [&self.home, &self.cache, &pack] {
            if !self.others.contains(dir) {
                self.others.push(dir.clone()); // clone: a path, once a home
            }
        }
        (self.home, self.cache) = (home, cache);
        self.log_read = 0;
        self.starts_from = 0;
        let out = self.blaze(&["store", "import", at, "--pin", "build", "--unsigned"], env);
        assert!(out.ok, "`packed {name}`: the import failed:\n{}", out.stderr);
    }

    /// The homes and every checkout set aside under `as_`, so the next build starts from
    /// nothing at the same paths.
    fn set_aside(&mut self, as_: &str) {
        let first = self.root.join("w");
        for dir in [&first, &self.home, &self.cache].into_iter().chain(&self.others) {
            if dir.exists() {
                let aside = dir.with_extension(as_);
                let _ = fs::remove_dir_all(&aside);
                fs::rename(dir, aside).unwrap();
            }
        }
        self.log_read = 0;
    }
}

/// `from`, a directory, copied to `to` with everything under it.
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap_or_else(|error| panic!("reading {}: {error}", from.display())) {
        let entry = entry.unwrap();
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        match entry.file_type().unwrap().is_dir() {
            true => copy_dir(&source, &target),
            false => {
                fs::copy(&source, &target).unwrap_or_else(|error| panic!("copying {}: {error}", source.display()));
            }
        }
    }
}

impl Drop for Real {
    fn drop(&mut self) {
        self.end_running();
        if self.home.exists() {
            self.kill_daemon();
        }
        if !thread::panicking() || !self.keep {
            let _ = fs::remove_dir_all(&self.root);
        }
        // The installed pair is no log: it goes whatever became of the test.
        let _ = fs::remove_dir_all(&self.bin);
    }
}

/// When a revision is also built from nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Clean {
    /// Every revision: the suite's cases.
    Always,
    /// Only a revision whose incremental build failed a check, to tell a build that is wrong
    /// only incrementally from one blaze gets wrong from nothing too: a corpus of hundreds.
    WhenWrong,
}

/// `test`'s rows under the real driver, and where its logs are.
pub fn rows(test: &Test, clean_when: Clean) -> (Vec<Row>, String) {
    let _slot = Slot::take();
    let mut real = Real::new(test);
    real.keep = clean_when == Clean::Always;
    fs::create_dir_all(real.root.join("t")).unwrap();
    let mut rows: Vec<Row> = Vec::new();
    // Each revision's runs after its first (`at_once`), held again beside the clean build.
    let mut others: Vec<Vec<Observed>> = Vec::new();
    for (index, revision) in test.revisions.iter().enumerate() {
        let env = test.env_at(index);
        let before = test.files_at(index.saturating_sub(1));
        real.starts_from = real.log_len();
        for step in &revision.steps {
            real.step(step, revision, &env, &before);
        }
        let mut runs = real.act(revision, &env, true).into_iter();
        let seen = runs.next().unwrap_or_else(|| panic!("{}: an action with no run", revision.label));
        let rest: Vec<Observed> = runs.collect();
        let mut failed = check::held(revision, &seen, None, &[]);
        failed.extend(of_other_runs(revision, &rest));
        others.push(rest);
        rows.push(Row { label: revision.label.clone(), seen, failed, clean_ok: None });
    }
    real.end_running();
    real.stop();
    real.set_aside("incremental");
    // Each revision from nothing, at the same paths. Two revisions of one state, action and
    // environment share one clean build.
    let mut built: Vec<(usize, Observed)> = Vec::new();
    for (index, revision) in test.revisions.iter().enumerate() {
        if clean_when == Clean::WhenWrong && rows[index].failed.is_empty() {
            continue;
        }
        let env = test.env_at(index);
        let same = built.iter().find(|(earlier, _)| {
            let before = &test.revisions[*earlier];
            test.files_at(*earlier) == test.files_at(index)
                && test.env_at(*earlier) == env
                && before.action == revision.action
                && before.args == revision.args
                && before.expect.bytes == revision.expect.bytes
        });
        let clean = match same {
            // clone: one clean build's outcome, read by a second revision
            Some((_, clean)) => clean.clone(),
            None => {
                // In its project's checkout, at the path the incremental build had it.
                real.enter(test.project_at(index).1);
                let files: Vec<Change> =
                    test.files_at(index).into_iter().map(|(path, bytes)| Change { path, bytes: Some(bytes) }).collect();
                real.write(&files, &Step::Edit);
                let clean = real.act(revision, &env, false).into_iter().next().unwrap_or_else(|| panic!("{}: a clean action with no run", revision.label));
                real.stop();
                real.set_aside("clean");
                clean
            }
        };
        rows[index].failed = check::held(revision, &rows[index].seen, Some(&clean), &[]);
        rows[index].failed.extend(of_other_runs(revision, &others[index]));
        rows[index].clean_ok = Some(clean.ok);
        built.push((index, clean));
    }
    let note = format!(
        "the daemon's log: {}; the last clean build's: {}\n",
        real.home.with_extension("incremental").join("v2/serve.log").display(),
        real.home.with_extension("clean").join("v2/serve.log").display()
    );
    (rows, note)
}

/// `test` under the real driver.
pub fn run(test: &Test) {
    let (rows, note) = rows(test, Clean::Always);
    check::verdict(test, "real", &rows, &note);
}
