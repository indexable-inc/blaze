//! The EXECUTION RUNGS through the new front end and code builder: every folder
//! `crates/blaze-cli/tests/ladder/front2/x-*/` is a small self-checking program on the blaze
//! edition (bstd and the real `core`), built and RUN in this test's process with fd 1 on a
//! pipe, as tests/ladder_front2.rs runs its rungs. Each program's `main` exits 0 when every
//! check it makes passed, and otherwise the number of its first failed check; its `expected`
//! file says the status and output that are right (each value derived in the program's doc
//! comment from the Rust Reference or from arithmetic, never from what a compiler printed).
//!
//! One line per rung, in name order:
//! - `PASS`: status and stdout equal `expected`;
//! - `REFUSED <class>: <first line>`: the front end or the builder said by name what it does
//!   not build yet (the work list; not a failure);
//! - `PANICKED <first line>`: the run path panicked (a bug, reported, not a wrong answer);
//! - `WRONG got status <n> stdout <..> want status <n> stdout <..>`: a wrong answer;
//! - `HUNG after <s> s`: the rung's child did not finish by its deadline and was killed;
//! - `CRASHED <exit status>`: the rung's child ended with no verdict (a signal, an abort).
//!
//! Each rung runs in its OWN CHILD PROCESS: this test binary run again with
//! `FRONT2_RUNGS_ONE=<rung>`, which builds and runs that one rung in-process (the run path
//! runs programs in-process) and prints one `VERDICT ..` line. The parent waits for it with a
//! deadline (`FRONT2_RUNG_SECS`, default 120), so one hang or crash costs one line, not the run.
//!
//! SKIPPED unless `FRONT2_RUNGS` is set: the runner is examples/front2_rungs.rs (a release
//! build, the toolchain library resolved once, a forked child per rung); the rungs' logic both
//! share is tests/rungs/x.rs.
//!
//! The test FAILS only when a rung is WRONG. `FRONT2_RUNGS_OUT` names a file the table goes to:
//! each line is appended as its rung finishes (the checker scripts keep no test output, and a
//! run cut short still leaves every finished line).

#![feature(allocator_api)]

mod rungs;

use blaze_ladder::run::{FrontChoice, run_with};
use rungs::{ladder, x};

/// One rung to its verdict in this process, the toolchain library resolved here (`run_with`):
/// the word and rest a child prints after `VERDICT `. The rungs' logic is rungs/x.rs's.
fn verdict_of(rung: &str) -> (&'static str, String) {
    let dir = ladder().join("front2").join(rung);
    let shelf = x::shelf();
    let verdict = x::run_one(&dir, Some(shelf.as_str()), |views, new| run_with(views, x::ROOT, None, FrontChoice::New(new), blaze_own::testing::heap().shared()));
    x::verdict_line(&dir, verdict)
}

/// Runs `rung` in a child (this binary, `FRONT2_RUNGS_ONE=rung`) with a deadline: the word and
/// rest of its verdict, `HUNG` when killed at the deadline, `CRASHED` when it gave none.
fn run_child(rung: &str, deadline: std::time::Duration) -> (String, String) {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().expect("this test binary's path");
    let mut child = Command::new(exe)
        .args(["every_x_rung_runs_right", "--exact", "--nocapture", "--test-threads=1"])
        .env("FRONT2_RUNGS_ONE", rung)
        .env_remove("FRONT2_RUNGS_OUT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("starting a rung's child");
    // Drained on its own thread so a child writing much can never block on a full pipe.
    let mut pipe = child.stdout.take().expect("the child's stdout");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = pipe.read_to_string(&mut text);
        text
    });
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("waiting for a rung's child") {
            break Some(status);
        }
        if started.elapsed() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let text = reader.join().expect("the child's stdout reader");
    let Some(status) = status else {
        return ("HUNG".to_string(), format!("after {} s", deadline.as_secs()));
    };
    match text.lines().find_map(|l| l.strip_prefix("VERDICT ")) {
        Some(v) => {
            let (word, rest) = v.split_once(' ').unwrap_or((v, ""));
            (word.to_string(), rest.to_string())
        }
        None => ("CRASHED".to_string(), format!("{status}")),
    }
}

/// Appends one line to `FRONT2_RUNGS_OUT` (when set) at once: a run cut short keeps it.
fn append_out(line: &str) {
    use std::io::Write;
    if let Some(path) = std::env::var_os("FRONT2_RUNGS_OUT") {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).expect("opening FRONT2_RUNGS_OUT");
        file.write_all(format!("{line}\n").as_bytes()).expect("writing FRONT2_RUNGS_OUT");
    }
}

#[test]
fn every_x_rung_runs_right() {
    // The child's side: one rung, in this process, one verdict line on stdout.
    if let Ok(rung) = std::env::var("FRONT2_RUNGS_ONE") {
        let (word, rest) = verdict_of(&rung);
        println!("VERDICT {word} {rest}");
        return;
    }
    // Not run unless asked: every child here resolves the toolchain library itself, in a debug
    // build (two runs never printed a table); examples/front2_rungs.rs is the runner.
    if std::env::var_os("FRONT2_RUNGS").is_none() {
        eprintln!("front2_rungs: skipped (FRONT2_RUNGS is not set); the runner is the example: mixcheck.sh blaze-ladder run front2_rungs");
        return;
    }
    let secs: u64 = std::env::var("FRONT2_RUNG_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
    let deadline = std::time::Duration::from_secs(secs);
    let all = x::x_rungs(&ladder().join("front2"));
    if let Some(path) = std::env::var_os("FRONT2_RUNGS_OUT") {
        std::fs::write(&path, format!("FRONT2 X RUNGS: started, {} rungs, {secs} s each at most\n", all.len()))
            .expect("starting FRONT2_RUNGS_OUT");
    }
    let mut wrong = Vec::new();
    let mut counts = x::Counts::default();
    for rung in &all {
        let (word, rest) = run_child(rung, deadline);
        let line = x::table_line(rung, &word, &rest);
        println!("{line}");
        append_out(&line);
        if word == "WRONG" {
            wrong.push(line.clone());
        }
        counts.add(&word);
    }
    let last = counts.last_line();
    println!("{last}");
    append_out(&last);
    assert!(wrong.is_empty(), "rungs that ran and gave a wrong answer:\n{}", wrong.join("\n"));
}
