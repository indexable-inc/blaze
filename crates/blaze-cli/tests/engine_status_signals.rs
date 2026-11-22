//! A running `blaze engine-check` asked for its status and then stopped, by real signals
//! (engine_status.rs): `--workers 1 --status-every 1` on one generated crate of 60,000 bodies;
//! once a periodic line names the `Bodies` stage, SIGINFO (macOS; SIGUSR1 on Linux) writes
//! `status at <t> s (asked by SIGINFO)` and the check goes on; SIGTERM then stops it within
//! the stop's bound with `stopped at <t> s by SIGTERM in stage Bodies` on stderr and in the
//! profile, and the exit status 143 (128 + 15).
//!
//! Expected line: `STATUS SIGNALS: status after <a> ms, stopped <b> ms after SIGTERM, exit 143`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).
//! Nothing here formats into the global heap: text goes to files by `write!` and is searched
//! in a buffer on the stack.

#![cfg(unix)]

mod support;

use std::fs::File;
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// Until a line names the Bodies stage: the plan, the outlines and the names of 60,000 bodies.
const DEADLINE: Duration = Duration::from_secs(300);
/// From the status signal to its text.
const ASKED: Duration = Duration::from_secs(10);
/// From SIGTERM to the exit: engine_status.rs `STOP_BOUND` (2 s) and one watcher tick, with
/// the process's own end.
const STOPPED: Duration = Duration::from_secs(3);
const MODULES: u32 = 300;
const FNS: u32 = 200;
/// The tail of stderr searched.
const TAIL: usize = 64 << 10;

#[cfg(target_os = "macos")]
const STATUS: (&str, &str) = ("-INFO", "(asked by SIGINFO)");
#[cfg(not(target_os = "macos"))]
const STATUS: (&str, &str) = ("-USR1", "(asked by SIGUSR1)");

/// Kills the check on every way out of the test, a failed assert too.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn create(path: &Path) -> File {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| panic!("making {}: {error}", dir.display()));
    }
    File::create(path).unwrap_or_else(|error| panic!("making {}: {error}", path.display()))
}

/// One crate, `MODULES` modules of `FNS` small bodies each.
fn workspace(ws: &Path) {
    let mut manifest = create(&ws.join("Cargo.toml"));
    manifest.write_all(b"[package]\nname = \"big\"\nversion = \"0.1.0\"\nedition = \"2024\"\n").expect("the manifest");
    let mut lock = create(&ws.join("Cargo.lock"));
    lock.write_all(b"version = 4\n\n[[package]]\nname = \"big\"\nversion = \"0.1.0\"\n").expect("the lock");
    let mut lib = create(&ws.join("src/lib.rs"));
    for m in 0..MODULES {
        writeln!(lib, "pub mod m{m};").expect("lib.rs");
        let mut name = blaze_own::FixedString::<32>::new();
        let _ = core::fmt::Write::write_fmt(&mut name, format_args!("src/m{m}.rs"));
        let mut module = create(&ws.join(name.as_str()));
        for f in 0..FNS {
            writeln!(module, "pub fn f{f}(x: u32) -> u32 {{\n    let y = x.wrapping_mul({f});\n    if y > 7 {{ y - 1 }} else {{ y + 1 }}\n}}").expect("a module");
        }
    }
}

/// Whether the last [`TAIL`] bytes of `file` hold `needle` (from its start, `head`).
fn holds(file: &Path, needle: &str, head: bool) -> bool {
    let Ok(open) = File::open(file) else { return false };
    let len = open.metadata().map_or(0, |meta| meta.len());
    let from = if head { 0 } else { len.saturating_sub(TAIL as u64) };
    let mut buffer = [0u8; TAIL];
    let read = open.read_at(&mut buffer, from).unwrap_or(0);
    let hay = &buffer[..read];
    match head {
        true => hay.starts_with(needle.as_bytes()),
        false => hay.windows(needle.len()).any(|window| window == needle.as_bytes()),
    }
}

/// `kill <signal> <pid>`.
fn signal(pid: u32, which: &str) {
    let mut id = blaze_own::FixedString::<16>::new();
    let _ = core::fmt::Write::write_fmt(&mut id, format_args!("{pid}"));
    let sent = Command::new("kill").arg(which).arg(id.as_str()).status().expect("kill runs");
    assert!(sent.success(), "kill {which} {pid}: {sent}");
}

/// Waits (at most `most`) until `done`; false at the bound, or when the check ended first.
fn until(child: &mut Child, most: Duration, done: impl Fn() -> bool) -> bool {
    let started = Instant::now();
    // Waiting for: the check's next text, at most `most` (rule 18).
    while started.elapsed() < most {
        if done() {
            return true;
        }
        if child.try_wait().ok().flatten().is_some() {
            return done();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn a_running_check_answers_its_status_and_stops_on_sigterm() {
    let base = fresh_home("engine_status_signals", true);
    let (home, ws) = (base.join("home"), base.join("ws"));
    workspace(&ws);
    let err = ws.with_extension("err");
    let said = create(&err);
    let child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .args(["engine-check", "--workers", "1", "--status-every", "1"])
        .current_dir(&ws)
        .env("HOME", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", &ws)
        .env_remove("SDKROOT")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(said))
        .spawn()
        .expect("blaze starts");
    let mut child = Reaped(child);
    let pid = child.0.id();
    let began = Instant::now();
    assert!(until(&mut child.0, DEADLINE, || holds(&err, " · Bodies ", false)), "no periodic line named the Bodies stage within {DEADLINE:?} (or the check ended first); stderr in {}", err.display());
    signal(pid, STATUS.0);
    let asked = Instant::now();
    assert!(until(&mut child.0, ASKED, || holds(&err, STATUS.1, false)), "no status after {} within {ASKED:?}; stderr in {}", STATUS.0, err.display());
    let asked_ms = asked.elapsed().as_millis();
    assert!(child.0.try_wait().ok().flatten().is_none(), "the status ended the check; stderr in {}", err.display());
    signal(pid, "-TERM");
    let term = Instant::now();
    // Waiting for: the stopped check's exit, at most STOPPED.
    let ended = loop {
        if let Some(status) = child.0.try_wait().expect("the check's state") {
            break status;
        }
        assert!(term.elapsed() < STOPPED, "the check did not end within {STOPPED:?} of SIGTERM (rule 18); stderr in {}", err.display());
        std::thread::sleep(Duration::from_millis(10));
    };
    let stopped_ms = term.elapsed().as_millis();
    assert!(!holds(&err, "did not stop:", false), "the worker reached a scheduling point: no task named stuck; stderr in {}", err.display());
    assert_eq!(ended.code(), Some(143), "the stop's status, 128 + SIGTERM; stderr in {}", err.display());
    assert!(holds(&err, "by SIGTERM in stage Bodies", false), "the stop's first line on stderr; stderr in {}", err.display());
    assert!(holds(&ws.join(".blaze/v2/profile.txt"), "stopped at ", true), "the profile is the stop's text, whole");
    println!("STATUS SIGNALS: status after {asked_ms} ms, stopped {stopped_ms} ms after SIGTERM, exit 143 ({} s in all)", began.elapsed().as_secs());
}
