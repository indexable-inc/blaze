//! A REDUCTION of tempo's `unreachable!` notes (rt-loop a0840 check.out, 2026-11-21): a
//! workspace body's tail call of a dependency's macro whose expansion chain (evm2's
//! `spec_to_generic!` through `for_each_spec!`) writes `match $s { $(arm => { $crate::m!(@one
//! ..) })* _ => unreachable!() }`, each arm's inner expansion a block declaring a `const`.
//! `blaze check` printed for `crates/evm/src/handler/config.rs` `execution_config`: "lowering
//! refused (.. the call of `unreachable!`): .. path: \"unreachable\", why: \"a path in module
//! b0a388..: segment 0: dependency source not read\"". The module is a block module front2
//! derived for an expansion's block (front2 scoped.rs `BlockModules::expansion_module`, keys by
//! scope_table.rs `expanded`) that the names world does not hold (names.rs `lexical`:
//! `world.chain(module)` is `None`, `Unheld::Step`).
//!
//! Rust's rule: "Macros by example", "Hygiene": names other than local variables, labels and
//! `$crate` are "looked up at the macro invocation site"; `unreachable!` is the std prelude's.
//! A block an expansion writes is a block like any other (expanded_blocks.rs module doc).
//!
//! 1. `check` reports no body not checked and no lowering refused; `test` runs `pick` (the
//!    workspace fn whose body is the call) and passes.
//! 2. TWIN: the same chain with an exhaustive match and no `_ => unreachable!()` arm: isolates
//!    the lookup in the match's block from the blocks the arms' expansions write.
//!
//! IGNORED until the cause is named: the reduction is not yet run (the writing lane could not
//! compile or run). Run it with `--ignored`: if 1 fails as tempo did and 2 passes, the cause
//! is the module derived for the match's braces; if both pass, the reduction misses a piece of
//! evm2's (its `#[allow(unreachable_patterns)]` on the arm, a `$crate::` path, or the call's
//! depth) and the next step is the names record of module `b0a388..` in tempo_evm.
//!
//! macOS only: Linux has no test sandbox. Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain` (linked into a fresh home).
//!
//! Run: `cargo test -p blaze-cli --test check_expansion_match_block -- --ignored`.

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// One command's bound: a check of a two-package workspace and a few first calls built.
const DEADLINE: Duration = Duration::from_secs(300);

const WORKSPACE: &str = "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"2\"\n";
const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n";
const DEP: &str = "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

/// evm2's shape: `to_generic!` -> `for_each!` -> `to_generic! { [@match ..] .. }` -> a match
/// whose arms each call `to_generic!(@one ..)`, a block declaring a `const`.
const DEP_LIB: &str = "#[derive(Clone, Copy)]
pub enum Spec {
    A = 1,
    B = 2,
}

pub fn get<const N: u32>() -> u32 {
    N
}

#[macro_export]
macro_rules! to_generic {
    (@one $v:ident, |$c:ident| $e:expr) => {{
        const $c: u32 = $crate::Spec::$v as u32;
        $e
    }};
    ([@match $s:expr, $c:ident, $e:expr] $($v:ident,)*) => {{
        match $s {
            $(
                $crate::Spec::$v => {
                    $crate::to_generic!(@one $v, |$c| $e)
                }
            )*
            #[allow(unreachable_patterns)]
            _ => unreachable!(),
        }
    }};
    ($s:expr, |$c:ident| $e:expr) => {{
        $crate::for_each!([@match $s, $c, $e] $crate::to_generic)
    }};
}

#[macro_export]
macro_rules! for_each {
    ([$($x:tt)*] $($m:tt)+) => {
        $($m)+! {
            [$($x)*]
            A,
            B,
        }
    };
}
";

/// [`DEP_LIB`] with an exhaustive match: no `_ => unreachable!()` arm.
const DEP_LIB_NO_UNREACHABLE: &str = "#[derive(Clone, Copy)]
pub enum Spec {
    A = 1,
    B = 2,
}

pub fn get<const N: u32>() -> u32 {
    N
}

#[macro_export]
macro_rules! to_generic {
    (@one $v:ident, |$c:ident| $e:expr) => {{
        const $c: u32 = $crate::Spec::$v as u32;
        $e
    }};
    ([@match $s:expr, $c:ident, $e:expr] $($v:ident,)*) => {{
        match $s {
            $(
                $crate::Spec::$v => {
                    $crate::to_generic!(@one $v, |$c| $e)
                }
            )*
        }
    }};
    ($s:expr, |$c:ident| $e:expr) => {{
        $crate::for_each!([@match $s, $c, $e] $crate::to_generic)
    }};
}

#[macro_export]
macro_rules! for_each {
    ([$($x:tt)*] $($m:tt)+) => {
        $($m)+! {
            [$($x)*]
            A,
            B,
        }
    };
}
";

/// tempo's `execution_config`: the call is the body's tail.
const APP_LIB: &str = "pub fn pick(s: dep::Spec) -> u32 {
    dep::to_generic!(s, |ID| dep::get::<ID>())
}

#[test]
fn picks_by_spec() {
    assert!(pick(dep::Spec::A) == 1);
    assert!(pick(dep::Spec::B) == 2);
}
";

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

/// `check` whole, then `test` passing, over `dep_lib`.
fn checks_and_runs(name: &str, dep_lib: &str) {
    let files = [("Cargo.toml", WORKSPACE), ("app/Cargo.toml", APP), ("app/src/lib.rs", APP_LIB), ("dep/Cargo.toml", DEP), ("dep/src/lib.rs", dep_lib)];
    fixture(name, &files, |dir, home| {
        let checked = blaze(dir, home, "check", &[]);
        assert!(!checked.says("lowering refused") && !checked.says("not read"), "{}", checked.evidence("`pick`'s body lowers: `unreachable` is read at the call site"));
        assert!(checked.says("0 errors"), "{}", checked.evidence("the check finds no error"));
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(!said.stdout.contains("NOT RUN") && !said.stdout.contains("FAILED"), "{}", said.evidence("the test runs"));
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed"), "{}", said.evidence("the test passes"));
    });
}

/// 1. evm2's shape, `_ => unreachable!()` in the match an expansion wrote.
#[test]
#[cfg(target_os = "macos")]
#[ignore = "class 4 reduction, not yet run: tempo's `unreachable!` 'dependency source not read' (rt-loop a0840 check.out)"]
fn unreachable_in_a_match_an_expansion_wrote_is_read_at_the_call_site() {
    checks_and_runs("check_expansion_match_block", DEP_LIB);
}

/// 2. TWIN: the same chain with no `unreachable!` arm.
#[test]
#[cfg(target_os = "macos")]
#[ignore = "class 4 reduction's twin, not yet run"]
fn twin_the_same_chain_without_unreachable_checks_and_runs() {
    checks_and_runs("check_expansion_match_block_twin", DEP_LIB_NO_UNREACHABLE);
}
