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
//! The test FAILS only when a rung is WRONG. `FRONT2_RUNGS_OUT` names a file the table goes to:
//! each line is appended as its rung finishes (the checker scripts keep no test output, and a
//! run cut short still leaves every finished line).

#![feature(allocator_api)]

mod rungs;

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};
use rungs::{expected, ladder, sources};

/// What one rung did.
enum Verdict {
    Pass,
    Refused { class: String, first: String },
    Panicked { first: String },
    Wrong { status: u8, out: Vec<u8> },
}

/// Runs `f` with fd 1 on a pipe; its status and what it wrote there. As ladder_front2.rs's.
fn capture(f: impl FnOnce() -> u8) -> (u8, Vec<u8>) {
    let mut fds = [0; 2];
    // SAFETY: as tests/ladder.rs: `pipe` fills the array; fd 1 is restored before returning
    // (`f` cannot unwind past here: the caller catches its panics inside `f`).
    unsafe {
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0, "a pipe");
        let saved = libc::dup(1);
        assert!(saved >= 0, "a copy of fd 1");
        assert!(libc::dup2(fds[1], 1) >= 0, "fd 1 onto the pipe");
        let status = f();
        assert!(libc::dup2(saved, 1) >= 0, "fd 1 back");
        libc::close(saved);
        libc::close(fds[1]);
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = libc::read(fds[0], buf.as_mut_ptr().cast(), buf.len());
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        libc::close(fds[0]);
        (status, out)
    }
}

/// The edition line of a rung's `Cargo.toml`.
fn edition_of(dir: &std::path::Path) -> String {
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("a rung's Cargo.toml");
    let line = manifest.lines().find_map(|l| l.trim().strip_prefix("edition")).expect("an edition line");
    line.trim().trim_start_matches('=').trim().trim_matches('"').to_string()
}

/// Every WHOLE `front2/x-*` rung's directory name, sorted. A folder still being written (no
/// `Cargo.toml`, `expected` or `src/main.rs` yet) is not a rung yet: its name goes to standard
/// error and it is left out.
fn x_rungs() -> Vec<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(ladder().join("front2")).expect("the ladder's front2 directory") {
        let entry = entry.expect("a directory entry");
        let name = entry.file_name().into_string().expect("a UTF-8 rung name");
        let dir = entry.path();
        if !(name.starts_with("x-") || name.starts_with("x0-")) || !dir.is_dir() {
            continue;
        }
        if ["Cargo.toml", "expected", "src/main.rs"].iter().all(|f| dir.join(f).is_file()) {
            names.push(name);
        } else {
            eprintln!("front2_rungs: {name} is not whole yet (Cargo.toml, expected, src/main.rs): left out");
        }
    }
    names.sort();
    names
}

/// The variant's name at the head of a `Debug` rendering: `Lower { .. }` gives `Lower`.
fn class_of(debug: &str) -> String {
    debug.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect()
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

fn run_one(rung: &str) -> Verdict {
    let dir = ladder().join("front2").join(rung);
    let files = sources(&dir);
    let views: Vec<SourceFile<'_>> = files.iter().map(|(path, text)| SourceFile { path, text }).collect();
    let edition = edition_of(&dir);
    // A blaze edition builds against the real libraries: the toolchain's shelf.
    let shelf = edition.starts_with("blaze-").then(|| format!("{}/.blaze/v2/toolchain", std::env::var("HOME").expect("HOME")));
    let choice = FrontChoice::New(NewFront { package: "ladder", edition: &edition, shelf: shelf.as_deref() });
    let mut refused = None;
    let mut panicked = None;
    let (status, out) = capture(|| {
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_with(&views, "src/main.rs", None, choice)));
        match ran {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                refused = Some((class_of(&format!("{error:?}")), first_line(&error.to_string())));
                u8::MAX
            }
            Err(payload) => {
                let said = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "a panic with no message".to_string());
                panicked = Some(first_line(&said));
                u8::MAX
            }
        }
    });
    if let Some((class, first)) = refused {
        return Verdict::Refused { class, first };
    }
    if let Some(first) = panicked {
        return Verdict::Panicked { first };
    }
    if (status, &out) == (expected(&dir).0, &expected(&dir).1) { Verdict::Pass } else { Verdict::Wrong { status, out } }
}

/// The verdict's word and its rest, one line: what a child prints after `VERDICT `.
fn verdict_line(rung: &str, verdict: Verdict) -> (&'static str, String) {
    match verdict {
        Verdict::Pass => ("PASS", String::new()),
        Verdict::Refused { class, first } => ("REFUSED", format!("{class}: {first}")),
        Verdict::Panicked { first } => ("PANICKED", first),
        Verdict::Wrong { status, out } => {
            let want = expected(&ladder().join("front2").join(rung));
            let rest = format!(
                "got status {status} stdout {:?} want status {} stdout {:?}",
                String::from_utf8_lossy(&out),
                want.0,
                String::from_utf8_lossy(&want.1)
            );
            ("WRONG", rest)
        }
    }
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
        let (word, rest) = verdict_line(&rung, run_one(&rung));
        println!("VERDICT {word} {rest}");
        return;
    }
    let secs: u64 = std::env::var("FRONT2_RUNG_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
    let deadline = std::time::Duration::from_secs(secs);
    let all = x_rungs();
    if let Some(path) = std::env::var_os("FRONT2_RUNGS_OUT") {
        std::fs::write(&path, format!("FRONT2 X RUNGS: started, {} rungs, {secs} s each at most\n", all.len()))
            .expect("starting FRONT2_RUNGS_OUT");
    }
    let mut wrong = Vec::new();
    let mut counts: Vec<(String, u32)> = Vec::new();
    for rung in &all {
        // `x0-*` are the runnable twins (no_core, edition 2024, success is status 42);
        // `x-*` the same checks on the blaze edition over the real `core` (success is 0).
        let family = if rung.starts_with("x0-") { "twin" } else { "core" };
        let (word, rest) = run_child(rung, deadline);
        let line = format!("[{family}] {rung}: {word} {rest}").trim_end().to_string();
        println!("{line}");
        append_out(&line);
        if word == "WRONG" {
            wrong.push(line.clone());
        }
        match counts.iter_mut().find(|(w, _)| *w == word) {
            Some((_, n)) => *n += 1,
            None => counts.push((word, 1)),
        }
    }
    let summary: Vec<String> = counts.iter().map(|(w, n)| format!("{n} {w}")).collect();
    let last = format!("FRONT2 X RUNGS: {} rungs: {}", all.len(), summary.join(", "));
    println!("{last}");
    append_out(&last);
    assert!(wrong.is_empty(), "rungs that ran and gave a wrong answer:\n{}", wrong.join("\n"));
}
