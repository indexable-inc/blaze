//! `blaze check` runs a workspace member's build script as a `Script` task and its crate reads
//! the run (decision D115; docs/v2/plan/design/drive.md "Build scripts in the command").
//!
//! MOVE: /Volumes/Projects/blaze-int-1112-notes/staging/blaze-cli/tests/build_scripts.rs ->
//! crates/blaze-cli/tests/build_scripts.rs (after the D115 batch: drive/proc-macro/cli d115-1,
//! scope/world/cli d115-2, proc-macro/cli d115-4).
//!
//! The fixture is one member, `app`, with a `build.rs` and `src/main.rs`. Cargo's rule (The
//! Cargo Book, "Build Scripts"): a package with a `build.rs` at its root runs it before
//! building, and the script's `cargo::rustc-cfg=KEY` line "enables the given config option" for
//! the package's own crates. The Rust Reference, "Conditional compilation": an item under
//! `#[cfg(KEY)]` exists exactly when that option is set.
//!
//! The script's module is written here byte by byte (no rustc, rule 1): a WASI command whose
//! `_start` writes `cargo:rustc-cfg=from_script\n` to standard output, and its record is put in
//! the home's module store (`<home>/.blaze/v2/build-script/modules/<dir>/{identity,module.wasm}`,
//! found by blaze-proc-macro script_unit.rs `module_of` by name, version and features). The
//! module reads no file, so the run is not a hole (scripts.rs `run`).
//!
//! Expected (`lanecheck.sh blaze-cli test '--test build_scripts'`):
//! "test blaze-cli: exit 0; 2 passed, 0 failed".
//! - `a_scripts_cfg_reaches_a_cfg_item`. Control: in blaze-world script.rs `joined`, drop
//!   `cfg.extend(outputs.cfgs())`. It then fails with "the script's cfg did not reach
//!   #[cfg(from_script)]".
//! - `a_failed_script_is_a_finding_naming_it_and_its_unit` (no module in the store). Control:
//!   none needed for the script's line: the failure words are the run's own. The UNIT line
//!   ("not checked because the build script `build.rs` of crate `app` failed: crate `app`") comes
//!   with wiring d115-5 (blaze-cli not_checked.rs). Control: without d115-5's main.rs blocks the
//!   old count line is printed and the test fails with "the unit whose script failed is not
//!   named as not checked".

mod support;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// One `blaze check` of the one small member; a hang is a bug (rule 18).
const DEADLINE: Duration = Duration::from_secs(300);
const REAP: Duration = Duration::from_secs(10);

const MAIN: &str = "#[cfg(from_script)]\nfn made() {}\n\nfn main() {\n    made();\n}\n";
const BUILD: &str = "fn main() {\n    println!(\"cargo:rustc-cfg=from_script\");\n}\n";
/// The line the module writes.
const LINE: &[u8] = b"cargo:rustc-cfg=from_script\n";

fn workspace(dir: &Path) {
    let files: [(&str, &str); 5] = [
        ("Cargo.toml", "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n"),
        ("Cargo.lock", "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n"),
        ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        ("app/build.rs", BUILD),
        ("app/src/main.rs", MAIN),
    ];
    for (path, text) in files {
        let at = dir.join(path);
        std::fs::create_dir_all(at.parent().expect("a file's folder")).unwrap_or_else(|error| panic!("making the folder of {}: {error}", at.display()));
        std::fs::write(&at, text).unwrap_or_else(|error| panic!("writing {}: {error}", at.display()));
    }
}

fn uleb(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn section(id: u8, body: &[u8], out: &mut Vec<u8>) {
    out.push(id);
    uleb(body.len() as u64, out);
    out.extend_from_slice(body);
}

fn name(text: &str, out: &mut Vec<u8>) {
    uleb(text.len() as u64, out);
    out.extend_from_slice(text.as_bytes());
}

/// A WASI command module: `_start` calls `fd_write(1, iovs = 16, 1, nwritten = 32)`, the iovec
/// at 16 pointing at `line` stored at 64. One page of memory, exported as `memory`.
fn module(line: &[u8]) -> Vec<u8> {
    let mut out = b"\0asm\x01\0\0\0".to_vec();
    // Types: 0 = (i32, i32, i32, i32) -> i32 (fd_write), 1 = () -> () (`_start`).
    section(1, &[2, 0x60, 4, 0x7f, 0x7f, 0x7f, 0x7f, 1, 0x7f, 0x60, 0, 0], &mut out);
    let mut imports = vec![1];
    name("wasi_snapshot_preview1", &mut imports);
    name("fd_write", &mut imports);
    imports.extend_from_slice(&[0x00, 0]);
    section(2, &imports, &mut out);
    section(3, &[1, 1], &mut out);
    section(5, &[1, 0x00, 1], &mut out);
    let mut exports = vec![2];
    name("memory", &mut exports);
    exports.extend_from_slice(&[0x02, 0]);
    name("_start", &mut exports);
    exports.extend_from_slice(&[0x00, 1]);
    section(7, &exports, &mut out);
    // i32.const 1; i32.const 16; i32.const 1; i32.const 32; call 0; drop; end.
    let body = [0x00, 0x41, 1, 0x41, 16, 0x41, 1, 0x41, 32, 0x10, 0, 0x1a, 0x0b];
    let mut code = vec![1];
    uleb(body.len() as u64, &mut code);
    code.extend_from_slice(&body);
    section(10, &code, &mut out);
    let mut data = vec![2];
    // At 16: the iovec { buf: 64, len }.
    data.extend_from_slice(&[0x00, 0x41, 16, 0x0b, 8]);
    data.extend_from_slice(&64u32.to_le_bytes());
    data.extend_from_slice(&(line.len() as u32).to_le_bytes());
    // At 64 (`i32.const 64` is 0xc0 0x00 in signed LEB128): the line.
    data.extend_from_slice(&[0x00, 0x41, 0xc0, 0x00, 0x0b]);
    uleb(line.len() as u64, &mut data);
    data.extend_from_slice(line);
    section(11, &data, &mut out);
    out
}

/// The module record of `app 0.1.0` with no features in `home`'s module store.
fn put_module(home: &Path) {
    let record = home.join(".blaze/v2/build-script/modules/app-0.1.0-test");
    std::fs::create_dir_all(&record).unwrap_or_else(|error| panic!("making {}: {error}", record.display()));
    std::fs::write(record.join("identity"), "name app\nversion 0.1.0\nfeatures \n").expect("the module's identity");
    let bytes = module(LINE);
    // The record's digest (script_modules.rs reads it as the lookup's out): the module's SHA-256.
    let digest: String = blaze_sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(record.join("wasm-sha256"), format!("{digest}\n")).expect("the module's digest");
    std::fs::write(record.join("module.wasm"), bytes).expect("the module's bytes");
}

/// `blaze check` in `dir` with the home `home`: whether it exited 0, and its output.
fn check(dir: &Path, home: &Path) -> (bool, String) {
    run(dir, home, "check")
}

/// `blaze <word>` in `dir` with the home `home`: whether it exited 0, and its output.
fn run(dir: &Path, home: &Path, word: &str) -> (bool, String) {
    let said = dir.with_extension(word);
    let file = File::create(&said).unwrap_or_else(|error| panic!("making {}: {error}", said.display()));
    let file_err = file.try_clone().expect("the output file, twice");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg(word)
        .current_dir(dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", dir)
        .env_remove("SDKROOT")
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(file_err))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    // Waiting for: `blaze check` of one member with a build script, at most DEADLINE.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > DEADLINE => {
                let _ = child.kill();
                let reaped = Instant::now();
                // Waiting for: the killed child's exit, at most REAP.
                while child.try_wait().ok().flatten().is_none() && reaped.elapsed() < REAP {
                    std::thread::sleep(Duration::from_millis(5));
                }
                panic!("`blaze {word}` in {} did not end within {DEADLINE:?} (rule 18)", dir.display());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("waiting for `blaze {word}` in {}: {error}", dir.display());
            }
        }
    };
    let text = std::fs::read_to_string(&said).unwrap_or_else(|error| panic!("reading {}: {error}", said.display()));
    (status.success(), text)
}

/// The script's `cargo:rustc-cfg=from_script` reaches `#[cfg(from_script)] fn made`, so
/// `made()` in `main` resolves and the check passes. Prints "cfg reached".
#[test]
fn a_scripts_cfg_reaches_a_cfg_item() {
    let base = fresh_home("build_scripts_cfg", true);
    let (home, at) = (base.join("home"), base.join("ws"));
    workspace(&at);
    put_module(&home);
    let (_, said) = check(&at, &home);
    assert!(!said.contains("the build script of app"), "the script did not run:\n{said}");
    // The status is not read: while the borrow check's findings stay notes, every check ends
    // "incomplete: NOT checked: borrows" (`BORROWS_ARE_ERRORS`). The type check's own line is.
    assert!(
        said.contains("0 errors: blaze check") && !said.lines().any(|line| line.contains("`made`")),
        "the script's cfg did not reach #[cfg(from_script)]:\n{said}"
    );
    println!("cfg reached");
}

/// With no module of `app`'s script in the store, the run fails: one finding naming the script
/// and its package, and the unit that reads it named as not checked (rule 20: the rest goes on).
/// Prints "failed by name".
#[test]
fn a_failed_script_is_a_finding_naming_it_and_its_unit() {
    let base = fresh_home("build_scripts_failed", true);
    let (home, at) = (base.join("home"), base.join("ws"));
    workspace(&at);
    let (ok, said) = check(&at, &home);
    assert!(!ok, "a check whose build script failed exited 0:\n{said}");
    assert!(said.contains("the build script of app 0.1.0"), "the failed script is not named:\n{said}");
    assert!(said.contains("no build-script module of app 0.1.0"), "the script's own failure is not said:\n{said}");
    // Ruling D115 (6), one shape for every cause (blaze-cli not_checked.rs).
    assert!(
        // Since d115-6 (piece 2c) the cause is the module's lookup, named by its script.
        // Since not_checked/waited.rs, the line under the failure names what waited on it.
        said.lines().any(|line| line.contains("not checked because of it") && line.ends_with("`app`")),
        "the unit whose script failed is not named as not checked:\n{said}"
    );
    assert!(!said.contains("task(s) not run: each needs one that failed above"), "the old count line is still printed:\n{said}");
    println!("failed by name");
}

/// Each phase `engine-check` ran, from the profile it wrote in `dir` (engine_check.rs
/// `profile_text`): (stage, wave, planned, entered).
fn engine_phases(dir: &Path) -> Vec<(String, u32, u32, u32)> {
    let at = dir.join(".blaze/v2/profile.txt");
    let text = std::fs::read_to_string(&at).unwrap_or_else(|error| panic!("reading {}: {error}", at.display()));
    let mut lines = text.lines().skip_while(|line| !line.starts_with("engine phases")).skip(1);
    let mut phases = Vec::new();
    while let Some(line) = lines.next().filter(|line| line.starts_with("  ")) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let number = |at: usize| words[at].trim_end_matches(':').parse::<u32>().unwrap_or_else(|_| panic!("a phase row's numbers: {line}"));
        phases.push((words[0].to_owned(), number(1), number(2), number(3)));
    }
    phases
}

/// The `Scripts` stage of `engine-check` (engine.md E6): the member's script runs as one
/// `Script` task after its module's lookup, its crate's names read the run, and `engine-check`
/// lists what `check` lists: nothing for `made()` (the script's cfg reached `#[cfg(from_script)]`).
/// Every phase entered each task it planned, once. Prints "engine scripts: <phases>". CONTROL:
/// before the stage, `engine-check` said "a crate with a build script: `engine-check` runs no
/// build script yet" here.
#[test]
fn engine_check_runs_the_script_and_lists_what_check_lists() {
    let base = fresh_home("build_scripts_engine", true);
    let (home, at) = (base.join("home"), base.join("ws"));
    workspace(&at);
    put_module(&home);
    let (_, old) = check(&at, &home);
    // Not the status: every check ends "incomplete: NOT checked: borrows" for now.
    assert!(old.contains("0 errors: blaze check") && !old.contains("not checked because"), "check of the fixture failed:\n{old}");
    let (_, said) = run(&at, &home, "engine-check");
    assert!(!said.contains("runs no build script"), "engine-check refused the build script:\n{said}");
    assert!(!said.contains("the build script of app"), "the script did not run:\n{said}");
    assert!(!said.lines().any(|line| line.contains("`made`")), "the script's cfg did not reach #[cfg(from_script)] on the engine:\n{said}");
    assert!(!said.contains("not run: each waits"), "a task waited on a failed one:\n{said}");
    let phases = engine_phases(&at);
    for (stage, wave, planned, entered) in phases.iter() {
        assert_eq!(planned, entered, "{stage} {wave}: tasks planned and entered:\n{phases:?}");
    }
    let scripts: Vec<(u32, u32)> = phases.iter().filter(|(stage, ..)| stage == "Scripts").map(|(_, wave, planned, _)| (*wave, *planned)).collect();
    assert_eq!(scripts, [(0, 1), (1, 1)], "one module lookup, then one run: {phases:?}");
    println!("engine scripts: {phases:?}");
}

/// With no module of the script in the store, `engine-check` fails as `check` does: the lookup's
/// own words name the script's package, and its crate is not resolved. Prints "engine failed by
/// name".
#[test]
fn engine_check_names_a_missing_script_module() {
    let base = fresh_home("build_scripts_engine_failed", true);
    let (home, at) = (base.join("home"), base.join("ws"));
    workspace(&at);
    let (ok, said) = run(&at, &home, "engine-check");
    assert!(!ok, "an engine-check whose build script failed exited 0:\n{said}");
    assert!(said.contains("no build-script module of app 0.1.0"), "the lookup's own failure is not said:\n{said}");
    assert!(said.contains("not run: each waits on a task that failed"), "the run and its crate did not wait on the lookup:\n{said}");
    let phases = engine_phases(&at);
    for (stage, wave, planned, entered) in phases.iter() {
        assert_eq!(planned, entered, "{stage} {wave}: tasks planned and entered:\n{phases:?}");
    }
    println!("engine failed by name");
}
