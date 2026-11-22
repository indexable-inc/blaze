//! `blaze run` compiles only what its program calls (run.md, "Run and test read the engine's
//! answer (D147"): THE GATE IS THE CALL. The engine checks every member and prints its answer
//! before any start; a program starts whatever the check found, and each workspace body is
//! admitted at its first call by its own facts, so an error in a fn the program never calls
//! does not stop it, and a call of a fn whose check has an error stops the program with that
//! fn's finding (run_parent.rs `ParentError::NotChecked`).
//!
//! One package, `two`: a library whose `broken` has a type error, and two binaries, `ok`, which
//! never calls it, and `bad`, which does:
//!
//! - `run --bin ok` prints its line and exits 0, the check's error line for `broken` printed
//!   before it (nothing hidden);
//! - `run --bin bad` is refused at the call, naming the body's check (`Errors`), and never
//!   prints what follows the call;
//! - control: `broken` fixed, `run --bin bad` runs to its end and exits 0, so the refusal above
//!   was the error's.
//!
//! Each case is the built `blaze` in a fresh package and a scratch home (tests/support), as
//! tests/engine_start.rs runs it, each command under a deadline (rule 18). macOS only, as
//! engine_start.rs.
//!
//! Run: `cargo test -p blaze-cli --test engine_run_scope` (needs the toolchain's shelf in the
//! owner's home, as tests/command_rungs.rs does).

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// One command's bound: a check of a small package and a few first calls built.
const DEADLINE: Duration = Duration::from_secs(300);

const MANIFEST: &str = "[package]\nname = \"two\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n\n[[bin]]\nname = \"ok\"\npath = \"src/bin/ok.rs\"\n\n[[bin]]\nname = \"bad\"\npath = \"src/bin/bad.rs\"\n";
/// `broken` returns a string where a `u8` is declared (E0308, at line 2).
const BROKEN: &str = "pub fn broken() -> u8 {\n    \"not a number\"\n}\n";
/// [`BROKEN`] fixed: the control.
const FIXED: &str = "pub fn broken() -> u8 {\n    7\n}\n";
const OK: &str = "fn main() {\n    println!(\"ok ran\");\n}\n";
const BAD: &str = "fn main() {\n    let n = two::broken();\n    println!(\"bad ran past the call: {n}\");\n}\n";

struct Said {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

impl Said {
    fn evidence(&self, why: &str) -> String {
        format!("--- status {:?}; {why}\n--- stderr\n{}\n--- stdout\n{}", self.code, self.stderr, self.stdout)
    }

    /// Standard output and error together: where a refusal is printed is the command's choice.
    fn both(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// A scratch package of `files` and its home; `check` is given both, then they are removed.
fn fixture<T>(name: &str, files: &[(&str, &str)], check: impl FnOnce(&Path, &Path) -> T) -> T {
    let base = fresh_home(name, true);
    let base = base.canonicalize().expect("the scratch folder's canonical path");
    let home = base.join("home");
    let dir = base.join("project");
    for &(name, source) in files {
        write(&dir, name, source);
    }
    let result = check(&dir, &home);
    std::fs::remove_dir_all(&base).expect("remove the scratch command fixture");
    result
}

fn write(dir: &Path, name: &str, source: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().expect("a file in the fixture")).unwrap_or_else(|error| panic!("making the folder for {name}: {error}"));
    std::fs::write(&path, source).unwrap_or_else(|error| panic!("writing {name}: {error}"));
}

/// `blaze <command> <args>` in `dir` with `home`, under [`DEADLINE`].
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
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).expect("capture blaze's standard output"))
        .stderr(std::fs::File::create(&stderr).expect("capture blaze's standard error"))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
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

/// The check's error line for `broken`: an `error[` line placed at its `"not a number"`. Under
/// `run` the check prints to standard error (standard output is the program's).
fn broken_error_printed(said: &Said) -> bool {
    said.stderr.lines().any(|line| line.contains("error[") && line.contains("src/lib.rs:2:"))
}

#[test]
#[cfg(target_os = "macos")]
fn run_compiles_only_what_its_program_calls() {
    let files = [("Cargo.toml", MANIFEST), ("src/lib.rs", BROKEN), ("src/bin/ok.rs", OK), ("src/bin/bad.rs", BAD)];
    fixture("engine_run_scope", &files, |dir, home| {
        // `ok` never calls `broken`: it runs, the check's error printed before it.
        let ok = blaze(dir, home, "run", &["--bin", "ok"]);
        assert!(broken_error_printed(&ok), "{}", ok.evidence("the check's error for `broken` is printed"));
        assert!(ok.stdout.lines().any(|line| line == "ok ran"), "{}", ok.evidence("`ok` ran"));
        assert_eq!(ok.code, Some(0), "{}", ok.evidence("a program that calls no erroneous body exits 0"));

        // `bad` calls `broken`: refused at the call with that body's check.
        let bad = blaze(dir, home, "run", &["--bin", "bad"]);
        assert!(broken_error_printed(&bad), "{}", bad.evidence("the check's error for `broken` is printed"));
        assert!(!bad.stdout.contains("bad ran past the call"), "{}", bad.evidence("nothing after the refused call ran"));
        let both = bad.both();
        assert!(both.contains("the program called fn") && both.contains("whose check today is Errors"), "{}", bad.evidence("the refusal names the called body and its check"));
        assert_ne!(bad.code, Some(0), "{}", bad.evidence("a refused call fails the command"));
    });
    // Control: `broken` fixed, `bad` runs past the call and exits 0: the refusal was the error's.
    let fixed = [("Cargo.toml", MANIFEST), ("src/lib.rs", FIXED), ("src/bin/ok.rs", OK), ("src/bin/bad.rs", BAD)];
    fixture("engine_run_scope_fixed", &fixed, |dir, home| {
        let bad = blaze(dir, home, "run", &["--bin", "bad"]);
        assert!(!bad.both().contains("error["), "{}", bad.evidence("no error is printed"));
        assert!(bad.stdout.lines().any(|line| line == "bad ran past the call: 7"), "{}", bad.evidence("`bad` ran to its end"));
        assert_eq!(bad.code, Some(0), "{}", bad.evidence("a clean program exits 0"));
    });
}
