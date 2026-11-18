//! `blaze run` of a program that dies on a trap says WHAT and WHERE (rule 19): the function by
//! its path and the trap by its reason (blaze-cli run_fault.rs, from the child's `Up::Fault`
//! and the record's own instruction at the pc), never only ids.

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// A trap is milliseconds after the start; a hang is a bug (rule 18), named, never waited out.
const DEADLINE: Duration = Duration::from_secs(60);

/// tests/run_trapped/abort: `main` calls `give_up`, which reaches `abort_immediate` (`brk
/// #0xfe00`). The program ends by SIGTRAP (status 128 + 5 = 133); `blaze run`'s error stream
/// names `give_up` by its path, the `brk` and the reason "an abort". Before RF8 the line named
/// the function by the first bytes of its symbol alone (the coordinator's std-hello run, 21:42):
/// with the parent's line removed, the path assertion fails.
#[test]
fn a_trap_is_said_by_the_function_s_path_and_its_reason() {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run_trapped/abort");
    let base = fresh_home("run_trapped", true);
    let home = base.join("home");
    let dir = base.join("checkouts/abort");
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
            panic!("`blaze run` on run_trapped/abort past {DEADLINE:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().expect("blaze's output");
    let said = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(128 + 5), "the program did not end by SIGTRAP: {said}");
    let line = said.lines().find(|line| line.contains("it struck at")).unwrap_or_else(|| panic!("no named line: {said}"));
    assert!(line.contains("killed by SIGTRAP"), "{line}");
    assert!(line.contains("in fn `") && line.contains("give_up`"), "the function is not named by its path: {line}");
    assert!(line.contains("`brk #0xfe00`") && line.contains("an abort"), "the trap's reason is not said: {line}");
    println!("run trapped: {line}");
    let _ = std::fs::remove_dir_all(&base);
}
