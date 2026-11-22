//! A glob `use` a macro of ANOTHER crate writes in the block it expands to, `use $crate::m::*;`,
//! is read at the MACRO's crate, reached through a facade's re-export, and the trait it brings
//! answers the next statement's method call (port27/mini/clap-root, 2026-11-17: clap_builder
//! 4.6.7's `value_parser!` writes `{ use $crate::builder::impl_prelude::*; let auto = ..;
//! (&&&&&&auto).value_parser() }`, called as `clap::value_parser!`; every clap derive's method
//! body was refused "a glob use in a block whose path does not resolve": the glob's `$crate`
//! was read as the CALLING crate, `app::builder`).
//!
//! The Reference, "Macros By Example", "Hygiene": "By default, all identifiers referred to in
//! a macro expansion are looked up as if they appear in the definition of the macro [..]
//! `$crate` [..] refers to the crate that defines the macro". "Use declarations": "Use
//! declarations may appear in modules and blocks". The let pattern after the `use` is what
//! asked: an identifier pattern is first looked up through the block's `use`s (blaze-ladder
//! front2 glob_uses.rs `by_uses`), which read the glob's own path.
//!
//! The workspace: `dep` (`m::Twice`, implemented for `u32`; `#[macro_export] twice_of!` and
//! `missing_of!`), `facade` (`pub use dep::twice_of; pub use dep::missing_of;`), `app` (on
//! `facade` only). 1. `main` calls `facade::twice_of!(21)`: `check` passes, nothing refused,
//! no body not checked. 2. CONTROL: `main` calls `facade::missing_of!(1)`, whose glob names
//! `$crate::nowhere`, which `dep` does not have: refused by that form, naming `main`. Before
//! the fix (glob_uses.rs `Start::dollar`, the marks of a block `use`'s words through
//! `BlockUse::range`) 1 printed "not checked because lowering refused (Head: Item
//! Unsupported("a glob use in a block whose path does not resolve") ..): `main`".
//!
//! Expected line: `BLOCK GLOB DOLLAR CRATE: found 3, missing <not 0>` (3: the borrows' notes
//! leave every check incomplete today, main.rs `BORROWS_ARE_ERRORS`).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).

mod support;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// How long `blaze check` of the three-package workspace may take; a hang is a bug (rule 18).
const DEADLINE: Duration = Duration::from_secs(300);

/// How long the child killed at [`DEADLINE`] may take to be reaped.
const REAP: Duration = Duration::from_secs(10);

const DEP: &str = "pub mod m {
    pub trait Twice {
        fn twice(&self) -> u32;
    }
    impl Twice for u32 {
        fn twice(&self) -> u32 {
            *self * 2
        }
    }
}

#[macro_export]
macro_rules! twice_of {
    ($e:expr) => {{
        use $crate::m::*;
        let v: u32 = $e;
        v.twice()
    }};
}

#[macro_export]
macro_rules! missing_of {
    ($e:expr) => {{
        use $crate::nowhere::*;
        let v: u32 = $e;
        v
    }};
}
";
const FACADE: &str = "pub use dep::twice_of;\npub use dep::missing_of;\n";
const MAIN_FOUND: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {\n    let n: u32 = facade::twice_of!(21);\n    if n == 42 {\n        out.line(cx, \"x\")?;\n    }\n    Ok(())\n}\n";
const MAIN_MISSING: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {\n    let n: u32 = facade::missing_of!(1);\n    if n == 1 {\n        out.line(cx, \"x\")?;\n    }\n    Ok(())\n}\n";

const NOT_CHECKED: &str = "not checked because";
const REFUSED: &str = "a glob use in a block whose path does not resolve";

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| panic!("making {}: {error}", dir.display()));
    }
    std::fs::write(path, text).unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
}

/// The workspace under `base/ws`, `app`'s `main` being `main`.
fn workspace(base: &Path, main: &str) -> std::path::PathBuf {
    let ws = base.join("ws");
    write(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"app\", \"facade\", \"dep\"]\nresolver = \"2\"\n");
    write(
        &ws.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"facade\"]\n\n[[package]]\nname = \"dep\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"facade\"\nversion = \"0.1.0\"\ndependencies = [\"dep\"]\n",
    );
    write(
        &ws.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\nfacade = { path = \"../facade\" }\n",
    );
    write(&ws.join("app/src/main.rs"), main);
    write(
        &ws.join("facade/Cargo.toml"),
        "[package]\nname = \"facade\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
    );
    write(&ws.join("facade/src/lib.rs"), FACADE);
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

/// Should print `BLOCK GLOB DOLLAR CRATE: found 3, missing <not 0>`; on failure, the assertion
/// and all `check` said.
#[test]
fn a_block_glob_a_dependency_s_macro_writes_is_read_at_the_macro_s_crate() {
    let base = fresh_home("check_block_glob_dollar_crate", true);
    let ws = workspace(&base, MAIN_FOUND);
    let found = check(&base, &ws, "found");
    write(&ws.join("app/src/main.rs"), MAIN_MISSING);
    let missing = check(&base, &ws, "missing");
    let _ = std::fs::remove_dir_all(&base);

    let (status, said) = &found;
    assert!(!said.contains(REFUSED), "found: the glob `$crate::m::*` was read at the calling crate:\n{said}");
    assert!(!said.contains(NOT_CHECKED), "found: a body was not checked:\n{said}");
    // The types are checked whole; the borrows' findings stay notes (main.rs
    // `BORROWS_ARE_ERRORS`), so the status is the incomplete check's (3), never an error's.
    assert!(said.contains("0 errors: blaze check") && !said.contains("NOT checked: types"), "found: status {status:?}:\n{said}");

    let (status, said) = &missing;
    assert!(status.is_some() && *status != Some(0), "missing: `$crate::nowhere` names nothing, status {status:?}:\n{said}");
    assert!(said.lines().any(|line| line.contains(REFUSED) && line.contains("`main`")), "missing: refused by that form, naming `main`:\n{said}");

    println!("BLOCK GLOB DOLLAR CRATE: found {}, missing {}", found.0.unwrap_or(-1), missing.0.unwrap_or(-1));
}
