//! A build script's question to the compiler, answered by the compiler it asks (end-state.md
//! D193 point 2; rule 1). blaze is the compiler: a try-compile is checked by blaze's own front
//! end for the asked target and edition, and the answer is a record keyed by the question, so
//! a second script asking the same snippet reads it (rules 21, 52, 77). Sans-IO: words, bytes
//! and an environment in, a typed question, its key and the child's exit out; the front end and
//! the store are the two seams a driver hands in ([`ProbeChecker`], [`ProbeRecords`]).
//!
//! - [`read_line`]: the arguments read as a [`Line`] by `probe_ask::classify` (the syntax), then
//!   held to what blaze models (the meaning): every flag that can change the answer is kept, in
//!   the key; every flag that cannot is named in [`DROPPED`] or [`CODEGEN_DROPPED`] with why; any
//!   other is [`Refused`] by name ([`Why`]), never guessed (rules 3, 19).
//! - [`probe`]: the line with what its child sees besides ([`Child`]: the host, the child's
//!   environment, whether the script reads its standard output) and the snippet's bytes, made a
//!   [`Probe`]: a version ([`Probe::Version`], the pin's text), a rejection rustc makes from the
//!   line alone ([`Probe::Rejected`]: an edition it lacks, an invalid crate name), or a
//!   [`Question`] for the checker.
//! - [`Question::key`]: what the answer is a function of, and nothing of the machine (rule 25):
//!   the pinned toolchain, the checker's identity ([`CheckerId`]: blaze's front end, the
//!   language features it implements, its target definitions), the line's normalized key
//!   (`probe_ask::Compile::key`: the target, crate types, edition, cap on lints, whether code is
//!   generated, cfgs, kept flags, the snippet's bytes, never a path), `RUSTC_BOOTSTRAP` (rustc's
//!   driver reads it: `-1` gates every `#![feature]` off), the child's variables the snippet can
//!   read with `env!`/`option_env!`, and the crate name only as far as it can change the answer.
//! - [`answer`]: the one place a question is answered: its record, else the checker, whose
//!   [`ProbeAnswer::Unknown`] (a body it could not take to the end, a sysroot it lacks: D193
//!   point 5) stops the script by name and is never stored as a failure.
//!
//! Every probe of the acceptance projects' scripts reads only the child's exit status: autocfg
//! 1.5.1 (`probe_fmt`: stdout and stderr inherited, `child.wait()`, then removes its `.ll`),
//! rustix 0.38.44 and 1.1.5 (`can_compile`: stdout and stderr `Stdio::null()`, `-o -` in 0.38),
//! proc-macro2 1.0.95 and thiserror 2.0.21 (`compile_probe`: stderr null, `cmd.status()`), so a
//! [`Exit`] writes no output file. A line whose output the script would read (`-o -` with its
//! stdout piped) is [`Why::OutputRead`].

#![blaze::sans_io]

use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_pins::{COMMIT_HASH, RUSTC_V};

use crate::probe_ask::{self, Ask, Compile, Snippet, classify};

const QUESTION_DOMAIN: &str = "blaze build-script probe question";
const CHECKER_DOMAIN: &str = "blaze build-script probe checker";

/// The most names in quotes a snippet may hold (the variables its key holds): a table's cap
/// (rule 18). Past it the line is [`Why::Names`].
pub const MOST_NAMES: usize = 64;

/// The `-C` keys blaze models, kept in the key in order and handed to the checker, each with
/// what it changes in a check.
pub const CODEGEN_KEPT: &[(&str, &str)] = &[
    ("opt-level", "`debug_assertions` and `overflow_checks` when not given (rustc turns them on at opt-level 0 only)"),
    ("debug-assertions", "`cfg(debug_assertions)`"),
    ("overflow-checks", "`cfg(overflow_checks)` and the constant evaluation of arithmetic"),
    ("panic", "`cfg(panic = \"..\")`"),
    ("target-cpu", "the `target_feature` cfgs the CPU implies"),
    ("target-feature", "`cfg(target_feature = \"..\")` and `#[target_feature]`'s checks"),
    ("relocation-model", "`cfg(relocation_model = \"..\")`"),
];

/// The `-C` keys dropped from the key: each changes only how or where code is generated or
/// linked, never a cfg nor whether the crate checks.
pub const CODEGEN_DROPPED: &[(&str, &str)] = &[
    ("codegen-units", "how the crate is split for LLVM"),
    ("incremental", "where rustc keeps its incremental state"),
    ("metadata", "a hash mixed into symbol names"),
    ("extra-filename", "a suffix of the output's file name"),
    ("strip", "what the linker strips"),
    ("split-debuginfo", "where debug info is written"),
    ("debuginfo", "how much debug info is emitted"),
    ("linker", "which linker links: a probe that links is refused (`Why::Links`)"),
    ("link-arg", "an argument of the link: a probe that links is refused"),
    ("link-args", "arguments of the link: a probe that links is refused"),
    ("linker-flavor", "the linker's kind: a probe that links is refused"),
    ("link-dead-code", "what the linker keeps"),
    ("prefer-dynamic", "how std is linked"),
    ("rpath", "the linked file's search path"),
    ("save-temps", "whether LLVM's temporaries are kept"),
    ("remark", "LLVM's optimization remarks"),
    ("force-frame-pointers", "frame pointers in generated code"),
];

/// The other arguments a probe's line may hold that are not in the key, each with why.
pub const DROPPED: &[(&str, &str)] = &[
    ("-o", "where the output goes; a line whose output the script reads is refused (`Why::OutputRead`)"),
    ("--out-dir", "where the outputs go; no output file is written"),
    ("--crate-name", "keyed only as what it can change: valid or not, or whole when the snippet names `crate_name`"),
    ("--error-format", "how errors are printed"),
    ("--json", "how errors are printed"),
    ("--color", "how errors are printed"),
    ("--diagnostic-width", "how errors are printed"),
    ("--remap-path-prefix", "paths in what is printed"),
    ("--verbose", "more words, never another answer"),
    ("-g", "debug info"),
    ("-L", "a search path: a probe's crates are the toolchain's; one found only by a path is `Cause::ExternCrate`"),
    ("-l", "a native library to link: a probe that links is refused (`Why::Links`)"),
    ("--emit", "the kinds beyond whether code is generated (`Compile::codegen`, keyed) change what is written, never whether it compiles"),
];

/// The crate types whose `link` output runs a linker (rlib, lib and staticlib are archives).
const LINKED: &[&str] = &["bin", "cdylib", "dylib", "proc-macro"];

/// Why a probe's line is not a question blaze answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// probe_ask's reader of the line refused it.
    Line(probe_ask::Why),
    /// A `-C` key neither [`CODEGEN_KEPT`] nor [`CODEGEN_DROPPED`] names.
    Codegen,
    /// A `-Z` option other than `unstable-options`: unstable compiler options blaze does not model.
    Unstable,
    /// `--extern`: a crate given by its path; a probe's crates are the toolchain's.
    Extern,
    /// `--sysroot`: the toolchain is blaze's pinned one.
    Sysroot,
    /// `--edition future`: an unstable edition blaze does not model.
    FutureEdition,
    /// A flag on a `--print` line that can change what is printed (`--cfg`, `-C`, an input).
    PrintFlag,
    /// A second `--print`: rustc prints each, and one answer is read.
    PrintTwice,
    /// `--print KIND=PATH`: the answer written to a file.
    PrintToFile,
    /// The output goes to the child's standard output (`-o -`, `--emit kind=-`) and the script
    /// reads it: the emitted bytes, which a check does not make.
    OutputRead,
    /// The compile links (no `--emit` or `link`, for a crate type a linker makes): whether it
    /// succeeds is the linker's, which a check does not run.
    Links,
    /// The snippet holds more than [`MOST_NAMES`] names in quotes.
    Names,
}

/// A refusal: the argument at `at` (the input's index where none is the cause) and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Refused {
    pub at: usize,
    pub why: Why,
}

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Why::Line(why) => core::fmt::Display::fmt(why, f),
            Why::Codegen => f.write_str("a `-C` option blaze's probe checker does not model"),
            Why::Unstable => f.write_str("a `-Z` option blaze's probe checker does not model"),
            Why::Extern => f.write_str("`--extern`: a crate given by its path; a probe's crates are the pinned toolchain's"),
            Why::Sysroot => f.write_str("`--sysroot`: the toolchain is blaze's pinned one"),
            Why::FutureEdition => f.write_str("the unstable `future` edition, which blaze's probe checker does not model"),
            Why::PrintFlag => f.write_str("a flag that can change what `--print` prints, which blaze does not model"),
            Why::PrintTwice => f.write_str("a second `--print`, which blaze does not answer"),
            Why::PrintToFile => f.write_str("a `--print` written to a file, which blaze does not answer"),
            Why::OutputRead => f.write_str("an output sent to the child's standard output, which the script reads; a check does not make the emitted bytes"),
            Why::Links => f.write_str("a compile that links, whose success is the linker's; a check does not link"),
            Why::Names => f.write_str("a snippet with more names in quotes than a probe's key holds"),
        }
    }
}

/// A spawn of `rustc` read as a question, before its snippet's bytes are read.
#[derive(Debug)]
pub enum Line<'a, 's> {
    /// `--version`, `-V`, `-vV`, `--version --verbose`.
    Version { verbose: bool },
    /// `--print <what>`, with its `--target`.
    Print { what: PrintKind<'s>, target: Option<&'s str> },
    /// A try-compile: its line, its flags held to what blaze models.
    Compile(Compile<'a, 's>),
}

/// What a `--print` asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrintKind<'s> {
    /// `cfg`: the target's cfgs, one a line.
    Cfg,
    /// Anything else (`target-list`, `sysroot`, ...): the checker answers or says why not.
    Other(&'s str),
}

impl<'s> PrintKind<'s> {
    pub fn text(self) -> &'s str {
        match self {
            PrintKind::Cfg => "cfg",
            PrintKind::Other(what) => what,
        }
    }
}

/// The index of the argument `value` is part of (the word after `-C`, else a joined `-Cv`): a
/// value is a slice of its argument, so its address names it; a whole word is looked for first,
/// so two equal words a compiler laid at one address still name the first. `args.len()` for
/// one not there.
fn at_of(args: &[&str], value: &str) -> usize {
    let p = value.as_ptr();
    let whole = args.iter().position(|arg| arg.as_ptr() == p && arg.len() == value.len());
    whole.or_else(|| {
        args.iter().position(|arg| {
            let range = arg.as_bytes().as_ptr_range();
            range.start <= p && p <= range.end
        })
    }).unwrap_or(args.len())
}

/// Reads a spawn of `rustc` as a [`Line`]: probe_ask's reader, then every flag held to what
/// blaze models (module docs).
pub fn read_line<'a, 's>(args: &[&'s str], alloc: Alloc<'a>) -> Result<Line<'a, 's>, Refused> {
    let ask = classify(args, alloc).map_err(|refused| Refused { at: refused.at, why: Why::Line(refused.why) })?;
    match ask {
        Ask::Version { verbose } => Ok(Line::Version { verbose }),
        Ask::Print { what, target } => {
            print_words(args)?;
            if what.contains('=') {
                return Err(Refused { at: at_of(args, what), why: Why::PrintToFile });
            }
            let what = if what == "cfg" { PrintKind::Cfg } else { PrintKind::Other(what) };
            Ok(Line::Print { what, target })
        }
        Ask::Compile(mut compile) => {
            let mut kept: AVec<'a, (u8, &'s str)> = Vec::with_capacity_in(compile.flags.len(), alloc);
            for &(letter, value) in compile.flags.iter() {
                let refuse = |why: Why| Err(Refused { at: at_of(args, value), why });
                match letter {
                    b'C' => {
                        let key = value.split_once('=').map_or(value, |(key, _)| key);
                        if CODEGEN_KEPT.iter().any(|(kept, _)| *kept == key) {
                            kept.push((letter, value));
                        } else if !CODEGEN_DROPPED.iter().any(|(dropped, _)| *dropped == key) {
                            return refuse(Why::Codegen);
                        }
                    }
                    b'Z' if value == "unstable-options" => kept.push((letter, value)),
                    b'Z' => return refuse(Why::Unstable),
                    b'A' | b'W' | b'D' | b'F' | b'O' => kept.push((letter, value)),
                    // Named in DROPPED.
                    b'g' | b'L' | b'l' => {}
                    b'e' => return refuse(Why::Extern),
                    b's' => return refuse(Why::Sysroot),
                    _ => return refuse(Why::Line(probe_ask::Why::UnknownFlag)),
                }
            }
            compile.flags = kept;
            Ok(Line::Compile(compile))
        }
    }
}

/// A `--print` line may hold only `--print` (once), `--target` and how errors are printed.
fn print_words(args: &[&str]) -> Result<(), Refused> {
    const TAKES: &[&str] = &["--print", "--target", "--color", "--error-format", "--diagnostic-width"];
    let mut prints = 0;
    let mut at = 0;
    while at < args.len() {
        let arg = args[at];
        let (name, joined) = match arg.split_once('=') {
            Some((name, _)) => (name, true),
            None => (arg, false),
        };
        if !TAKES.contains(&name) {
            return Err(Refused { at, why: Why::PrintFlag });
        }
        if name == "--print" {
            prints += 1;
            if prints > 1 {
                return Err(Refused { at, why: Why::PrintTwice });
            }
        }
        at += if joined { 1 } else { 2 };
    }
    Ok(())
}

/// What a probe's child sees besides its line.
#[derive(Clone, Copy, Debug)]
pub struct Child<'s> {
    /// The build's host: the target when the line names none (rustc's default), and `host:` of
    /// `-vV`.
    pub host: &'s str,
    /// The child's environment, `(name, value)`, std's `Command` semantics applied (the
    /// script's own unless cleared, then its sets and unsets).
    pub env: &'s [(&'s str, &'s str)],
    /// Whether the script reads the child's standard output (piped to it).
    pub stdout_read: bool,
}

impl<'s> Child<'s> {
    fn var(&self, name: &str) -> Option<&'s str> {
        self.env.iter().find(|(have, _)| *have == name).map(|(_, value)| *value)
    }
}

/// The editions the pinned nightly has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edition {
    E2015,
    E2018,
    E2021,
    E2024,
}

/// The crate's name as rustc takes it (its `--crate-name`, else the input file's stem with `-`
/// read as `_`, else `rust_out` for stdin), and how much of it the key holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CrateName<'s> {
    /// A valid name the snippet does not name: the answer does not depend on which, so only its
    /// validity is keyed (autocfg's numbered names ask one question).
    Plain(&'s str),
    /// The snippet's text says `crate_name` (a `#![crate_name = ".."]` must match it): keyed whole.
    Keyed(&'s str),
}

impl<'s> CrateName<'s> {
    pub fn name(self) -> &'s str {
        match self {
            CrateName::Plain(name) | CrateName::Keyed(name) => name,
        }
    }
}

/// A try-compile with everything its answer depends on.
#[derive(Debug)]
pub struct CompileProbe<'a, 's> {
    /// The line, its flags held to what blaze models ([`read_line`]).
    pub line: Compile<'a, 's>,
    /// The snippet's bytes: stdin's, or the file the script named (in `OUT_DIR` or its package).
    pub source: &'s [u8],
    /// The target asked: the line's `--target`, else the host.
    pub target: &'s str,
    pub edition: Edition,
    pub crate_name: CrateName<'s>,
    /// The child's `RUSTC_BOOTSTRAP` (`None`: unset; proc-macro2 and thiserror remove it).
    pub bootstrap: Option<&'s str>,
    /// Each name the snippet holds in quotes, sorted, once, with the child's value (`None`:
    /// unset): every variable `env!` or `option_env!` of a literal can read. A read of another
    /// (a name built by `concat!`) is [`Cause::EnvOutsideKey`], found by [`answer`].
    pub env: AVec<'a, (&'s str, Option<&'s str>)>,
}

/// A `--print` with everything its answer depends on.
#[derive(Clone, Copy, Debug)]
pub struct PrintProbe<'s> {
    pub what: PrintKind<'s>,
    /// The target asked: the line's `--target`, else the host.
    pub target: &'s str,
    /// The child's `RUSTC_BOOTSTRAP`: a nightly made stable (`-1`) prints no nightly-only cfg.
    pub bootstrap: Option<&'s str>,
}

/// What the checker is asked.
#[derive(Debug)]
pub enum Question<'a, 's> {
    Compile(CompileProbe<'a, 's>),
    Print(PrintProbe<'s>),
}

/// What a spawn of `rustc` asks, read whole.
#[derive(Debug)]
pub enum Probe<'a, 's> {
    /// The pin's version text ([`crate::probe::version`]): a fact of the pin, never stored.
    Version { verbose: bool, host: &'s str },
    /// rustc refuses the line itself (an edition it lacks, an invalid crate name): exit 1 and
    /// its first error's line, decided from the line, never stored.
    Rejected { stderr: AString<'a> },
    /// A question for the checker, answered through its record.
    Asks(Question<'a, 's>),
}

/// Whether `name` is a crate name rustc takes: not empty, each character alphanumeric or `_`.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// The file stem of `path`, as `Path::file_stem` makes it.
fn stem(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    match file.rfind('.') {
        None | Some(0) => file,
        Some(at) => &file[..at],
    }
}

/// The names the snippet holds in quotes (`"NAME"`, an identifier between two quotes), sorted,
/// each once: a superset of the literal names `env!` and `option_env!` read.
fn quoted_names<'a, 's>(source: &'s [u8], alloc: Alloc<'a>) -> Option<AVec<'a, &'s str>> {
    let mut names: AVec<'a, &'s str> = Vec::new_in(alloc);
    for (at, _) in source.iter().enumerate().filter(|(_, b)| **b == b'"') {
        let start = at + 1;
        let mut end = start;
        while end < source.len() && (source[end] == b'_' || source[end].is_ascii_alphanumeric()) {
            end += 1;
        }
        if end > start && source.get(end) == Some(&b'"') && !source[start].is_ascii_digit() {
            let name = core::str::from_utf8(&source[start..end]).expect("ASCII identifier bytes");
            names.push(name);
        }
    }
    names.sort_unstable();
    names.dedup();
    (names.len() <= MOST_NAMES).then_some(names)
}

/// The line made a [`Probe`] with what its child sees and the snippet's bytes (`source`: empty
/// for a version or a print, which read none).
pub fn probe<'a, 's>(line: Line<'a, 's>, args: &[&'s str], source: &'s [u8], child: &Child<'s>, alloc: Alloc<'a>) -> Result<Probe<'a, 's>, Refused> {
    let bootstrap = child.var("RUSTC_BOOTSTRAP");
    let line = match line {
        Line::Version { verbose } => return Ok(Probe::Version { verbose, host: child.host }),
        Line::Print { what, target } => return Ok(Probe::Asks(Question::Print(PrintProbe { what, target: target.unwrap_or(child.host), bootstrap }))),
        Line::Compile(line) => line,
    };
    let edition = match line.edition {
        None | Some("2015") => Edition::E2015,
        Some("2018") => Edition::E2018,
        Some("2021") => Edition::E2021,
        Some("2024") => Edition::E2024,
        Some(future @ "future") => return Err(Refused { at: at_of(args, future), why: Why::FutureEdition }),
        Some(_) => return Ok(Probe::Rejected { stderr: probe_ask::Rejection::Edition.stderr(&line, b"", alloc) }),
    };
    if child.stdout_read {
        if let Some(out @ "-") = line.out {
            return Err(Refused { at: at_of(args, out), why: Why::OutputRead });
        }
        // `--emit=kind=-` or `--emit kind=-`: the word holding the kinds.
        let kinds = |at: usize, arg: &str| arg.starts_with("--emit=") || (at > 0 && args[at - 1] == "--emit");
        if let Some(at) = args.iter().enumerate().position(|(at, arg)| kinds(at, arg) && arg.split(',').any(|kind| kind.ends_with("=-"))) {
            return Err(Refused { at, why: Why::OutputRead });
        }
    }
    let types: &[&str] = if line.crate_types.is_empty() { &["bin"] } else { &line.crate_types };
    if line.emits.has("link") && types.iter().any(|kind| LINKED.contains(kind)) {
        return Err(Refused { at: line.input_at, why: Why::Links });
    }
    let (name, explicit) = match (line.crate_name, line.input) {
        (Some(name), _) => (name, true),
        (None, Snippet::File(path)) => (stem(path), false),
        (None, Snippet::Stdin) => ("rust_out", false),
    };
    // A name from a file has `-` read as `_` (and may not start with `-`); a given one is as given.
    let valid = if explicit { valid_name(name) } else { !name.is_empty() && !name.starts_with('-') && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') };
    if !valid {
        let mut stderr = String::new_in(alloc);
        stderr.push_str("error: invalid crate name: `");
        stderr.push_str(name);
        stderr.push_str("`\n");
        return Ok(Probe::Rejected { stderr });
    }
    let crate_name = if source.windows(b"crate_name".len()).any(|w| w == b"crate_name") { CrateName::Keyed(name) } else { CrateName::Plain(name) };
    let Some(names) = quoted_names(source, alloc) else {
        return Err(Refused { at: line.input_at, why: Why::Names });
    };
    let mut env: AVec<'a, (&'s str, Option<&'s str>)> = Vec::with_capacity_in(names.len(), alloc);
    for name in names.iter() {
        env.push((*name, child.var(name)));
    }
    let target = line.target_or(child.host);
    Ok(Probe::Asks(Question::Compile(CompileProbe { line, source, target, edition, crate_name, bootstrap, env })))
}

/// The identity of a checker: what every answer it gives is a function of besides the
/// question (rule 21). Made by [`checker_id`] only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckerId(pub ContentId);

/// A checker's identity from its parts: `front_end`, the code identity of blaze's front end
/// that checks a snippet (the binary's id until a function's code identity is kept: D162);
/// `implemented`, the language features it implements, sorted (`#![feature(x)]` of an
/// unstable one it lacks is a rejection, the answer the script can act on); `targets`, the
/// identity of blaze's target definitions (a target's cfgs and its sysroot's records).
pub fn checker_id(front_end: &ContentId, implemented: &[&str], targets: &ContentId) -> CheckerId {
    let mut h = Hasher::new(CHECKER_DOMAIN);
    h.id(front_end).id(targets).update(&(implemented.len() as u64).to_le_bytes());
    for name in implemented {
        h.part(name.as_bytes());
    }
    CheckerId(h.finish())
}

/// A question's key: its record's name in the store.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct ProbeKey(pub ContentId);

impl Question<'_, '_> {
    /// What the answer is a function of, normalized (module docs).
    pub fn key(&self, checker: &CheckerId) -> ProbeKey {
        let mut h = Hasher::new(QUESTION_DOMAIN);
        h.part(RUSTC_V.as_bytes()).part(COMMIT_HASH.as_bytes()).id(&checker.0);
        match self {
            Question::Compile(c) => {
                // The line's own key holds the target (`target_or` of the target already
                // chosen is that target), the crate types, the edition, the cap, `codegen`,
                // `--test`, the cfgs, the kept flags and the snippet's bytes.
                h.part(b"compile").id(&c.line.key(c.target, c.source));
                h.opt_part(c.bootstrap);
                h.update(&(c.env.len() as u64).to_le_bytes());
                for (name, value) in c.env.iter() {
                    h.part(name.as_bytes()).opt_part(*value);
                }
                match c.crate_name {
                    CrateName::Plain(_) => {
                        h.update(&[0]);
                    }
                    CrateName::Keyed(name) => {
                        h.update(&[1]).part(name.as_bytes());
                    }
                }
            }
            Question::Print(p) => {
                h.part(b"print").part(p.what.text().as_bytes()).part(p.target.as_bytes()).opt_part(p.bootstrap);
            }
        }
        ProbeKey(h.finish())
    }
}

/// A compile checked to the end by the front end.
#[derive(Debug)]
pub struct Checked<'a> {
    /// The errors found: hard errors, and lint findings at `deny` or `forbid` after the line's
    /// levels (`-A`/`-W`/`-D`/`-F` in order), the source's attributes and the line's
    /// `--cap-lints` (rustc's order); a finding below `deny` is no error.
    pub errors: u32,
    /// The first error's line, as the check words it (`error[E0599]: no method named ..`).
    pub first: Option<AString<'a>>,
    /// The variables the check read (`env!`, `option_env!`), by name.
    pub env_read: AVec<'a, AString<'a>>,
}

/// What the checker says.
#[derive(Debug)]
pub enum ProbeAnswer<'a> {
    /// A compile checked to the end: every body taken to its end, every name it uses found or
    /// known absent (D193 point 5).
    Checked(Checked<'a>),
    /// A print answered: rustc's standard output for it.
    Printed(AString<'a>),
    /// The checker cannot say, and why: neither success nor failure. Stops the script by name;
    /// never stored.
    Unknown(Cause<'a>),
}

/// Why a question has no answer.
#[derive(PartialEq, Eq, Debug)]
pub enum Cause<'a> {
    /// A body or item the check could not take to its end: the item, and the front end's why.
    Incomplete { item: AString<'a>, why: AString<'a> },
    /// The toolchain's crate `krate` for the target has no records here.
    Sysroot { krate: AString<'a> },
    /// `extern crate name` (or a path through it) of a crate that is not the toolchain's.
    ExternCrate { name: AString<'a> },
    /// `#![feature(name)]` of a language feature the checker's list says neither that blaze
    /// implements nor that it does not.
    Feature { name: AString<'a> },
    /// The target is none blaze defines.
    Target,
    /// A compile that generates code (`--emit=llvm-ir`, `obj`, `link`), whose
    /// post-monomorphization errors (a constant of an instance) the check could not rule out:
    /// the item.
    PostMono { item: AString<'a> },
    /// The snippet reads a file (`include!`, `include_str!`): its path as written.
    FileRead { path: AString<'a> },
    /// A print blaze does not answer (`sysroot`, `target-libdir`: paths of a machine).
    Print,
    /// The checker's answer is not of the question's kind (a print for a compile).
    Mismatched,
    /// The check read a variable the key does not hold (a name not in quotes in the snippet).
    EnvOutsideKey { name: AString<'a> },
}

impl core::fmt::Display for Cause<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Cause::Incomplete { item, why } => write!(f, "blaze's check of the snippet did not reach the end of `{item}`: {why}"),
            Cause::Sysroot { krate } => write!(f, "the pinned toolchain's `{krate}` for this target has no records here"),
            Cause::ExternCrate { name } => write!(f, "the snippet uses the crate `{name}`, which is not the pinned toolchain's"),
            Cause::Feature { name } => write!(f, "`#![feature({name})]`: blaze's list of the language features it implements does not hold `{name}`"),
            Cause::Target => f.write_str("the target is none blaze defines"),
            Cause::PostMono { item } => write!(f, "the compile generates code, and the check could not rule out a post-monomorphization error in `{item}`"),
            Cause::FileRead { path } => write!(f, "the snippet reads the file `{path}`, which a probe's check is not given"),
            Cause::Print => f.write_str("a `--print` blaze does not answer"),
            Cause::Mismatched => f.write_str("blaze's probe checker answered with another kind of answer than asked"),
            Cause::EnvOutsideKey { name } => write!(f, "the check read the variable `{name}`, which the probe's key does not hold"),
        }
    }
}

/// The front end, as a probe asks it. The real one checks the snippet as a crate of its own
/// for the asked target, with the sysroot crates as records (handoff p1-probe-checker.txt);
/// [`fake::FakeChecker`] answers from rows.
pub trait ProbeChecker {
    /// The checker's identity ([`checker_id`]), in every key.
    fn id(&self) -> CheckerId;
    /// The question checked (a compile) or printed (a print): [`ProbeAnswer::Checked`] or
    /// [`ProbeAnswer::Printed`] only for an answer it is sure of; anything else is
    /// [`ProbeAnswer::Unknown`] with its cause, never a guess (rule 3).
    fn check<'a>(&self, question: &Question<'_, '_>, alloc: Alloc<'a>) -> ProbeAnswer<'a>;
}

/// The records of answers, by key: written once, read by the next script asking.
pub trait ProbeRecords {
    /// The store's refusal, naming what failed.
    type Error: core::fmt::Debug + core::fmt::Display;
    /// The record under `key`, read into `alloc`, if one was written.
    fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, Self::Error>;
    /// Writes `bytes` under `key`. Equal bytes again are no write (rule 25: two builders of a
    /// key write equal bytes); other bytes under a written key are refused (rule 21).
    fn put(&mut self, key: &ProbeKey, bytes: &[u8]) -> Result<(), Self::Error>;
}

/// A stored answer, read in place from its record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stored<'b> {
    /// Exit 0, no output.
    Accepted,
    /// Exit 1; standard error the first error's line.
    Rejected { first: &'b str },
    /// Exit 0; standard output the text.
    Printed { text: &'b str },
}

/// A record whose bytes are not a [`Stored`]: the byte where reading stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Malformed {
    pub at: usize,
}

/// A record: the kind (0 accepted, 1 rejected, 2 printed), then for 1 and 2 the text's length
/// (u32 little-endian) and its UTF-8 bytes. Nothing else: the key names the question.
pub fn encode<'a>(stored: &Stored<'_>, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let (kind, text) = match stored {
        Stored::Accepted => (0u8, None),
        Stored::Rejected { first } => (1, Some(*first)),
        Stored::Printed { text } => (2, Some(*text)),
    };
    let mut out = Vec::with_capacity_in(5 + text.map_or(0, str::len), alloc);
    out.push(kind);
    if let Some(text) = text {
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    out
}

/// Reads a record [`encode`] wrote, exactly: a byte more or less is [`Malformed`].
pub fn decode(bytes: &[u8]) -> Result<Stored<'_>, Malformed> {
    let (&kind, rest) = bytes.split_first().ok_or(Malformed { at: 0 })?;
    if kind == 0 {
        return if rest.is_empty() { Ok(Stored::Accepted) } else { Err(Malformed { at: 1 }) };
    }
    if kind > 2 {
        return Err(Malformed { at: 0 });
    }
    let len: [u8; 4] = rest.get(..4).and_then(|len| len.try_into().ok()).ok_or(Malformed { at: 1 })?;
    let text = &rest[4..];
    if text.len() != u32::from_le_bytes(len) as usize {
        return Err(Malformed { at: 5 });
    }
    let text = core::str::from_utf8(text).map_err(|e| Malformed { at: 5 + e.valid_up_to() })?;
    Ok(if kind == 1 { Stored::Rejected { first: text } } else { Stored::Printed { text } })
}

/// What the script's child does: its exit code and its two outputs.
#[derive(PartialEq, Eq, Debug)]
pub struct Exit<'a> {
    pub code: i32,
    pub stdout: AVec<'a, u8>,
    pub stderr: AVec<'a, u8>,
}

fn exit<'a>(code: i32, stdout: &[u8], stderr: &[u8], alloc: Alloc<'a>) -> Exit<'a> {
    let mut out = Vec::with_capacity_in(stdout.len(), alloc);
    out.extend_from_slice(stdout);
    let mut err = Vec::with_capacity_in(stderr.len() + 1, alloc);
    err.extend_from_slice(stderr);
    if !err.is_empty() && !err.ends_with(b"\n") {
        err.push(b'\n');
    }
    Exit { code, stdout: out, stderr: err }
}

impl Stored<'_> {
    pub fn exit<'a>(&self, alloc: Alloc<'a>) -> Exit<'a> {
        match self {
            Stored::Accepted => exit(0, b"", b"", alloc),
            Stored::Rejected { first } => exit(1, b"", first.as_bytes(), alloc),
            Stored::Printed { text } => exit(0, text.as_bytes(), b"", alloc),
        }
    }
}

/// Why [`answer`] stops the script: never an exit a script could swallow.
#[derive(Debug)]
pub enum Stop<'a, E> {
    /// The checker cannot say (or read a variable the key lacks): the cause.
    Unknown { key: ProbeKey, cause: Cause<'a> },
    /// The store refused a read.
    Read { key: ProbeKey, why: E },
    /// The store refused a write.
    Write { key: ProbeKey, why: E },
    /// The record under the key is not a stored answer.
    Malformed { key: ProbeKey, at: usize },
}

impl<E: core::fmt::Display> core::fmt::Display for Stop<'_, E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Stop::Unknown { key, cause } => write!(f, "blaze cannot answer this probe (key {:02x?}): {cause}", &key.0.0[..8]),
            Stop::Read { key, why } => write!(f, "the record of probe {:02x?} could not be read: {why}", &key.0.0[..8]),
            Stop::Write { key, why } => write!(f, "the record of probe {:02x?} could not be written: {why}", &key.0.0[..8]),
            Stop::Malformed { key, at } => write!(f, "the record of probe {:02x?} is malformed at byte {at}", &key.0.0[..8]),
        }
    }
}

/// The one place a probe is answered (module docs): a version from the pin, a line rustc
/// refuses from its text, else the question's record, else the checker, whose known answer is
/// stored once and whose [`ProbeAnswer::Unknown`] stops the script, stored never.
pub fn answer<'a, C: ProbeChecker + ?Sized, R: ProbeRecords + ?Sized>(probe: &Probe<'_, '_>, checker: &C, records: &mut R, alloc: Alloc<'a>) -> Result<Exit<'a>, Stop<'a, R::Error>> {
    let question = match probe {
        Probe::Version { verbose, host } => return Ok(exit(0, crate::probe::version(*verbose, host, alloc).as_bytes(), b"", alloc)),
        Probe::Rejected { stderr } => return Ok(exit(1, b"", stderr.as_bytes(), alloc)),
        Probe::Asks(question) => question,
    };
    let key = question.key(&checker.id());
    if let Some(bytes) = records.get(&key, alloc).map_err(|why| Stop::Read { key, why })? {
        let stored = decode(&bytes).map_err(|bad| Stop::Malformed { key, at: bad.at })?;
        return Ok(stored.exit(alloc));
    }
    let stored_text: AString<'a>;
    let stored = match (question, checker.check(question, alloc)) {
        (_, ProbeAnswer::Unknown(cause)) => return Err(Stop::Unknown { key, cause }),
        (Question::Compile(c), ProbeAnswer::Checked(checked)) => {
            // `RUSTC_BOOTSTRAP` is keyed on its own; every other read must be a held name.
            if let Some(name) = checked.env_read.iter().find(|name| name.as_str() != "RUSTC_BOOTSTRAP" && !c.env.iter().any(|(held, _)| *held == name.as_str())) {
                return Err(Stop::Unknown { key, cause: Cause::EnvOutsideKey { name: String::from_str_in(name.as_str(), alloc) } });
            }
            match (checked.errors, checked.first) {
                (0, _) => Stored::Accepted,
                (_, Some(first)) => {
                    stored_text = first;
                    Stored::Rejected { first: stored_text.as_str() }
                }
                (_, None) => Stored::Rejected { first: "error: blaze's check of the probe found an error" },
            }
        }
        (Question::Print(_), ProbeAnswer::Printed(text)) => {
            stored_text = text;
            Stored::Printed { text: stored_text.as_str() }
        }
        (Question::Compile(_), ProbeAnswer::Printed(_)) | (Question::Print(_), ProbeAnswer::Checked(_)) => return Err(Stop::Unknown { key, cause: Cause::Mismatched }),
    };
    let bytes = encode(&stored, alloc);
    records.put(&key, &bytes).map_err(|why| Stop::Write { key, why })?;
    Ok(stored.exit(alloc))
}

/// A checker and a store for tests: rows in, calls counted.
pub mod fake {
    use core::cell::Cell;

    use blaze_own::{AVec, Alloc, String, Vec};

    use super::{Cause, CheckerId, Checked, ProbeAnswer, ProbeChecker, ProbeKey, ProbeRecords, Question};

    /// What a fake row answers.
    #[derive(Clone, Copy, Debug)]
    pub enum FakeVerdict<'r> {
        Accepted,
        /// One error, worded so.
        Rejected(&'r str),
        Printed(&'r str),
        /// Not checked to the end (D193 point 5).
        Incomplete,
        /// Accepted, having read this variable.
        ReadsEnv(&'r str),
    }

    /// A row: the snippet (a print's `what`) and the target it answers.
    #[derive(Clone, Copy, Debug)]
    pub struct FakeRow<'r> {
        pub source: &'r [u8],
        pub target: &'r str,
        pub verdict: FakeVerdict<'r>,
    }

    /// Answers from its rows; a question no row holds is `Incomplete`. Counts its calls.
    pub struct FakeChecker<'r> {
        pub id: CheckerId,
        pub rows: &'r [FakeRow<'r>],
        pub calls: Cell<u32>,
    }

    impl ProbeChecker for FakeChecker<'_> {
        fn id(&self) -> CheckerId {
            self.id
        }

        fn check<'a>(&self, question: &Question<'_, '_>, alloc: Alloc<'a>) -> ProbeAnswer<'a> {
            self.calls.set(self.calls.get() + 1);
            let (source, target): (&[u8], &str) = match question {
                Question::Compile(c) => (c.source, c.target),
                Question::Print(p) => (p.what.text().as_bytes(), p.target),
            };
            let incomplete = |why: &str| ProbeAnswer::Unknown(Cause::Incomplete { item: String::from_str_in("the snippet", alloc), why: String::from_str_in(why, alloc) });
            let Some(row) = self.rows.iter().find(|row| row.source == source && row.target == target) else {
                return incomplete("no fake row");
            };
            let checked = |errors: u32, first: Option<&str>, read: Option<&str>| {
                let mut env_read: AVec<'a, _> = Vec::new_in(alloc);
                if let Some(name) = read {
                    env_read.push(String::from_str_in(name, alloc));
                }
                ProbeAnswer::Checked(Checked { errors, first: first.map(|first| String::from_str_in(first, alloc)), env_read })
            };
            match row.verdict {
                FakeVerdict::Accepted => checked(0, None, None),
                FakeVerdict::Rejected(first) => checked(1, Some(first), None),
                FakeVerdict::Printed(text) => ProbeAnswer::Printed(String::from_str_in(text, alloc)),
                FakeVerdict::Incomplete => incomplete("a planted gap"),
                FakeVerdict::ReadsEnv(name) => checked(0, None, Some(name)),
            }
        }
    }

    /// Two different values under one key.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Twice(pub ProbeKey);

    impl core::fmt::Display for Twice {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "another answer is already written under probe {:02x?} (rule 21)", &self.0.0.0[..8])
        }
    }

    /// Records in memory, counted: `hits` reads that found a record, `writes` records written.
    pub struct MemRecords<'a> {
        pub rows: AVec<'a, (ProbeKey, AVec<'a, u8>)>,
        pub hits: Cell<u32>,
        pub writes: u32,
        alloc: Alloc<'a>,
    }

    impl<'a> MemRecords<'a> {
        pub fn new(alloc: Alloc<'a>) -> MemRecords<'a> {
            MemRecords { rows: Vec::new_in(alloc), hits: Cell::new(0), writes: 0, alloc }
        }
    }

    impl ProbeRecords for MemRecords<'_> {
        type Error = Twice;

        fn get<'b>(&self, key: &ProbeKey, alloc: Alloc<'b>) -> Result<Option<AVec<'b, u8>>, Twice> {
            Ok(self.rows.iter().find(|(have, _)| have == key).map(|(_, bytes)| {
                self.hits.set(self.hits.get() + 1);
                let mut out = Vec::with_capacity_in(bytes.len(), alloc);
                out.extend_from_slice(bytes);
                out
            }))
        }

        fn put(&mut self, key: &ProbeKey, bytes: &[u8]) -> Result<(), Twice> {
            if let Some((_, held)) = self.rows.iter().find(|(have, _)| have == key) {
                return if held[..] == *bytes { Ok(()) } else { Err(Twice(*key)) };
            }
            let mut held = Vec::with_capacity_in(bytes.len(), self.alloc);
            held.extend_from_slice(bytes);
            self.rows.push((*key, held));
            self.writes += 1;
            Ok(())
        }
    }
}
