//! The lints of code blaze cannot analyse (`proc_macro_derive`, `proc_macro_attribute`,
//! `proc_macro_fn_like`, `build_script_ambient`) over fixtures of their own: a proc-macro crate
//! with one macro of each kind, and a build script over a `std` whose `fs`, `env`, `process`
//! and `time` have what the lint knows. The crate linted is the one at `/src/lib.rs`; its kind
//! is its `crate:` line's (`proc-macro:`, `custom-build:`), as a build gives it.

use blaze_diagnostics::lints::registry::ready;

use super::Lint;
use crate::{
    level::{self, CommandLine, Level, Levels},
    tests::{CLIPPY_OFF, lint},
};

/// The findings of these lints in `src/lib.rs` of `out`, as (line, message), in the order of
/// the lines.
fn found(out: &str) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    let mut message = None;
    for line in out.lines() {
        if let Some(it) = line.strip_prefix("error: ").or_else(|| line.strip_prefix("warning: ")) {
            let ours = it.ends_with("is a procedural macro, which blaze cannot see through")
                || it.contains(" in this build script, ");
            message = ours.then(|| it.to_owned());
            continue;
        }
        let Some(at) = line.trim_start().strip_prefix("--> src/lib.rs:") else { continue };
        let Some(message) = message.take() else { continue };
        let number = at.split(':').next().and_then(|it| it.parse().ok());
        found.push((number.expect("a line number after the path"), message));
    }
    found.sort();
    found
}

/// Every lint off but those `flags` turn on, so a fixture's findings are these lints' alone.
fn flags(flags: &str) -> String {
    format!("{CLIPPY_OFF} -A blaze::general {flags}")
}

/// A proc-macro crate with one macro of each kind and a function that is none. The first line
/// of the source is line 1 of `src/lib.rs`.
const MACROS: &str = r#"pub struct TokenStream;
#[proc_macro_derive(Describe, attributes(describe))]
pub fn derive_describe(input: TokenStream) -> TokenStream {
    helper(input)
}
#[proc_macro_attribute]
pub fn route(_args: TokenStream, item: TokenStream) -> TokenStream {
    item
}
#[proc_macro]
pub fn sql(input: TokenStream) -> TokenStream {
    input
}
fn helper(input: TokenStream) -> TokenStream {
    input
}
"#;

fn macros_as(krate: &str, source: &str) -> String {
    format!("//- minicore: sized\n//- /src/lib.rs {krate}\n{source}")
}

fn cannot_see(kind: &str, name: &str) -> String {
    format!("the {kind} `{name}` is a procedural macro, which blaze cannot see through")
}

/// One finding per macro a proc-macro crate defines, at the macro's function, naming the macro
/// by the name it is called by (a derive's is its attribute's, not its function's) and its
/// kind, each with the replacement of its kind; a function that is no macro has none.
#[test]
fn each_proc_macro_a_crate_defines_is_found_with_its_kinds_replacement() {
    let out =
        lint(&macros_as("crate:proc-macro:main", MACROS), &flags("--warn=blaze::proc_macro"));
    let expected: Vec<(u32, String)> = vec![
        (3, cannot_see("derive macro", "Describe")),
        (7, cannot_see("attribute macro", "route")),
        (11, cannot_see("function-like macro", "sql")),
    ];
    assert_eq!(found(&out), expected, "{out}");
    let opaque = "a procedural macro is opaque code that runs at build time and rewrites tokens";
    assert_eq!(out.matches(opaque).count(), 3, "every finding says why:\n{out}");
    for lint in super::PROC_MACRO {
        assert!(out.contains(lint.help()), "`{}`'s help is said:\n{out}", lint.name());
    }
    // What each kind is told to write instead, and where the plan is.
    assert!(out.contains("`for const f in fields_of::<T>()`"), "a derive's:\n{out}");
    assert!(out.contains("`fn_info(&f)`"), "an attribute macro's:\n{out}");
    assert!(out.contains("a `const fn` or a `const { .. }` block"), "a function-like's:\n{out}");
    assert!(
        out.contains("a macro that parses a foreign syntax (a DSL in a string or in tokens) is not replaced by reflection and may stay"),
        "a function-like macro that parses a DSL may stay:\n{out}"
    );
    assert_eq!(out.matches("docs/v2/plan/design/comptime.md").count(), 3, "{out}");
    assert!(out.contains("lints.md#proc_macro"), "{out}");
}

/// The text never sends anyone to a feature that is not there: until reflection replaces a
/// derive and an attribute macro, their help says it is planned and that nothing changes yet.
#[test]
fn a_replacement_that_is_not_in_the_tree_is_called_planned() {
    let planned = "nothing to change yet";
    for lint in [Lint::ProcMacroDerive, Lint::ProcMacroAttribute] {
        assert_eq!(
            lint.help().contains(planned),
            !ready::REFLECTION_REPLACES_MACROS,
            "{}",
            lint.name()
        );
        assert_eq!(
            lint.help().contains("not complete in this blaze"),
            !ready::REFLECTION_REPLACES_MACROS,
            "{}",
            lint.name()
        );
        assert_eq!(lint.warns_by_default(), ready::REFLECTION_REPLACES_MACROS, "{}", lint.name());
    }
    assert_eq!(
        Lint::BuildScriptAmbient.help().contains(planned),
        !ready::CAPABILITIES_FOR_BUILD_SCRIPTS
    );
    assert_eq!(
        Lint::BuildScriptAmbient.warns_by_default(),
        ready::CAPABILITIES_FOR_BUILD_SCRIPTS
    );
    // `const fn` is Rust's: a function-like macro's replacement is there today.
    assert!(!Lint::ProcMacroFnLike.help().contains(planned));
    assert!(Lint::ProcMacroFnLike.warns_by_default());
}

/// Every edition lint says whether it warns by default, so a new one chooses.
#[test]
fn every_edition_lint_says_whether_its_advice_is_ready() {
    for &lint in Lint::ALL {
        let listed = super::EDITION_READY.iter().any(|(it, _)| *it == lint);
        assert_eq!(listed, lint.kind() == super::Kind::Edition, "{}", lint.name());
    }
}

/// With no level set, a kind is found exactly when its replacement is in this blaze.
#[test]
fn proc_macros_are_found_by_default_once_their_replacement_exists() {
    let out = lint(&macros_as("crate:proc-macro:main", MACROS), &flags(""));
    let all: [(Lint, u32, String); 3] = [
        (Lint::ProcMacroDerive, 3, cannot_see("derive macro", "Describe")),
        (Lint::ProcMacroAttribute, 7, cannot_see("attribute macro", "route")),
        (Lint::ProcMacroFnLike, 11, cannot_see("function-like macro", "sql")),
    ];
    let expected: Vec<(u32, String)> = all
        .into_iter()
        .filter(|(lint, ..)| lint.warns_by_default())
        .map(|(_, line, message)| (line, message))
        .collect();
    assert_eq!(found(&out), expected, "{out}");
    assert!(out.contains("`#[warn(blaze::proc_macro_fn_like)]` on by default"), "{out}");
}

/// The same functions in a crate that is no proc-macro crate are no macros.
#[test]
fn a_crate_that_is_no_proc_macro_crate_has_no_finding() {
    let out = lint(&macros_as("crate:main", MACROS), &flags("--warn=blaze::proc_macro"));
    assert!(found(&out).is_empty(), "{out}");
}

/// A dependency's proc-macro crate is not the workspace's to rewrite: its lints are capped at
/// `allow` (`--cap-lints allow`, what a registry or git crate is given), and a workspace crate
/// that depends on it and re-exports its macros defines none.
#[test]
fn a_dependencys_proc_macro_crate_has_no_finding() {
    let capped = lint(
        &macros_as("crate:proc-macro:main library", MACROS),
        &flags("--warn=blaze::proc_macro --cap-lints allow"),
    );
    assert!(found(&capped).is_empty(), "{capped}");
    let user = format!(
        "//- minicore: sized\n\
         //- /src/lib.rs crate:main deps:macros\n\
         pub use macros::{{Describe, route, sql}};\n\
         //- /macros/lib.rs crate:proc-macro:macros library\n\
         {MACROS}"
    );
    let out = lint(&user, &flags("--warn=blaze::proc_macro"));
    assert!(found(&out).is_empty(), "{out}");
}

/// A use of a macro another crate defines (`#[derive(..)]`) is not a finding: only definitions
/// are, since a workspace cannot rewrite the crate that defines it.
#[test]
fn a_use_of_another_crates_proc_macro_has_no_finding() {
    let user = "//- proc_macros: derive_identity\n\
                //- minicore: sized\n\
                //- /src/lib.rs crate:main\n\
                #[derive(proc_macros::DeriveIdentity)]\n\
                pub struct Row {\n    pub id: u32,\n}\n";
    let out = lint(user, &flags("--warn=blaze::proc_macro"));
    assert!(found(&out).is_empty(), "{out}");
}

/// `#![allow(blaze::proc_macro)]` is the group's name for all three.
#[test]
fn the_group_allows_every_kind() {
    let source = format!("#![allow(blaze::proc_macro)]\n{MACROS}");
    let out = lint(&macros_as("crate:proc-macro:main", &source), &flags("--warn=blaze::edition"));
    assert!(found(&out).is_empty(), "{out}");
}

/// A `std` with what `build_script_ambient` knows, and a `println!` that calls into `io`.
const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod fs {
    pub struct File(u32);
    impl File {
        pub fn open(_path: &str) -> File {
            loop {}
        }
    }
    pub fn read_to_string(_path: &str) -> u32 {
        loop {}
    }
}
pub mod env {
    pub fn var(_key: &str) -> u32 {
        loop {}
    }
    pub fn split_paths(_paths: &str) -> u32 {
        loop {}
    }
}
pub mod process {
    pub struct Command(u32);
    impl Command {
        pub fn new(_program: &str) -> Command {
            loop {}
        }
        pub fn status(&mut self) -> u32 {
            loop {}
        }
    }
}
pub mod time {
    pub struct Instant(u64);
    impl Instant {
        pub fn now() -> Instant {
            loop {}
        }
    }
}
pub mod io {
    pub fn _print(_line: &str) {}
}
#[macro_export]
macro_rules! println {
    ($line:literal) => {
        $crate::io::_print($line)
    };
}
"#;

/// A build script: the file system by a full path, the environment through a `use` and
/// through an alias of the module, a process through a `use` of the type, a function of `env`
/// that only computes, and a cargo directive. The first line is line 1 of `src/lib.rs`.
const SCRIPT: &str = r#"use std::env as environment;
use std::env::var;
use std::process::Command;
fn main() {
    let _manifest = std::fs::read_to_string("Cargo.toml");
    let _out = var("OUT_DIR");
    let _target = environment::var("TARGET");
    let mut cc = Command::new("cc");
    let _status = cc.status();
    let _paths = environment::split_paths("a:b");
    std::println!("cargo::rerun-if-changed=build.rs");
}
fn probe() {
    let _source = std::fs::read_to_string("probe.c");
}
"#;

fn script_as(krate: &str, source: &str) -> String {
    format!("//- minicore: sized\n//- /src/lib.rs {krate} deps:std\n{source}{STD}")
}

fn script_findings() -> Vec<(u32, String)> {
    vec![
        (
            5,
            "`std::fs::read_to_string`, used 2 times in this build script, reaches the file system by itself"
                .to_owned(),
        ),
        (
            6,
            "`std::env::var`, used 2 times in this build script, reads or changes the environment by itself"
                .to_owned(),
        ),
        (
            8,
            "`std::process::Command`, used 2 times in this build script, starts or ends a process by itself"
                .to_owned(),
        ),
    ]
}

/// One finding per item of std's ambient authority a build script uses, at its first use, with
/// the count of its uses in every body: a path through a `use` and one through an alias are the
/// same item, and a method called on a value is a use of its type (`Command::new` and
/// `cc.status()` are two of `Command`); a function that only computes and the `println!` of a
/// cargo directive are none. Each names the capability that stands for it.
#[test]
fn a_build_scripts_ambient_uses_are_found_once_each_with_their_count() {
    let out = lint(
        &script_as("crate:custom-build:main", SCRIPT),
        &flags("--warn=blaze::build_script_ambient"),
    );
    assert_eq!(found(&out), script_findings(), "{out}");
    let why = "blaze must run it in a sandbox, trace what it read, and rerun it whenever anything it might have read changes";
    assert_eq!(out.matches(why).count(), 3, "every finding says why:\n{out}");
    for capability in ["`Fs` (a `Dir` of it", "`Env` (`Args` for", "`Process` (`Runner`, to run"] {
        assert!(out.contains(capability), "{capability} is named:\n{out}");
    }
    assert_eq!(out.matches(Lint::BuildScriptAmbient.help()).count(), 3, "{out}");
    assert_eq!(out.matches("docs/v2/plan/design/std.md").count(), 3, "{out}");
    assert_eq!(
        out.contains("which is planned and not in this blaze"),
        !ready::CAPABILITIES_FOR_BUILD_SCRIPTS,
        "the note says whether the capability can be taken today:\n{out}"
    );
    assert!(!out.contains("_print") && !out.contains("split_paths"), "{out}");
}

/// The same code in a crate that is no build script is not this lint's.
#[test]
fn the_same_code_outside_a_build_script_has_no_finding() {
    for krate in ["crate:main", "crate:bin:main"] {
        let out = lint(&script_as(krate, SCRIPT), &flags("--warn=blaze::build_script_ambient"));
        assert!(found(&out).is_empty(), "{krate}:\n{out}");
    }
}

/// `#![allow(blaze::build_script_ambient)]` at the script's root silences it.
#[test]
fn an_allow_at_the_scripts_root_silences_it() {
    let source = format!("#![allow(blaze::build_script_ambient)]\n{SCRIPT}");
    let out = lint(
        &script_as("crate:custom-build:main", &source),
        &flags("--warn=blaze::build_script_ambient"),
    );
    assert!(found(&out).is_empty(), "{out}");
}

/// With no level set, a build script is found exactly when it can take capabilities instead;
/// a dependency's (`--cap-lints allow`) never.
#[test]
fn a_build_script_is_found_by_default_once_capabilities_exist() {
    let out = lint(&script_as("crate:custom-build:main", SCRIPT), &flags(""));
    let expected = match ready::CAPABILITIES_FOR_BUILD_SCRIPTS {
        true => script_findings(),
        false => Vec::new(),
    };
    assert_eq!(found(&out), expected, "{out}");
    let capped = lint(
        &script_as("crate:custom-build:main library", SCRIPT),
        &flags("--warn=blaze::build_script_ambient --cap-lints allow"),
    );
    assert!(found(&capped).is_empty(), "{capped}");
}

/// Linting the same script twice writes the same bytes (`deterministic-output`): the findings'
/// order and counts come from the order of the script's bodies, never from a hash map's.
#[test]
fn a_build_scripts_findings_are_the_same_each_run() {
    let fixture = script_as("crate:custom-build:main", SCRIPT);
    let flags = flags("--warn=blaze::build_script_ambient");
    assert_eq!(lint(&fixture, &flags), lint(&fixture, &flags));
}

/// The default level docs/lints.md's table gives each of these lints is the one it has.
#[test]
fn the_docs_table_has_each_lints_default_level() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/lints.md");
    let docs = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{path}: {error}"));
    let levels = Levels::new(&CommandLine::default());
    for lint in [
        Lint::ProcMacroDerive,
        Lint::ProcMacroAttribute,
        Lint::ProcMacroFnLike,
        Lint::BuildScriptAmbient,
        Lint::StdUse,
    ] {
        let row = format!("| `blaze::{}` | `", lint.name());
        let said = docs
            .lines()
            .find_map(|line| line.strip_prefix(row.as_str()))
            .and_then(|rest| rest.split('`').next())
            .unwrap_or_else(|| panic!("docs/lints.md has no row starting `{row}`"));
        let has = levels.of(level::Lint::Blaze(lint), &[]).level;
        assert_eq!(Level::of_attr(said), Some(has), "`blaze::{}` in docs/lints.md", lint.name());
    }
}
