//! Every ladder rung of tests/ladder/front2/, and the owner's hello world, through the REAL
//! command: a RELEASE `blaze` binary, started as a process, as a person runs it. The binary is
//! the one `BLAZE_RUNGS_BIN` names (a debug `CARGO_BIN_EXE_blaze` takes minutes per
//! blaze-edition child); unset, the test fails naming the variable and runs nothing. The library
//! harness (crates/blaze-ladder/tests/ladder_front2.rs) runs the same rungs by calling
//! blaze-ladder directly; twice on 2026-11-15/16 a stage it proved did not happen through the
//! command (`blaze check` skipped the stages, and the owner's hello world failed under
//! `blaze run` while it passed in the harness). This test is the command's side of that
//! comparison, line by line: each program prints one line in the harness's own form,
//!
//!     <name>: PASS (status N, stdout byte-equal)
//!     <name>: REFUSED: <the command's first error line>
//!     <name>: WRONG: status N, stdout "..", want status M, stdout ".."
//!     <name>: HUNG: no answer in N s; its process group was killed
//!     <name>: CRASHED: signal N
//!
//! and the last line is the census:
//!
//!     command rungs: N rungs: a PASS, b REFUSED, c WRONG, d HUNG, e CRASHED
//!
//! The programs, in this order: `hello-blaze` (tests/e2e/hello-blaze: the owner's three lines,
//! edition blaze-2026.11, no dependency; `blaze run`, then `blaze check` on the same copy as
//! `hello-blaze (check)`), then every folder of tests/ladder/front2/ sorted by name, as
//! `front2/<folder>`, the harness's names for them.
//!
//! Each program is copied into a fresh folder under this test's temporary root (the checkout
//! is never written) and `blaze <command>` runs there as one child, the leader of its own
//! process group, with an environment built from nothing: `HOME` (a scratch home under the
//! temporary root, the toolchain's shelf hard-linked in by tests/support's `fresh_home`),
//! `PATH`, `TERM=dumb`, `LANG=C.UTF-8`. Children run one at a time, so the lines come in the
//! same order on any machine (each child peaks at about 2.3 GB). A child still running at its
//! deadline ([`HELLO_DEADLINE`], 300 s, for the hello world, run first and cold; [`DEADLINE`],
//! 120 s, for a rung) has its whole group killed and is HUNG. No network, no retry, no sleep.
//!
//! The test FAILS when a program is WRONG, HUNG or CRASHED, or when a program named in
//! tests/command_rungs.must-pass is not PASS (a name there covers its `(check)` line too).
//! A REFUSED program outside that list does not fail it: it is the census of what the command
//! cannot do yet. The must-pass list only grows: a name is added when its program passes and
//! is never taken out.
//!
//! `COMMAND_RUNGS_LIST`, of the same form as the harness's `FRONT2_RUNGS_LIST`, names a file the
//! lines are also written to (the checker scripts keep no test output).
//!
//! How a refusal is told from a program's own status (crates/blaze-cli/src/main.rs, D86 and the
//! ladder's L6): every failure of `blaze run` before the program starts (the manifest, a
//! finding, the build, the start) exits 125 (`program::NOT_RUN`) with an `error: <what>` line
//! on standard error: REFUSED, named by that line. Any other status, 1 (`main` returned `Err`)
//! and 101 (the program panicked) among them, is the program's own, and is compared with
//! `expected` (tests/run_not_run.rs proves the 125).
//!
//! Expected today: every rung REFUSED and `hello-blaze` not PASS until `blaze run` and
//! `blaze check` use the new front end; the must-pass assertion names it. `blaze check` is the
//! engine (check_command.rs) since 2026-11-18; its line passes at status 0, and at status 3
//! (the borrows stage, not built yet, the only one NOT checked) only once a root `main` cannot
//! be silently unchecked (`judge_check`, `ROOT_MAIN_CHECKED`, false today).

mod support;
#[path = "support/switch.rs"]
mod switch;

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use blaze_core::chan::{self, RecvTimeoutError};
use support::fresh_home;

/// How long one rung's `blaze` child may take before its process group is killed and it is
/// HUNG: a blaze-edition child resolves the toolchain's names (26 to 32 s in release on
/// 2026-11-16); a hang is a bug (rule 18), named, never waited out.
const DEADLINE: Duration = Duration::from_secs(120);

/// [`DEADLINE`] for the owner's hello world, run first and cold (a fresh scratch home).
const HELLO_DEADLINE: Duration = Duration::from_secs(300);

/// The variable naming the prebuilt RELEASE `blaze` this test runs. A debug
/// `CARGO_BIN_EXE_blaze` takes minutes per blaze-edition child; the test never falls back to
/// it: unset, the test fails naming this variable and runs nothing.
const BIN_VAR: &str = "BLAZE_RUNGS_BIN";

/// `blaze run`'s status when blaze itself did not build or start the program (D86,
/// crates/blaze-cli/src/program.rs `NOT_RUN`), apart from any status the program ends with.
const NOT_RUN: u8 = 125;

/// How long a child killed at its deadline may take to be reaped: the kill is SIGKILL.
const REAP: Duration = Duration::from_secs(10);

/// `SIGKILL`, the same number on macOS and Linux.
const SIGKILL: i32 = 9;

/// The old note of a `check` that printed a pass while its stages did not run.
const HOLLOW_CHECK: &str = "does not read the dependencies";

/// The owner's hello world, the first program.
const HELLO: &str = "hello-blaze";

unsafe extern "C" {
    /// POSIX `kill(2)`: `pid_t` is `i32` on macOS and Linux.
    fn kill(pid: i32, signal: i32) -> i32;
}

/// The command a child runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Word {
    Run,
    Check,
}

impl Word {
    fn arg(self) -> &'static str {
        match self {
            Word::Run => "run",
            Word::Check => "check",
        }
    }
}

/// How one `blaze` child ended.
enum Ended {
    /// It exited by itself before the deadline.
    Exited { status: u8, stdout: Vec<u8>, stderr: Vec<u8> },
    /// A signal ended it before the deadline.
    Signalled { signal: i32 },
    /// It was running at its deadline, `after` from its start; its process group was killed.
    Hung { after: Duration },
}

/// A program's `expected`: its exit status, then its standard output byte for byte.
struct Expected {
    status: u8,
    stdout: Vec<u8>,
}

/// What a program's answer differed in.
enum Wrong {
    /// The program ran and its status or standard output is not the expected one.
    Answer { status: u8, stdout: Vec<u8>, want: Expected },
    /// `check` printed the old note that its stages did not read the dependencies.
    HollowCheck { status: u8 },
}

/// One program's verdict through the command.
enum Verdict {
    /// The answer is the expected one; `stdout_compared` is false for `check`, which has none.
    Pass { status: u8, stdout_compared: bool },
    /// The command answered with its own error, this line of it.
    Refused { said: String },
    Wrong(Wrong),
    Hung { after: Duration },
    Crashed { signal: i32 },
}

/// The verdicts' kinds, counted in the census.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pass,
    Refused,
    Wrong,
    Hung,
    Crashed,
}

impl Verdict {
    fn kind(&self) -> Kind {
        match self {
            Verdict::Pass { .. } => Kind::Pass,
            Verdict::Refused { .. } => Kind::Refused,
            Verdict::Wrong(_) => Kind::Wrong,
            Verdict::Hung { .. } => Kind::Hung,
            Verdict::Crashed { .. } => Kind::Crashed,
        }
    }

    /// The verdict's line, in the library harness's form (`<name>: PASS ..`).
    fn line(&self, name: &str) -> String {
        match self {
            Verdict::Pass { status, stdout_compared: true } => format!("{name}: PASS (status {status}, stdout byte-equal)"),
            Verdict::Pass { status, stdout_compared: false } => format!("{name}: PASS (status {status})"),
            Verdict::Refused { said } => format!("{name}: REFUSED: {said}"),
            Verdict::Wrong(Wrong::Answer { status, stdout, want }) => format!(
                "{name}: WRONG: status {status}, stdout {:?}, want status {}, stdout {:?}",
                String::from_utf8_lossy(stdout),
                want.status,
                String::from_utf8_lossy(&want.stdout)
            ),
            Verdict::Wrong(Wrong::HollowCheck { status }) => {
                format!("{name}: WRONG: status {status}, said {HOLLOW_CHECK:?}: a pass printed over stages that did not run")
            }
            Verdict::Hung { after } => format!("{name}: HUNG: no answer in {} s; its process group was killed", after.as_secs()),
            Verdict::Crashed { signal } => format!("{name}: CRASHED: signal {signal}"),
        }
    }
}

/// One verdict and what it is of: `program` with `word`.
struct Outcome {
    program: String,
    word: Word,
    verdict: Verdict,
}

impl Outcome {
    /// The name the line gives it: the program's for `run`, `<program> (check)` for `check`.
    fn name(&self) -> String {
        match self.word {
            Word::Run => self.program.clone(),
            Word::Check => format!("{} (check)", self.program),
        }
    }
}

/// A program's `expected` (line 1 `status <n>`, line 2 `stdout`, then the bytes), read as the
/// library harness reads it (crates/blaze-ladder/tests/rungs/mod.rs `expected`).
fn expected(dir: &Path) -> Expected {
    let path = dir.join("expected");
    let text = std::fs::read(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    let mut lines = text.splitn(3, |&b| b == b'\n');
    let status = lines.next().and_then(|l| std::str::from_utf8(l).ok()).and_then(|l| l.strip_prefix("status "));
    let status: u8 = status.and_then(|s| s.trim().parse().ok()).unwrap_or_else(|| panic!("line 1 of {}: `status <n>`", path.display()));
    assert_eq!(lines.next(), Some(&b"stdout"[..]), "line 2 of {}: `stdout`", path.display());
    Expected { status, stdout: lines.next().unwrap_or(&[]).to_vec() }
}

/// `from`'s files under `to`, folder by folder from a work list (no recursion). A link or a
/// special file is refused by name: no rung holds one.
fn copy_tree(from: &Path, to: &Path) {
    let mut todo = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((from, to)) = todo.pop() {
        std::fs::create_dir_all(&to).unwrap_or_else(|error| panic!("making {}: {error}", to.display()));
        for entry in std::fs::read_dir(&from).unwrap_or_else(|error| panic!("listing {}: {error}", from.display())) {
            let entry = entry.unwrap_or_else(|error| panic!("an entry of {}: {error}", from.display()));
            let kind = entry.file_type().unwrap_or_else(|error| panic!("the kind of {}: {error}", entry.path().display()));
            let (path, at) = (entry.path(), to.join(entry.file_name()));
            if kind.is_dir() {
                todo.push((path, at));
            } else if kind.is_file() {
                std::fs::copy(&path, &at).unwrap_or_else(|error| panic!("copying {} to {}: {error}", path.display(), at.display()));
            } else {
                panic!("{}: a link or special file in a program's folder, which this test does not copy", path.display());
            }
        }
    }
}

/// `blaze <word>` in `dir` as one child leading its own process group, with exactly `HOME`,
/// `PATH`, `TERM` and `LANG`, no standard input, its outputs to `<logs>/<word>.out` and `.err`.
/// `blaze` is the release binary [`BIN_VAR`] names. A waiter thread reaps it; at `deadline` the
/// whole group is killed and it is [`Ended::Hung`].
fn run_blaze(blaze: &Path, word: Word, dir: &Path, home: &Path, path: &OsStr, logs: &Path, deadline: Duration) -> Ended {
    let out_path = logs.join(format!("{}.out", word.arg()));
    let err_path = logs.join(format!("{}.err", word.arg()));
    let file = |at: &Path| File::create(at).unwrap_or_else(|error| panic!("making {}: {error}", at.display()));
    #[allow(clippy::disallowed_methods, reason = "the test starts the release `blaze` binary itself, its working directory and whole environment given")]
    let mut command = Command::new(blaze);
    command
        .arg(word.arg())
        .current_dir(dir)
        .env_clear()
        .env("HOME", home)
        .env("PATH", path)
        .env("TERM", "dumb")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(file(&out_path))
        .stderr(file(&err_path))
        .process_group(0);
    let mut child = command.spawn().unwrap_or_else(|error| panic!("`blaze {}` did not start in {}: {error}", word.arg(), dir.display()));
    let pid = i32::try_from(child.id()).unwrap_or_else(|_| panic!("pid {} is not a pid_t", child.id()));
    let (sent, got) = chan::bounded(1);
    let waiter = std::thread::Builder::new()
        .name("command-rungs: waits for blaze".to_owned())
        .spawn(move || {
            let status = child.wait();
            if sent.send(status).is_err() {
                panic!("the test stopped waiting for blaze's status");
            }
        })
        .unwrap_or_else(|error| panic!("the waiter thread did not start: {error}"));
    let status = match got.recv_timeout(deadline) {
        Ok(status) => status,
        Err(RecvTimeoutError::Timeout) => {
            // SAFETY: `kill` takes no pointer. `-pid` names the child's process group
            // (`process_group(0)`: the group's id is the child's pid). The child is reaped only
            // by the waiter, which had not sent its status at the deadline; if it ended in the
            // instant since, the group's id is still held by any member left, and an empty
            // group makes the call fail with ESRCH, which changes nothing: the reap below takes
            // the status either way, and the run was past its deadline.
            unsafe { kill(-pid, SIGKILL) };
            if let Err(error) = got.recv_timeout(REAP) {
                panic!("`blaze {}` in {} was killed at its deadline and not reaped within {REAP:?}: {error:?}", word.arg(), dir.display());
            }
            waiter.join().unwrap_or_else(|_| panic!("the waiter on `blaze {}` panicked", word.arg()));
            return Ended::Hung { after: deadline };
        }
        Err(RecvTimeoutError::Disconnected) => panic!("the waiter on `blaze {}` in {} ended without its status", word.arg(), dir.display()),
    };
    waiter.join().unwrap_or_else(|_| panic!("the waiter on `blaze {}` panicked", word.arg()));
    let status = status.unwrap_or_else(|error| panic!("waiting for `blaze {}` in {}: {error}", word.arg(), dir.display()));
    if let Some(signal) = status.signal() {
        return Ended::Signalled { signal };
    }
    let code = status.code().unwrap_or_else(|| panic!("`blaze {}`: an exit that is no signal has a code", word.arg()));
    let status = u8::try_from(code).unwrap_or_else(|_| panic!("`blaze {}`: exit status {code} is not a byte", word.arg()));
    let read = |at: &Path| std::fs::read(at).unwrap_or_else(|error| panic!("reading {}: {error}", at.display()));
    Ended::Exited { status, stdout: read(&out_path), stderr: read(&err_path) }
}

/// The first `error: <what>` line of `words` (bstd's exit path), as `<what>`.
fn error_line(words: &[u8]) -> Option<String> {
    String::from_utf8_lossy(words).lines().find_map(|line| line.strip_prefix("error: ").map(str::to_owned))
}

/// The first line of `words` that holds `needle`.
fn line_with(words: &[u8], needle: &str) -> Option<String> {
    String::from_utf8_lossy(words).lines().find(|line| line.contains(needle)).map(str::to_owned)
}

/// The first line of `words` with anything on it.
fn first_line(words: &[u8]) -> Option<String> {
    String::from_utf8_lossy(words).lines().map(str::trim).find(|line| !line.is_empty()).map(str::to_owned)
}

/// Whether `words` holds `needle`.
fn holds(words: &[u8], needle: &str) -> bool {
    words.windows(needle.len()).any(|window| window == needle.as_bytes())
}

/// `blaze run`'s verdict: a refusal first (as the harness checks its error first), then the
/// answer against `want`. Read from the status alone (D86, L6): [`NOT_RUN`] (125) is blaze's
/// refusal to build or start the program, named by its `error: ` line; any other status, 1
/// and 101 (the program panicked) among them, is the program's own, compared with `want`.
fn judge_run(ended: Ended, want: Expected) -> Verdict {
    let (status, stdout, stderr) = match ended {
        Ended::Hung { after } => return Verdict::Hung { after },
        Ended::Signalled { signal } => return Verdict::Crashed { signal },
        Ended::Exited { status, stdout, stderr } => (status, stdout, stderr),
    };
    if status == NOT_RUN {
        let said = error_line(&stderr).or_else(|| first_line(&stderr)).unwrap_or_else(|| format!("exit status {NOT_RUN}, nothing said"));
        return Verdict::Refused { said };
    }
    if status == want.status && stdout == want.stdout {
        return Verdict::Pass { status, stdout_compared: true };
    }
    Verdict::Wrong(Wrong::Answer { status, stdout, want })
}

/// The closing coverage line (agent view, blaze-report plain.rs `agent_closing`) of a check
/// whose only stage not checked is the borrows stage this build does not build yet.
const ONLY_BORROWS_NOT_BUILT: &str = "incomplete: NOT checked: borrows";

/// Whether the engine checks a binary root's `main` whole, so that a status 3 whose only stage
/// NOT checked is the unbuilt borrows stage cannot hide an unchecked `main`. FALSE today: a
/// root `main` can be left unchecked with nothing said (lane SW2, Q1), so status 3 is NOT a
/// pass. THE LINE TO FLIP, to `true`, is this one: once lane BD1's root-main check
/// (engine_bodies.rs `groups` / `absent_entry`, written 2026-11-18, uncompiled when this was
/// written) is compiled and a rung run shows a root `main` it cannot check as a note and status
/// 4, never a quiet status 3.
const ROOT_MAIN_CHECKED: bool = false;

/// `blaze check`'s verdict: status 0 without the old hollow note passes; so does status 3 when
/// the ONLY stage its closing line names NOT checked is the borrows stage and this build does not
/// build it (`BORROWS_ARE_ERRORS` false), once a root `main` cannot be silently unchecked
/// ([`ROOT_MAIN_CHECKED`]): every stage the build runs ran whole and found nothing, which is all
/// `check` can do until the flow census is clean (rule 7: the expectation states what is right).
/// Until then status 3 is the command's refusal, as any other. The note is WRONG whatever the
/// status; any other status is the command's refusal, named by its error line, else its "NOT
/// checked" line, else its first line.
fn judge_check(ended: Ended) -> Verdict {
    let (status, stdout, stderr) = match ended {
        Ended::Hung { after } => return Verdict::Hung { after },
        Ended::Signalled { signal } => return Verdict::Crashed { signal },
        Ended::Exited { status, stdout, stderr } => (status, stdout, stderr),
    };
    if holds(&stdout, HOLLOW_CHECK) || holds(&stderr, HOLLOW_CHECK) {
        return Verdict::Wrong(Wrong::HollowCheck { status });
    }
    if status == 0 {
        return Verdict::Pass { status, stdout_compared: false };
    }
    let text = String::from_utf8_lossy(&stdout);
    let only_borrows_unbuilt = ROOT_MAIN_CHECKED && status == 3 && !switch::borrows_are_errors() && text.lines().filter(|line| line.starts_with("incomplete: ")).all(|line| line == ONLY_BORROWS_NOT_BUILT) && holds(&stdout, ONLY_BORROWS_NOT_BUILT);
    if only_borrows_unbuilt {
        return Verdict::Pass { status, stdout_compared: false };
    }
    let said = error_line(&stderr)
        .or_else(|| error_line(&stdout))
        .or_else(|| line_with(&stdout, "NOT checked"))
        .or_else(|| first_line(&stdout))
        .or_else(|| first_line(&stderr))
        .unwrap_or_else(|| format!("exit status {status}, nothing said"));
    Verdict::Refused { said }
}

/// The checked-in must-pass names: one a line, `#` comments and blank lines skipped.
fn must_pass(cli: &Path) -> Vec<String> {
    let path = cli.join("tests/command_rungs.must-pass");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')).map(str::to_owned).collect()
}

/// Every program, in order: the owner's hello world, then front2's rungs sorted by name.
fn programs(cli: &Path) -> Vec<(String, PathBuf)> {
    let front2 = cli.join("tests/ladder/front2");
    let mut rungs = Vec::new();
    for entry in std::fs::read_dir(&front2).unwrap_or_else(|error| panic!("listing {}: {error}", front2.display())) {
        let entry = entry.unwrap_or_else(|error| panic!("an entry of {}: {error}", front2.display()));
        if entry.file_type().unwrap_or_else(|error| panic!("the kind of {}: {error}", entry.path().display())).is_dir() {
            let name = entry.file_name().into_string().unwrap_or_else(|name| panic!("a rung's name that is not UTF-8: {name:?}"));
            rungs.push(name);
        }
    }
    rungs.sort();
    let mut programs = vec![(HELLO.to_owned(), cli.join("tests/e2e").join(HELLO))];
    programs.extend(rungs.into_iter().map(|rung| (format!("front2/{rung}"), front2.join(&rung))));
    programs
}

/// The test: run every program through the command, print each line and the census, and fail
/// on WRONG, HUNG, CRASHED, or a must-pass program not PASS. Expected line, last:
/// `command rungs: N rungs: a PASS, b REFUSED, c WRONG, d HUNG, e CRASHED`.
#[test]
#[ignore = "requires BLAZE_RUNGS_BIN pointing to a prebuilt release blaze binary"]
fn every_rung_through_the_command() {
    let cli = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The release blaze to run, never the debug `CARGO_BIN_EXE_blaze` in its place.
    let Some(blaze) = std::env::var_os(BIN_VAR) else {
        panic!("{BIN_VAR} is not set: name a prebuilt RELEASE blaze binary (a debug one passes the deadlines); nothing was run");
    };
    let blaze = PathBuf::from(blaze);
    assert!(blaze.is_file(), "{BIN_VAR}={}: no such file; nothing was run", blaze.display());
    // The scratch home's toolchain is the owner's shelf, hard-linked (read only) by
    // `fresh_home`; without one the command cannot build a blaze edition.
    let owner = std::env::var_os("HOME").expect("HOME, to find the toolchain's shelf a scratch home is given");
    let shelf = Path::new(&owner).join(".blaze/v2/toolchain");
    assert!(shelf.is_dir(), "no toolchain shelf for a scratch home: {} is not a folder", shelf.display());
    let path = std::env::var_os("PATH").expect("PATH, the one variable of this process's environment blaze is given");
    let base = fresh_home("command_rungs", true);
    let home = base.join("home");
    let must = must_pass(cli);
    let programs = programs(cli);

    let mut outcomes: Vec<Outcome> = Vec::new();
    for (program, from) in &programs {
        let copy = base.join("checkouts").join(program);
        copy_tree(from, &copy);
        let logs = base.join("logs").join(program);
        std::fs::create_dir_all(&logs).unwrap_or_else(|error| panic!("making {}: {error}", logs.display()));
        let (words, deadline): (&[Word], Duration) = if program == HELLO { (&[Word::Run, Word::Check], HELLO_DEADLINE) } else { (&[Word::Run], DEADLINE) };
        for &word in words {
            let ended = run_blaze(&blaze, word, &copy, &home, &path, &logs, deadline);
            let verdict = match word {
                Word::Run => judge_run(ended, expected(from)),
                Word::Check => judge_check(ended),
            };
            let outcome = Outcome { program: program.clone(), word, verdict };
            println!("{}", outcome.verdict.line(&outcome.name()));
            outcomes.push(outcome);
        }
    }

    let count = |kind: Kind| outcomes.iter().filter(|outcome| outcome.verdict.kind() == kind).count();
    let summary = format!(
        "command rungs: {} rungs: {} PASS, {} REFUSED, {} WRONG, {} HUNG, {} CRASHED",
        outcomes.len(),
        count(Kind::Pass),
        count(Kind::Refused),
        count(Kind::Wrong),
        count(Kind::Hung),
        count(Kind::Crashed)
    );
    println!("{summary}");
    let lines: Vec<String> = outcomes.iter().map(|outcome| outcome.verdict.line(&outcome.name())).collect();
    if let Some(list) = std::env::var_os("COMMAND_RUNGS_LIST") {
        std::fs::write(&list, format!("COMMAND RUNGS:\n{}\n{summary}\n", lines.join("\n"))).expect("writing COMMAND_RUNGS_LIST");
    }

    let mut failures = Vec::new();
    for name in &must {
        if !outcomes.iter().any(|outcome| &outcome.program == name) {
            failures.push(format!("{name}: named in tests/command_rungs.must-pass, but this test runs no such program"));
        }
    }
    for (outcome, line) in outcomes.iter().zip(&lines) {
        let kind = outcome.verdict.kind();
        let required = must.contains(&outcome.program);
        if matches!(kind, Kind::Wrong | Kind::Hung | Kind::Crashed) || (required && kind != Kind::Pass) {
            failures.push(format!("{line}{}", if required { " (must pass)" } else { "" }));
        }
    }
    if failures.is_empty() {
        std::fs::remove_dir_all(&base).unwrap_or_else(|error| panic!("removing {}: {error}", base.display()));
    }
    assert!(
        failures.is_empty(),
        "through the real `blaze` command:\n{}\n{summary}\n(the copies and each child's outputs are kept in {})",
        failures.join("\n"),
        base.display()
    );
}
