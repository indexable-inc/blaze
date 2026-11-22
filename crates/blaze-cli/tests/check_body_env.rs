//! A body's `env!`/`option_env!` reads the crate's environment, the one its items read
//! (port27/mini/clap-root, 2026-11-17: clap_derive 4.6.7 writes `{ let _ = ::core::env!(
//! "CARGO_PKG_NAME"); "app" }` and `::core::env!("CARGO_PKG_VERSION")` into the bodies of
//! `Args`'s `CommandFactory` impl; `blaze check` printed "not checked because lowering refused
//! (Lowered: Macro Failed(Expand(Host(HostError Env, the call of `:: core :: env!`): `Args`":
//! a body job's texts had no environment (blaze-cli job_texts.rs `env: None`), so every body's
//! `env!` was refused).
//!
//! core's `env!` doc (pinned library, `macros/mod.rs`): "Inspects an environment variable at
//! compile time. ... If the environment variable is not defined, then a compilation error will
//! be emitted." `option_env!`: "If the named environment variable is present at compile time,
//! this will expand into an expression of type `Option<&'static str>` whose value is `Some` of
//! the value ...; if not, `None`." The cargo book, "Environment variables Cargo sets for
//! crates": `CARGO_PKG_VERSION` is "The full version of your package".
//!
//! The workspace: `dep` (`#[macro_export] version_of!`, whose expansion is a block reading
//! `env!("CARGO_PKG_VERSION")` and `option_env!("NOT_SET_ANYWHERE")`, and `missing_of!`, whose
//! is `env!("NOT_SET_ANYWHERE")`), `app` 1.2.3 (on `dep`). 1. `main` calls `dep::version_of!()`:
//! `check` passes, nothing refused, no body not checked. 2. CONTROL: `main` calls
//! `dep::missing_of!()` while the SHELL sets `NOT_SET_ANYWHERE` (a variable Cargo does not
//! set): the variable is unset for the crate (rule 75: nothing ambient), so the call is the
//! expansion's own error naming it, never a host refusal. TODAY a body's expansion error is
//! filed as a "not checked because lowering refused (.. Failed(Diagnostic, the call of `env!`:
//! its expansion's error EnvNotDefined: `NOT_SET_ANYWHERE`)" note; since lane MZ (2026-11-17)
//! it is the error finding an item-level call's is (blaze-cli names/failed_call.rs, rule 22):
//! "1 error", at the call, naming the variable.
//! Before the fix 1 printed "not checked because lowering refused (Lowered: Macro
//! Failed(Expand(Host(HostError Env, ..".
//!
//! Expected line: `BODY ENV: found 0, missing <not 0>`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).

mod support;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// How long `blaze check` of the two-package workspace may take; a hang is a bug (rule 18).
const DEADLINE: Duration = Duration::from_secs(200);

/// How long the child killed at [`DEADLINE`] may take to be reaped.
const REAP: Duration = Duration::from_secs(10);

const DEP: &str = "#[macro_export]
macro_rules! version_of {
    () => {{
        let v: &'static str = env!(\"CARGO_PKG_VERSION\");
        let unset: Option<&'static str> = option_env!(\"NOT_SET_ANYWHERE\");
        v.len() + unset.map_or(0, |s| s.len())
    }};
}

#[macro_export]
macro_rules! missing_of {
    () => {
        env!(\"NOT_SET_ANYWHERE\")
    };
}
";
const MAIN_FOUND: &str = "fn main() {\n    let n: usize = dep::version_of!();\n    let _ = n;\n}\n";
const MAIN_MISSING: &str = "fn main() {\n    let s: &'static str = dep::missing_of!();\n    let _ = s;\n}\n";

const NOT_CHECKED: &str = "not checked because";
const HOST_REFUSED: &str = "HostError";

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| panic!("making {}: {error}", dir.display()));
    }
    std::fs::write(path, text).unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
}

/// The workspace under `base/ws`, `app`'s `main` being `main`.
fn workspace(base: &Path, main: &str) -> std::path::PathBuf {
    let ws = base.join("ws");
    write(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"2\"\n");
    write(
        &ws.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"1.2.3\"\ndependencies = [\"dep\"]\n\n[[package]]\nname = \"dep\"\nversion = \"0.1.0\"\n",
    );
    write(&ws.join("app/Cargo.toml"), "[package]\nname = \"app\"\nversion = \"1.2.3\"\nedition = \"2021\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n");
    write(&ws.join("app/src/main.rs"), main);
    write(&ws.join("dep/Cargo.toml"), "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&ws.join("dep/src/lib.rs"), DEP);
    ws
}

/// `blaze check` in `ws` with `shell` set in its environment: its status and everything it
/// said, kept as `<base>/<what>.said`.
fn check(base: &Path, ws: &Path, what: &str, shell: &[(&str, &str)]) -> (Option<i32>, String) {
    let home = base.join("home");
    let said = base.join(format!("{what}.said"));
    let file = File::create(&said).unwrap_or_else(|error| panic!("making {}: {error}", said.display()));
    let file_err = file.try_clone().expect("the output file, twice");
    let mut command = Command::new(env!("CARGO_BIN_EXE_blaze"));
    command
        .arg("check")
        .current_dir(ws)
        .env("HOME", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", ws)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(file_err));
    for (name, value) in shell {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("blaze starts");
    let started = Instant::now();
    // Waiting for: `blaze check` of the workspace (`what`), at most DEADLINE.
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
                panic!("`blaze check` ({what}) in {} did not end within {DEADLINE:?} (rule 18)", ws.display());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("waiting for `blaze check` ({what}) in {}: {error}", ws.display());
            }
        }
    };
    let text = std::fs::read_to_string(&said).unwrap_or_else(|error| panic!("reading {}: {error}", said.display()));
    (status.code(), text)
}

/// Should print `BODY ENV: found 0, missing <not 0>`; on failure, the assertion and all
/// `check` said.
#[test]
fn a_body_s_env_reads_the_crate_s_environment() {
    let base = fresh_home("check_body_env", true);
    let ws = workspace(&base, MAIN_FOUND);
    let found = check(&base, &ws, "found", &[]);
    write(&ws.join("app/src/main.rs"), MAIN_MISSING);
    let missing = check(&base, &ws, "missing", &[("NOT_SET_ANYWHERE", "from the shell")]);
    let _ = std::fs::remove_dir_all(&base);

    let (status, said) = &found;
    assert!(!said.contains(HOST_REFUSED), "found: a body's `env!` was refused by its host:\n{said}");
    assert!(!said.contains(NOT_CHECKED), "found: a body was not checked:\n{said}");
    assert!(said.contains("0 errors: blaze check"), "found: status {status:?}:\n{said}");

    let (status, said) = &missing;
    assert!(status.is_some() && *status != Some(0), "missing: `NOT_SET_ANYWHERE` is no variable of the crate, status {status:?}:\n{said}");
    assert!(!said.contains(HOST_REFUSED), "missing: refused by the host, not the Reference's error:\n{said}");
    assert!(said.lines().any(|line| line.contains("NOT_SET_ANYWHERE")), "missing: the error names the variable:\n{said}");
    // The expansion's own error is the program's ERROR at the call (rule 22, as an item-level
    // call's), never a "not checked" note.
    assert!(said.lines().any(|line| line.contains("error[") && line.contains("NOT_SET_ANYWHERE")), "missing: the variable is not named by an error:\n{said}");
    assert!(said.contains("1 error"), "missing: not one error:\n{said}");
    assert!(!said.contains(NOT_CHECKED), "missing: the failed call is a not-checked note:\n{said}");

    println!("BODY ENV: found {}, missing {}", found.0.unwrap_or(-1), missing.0.unwrap_or(-1));
}
