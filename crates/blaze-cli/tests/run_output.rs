//! `blaze run` on a clean rung, as a process: its standard output is byte-equal to the rung's
//! `expected` (the program's alone, so `blaze run | consumer` sees only what the program wrote),
//! its standard error is empty (a run with no findings says nothing), and its status is the
//! program's. The 03:10 build of 2026-11-16 printed the left-out note after rung 3's "hello".

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// The names and lengths in the owner's one store, `~/.blaze/v2/store`, read only: a test compares
/// them before and after to show it wrote nothing there. Another blaze the owner runs at the
/// same moment would change them too: the assertion names what changed.
fn real_shared_listing() -> Vec<(String, u64)> {
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(Path::new(&home).join(".blaze/v2/store")) else { return Vec::new() };
    let mut listed: Vec<(String, u64)> = entries
        .filter_map(Result::ok)
        .map(|entry| (entry.file_name().to_string_lossy().into_owned(), entry.metadata().map(|meta| meta.len()).unwrap_or(0)))
        .collect();
    listed.sort();
    listed
}

/// The rungs whose program runs clean today and prints through the C library.
/// `x0-static-05-interior-mut`: six statics, one per shape of interior mutability (and a plain
/// one), written through the child's writable data area and read back.
const CLEAN: &[&str] = &["3-hello", "front2/x0-static-05-interior-mut"];

/// How long one `blaze run` of a rung may take: a cold run is milliseconds; a hang is a bug
/// (rule 18), reported here by name, never waited out.
const DEADLINE: Duration = Duration::from_secs(60);

/// `build` is no command of blaze (the owner, 2026-11-15: the commands are `check`, `run`,
/// `test` and later `export`): the process says so, names the two commands that do its work,
/// and fails; it never says `build` is yet to be built. With the old arm
/// (`CliError::NotBuilt { command: "build" }`) the first assertion fails: "`blaze build` is not
/// built on the new item layer yet".
#[test]
fn build_is_no_command_and_names_run_and_export() {
    let base = fresh_home("build", false);
    let output = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("build")
        .env("HOME", base.join("home"))
        .env("CARGO_HOME", base.join("home/.cargo"))
        .stdin(Stdio::null())
        .output()
        .expect("blaze starts");
    let _ = std::fs::remove_dir_all(&base);
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("blaze has no `build` command"), "{said}");
    assert!(said.contains("`blaze run`") && said.contains("`blaze export`"), "{said}");
    assert!(!said.contains("is not built on the new item layer"), "{said}");
    assert_ne!(output.status.code(), Some(0), "{said}");
}

/// Run with `HOME` at an empty temporary folder (its toolchain shelf hard-linked in): the run
/// passes, its one store lands in that folder, and the owner's `~/.blaze/v2/store` lists
/// the same names and lengths before and after (read only). Expected: passes; with `HOME`
/// left to the owner's, the last assertion fails ("the temporary home's shared store").
#[test]
fn a_clean_run_prints_only_its_program() {
    let ladder = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ladder");
    let base = fresh_home("run_output", true);
    let home = base.join("home");
    let scratch = base.join("checkouts");
    let before = real_shared_listing();
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
                panic!("`blaze run` on {rung} past {DEADLINE:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().expect("blaze's output");
        assert_eq!(output.status.code(), Some(0), "{rung}: {}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(stdout), "{rung}: standard output");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{rung}: a clean run says nothing");
    }
    assert_eq!(real_shared_listing(), before, "the owner's ~/.blaze/v2/store changed while the test ran (another blaze of the owner's, or this test writing there)");
    assert!(home.join(".blaze/v2/store").is_dir(), "the temporary home's shared store: the run kept its records there");
    let _ = std::fs::remove_dir_all(&base);
}

/// Rule 4 (blaze-ladder personality.rs): a `no_core` program built `panic = "unwind"` with no
/// `#[lang = "eh_personality"]` fn runs while no record lands (`3-hello` and
/// `x0-static-05-interior-mut` above), and the one whose `main` holds a value with a `Drop`
/// across a call that may unwind (tests/run_refused/no-routine) is refused at `main`'s record,
/// naming `main` by its path, with status 125 (not run). A parent that refused at the start
/// fails the clean test above; one that placed the record fails here (the child's own
/// `EhError::NoPersonality` names no fn).
#[test]
fn a_landing_record_with_no_routine_is_refused_naming_its_fn() {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run_refused/no-routine");
    let base = fresh_home("run_refused", true);
    let home = base.join("home");
    let dir = base.join("checkouts/no-routine");
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
            panic!("`blaze run` on no-routine past {DEADLINE:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().expect("blaze's output");
    let said = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(125), "{said}");
    assert!(said.contains("::main`") && said.contains("eh_personality"), "the refusal does not name `main` and the missing routine: {said}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "", "a refused program printed");
    println!("run output: no routine, a landing record refused: {said}");
    let _ = std::fs::remove_dir_all(&base);
}
