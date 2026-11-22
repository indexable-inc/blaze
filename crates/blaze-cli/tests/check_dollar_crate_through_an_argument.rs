//! A `$crate` a macro passes as ANOTHER macro's argument names the crate of the macro that
//! WROTE it, wherever the inner macro re-emits it (tempo, 2026-11-17 g105938: 27 "no `assert`
//! in `crate`" in the antithesis members: antithesis_sdk 0.2's `assert_sometimes!` writes
//! `$crate::assert_helper!(.., $crate::assert::AssertType::Sometimes, ..)`, captured by
//! `$assert_type:path` and written into a block's `static` and its call; and one "no
//! `__bitflags_expr_safe_attrs` in `crate`").
//!
//! The Reference, "Macros By Example", "Hygiene": "`$crate` [..] refers to the crate that
//! defines the macro"; a token keeps the context it was written in when another macro's
//! transcriber substitutes it from a fragment (`$p:path`, `$t:tt`, `$e:expr`).
//!
//! The cause (blaze-scope resolve.rs): a call written in a text the names stage REWRITES from
//! an expansion (a statement call's statements read as items, expanded_blocks.rs
//! `statements_as_items`; a block's module of a body a macro wrote, `make_blocks`) took its
//! input as that text lexed again, where another crate's `$crate` is the keyword `crate`:
//! antithesis' block `#[$crate::linkme::distributed_slice($crate::assert::ANTITHESIS_CATALOG)]
//! static ..` gave linkme a bare `crate`, whose output `crate::assert::ANTITHESIS_CATALOG! {..}`
//! was looked for in the calling crate. The fix moves the expansion's tokens with the slices
//! the text copies (nested_input.rs `ExpansionTokens::moved`).
//!
//! The workspace: `dep` (`m::X`, `m::C`, `m::Info`; `outer_*!` calling `inner_*!` with a
//! `$crate::m::..` argument; `sometimes!` calling `helper!`, the antithesis shape: a block of
//! a `use`, a `static` and a statement call `$crate::call_it!($crate::leaf)` whose argument is
//! a macro path; `body_call!` writing a fn whose body holds that call; `flags!` the bitflags
//! shape, a body passed through `wrap_fn!`'s argument), `app` (on `dep`) calling each at item
//! level and inside a fn body. 1. Nothing unresolved, `check` passes. Before the fix: "no
//! `leaf` in `crate`" for `three` and `g`. 2. CONTROL: `app` calls `dep::missing_call!()` at
//! item level and in `h`'s body, whose `call_it!` argument is `$crate::m::nowhere`, which
//! `dep` lacks: two "no `nowhere` in" lines, and never "no `m` in" (that would be `m` looked
//! for in `app`).
//!
//! Expected line: `DOLLAR CRATE THROUGH AN ARGUMENT: found <status> (<n> not checked), missing
//! <not 0>`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).

mod support;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// How long `blaze check` of the two-package workspace may take; a hang is a bug (rule 18).
const DEADLINE: Duration = Duration::from_secs(300);

/// How long the child killed at [`DEADLINE`] may take to be reaped.
const REAP: Duration = Duration::from_secs(10);

const DEP: &str = "pub mod m {
    pub struct X;
    pub const C: u32 = 1;
    pub struct Info {
        pub t: u32,
    }
}

#[macro_export]
macro_rules! inner_path {
    ($p:path) => {
        const _: $p = $p;
    };
}

#[macro_export]
macro_rules! outer_path {
    () => {
        $crate::inner_path!($crate::m::X);
    };
}

#[macro_export]
macro_rules! inner_tt {
    ($($t:tt)*) => {
        const _: u32 = $($t)*;
    };
}

#[macro_export]
macro_rules! outer_tt {
    () => {
        $crate::inner_tt!($crate::m::C);
    };
}

#[macro_export]
macro_rules! inner_expr {
    ($e:expr) => {
        const _: u32 = $e;
    };
}

#[macro_export]
macro_rules! outer_expr {
    () => {
        $crate::inner_expr!($crate::m::C);
    };
}

#[macro_export]
macro_rules! leaf {
    () => {
        struct Leaf;
    };
}

#[macro_export]
macro_rules! call_it {
    ($($t:tt)*) => {
        $($t)*!();
    };
}

#[macro_export]
macro_rules! helper {
    (condition = $e:expr, $p:path) => {{
        let c: u32 = $e;
        use $crate::m::Info;
        static S: Info = Info { t: $p };
        $crate::call_it!($crate::leaf);
        c + $p
    }};
}

#[macro_export]
macro_rules! body_call {
    () => {
        fn g() -> u32 {
            $crate::call_it!($crate::leaf);
            1
        }
    };
}

#[macro_export]
macro_rules! sometimes {
    ($e:expr) => {
        $crate::helper!(condition = $e, $crate::m::C)
    };
}

#[macro_export]
macro_rules! stmt_inner {
    ($b:block) => {
        $crate::stmt_inner! { done: $b }
    };
    (done: $b:block) => {
        $b
    };
}

#[macro_export]
macro_rules! wrap_fn {
    ($($body:tt)*) => {
        pub fn all() -> u32 {
            $($body)*
        }
    };
}

#[macro_export]
macro_rules! flags {
    () => {
        $crate::wrap_fn! {
            const ALL: u32 = {
                let mut t = 0;
                $crate::stmt_inner!({
                    t |= $crate::m::C;
                });
                t
            };
            ALL
        }
    };
}

#[macro_export]
macro_rules! missing_call {
    () => {
        $crate::call_it!($crate::m::nowhere);
    };
}
";

const MAIN_FOUND: &str = "dep::outer_path!();
dep::outer_tt!();
dep::outer_expr!();
dep::flags!();
dep::body_call!();

fn three() -> u32 {
    dep::outer_path!();
    dep::outer_tt!();
    dep::outer_expr!();
    dep::sometimes!(1);
    let n: u32 = dep::sometimes!(1);
    n + all() + g()
}

fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    if three() == 4 {
        out.line(cx, \"x\")?;
    }
    Ok(())
}
";

const MAIN_MISSING: &str = "dep::missing_call!();

fn h() {
    dep::missing_call!();
}

fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    h();
    out.line(cx, \"x\")?;
    Ok(())
}
";

const NOT_CHECKED: &str = "not checked because";
/// The bug's words: a `dep` path looked for in `app`.
const IN_APP: &str = "no `m` in";
const MISSING: &str = "no `nowhere` in";

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| panic!("making {}: {error}", dir.display()));
    }
    std::fs::write(path, text).unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
}

/// The workspace under `base/ws`, `app`'s `src/main.rs` being `main`.
fn workspace(base: &Path, main: &str) -> std::path::PathBuf {
    let ws = base.join("ws");
    write(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"2\"\n");
    write(
        &ws.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"dep\"]\n\n[[package]]\nname = \"dep\"\nversion = \"0.1.0\"\n",
    );
    write(
        &ws.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
    );
    write(&ws.join("app/src/main.rs"), main);
    write(&ws.join("dep/Cargo.toml"), "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n");
    write(&ws.join("dep/src/lib.rs"), DEP);
    ws
}

/// `blaze check` in `ws`: its status and everything it said, kept as `<base>/<what>.said`.
fn check(base: &Path, ws: &Path, what: &str) -> (Option<i32>, String) {
    let home = base.join("home");
    let said = base.join(format!("{what}.said"));
    let file = File::create(&said).unwrap_or_else(|error| panic!("making {}: {error}", said.display()));
    let file_err = file.try_clone().expect("the output file, twice");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("check")
        .current_dir(ws)
        .env("HOME", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", ws)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(file_err))
        .spawn()
        .expect("blaze starts");
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

/// Should print `DOLLAR CRATE THROUGH AN ARGUMENT: found <status>, missing <not 0>`; on
/// failure, the assertion and all `check` said.
#[test]
fn a_dollar_crate_passed_as_an_argument_names_the_crate_that_wrote_it() {
    let base = fresh_home("check_dollar_crate_through_an_argument", true);
    let ws = workspace(&base, MAIN_FOUND);
    let found = check(&base, &ws, "found");
    write(&ws.join("app/src/main.rs"), MAIN_MISSING);
    let missing = check(&base, &ws, "missing");
    let _ = std::fs::remove_dir_all(&base);

    let (status, said) = &found;
    assert!(!said.contains(IN_APP), "found: a `$crate::m` argument was looked for in `app`, status {status:?}:\n{said}");
    assert!(!said.contains("unresolved"), "found: a name is unresolved, status {status:?}:\n{said}");
    assert!(said.contains("0 errors: blaze check"), "found: status {status:?}:\n{said}");
    // What the bodies' stage says is not this test's (a body holding `const _` items in a
    // block is "not checked" today: a head the store could not make); it is printed, named.
    let unchecked = said.lines().filter(|line| line.contains(NOT_CHECKED)).count();

    let (status, said) = &missing;
    assert!(status.is_some() && *status != Some(0), "missing: `$crate::m::nowhere` names nothing, status {status:?}:\n{said}");
    assert_eq!(said.lines().filter(|line| line.contains(MISSING)).count(), 2, "missing: \"no `nowhere` in\" at item level and in `h`'s body:\n{said}");
    assert!(!said.contains(IN_APP), "missing: `m` was looked for in `app`:\n{said}");

    println!("DOLLAR CRATE THROUGH AN ARGUMENT: found {} ({unchecked} not checked), missing {}", found.0.unwrap_or(-1), missing.0.unwrap_or(-1));
}
