//! `blaze test` as a process on a package of three tests (crates/blaze-cli/src/test_run.rs; test.md
//! "End state", "One process per test"): `tests::passes` returns, `tests::fails` panics, and
//! `tests::hangs` sleeps for ever against `--deadline 2s`. The process's `HOME` is a scratch
//! home (support/mod.rs): the real `$HOME` is never read or written beyond the toolchain shelf's
//! hard links.
//!
//! UNTIL THE CODE BUILDER BUILDS TEST BODIES (test.md piece 8), this run prints a `NOT RUN` line
//! per test and no test runs: [`BODIES_BUILT`] is `false`, and the test asserts exactly that:
//!
//! ```text
//! NOT RUN tests::fails: cannot be run yet: the code builder cannot build test bodies yet (test.md, piece 8)
//! NOT RUN tests::hangs: cannot be run yet: the code builder cannot build test bodies yet (test.md, piece 8)
//! NOT RUN tests::passes: cannot be run yet: the code builder cannot build test bodies yet (test.md, piece 8)
//! test result: FAILED. ran 0: 0 passed, 0 failed; ...; 3 could not be run yet
//! ```
//!
//! THE END STATE, asserted when [`BODIES_BUILT`] is flipped to `true` (the one switch; nothing
//! else changes): each test in its own forked child; the failure printed with its panic text;
//! the hang killed at its 2 s deadline without holding the others; the summary line holds
//!
//! ```text
//! test result: FAILED. ran 3: 1 passed, 2 failed
//! ```
//!
//! and the whole command ends within [`DEADLINE`]. In both states the status is not 0.
//!
//! IGNORED, BY NAME, UNTIL THE DRIVER WIRES THE LOOP: today main.rs `close` answers `Command::Test`
//! with "tests are not run yet: this build of blaze checks them" and never builds a `TestLoop`, so
//! not even the `NOT RUN` lines above are printed yet. The driver lane's change (main.rs's test
//! arm builds test_loop.rs `TestLoop`, performs its effects, and hands each `Effect::Run` and
//! `Effect::Kill` to test_run.rs `Runner` over `Unbuilt`) removes the `#[ignore]` below.
//!
//! Run: `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-cli test
//! '--test test_command'`; expected today: `test a_pass_a_failure_and_a_hang ... ignored, needs
//! main.rs's test arm to drive TestLoop and test_run::Runner`; once wired (the `#[ignore]` removed):
//! `test a_pass_a_failure_and_a_hang ... ok`.

mod support;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// The one switch: `true` once the code builder builds test bodies and a zygote runs them.
const BODIES_BUILT: bool = false;

/// How long the whole command may take: a hang killed at 2 s plus the pool's graces is far
/// below it; past it the command hung, which is a bug (rule 18), reported here by name.
const DEADLINE: Duration = Duration::from_secs(60);

const MANIFEST: &str = "[package]\nname = \"three\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n";

const LIB: &str = "\
pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[cfg(test)]
mod tests {
    #[test]
    fn passes() {
        assert_eq!(super::add(1, 1), 2);
    }

    #[test]
    fn fails() {
        assert_eq!(super::add(1, 1), 3, \"one and one\");
    }

    #[test]
    fn hangs() {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}
";

#[test]
#[ignore = "needs main.rs's test arm to drive TestLoop and test_run::Runner"]
fn a_pass_a_failure_and_a_hang() {
    let base = fresh_home("test_command", true);
    let home = base.join("home");
    let dir = base.join("three");
    std::fs::create_dir_all(dir.join("src")).expect("the fixture's folders");
    std::fs::write(dir.join("Cargo.toml"), MANIFEST).expect("the fixture's manifest");
    std::fs::write(dir.join("src/lib.rs"), LIB).expect("the fixture's lib.rs");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .args(["test", "--deadline", "2s"])
        .current_dir(&dir)
        .env("HOME", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    while child.try_wait().expect("a wait on blaze").is_none() {
        if started.elapsed() > DEADLINE {
            let _ = child.kill();
            let _ = std::fs::remove_dir_all(&base);
            panic!("`blaze test` past {DEADLINE:?}: a hang held the command");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let took = started.elapsed();
    let output = child.wait_with_output().expect("blaze's output");
    let _ = std::fs::remove_dir_all(&base);
    let said = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);
    assert_ne!(output.status.code(), Some(0), "a failing suite fails the command\n{said}\n{errors}");
    if BODIES_BUILT {
        assert!(said.contains("test result: FAILED. ran 3: 1 passed, 2 failed"), "{said}\n{errors}");
        assert!(said.contains("tests::fails") && said.contains("one and one"), "the failure with its panic text\n{said}");
        assert!(said.contains("tests::hangs") && said.contains("killed past its deadline"), "the hang killed at its deadline\n{said}");
        assert!(!said.contains("NOT RUN"), "{said}");
        assert!(took < Duration::from_secs(20), "the hang held the command {took:?}");
    } else {
        for test in ["tests::fails", "tests::hangs", "tests::passes"] {
            let line = format!("NOT RUN {test}: cannot be run yet: the code builder cannot build test bodies yet (test.md, piece 8)");
            assert!(said.contains(&line), "{line}\n{said}\n{errors}");
        }
        assert!(said.contains("3 could not be run yet"), "{said}\n{errors}");
    }
}
