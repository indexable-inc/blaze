//! The compiler a script spawns, answered by blaze inside the sandbox (rule 1: never the real
//! rustc). A script is told `RUSTC=rustc`; the runner's process host hands each spawn here.
//!
//! - `rustc --version`, `-V`, `-vV`, `-Vv` (evm2), `--version --verbose` (either order), `-V -v`: the pinned
//!   nightly's text, `host:` the build's host. Every version probe of tempo's scripts is one of
//!   these.
//! - A try-compile (a snippet given by file or stdin, with `--emit`, `--crate-type` and the
//!   like): read by [`crate::probe_ask::classify`], keyed by what its verdict depends on
//!   ([`crate::probe_ask::Compile::key`]) and answered by [`crate::probe_ask::answer`]: decided
//!   by blaze from the snippet where it can, else by a table of answers for the pinned nightly
//!   ([`Table`]). A probe neither answers is the runtime's `CannotAnswerYet`, which fails that
//!   one script naming the probe.
//! - `cargo -V` (`--version`, `version`): the pinned nightly's cargo line ([`blaze_pins::CARGO_V`]),
//!   a fact of the pin keyed with the rustc text ([`answers_id`]); any other cargo command is
//!   routed like any other program.
//! - Any other program (a C compiler, `git`, `cmake`, `pkg-config`) is routed by the runner
//!   (blaze-proc-macro `spawn_route`): answered by the C mode or refused. A spawn is never
//!   answered with an error the script could swallow: a version probe that fails is taken by
//!   most scripts as "an old compiler" (`.output().ok()?`).

#![blaze::sans_io]

use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, Alloc, String};

use blaze_pins::{CARGO_V, COMMIT_DATE, COMMIT_HASH, RELEASE, RUSTC_LLVM, RUSTC_V};

use crate::probe_ask::{Ask, classify};

// The version answers are the pin's facts (pins/rust.txt and the recorded `rustc -vV` in
// pins/answers/rustc.json, read at compile time by blaze-pins): what `rustc -V` and `cargo -V`
// print, the commit and release, and the `LLVM version:` portable-atomic reads. A script that
// asks the toolchain's cargo for its version (codspeed) is told `CARGO_V`, a fact of the pin,
// never a program run.

/// Whether a spawn of `cargo` with `args` is a version question the pin answers: `-V`,
/// `--version` or the `version` command, alone. `-vV` and `version --verbose` print the
/// host's libgit2, curl and OS lines, which are no fact of the pin: not answered here.
pub fn is_cargo_version(program: &str, args: &[&str]) -> bool {
    let name = program.rsplit('/').next().unwrap_or(program);
    name == "cargo" && matches!(args, ["-V"] | ["--version"] | ["version"])
}

/// The text `cargo -V` prints.
pub fn cargo_version<'a>(alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::from_str_in(CARGO_V, alloc);
    out.push('\n');
    out
}

/// The programs of a C toolchain a script spawns through `cc`, `cmake` or by hand: answered
/// by the C mode ([`crate::key::CMode`]), never refused, never run in `check`.
pub const C_TOOLS: &[&str] = &[
    "ar", "as", "c++", "cc", "clang", "clang++", "cmake", "cpp", "g++", "gcc", "ld", "llvm-ar", "nasm", "pkg-config", "ranlib", "xcrun",
];

/// Whether `program` (a path or a name) is a C toolchain program.
pub fn is_c_tool(program: &str) -> bool {
    let name = program.rsplit('/').next().unwrap_or(program);
    C_TOOLS.contains(&name)
}

/// Native tools other than the C toolchain that make a script's native code or its inputs
/// (`perl` for sha3-asm's assembly, `make` after a `configure`): deferred in `check` like the C
/// toolchain (D53), refused in a build until they run as sandboxed tools.
pub const NATIVE_TOOLS: &[&str] = &["gmake", "make", "perl"];

/// Whether the spawn of `program` with `args` is one `check` defers: a C toolchain program, a
/// [`NATIVE_TOOLS`] one, or a shell running a `configure` script (tikv-jemalloc-sys).
pub fn is_deferred_tool(program: &str, args: &[&str]) -> bool {
    let name = program.rsplit('/').next().unwrap_or(program);
    is_c_tool(name)
        || NATIVE_TOOLS.contains(&name)
        || (matches!(name, "sh" | "bash") && args.first().is_some_and(|script| script.rsplit('/').next() == Some("configure")))
}

/// The text `rustc` prints for a version request.
pub fn version<'a>(verbose: bool, host: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::from_str_in(RUSTC_V, alloc);
    out.push('\n');
    if verbose {
        for (key, value) in [
            ("binary", "rustc"),
            ("commit-hash", COMMIT_HASH),
            ("commit-date", COMMIT_DATE),
            ("host", host),
            ("release", RELEASE),
            ("LLVM version", RUSTC_LLVM),
        ] {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }
    }
    out
}

/// One answer of the table: did the pinned nightly accept the snippet.
#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub key: ContentId,
    pub accepted: bool,
}

/// One try-compile tempo's scripts make, answered for the pinned nightly by reading, with the
/// reason it is true for this compiler. Never by running rustc (rule 1).
#[derive(Clone, Copy, Debug)]
pub struct Known {
    /// The script and the question, for people.
    pub probe: &'static str,
    /// The arguments its verdict depends on, as the script gives them, without the input, the
    /// output paths and `--target`: the row is keyed for the target it is made for
    /// ([`pinned_rows`]), so its reason must hold for any target that ships [`Known::needs`].
    pub args: &'static [&'static str],
    pub snippet: &'static str,
    pub accepted: bool,
    pub why: &'static str,
    /// The toolchain crates the reason rests on (`core`, `std`, `proc_macro`): the row is made
    /// for a target only when its sysroot holds each.
    pub needs: &'static [&'static str],
}

/// The table's rows for `target`, sorted by key: each known probe's key as
/// [`crate::probe_ask::Compile::key`] makes it for a probe of `target` with its arguments.
///
/// Panics when a row's arguments are not a try-compile: they are fixed text, each row read by
/// `pinned_rows_read_as_try_compiles` (tests.rs).
pub fn pinned_rows<'a>(target: &str, known: &[Known], alloc: Alloc<'a>) -> blaze_own::AVec<'a, Row> {
    let mut rows = blaze_own::Vec::with_capacity_in(known.len(), alloc.clone());
    for k in known {
        let mut args = blaze_own::Vec::with_capacity_in(k.args.len() + 1, alloc.clone());
        args.extend_from_slice(k.args);
        args.push("-");
        let key = match classify(&args, alloc.clone()) {
            Ok(Ask::Compile(compile)) => compile.key(target, k.snippet.as_bytes()),
            other => panic!("the pinned probe `{}` is not a try-compile: {other:?}", k.probe),
        };
        rows.push(Row { key, accepted: k.accepted });
    }
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows
}

/// The answers for the pinned nightly, sorted by key: [`pinned_rows`] of the known probes and
/// the [`crate::probe_ask::ships_key`] facts of the side's sysroot. A try-compile neither blaze
/// nor a row answers is the runtime's `CannotAnswerYet`.
#[derive(Clone, Copy, Debug)]
pub struct Table<'t> {
    pub rows: &'t [Row],
}

impl Table<'_> {
    pub const EMPTY: Table<'static> = Table { rows: &[] };

    pub fn answer(&self, key: &ContentId) -> Option<bool> {
        self.rows.binary_search_by(|row| row.key.cmp(key)).ok().map(|at| self.rows[at].accepted)
    }

    /// What the base key holds of the answers: the version text and the rows.
    pub fn id(&self) -> ContentId {
        answers_id(self)
    }
}

/// The identity of everything a spawn can be answered with.
pub fn answers_id(table: &Table<'_>) -> ContentId {
    let mut hasher = Hasher::new("blaze v2 build-script answers 3");
    hasher.part(RUSTC_V.as_bytes()).part(COMMIT_HASH.as_bytes()).part(RUSTC_LLVM.as_bytes()).part(CARGO_V.as_bytes());
    hasher.part(crate::probe_ask::DECIDER.as_bytes());
    for row in table.rows {
        hasher.id(&row.key).update(&[u8::from(row.accepted)]);
    }
    hasher.finish()
}
