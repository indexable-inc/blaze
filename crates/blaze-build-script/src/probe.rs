//! The compiler a script spawns, answered by blaze inside the sandbox (rule 1: never the real
//! rustc). A script is told `RUSTC=rustc`; the runner's process host hands each spawn here.
//!
//! - `rustc --version`, `-V`, `-vV`, `-Vv` (evm2), `--version --verbose` (either order), `-V -v`: the pinned
//!   nightly's text, `host:` the build's host. Every version probe of tempo's scripts is one of
//!   these.
//! - A try-compile (a snippet given by file or stdin, with `--emit`, `--crate-type` and the
//!   like): [`Request::Compile`], keyed by its arguments and the snippet's bytes
//!   ([`compile_key`]); answered by blaze's front end once it can, until then by a table of
//!   answers for the pinned nightly ([`Table`]). A probe neither answers is
//!   [`Refused::CannotAnswerYet`], which fails that one script naming the probe.
//! - Any other program (a C compiler, `git`, `cmake`, `pkg-config`) is [`Request::Other`]:
//!   the runner answers it by the C mode or refuses it. A spawn is never answered with an
//!   error the script could swallow: a version probe that fails is taken by most scripts as
//!   "an old compiler" (`.output().ok()?`).

#![blaze::sans_io]

use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, Alloc, String};

/// What `rustc -V` of the pinned nightly prints (crates/blaze-cargo/src/toolchain/pins.txt's
/// `rustc` line; a test holds them equal).
pub const VERSION: &str = "rustc 1.101.0-nightly (16d1ffac1 2026-10-26)";
pub const COMMIT_HASH: &str = "16d1ffac17ad8b739e32483ac2e65fe7e1804977";
pub const COMMIT_DATE: &str = "2026-10-26";
pub const RELEASE: &str = "1.101.0-nightly";
/// `LLVM version:` of `rustc -vV` of nightly-2026-10-27 (read once from the pinned toolchain
/// on 2026-11-16; the pin bump records it from then on). portable-atomic reads it.
pub const LLVM: &str = "23.1.4";

/// What a spawn asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request<'s> {
    /// `rustc` asked its version; `verbose` for `-vV`.
    Version { verbose: bool },
    /// `rustc` asked to compile a snippet: the arguments as given.
    Compile,
    /// `rustc --print ...`: not answered yet ([`Refused::RustcArgs`]).
    Print,
    /// Another program.
    Other { program: &'s str },
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

/// Why a spawn is not answered: the script stops, naming the package and this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused<'a> {
    /// A try-compile no answer exists for yet: its key, the snippet's first line.
    CannotAnswerYet { key: ContentId, first_line: AString<'a> },
    /// `rustc` with arguments that are neither a version nor a compile (`--print`, say).
    RustcArgs { args: AString<'a> },
    /// A program the sandbox does not answer.
    Program { program: AString<'a> },
}

/// Reads a spawn: `program` and its arguments.
pub fn request<'s>(program: &'s str, args: &[&str]) -> Request<'s> {
    if program != "rustc" {
        return Request::Other { program };
    }
    let has = |want: &str| args.contains(&want);
    if args.iter().any(|a| *a == "--print" || a.starts_with("--print=")) {
        return Request::Print;
    }
    let only_version = args.iter().all(|a| matches!(*a, "--version" | "-V" | "-vV" | "-Vv" | "--verbose" | "-v"));
    if only_version && !args.is_empty() {
        let verbose = has("-vV") || has("-Vv") || has("--verbose") || has("-v");
        return Request::Version { verbose };
    }
    Request::Compile
}

/// The text `rustc` prints for a version request.
pub fn version<'a>(verbose: bool, host: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::from_str_in(VERSION, alloc);
    out.push('\n');
    if verbose {
        for (key, value) in [
            ("binary", "rustc"),
            ("commit-hash", COMMIT_HASH),
            ("commit-date", COMMIT_DATE),
            ("host", host),
            ("release", RELEASE),
            ("LLVM version", LLVM),
        ] {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(value);
            out.push('\n');
        }
    }
    out
}

/// The flags of `rustc` that take their value as the next argument.
const VALUED: &[&str] = &[
    "--out-dir", "-o", "--emit", "--crate-name", "--crate-type", "--edition", "--cfg", "--check-cfg", "--target", "-L", "-l", "--extern", "-C",
    "-Z", "-A", "-W", "-D", "-F", "--cap-lints", "--error-format", "--json", "--color", "--sysroot", "--diagnostic-width", "--explain",
    "--remap-path-prefix",
];

/// The flags that say only where and how the output goes, never whether the snippet is
/// accepted: left out of a probe's key, so the configuration's `OUT_DIR` and autocfg's
/// numbered crate names do not split one question into many.
const OUTPUT_ONLY: &[&str] = &["--out-dir", "-o", "--emit", "--crate-name", "--error-format", "--json", "--color", "--diagnostic-width"];

/// Where a try-compile's snippet comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Snippet<'s> {
    /// `-`: the bytes written to its stdin.
    Stdin,
    /// A path: the file's bytes (in `OUT_DIR` or the package).
    File(&'s str),
}

/// The snippet argument of a try-compile: its index and where it comes from. `None` when the
/// arguments name no input.
pub fn snippet<'s>(args: &[&'s str]) -> Option<(usize, Snippet<'s>)> {
    let mut found = None;
    let mut at = 0;
    while at < args.len() {
        let arg = args[at];
        if VALUED.contains(&arg) {
            at += 2;
            continue;
        }
        if arg == "-" {
            found = Some((at, Snippet::Stdin));
        } else if !arg.starts_with('-') {
            found = Some((at, Snippet::File(arg)));
        }
        at += 1;
    }
    found
}

/// The arguments a probe's key holds, in order: every one but the snippet and the output-only
/// flags with their values.
pub fn kept<'s>(args: &[&'s str], snippet_arg: Option<usize>, mut each: impl FnMut(&'s str)) {
    let mut at = 0;
    while at < args.len() {
        let arg = args[at];
        let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
        let dropped = OUTPUT_ONLY.contains(&flag);
        let joined = arg.contains('=') && arg.starts_with("--");
        if dropped {
            at += if joined || !VALUED.contains(&arg) { 1 } else { 2 };
            continue;
        }
        if Some(at) != snippet_arg {
            each(arg);
        }
        if VALUED.contains(&arg) && at + 1 < args.len() {
            each(args[at + 1]);
            at += 1;
        }
        at += 1;
    }
}

/// The key of a probe from its kept arguments ([`kept`]) and the snippet's bytes.
pub fn probe_key<'s>(target: &str, kept_args: impl IntoIterator<Item = &'s str>, snippet: &[u8]) -> ContentId {
    let mut hasher = Hasher::new("blaze v2 build-script probe 2");
    hasher.part(VERSION.as_bytes()).part(target.as_bytes());
    for arg in kept_args {
        hasher.part(arg.as_bytes());
    }
    hasher.part(b"<snippet>").part(snippet);
    hasher.finish()
}

/// The key of a try-compile: the pinned compiler, the target, the arguments without the
/// snippet and the output-only flags ([`kept`]), and the snippet's bytes.
pub fn compile_key(target: &str, args: &[&str], snippet_arg: Option<usize>, snippet: &[u8]) -> ContentId {
    let mut hasher = Hasher::new("blaze v2 build-script probe 2");
    hasher.part(VERSION.as_bytes()).part(target.as_bytes());
    kept(args, snippet_arg, |arg| {
        hasher.part(arg.as_bytes());
    });
    hasher.part(b"<snippet>").part(snippet);
    hasher.finish()
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
    /// The kept arguments ([`kept`]), in order.
    pub args: &'static [&'static str],
    pub snippet: &'static str,
    pub accepted: bool,
    pub why: &'static str,
}

/// The table's rows for `target`, sorted by key.
pub fn pinned_rows<'a>(target: &str, known: &[Known], alloc: Alloc<'a>) -> blaze_own::AVec<'a, Row> {
    let mut rows = blaze_own::Vec::with_capacity_in(known.len(), alloc);
    for k in known {
        rows.push(Row { key: probe_key(target, k.args.iter().copied(), k.snippet.as_bytes()), accepted: k.accepted });
    }
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows
}

/// The answers for the pinned nightly, sorted by key ([`pinned_rows`] of the known probes). A
/// try-compile no row answers is [`Refused::CannotAnswerYet`] until blaze's front end answers
/// probes itself.
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
    let mut hasher = Hasher::new("blaze v2 build-script answers 1");
    hasher.part(VERSION.as_bytes()).part(COMMIT_HASH.as_bytes()).part(LLVM.as_bytes());
    for row in table.rows {
        hasher.id(&row.key).update(&[u8::from(row.accepted)]);
    }
    hasher.finish()
}
