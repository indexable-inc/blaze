//! `blaze run` on a clean rung, as a process: its standard output is byte-equal to the rung's
//! `expected` (the program's alone, so `blaze run | consumer` sees only what the program wrote),
//! its standard error is empty (a run with no findings says nothing), and its status is the
//! program's. The 03:10 build of 2026-11-16 printed the left-out note after rung 3's "hello".

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The rungs whose program runs clean today and prints through the C library.
const CLEAN: &[&str] = &["3-hello"];

/// How long one `blaze run` of a rung may take: a cold run is milliseconds; a hang is a bug
/// (rule 18), reported here by name, never waited out.
const DEADLINE: Duration = Duration::from_secs(60);

#[test]
fn a_clean_run_prints_only_its_program() {
    let ladder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ladder");
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("run_output");
    for rung in CLEAN {
        let from = ladder.join(rung);
        let dir = scratch.join(rung);
        // A fresh copy each time: the rung's own folder is never written.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).expect("the copy's folders");
        std::fs::copy(from.join("Cargo.toml"), dir.join("Cargo.toml")).expect("the rung's manifest");
        std::fs::copy(from.join("src/main.rs"), dir.join("src/main.rs")).expect("the rung's main.rs");
        let expected = std::fs::read(from.join("expected")).expect("the rung's expected");
        let stdout = expected.strip_prefix(b"status 0\nstdout\n".as_slice()).expect("expected: status 0, then stdout");
        let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
            .arg("run")
            .current_dir(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("blaze starts");
        let started = Instant::now();
        while child.try_wait().expect("a wait on blaze").is_none() {
            if started.elapsed() > DEADLINE {
                let _ = child.kill();
                panic!("`blaze run` on {rung} past {DEADLINE:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().expect("blaze's output");
        assert_eq!(output.status.code(), Some(0), "{rung}: {}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(stdout), "{rung}: standard output");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{rung}: a clean run says nothing");
    }
}
