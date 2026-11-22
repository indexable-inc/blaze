//! `blaze run` of a program that copies an enum holding no variant's discriminant says the value
//! it read and the enum (blaze-cli run_fault.rs `held_value`, `subject_said`; the trap row's
//! held value from blaze-emit pass.rs `held_at_unreachable` and its subject from blaze-build
//! walk.rs `subject_of`; the registers from the child's handler, run_mode/fault_report.rs
//! `thread_state`).

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// A trap is milliseconds after the start; a hang is a bug (rule 18), named, never waited out.
const DEADLINE: Duration = Duration::from_secs(60);

/// tests/run_trapped/enum_value: 3 transmuted into `Two` (discriminants 1 and 2) and passed in
/// a struct by value. The program ends by SIGTRAP (128 + 5 = 133); the named line says the
/// discriminant read found no variant, that it read 0x3 from x9, and that `Two` has
/// discriminants 0x1, 0x2. With the registers left out of the child's fault (fault_report.rs
/// `on_signal`'s `fault.regs = state`), the line says "the program sent no registers" and the
/// 0x3 assertion fails; with the row's subject left out (walk.rs `unreachable_of_enum`), the
/// `Two` assertion fails.
#[test]
fn a_bad_discriminant_is_said_by_its_value_and_the_enum() {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run_trapped/enum_value");
    let base = fresh_home("run_trapped_enum", true);
    let home = base.join("home");
    let dir = base.join("checkouts/enum_value");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("the copy's folders");
    std::fs::copy(from.join("Cargo.toml"), dir.join("Cargo.toml")).expect("the program's manifest");
    std::fs::copy(from.join("src/main.rs"), dir.join("src/main.rs")).expect("the program's main.rs");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("run")
        .current_dir(&dir)
        .env("HOME", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    while child.try_wait().expect("a wait on blaze").is_none() {
        if started.elapsed() > DEADLINE {
            let _ = child.kill();
            panic!("`blaze run` on run_trapped/enum_value past {DEADLINE:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().expect("blaze's output");
    let said = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(128 + 5), "the program did not end by SIGTRAP: {said}");
    let line = said.lines().find(|line| line.contains("it struck at")).unwrap_or_else(|| panic!("no named line: {said}"));
    assert!(line.contains("the read of the enum's discriminant") && line.contains("found no variant's discriminant"), "{line}");
    assert!(line.contains("it read 0x3 (x9)"), "the value read is not said: {line}");
    assert!(line.contains("`Two` has discriminants 0x1, 0x2"), "the enum and its discriminants are not said: {line}");
    println!("run trapped enum: {line}");
    // The callers (run schema 3, the child's frame-pointer walk): the trap is in `make` (or
    // `pass`), two or more calls under `main` through `build` and `run`; the line names at least
    // two callers by their paths, `run` and `main` among them, innermost first. With the walk
    // left out of the child (fault_report.rs `on_signal`'s `fault.callers = ..`), no
    // "called from" is said and this fails.
    let called = line.find("; called from ").unwrap_or_else(|| panic!("no callers are said: {line}"));
    let callers = &line[called..];
    assert!(callers.matches("from fn `").count() >= 2, "fewer than two callers named: {callers}");
    let (run, main) = (callers.find("run`"), callers.find("main`"));
    assert!(matches!((run, main), (Some(r), Some(m)) if r < m), "`run` then `main` are not named, innermost first: {callers}");
    let _ = std::fs::remove_dir_all(&base);
}
