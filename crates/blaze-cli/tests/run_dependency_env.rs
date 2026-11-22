//! A dependency's body lowered by need on the run path reads ITS crate's environment and ITS
//! files, as `blaze check` gives a body its owner's (run_names.rs `RunEnvs`, job_texts.rs
//! `EnvsByNeed`, body_types.rs `row_env`: one construction for both paths, rule 30).
//!
//! tempo, sweep of 2026-11-21 (75 tests NOT RUN): `tempo_chainspec`'s `MODERATO` static
//! (`include_str!("genesis/moderato.json")`) was NOT RUN for `Host(HostError { ask: Include,
//! why: Refused })`, and the impls `derive(Parser)` writes (`tempo_chainspec`'s test `Cli`,
//! `tempo_faucet`'s `CommandParser`) for `HostError { ask: Env, why: Refused }`. CAUSE:
//! run_names.rs `open` built the run's names records with no crate's environment (`env:
//! None`, "No crate's environment yet"), so body_host.rs `Sourced::env` and `folder` were
//! `None` for every crate, and `BodyHost::include` refuses when `folder` is `None`.
//!
//! Rust's rules. core's `env!` doc (pinned library, `macros/mod.rs`): "Inspects an environment
//! variable at compile time." The variable is the compiled crate's: the cargo book,
//! "Environment variables Cargo sets for crates": "`CARGO_PKG_NAME` — The name of your
//! package", set for each crate as it is compiled. `include_str!`: "The file is located
//! relative to the current file (similarly to how modules are found)."
//!
//! The workspace: `dep` 0.4.2 (`pub static S: &str = include_str!("data.txt");`, `pub fn
//! name() -> &'static str { env!("CARGO_PKG_NAME") }`), `app` on `dep`.
//!
//! 1. `check`, then `test`: `app`'s test asserts `dep::S` is `data.txt`'s text and
//!    `dep::name()` is `"dep"`; it runs and passes. TWIN: `app`'s own test asserts its own
//!    `env!("CARGO_PKG_NAME")` is `"app"`: each crate reads its own, never the root's for
//!    all (the owner-only environment of `blaze check`'s body jobs, made per crate here).
//! 2. `run` of `app`'s bin prints both (the run path's `program_of` rows, not the test path's
//!    `rows_of`): `dep hello from dep`.
//! 3. CONTROL, Rust's refusal kept: `dep`'s `missing()` reads `env!("NOT_SET_ANYWHERE")`, which
//!    no crate's environment holds (and the shell sets: rule 75, nothing ambient), and a test
//!    calls it: the check's error names the variable, and that test is NOT RUN; never a value,
//!    never the host's refusal (rule 3).
//!
//! THE PLANTED CONTROL (rule 23, in the main agent's hands: no switch exists for it): with
//! run_names.rs `open`'s `Envs { owner: None, rest: Some(envs) }` made `rest: None`, case 1
//! prints the sweep's `NOT RUN` with `HostError` for `Include` and `Env` again.
//!
//! macOS only: Linux has no test sandbox (test_command_runs.rs says why). Needs the installed
//! toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).
//!
//! Run: `cargo test -p blaze-cli --test run_dependency_env`.

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// One command's bound: a check of a two-package workspace and a few first calls built.
const DEADLINE: Duration = Duration::from_secs(300);

const WORKSPACE: &str = "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"2\"\n";
const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n";
const DEP: &str = "[package]\nname = \"dep\"\nversion = \"0.4.2\"\nedition = \"2024\"\n";
const DATA: &str = "hello from dep\n";
const DEP_LIB: &str = "pub static S: &str = include_str!(\"data.txt\");\n\npub fn name() -> &'static str {\n    env!(\"CARGO_PKG_NAME\")\n}\n";
/// [`DEP_LIB`] with a fn whose `env!` names a variable no crate's environment holds.
const DEP_LIB_MISSING: &str = "pub static S: &str = include_str!(\"data.txt\");\n\npub fn name() -> &'static str {\n    env!(\"CARGO_PKG_NAME\")\n}\n\npub fn missing() -> &'static str {\n    env!(\"NOT_SET_ANYWHERE\")\n}\n";
/// The dependency's static and fn (case 1), and the twin: `app`'s own name.
const APP_LIB: &str = "#[test]\nfn reads_the_dependencys_file_and_name() {\n    assert!(dep::S == \"hello from dep\\n\");\n    assert!(dep::name() == \"dep\");\n}\n\n#[test]\nfn reads_its_own_name() {\n    assert!(env!(\"CARGO_PKG_NAME\") == \"app\");\n}\n";
/// [`APP_LIB`] and a test of `dep::missing` (case 3).
const APP_LIB_MISSING: &str = "#[test]\nfn reads_the_dependencys_file_and_name() {\n    assert!(dep::S == \"hello from dep\\n\");\n    assert!(dep::name() == \"dep\");\n}\n\n#[test]\nfn calls_missing() {\n    assert!(!dep::missing().is_empty());\n}\n";
const APP_MAIN: &str = "fn main() {\n    println!(\"{} {}\", dep::name(), dep::S.trim_end());\n}\n";

struct Said {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

impl Said {
    fn evidence(&self, why: &str) -> String {
        format!("--- status {:?}; {why}\n--- stderr\n{}\n--- stdout\n{}", self.code, self.stderr, self.stdout)
    }

    fn says(&self, words: &str) -> bool {
        self.stdout.contains(words) || self.stderr.contains(words)
    }
}

/// A scratch workspace of `files` and its home; `check` is given both, then they are removed.
fn fixture<T>(name: &str, files: &[(&str, &str)], check: impl FnOnce(&Path, &Path) -> T) -> T {
    let base = fresh_home(name, true);
    let base = base.canonicalize().expect("the scratch folder's canonical path");
    let home = base.join("home");
    let dir = base.join("project");
    for &(name, source) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("a file in the fixture")).unwrap_or_else(|error| panic!("making the folder for {name}: {error}"));
        std::fs::write(&path, source).unwrap_or_else(|error| panic!("writing {name}: {error}"));
    }
    let result = check(&dir, &home);
    std::fs::remove_dir_all(&base).expect("remove the scratch command fixture");
    result
}

/// `blaze <command> <args>` in `dir` with `home`, under [`DEADLINE`]. The shell sets
/// `NOT_SET_ANYWHERE`: no crate's environment may take it (rule 75).
fn blaze(dir: &Path, home: &Path, command: &str, args: &[&str]) -> Said {
    let stdout = home.join("command.stdout");
    let stderr = home.join("command.stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg(command)
        .args(args)
        .current_dir(dir)
        .env("PWD", dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("NOT_SET_ANYWHERE", "from the shell")
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).expect("capture blaze's standard output"))
        .stderr(std::fs::File::create(&stderr).expect("capture blaze's standard error"))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    // Waiting for: `blaze <command>` in the fixture, at most DEADLINE.
    let status = loop {
        if let Some(status) = child.try_wait().expect("a wait on blaze") {
            break status;
        }
        if started.elapsed() > DEADLINE {
            child.kill().expect("stop the timed-out command");
            let _ = child.wait();
            panic!("`blaze {command}` past {DEADLINE:?} in {}: the command hung (rule 18)", dir.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Said {
        stdout: std::fs::read_to_string(stdout).expect("read blaze's standard output"),
        stderr: String::from_utf8_lossy(&std::fs::read(stderr).expect("read blaze's standard error")).into_owned(),
        code: status.code(),
    }
}

/// Nothing the run path's host refused: the sweep's words.
fn assert_no_host_refusal(said: &Said, what: &str) {
    assert!(!said.says("HostError"), "{}", said.evidence(&format!("{what}: no host refusal (the sweep's `HostError` for `Include`/`Env`)")));
}

/// 1. `check`, then `test`: the dependency's static and fn run; the twin reads `app`'s name.
#[test]
#[cfg(target_os = "macos")]
fn a_dependencys_include_and_env_run_after_a_check() {
    let files = [("Cargo.toml", WORKSPACE), ("app/Cargo.toml", APP), ("app/src/lib.rs", APP_LIB), ("dep/Cargo.toml", DEP), ("dep/src/lib.rs", DEP_LIB), ("dep/src/data.txt", DATA)];
    fixture("run_dependency_env_test", &files, |dir, home| {
        let checked = blaze(dir, home, "check", &[]);
        assert!(checked.says("0 errors"), "{}", checked.evidence("the check finds no error"));
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert_no_host_refusal(&said, "test");
        assert!(!said.stdout.contains("NOT RUN") && !said.stdout.contains("FAILED"), "{}", said.evidence("both tests run"));
        assert!(said.stdout.contains("ran 2: 2 passed, 0 failed"), "{}", said.evidence("the dependency's texts and the twin pass"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
    });
}

/// 2. `run` of the bin: the run path's rows (`program_of`), the same environments.
#[test]
#[cfg(target_os = "macos")]
fn a_dependencys_include_and_env_print_through_run() {
    let files = [("Cargo.toml", WORKSPACE), ("app/Cargo.toml", APP), ("app/src/main.rs", APP_MAIN), ("dep/Cargo.toml", DEP), ("dep/src/lib.rs", DEP_LIB), ("dep/src/data.txt", DATA)];
    fixture("run_dependency_env_run", &files, |dir, home| {
        let said = blaze(dir, home, "run", &["--bin", "app"]);
        assert_no_host_refusal(&said, "run");
        assert!(said.stdout.contains("dep hello from dep"), "{}", said.evidence("the bin prints the dependency's name and file"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("the run exits 0"));
    });
}

/// 3. CONTROL: a variable no crate's environment holds is Rust's error, by name, and its test
/// is NOT RUN; the shell's value is never taken, and the host never refuses.
#[test]
#[cfg(target_os = "macos")]
fn an_env_no_crate_holds_is_an_error_never_the_shells_value() {
    let files = [("Cargo.toml", WORKSPACE), ("app/Cargo.toml", APP), ("app/src/lib.rs", APP_LIB_MISSING), ("dep/Cargo.toml", DEP), ("dep/src/lib.rs", DEP_LIB_MISSING), ("dep/src/data.txt", DATA)];
    fixture("run_dependency_env_missing", &files, |dir, home| {
        let checked = blaze(dir, home, "check", &[]);
        assert!(checked.says("NOT_SET_ANYWHERE"), "{}", checked.evidence("the check's error names the variable"));
        assert!(!checked.says("from the shell"), "{}", checked.evidence("the shell's value is never the crate's"));
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert_no_host_refusal(&said, "test of the control");
        assert!(said.stdout.contains("NOT RUN"), "{}", said.evidence("`calls_missing` is NOT RUN"));
        assert!(!said.says("from the shell"), "{}", said.evidence("the shell's value is never the crate's"));
        assert!(said.stdout.contains("1 passed"), "{}", said.evidence("the other test still runs and passes"));
        assert_ne!(said.code, Some(0), "{}", said.evidence("a suite with a test NOT RUN exits nonzero"));
    });
}
