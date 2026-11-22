//! The compiler line a build script's probe hands `rustc`, read, keyed and, where blaze can
//! decide it from the snippet alone, answered (docs/v2/plan/design/build-scripts.md, "A,
//! probing the compiler"; rule 1: never the real rustc). Sans-IO: words and bytes in, a typed
//! ask and a verdict out.
//!
//! - [`classify`]: the arguments read as one [`Ask`]: a version ([`Ask::Version`]), a print
//!   request ([`Ask::Print`]) or a try-compile ([`Compile`]) of one input (a file, or `-`: the
//!   snippet on stdin). An argument this reader does not know is [`Refused`] by its index
//!   ([`Why::UnknownFlag`]), never skipped: an unknown flag can change the verdict.
//! - [`Compile::key`]: what the verdict is a function of, normalized: the pinned compiler, the
//!   target the probe asks about (its `--target`, else the host: what rustc compiles for), the
//!   crate types as a set, the edition (rustc's default 2015 when none), the cap on lints,
//!   whether code is generated (an `--emit` past `metadata` and `dep-info`, or none: `link`;
//!   post-monomorphization errors appear only then), the cfgs and check-cfgs as sets, every
//!   other flag that can move the verdict in order, and the snippet's bytes. Where the output
//!   goes (`-o`, `--out-dir`, `--crate-name`, the error format, the colour) is not in it, so
//!   autocfg's numbered crate names and each unit's `OUT_DIR` ask one question. The target is
//!   keyed once, as the target: a row written for one triple never answers another.
//! - [`decide`]: the probes blaze decides from the snippet's own text: a crate of inner
//!   attributes and comments only (autocfg's first probe, the empty crate, `#![no_std]`, and
//!   rustix's `#![feature(..)]` probes), whose verdict is the crate type, whether the
//!   toolchain ships the crate the snippet implies (`std`, or `core` under `no_std`) for the
//!   target (a [`ships_key`] row of the table, made from the toolchain's sysroot of that
//!   side), and whether each feature it names exists: a language feature of the pinned
//!   nightly (blaze-surface `lang_features`; a removed one is E0557), else a library feature
//!   the crates the probe loads declare for the target ([`feature_key`] rows, listed from the
//!   library's own text by blaze-scope `lib_features`; a name no row holds while the listing
//!   is there, [`features_listed_key`], is E0635). A snippet with an item, an attribute it
//!   does not model (`cfg_attr`, `deny`), or a flag it does not model is [`Decided::Open`],
//!   named.
//! - [`answer`]: the one place a try-compile is answered: decided by [`decide`], else by the
//!   table's row for its key (`pinned::TEMPO`, each row with its reason in the pinned library),
//!   else unanswered, which the runtime turns into `CannotAnswerYet` naming the key.
//!
//! The answer writes no output file: every probe tempo's scripts make reads only the exit
//! status (autocfg's `status.success()`, rustix's `wait().success()`, thiserror's, anyhow's
//! and eyre's `status()`); rustix then removes its `-o` file and ignores the error. A script
//! that reads the emitted file's content finds it absent (ENOENT), which none of tempo's does.

#![blaze::sans_io]

use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_surface::lang_features::Lang;

use blaze_pins::{COMMIT_HASH, RUSTC_V};

use crate::probe::{Row, Table};

/// The identity of [`decide`]: held in the answers' identity (`probe::answers_id`), so a
/// change to what blaze decides itself reruns every script that probed.
pub const DECIDER: &str = "blaze v2 probe decider 2: inner attributes and #![feature] (language list at rustc 16d1ffac17, library listings)";

const KEY_DOMAIN: &str = "blaze v2 build-script probe 3";
const SHIPS_DOMAIN: &str = "blaze v2 build-script ships 1";
const FEATURE_DOMAIN: &str = "blaze v2 build-script library feature 1";

/// Where a try-compile's snippet comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Snippet<'s> {
    /// `-`: the bytes written to its stdin.
    Stdin,
    /// A path: the file's bytes (in `OUT_DIR` or the package).
    File(&'s str),
}

/// What one argument is to rustc's driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flag {
    CrateType,
    /// Where or how the output goes, never whether the snippet is accepted.
    Quiet,
    Edition,
    Emit,
    Target,
    Cfg,
    CheckCfg,
    Codegen,
    Unstable,
    CapLints,
    /// `-A`, `-W`, `-D`, `-F` (the level's letter).
    Lint(u8),
    Extern,
    Search,
    Link,
    Sysroot,
    Print,
    Version,
    Verbose,
    Test,
    Optimize,
    Debuginfo,
    /// `--crate-name`: output only, but given once.
    CrateName,
    /// `-o`: output only, but given once.
    Out,
    /// `--out-dir`: output only, but given once.
    OutDir,
    /// Short letters without values joined in one argument (`-vV`): the argument.
    Joined,
}

/// The long flags: name, meaning, whether a value follows (`--x v` or `--x=v`).
const LONG: &[(&str, Flag, bool)] = &[
    ("--crate-type", Flag::CrateType, true),
    ("--crate-name", Flag::CrateName, true),
    ("--edition", Flag::Edition, true),
    ("--emit", Flag::Emit, true),
    ("--target", Flag::Target, true),
    ("--cfg", Flag::Cfg, true),
    ("--check-cfg", Flag::CheckCfg, true),
    ("--codegen", Flag::Codegen, true),
    ("--cap-lints", Flag::CapLints, true),
    ("--allow", Flag::Lint(b'A'), true),
    ("--warn", Flag::Lint(b'W'), true),
    ("--deny", Flag::Lint(b'D'), true),
    ("--forbid", Flag::Lint(b'F'), true),
    ("--out-dir", Flag::OutDir, true),
    ("--extern", Flag::Extern, true),
    ("--sysroot", Flag::Sysroot, true),
    ("--print", Flag::Print, true),
    ("--error-format", Flag::Quiet, true),
    ("--json", Flag::Quiet, true),
    ("--color", Flag::Quiet, true),
    ("--diagnostic-width", Flag::Quiet, true),
    ("--remap-path-prefix", Flag::Quiet, true),
    ("--version", Flag::Version, false),
    ("--verbose", Flag::Verbose, false),
    ("--test", Flag::Test, false),
];

/// The short flags: letter, meaning, whether a value follows (`-C v` or `-Cv`). The ones
/// without a value may be joined (`-vV`).
const SHORT: &[(u8, Flag, bool)] = &[
    (b'C', Flag::Codegen, true),
    (b'Z', Flag::Unstable, true),
    (b'A', Flag::Lint(b'A'), true),
    (b'W', Flag::Lint(b'W'), true),
    (b'D', Flag::Lint(b'D'), true),
    (b'F', Flag::Lint(b'F'), true),
    (b'o', Flag::Out, true),
    (b'L', Flag::Search, true),
    (b'l', Flag::Link, true),
    (b'V', Flag::Version, false),
    (b'v', Flag::Verbose, false),
    (b'O', Flag::Optimize, false),
    (b'g', Flag::Debuginfo, false),
];

/// The `--emit` kinds rustc takes (each may be `kind=path`), in [`Emits`]' bit order.
pub const EMIT_KINDS: &[&str] = &["asm", "dep-info", "link", "llvm-bc", "llvm-ir", "metadata", "mir", "obj"];

/// The `--emit` kinds a line asks, one bit per [`EMIT_KINDS`] entry; none given is rustc's
/// `link` ([`Emits::LINK_ONLY`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Emits(pub u8);

impl Emits {
    /// What rustc emits with no `--emit`: the linked output alone.
    pub const LINK_ONLY: Emits = Emits(1 << 2);

    /// Whether `kind` (an [`EMIT_KINDS`] name) is asked.
    pub fn has(self, kind: &str) -> bool {
        EMIT_KINDS.iter().position(|k| *k == kind).is_some_and(|at| self.0 & (1 << at) != 0)
    }

    /// How many kinds are asked: `-o` names one output only when this is one.
    pub fn count(self) -> u32 {
        self.0.count_ones()
    }
}

/// Why the arguments are not an ask blaze reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// An argument starting with `-` that rustc's driver may take and this reader does not.
    UnknownFlag,
    /// A flag that takes a value, last on the line.
    NoValue,
    /// A flag rustc takes once, given again.
    Repeated,
    /// A flag rustc takes again (`--cap-lints`), given again with another value: which one it
    /// applies is not read.
    Differs,
    /// A second input.
    TwoInputs,
    /// A compile with no input.
    NoInput,
    /// An `--emit` kind rustc does not have.
    Emit,
}

/// A refusal: the argument at `at` (the line's length for [`Why::NoInput`]) and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Refused {
    pub at: usize,
    pub why: Why,
}

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Why::UnknownFlag => "a flag blaze's reader of a probe's compiler line does not know",
            Why::NoValue => "a flag whose value is missing",
            Why::Repeated => "a flag rustc takes once, given twice",
            Why::Differs => "a flag given again with another value (`--cap-lints`): which one rustc applies is not read",
            Why::TwoInputs => "a second input",
            Why::NoInput => "a compile with no input",
            Why::Emit => "an --emit kind rustc does not have",
        })
    }
}

/// What a spawn of `rustc` asks.
#[derive(Debug)]
pub enum Ask<'a, 's> {
    /// `--version`, `-V`, `-vV`, `--version --verbose`: `verbose` for the long form.
    Version { verbose: bool },
    /// `--print <what>` (the first one asked), with the line's `--target` (rustc prints the
    /// host's when none).
    Print { what: &'s str, target: Option<&'s str> },
    /// A try-compile.
    Compile(Compile<'a, 's>),
}

/// A try-compile, read: what its verdict depends on.
#[derive(Debug)]
pub struct Compile<'a, 's> {
    pub input: Snippet<'s>,
    /// The input's index in the arguments.
    pub input_at: usize,
    pub target: Option<&'s str>,
    pub edition: Option<&'s str>,
    pub cap_lints: Option<&'s str>,
    /// The crate types, sorted, each once (none given: rustc's `bin`).
    pub crate_types: AVec<'a, &'s str>,
    /// `--cfg` and `--check-cfg` values, sorted, each once.
    pub cfgs: AVec<'a, &'s str>,
    pub check_cfgs: AVec<'a, &'s str>,
    /// Every other flag that can move the verdict, in order: (its letter, its value).
    pub flags: AVec<'a, (u8, &'s str)>,
    /// Code is generated: an `--emit` kind past `metadata` and `dep-info`, or no `--emit`.
    pub codegen: bool,
    /// `--test`: the test harness's crate.
    pub test: bool,
    /// The `--emit` kinds asked ([`Emits::LINK_ONLY`] when none is given).
    pub emits: Emits,
    /// `--crate-name`'s value: never in [`Compile::key`] (autocfg numbers each probe's), read
    /// by blaze-build-script `probe_check` for what the name can change.
    pub crate_name: Option<&'s str>,
    /// `-o`'s value: where the output goes (`-` is the child's standard output), never in the
    /// key; `probe_check` refuses a line whose output the script reads.
    pub out: Option<&'s str>,
}

/// One argument, read.
enum Arg<'s> {
    Flag(Flag, &'s str),
    Input(Snippet<'s>),
}

/// The argument at `at` and the next one, read: the argument and how many words it took.
fn read<'s>(args: &[&'s str], at: usize) -> Result<(Arg<'s>, usize), Refused> {
    let arg = args[at];
    let valued = |flag: Flag, joined: Option<&'s str>| -> Result<(Arg<'s>, usize), Refused> {
        match joined {
            Some(value) => Ok((Arg::Flag(flag, value), 1)),
            None => match args.get(at + 1) {
                Some(value) => Ok((Arg::Flag(flag, value), 2)),
                None => Err(Refused { at, why: Why::NoValue }),
            },
        }
    };
    if arg == "-" {
        return Ok((Arg::Input(Snippet::Stdin), 1));
    }
    if !arg.starts_with('-') {
        return Ok((Arg::Input(Snippet::File(arg)), 1));
    }
    if arg.starts_with("--") {
        let (name, joined) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (arg, None),
        };
        let Some((_, flag, takes)) = LONG.iter().find(|(long, _, _)| *long == name) else {
            return Err(Refused { at, why: Why::UnknownFlag });
        };
        return match (takes, joined) {
            (true, _) => valued(*flag, joined),
            (false, None) => Ok((Arg::Flag(*flag, ""), 1)),
            (false, Some(_)) => Err(Refused { at, why: Why::UnknownFlag }),
        };
    }
    let bytes = arg.as_bytes();
    let Some((_, flag, takes)) = bytes.get(1).and_then(|letter| SHORT.iter().find(|(short, _, _)| short == letter)) else {
        return Err(Refused { at, why: Why::UnknownFlag });
    };
    if *takes {
        let rest = &arg[2..];
        return valued(*flag, (!rest.is_empty()).then_some(rest));
    }
    // Joined letters without values (`-vV`): each must be one; the flag read is the last
    // that matters, and a version among them makes it a version ask (see `classify`).
    if bytes.len() > 2 {
        if !bytes[1..].iter().all(|letter| SHORT.iter().any(|(short, _, takes)| short == letter && !takes)) {
            return Err(Refused { at, why: Why::UnknownFlag });
        }
        return Ok((Arg::Flag(Flag::Joined, arg), 1));
    }
    Ok((Arg::Flag(*flag, ""), 1))
}

/// `values` sorted, each once.
fn set<'a, 's>(mut values: AVec<'a, &'s str>) -> AVec<'a, &'s str> {
    values.sort_unstable();
    values.dedup();
    values
}

/// Reads the arguments of a spawn of `rustc` as one ask.
pub fn classify<'a, 's>(args: &[&'s str], alloc: Alloc<'a>) -> Result<Ask<'a, 's>, Refused> {
    let mut print: Option<&'s str> = None;
    let (mut version, mut verbose) = (false, false);
    let mut input: Option<(usize, Snippet<'s>)> = None;
    let (mut target, mut edition, mut cap_lints) = (None, None, None);
    let mut once: [bool; 3] = [false; 3];
    let mut crate_types: AVec<'a, &'s str> = Vec::new_in(alloc.clone());
    let mut cfgs: AVec<'a, &'s str> = Vec::new_in(alloc.clone());
    let mut check_cfgs: AVec<'a, &'s str> = Vec::new_in(alloc.clone());
    let mut flags: AVec<'a, (u8, &'s str)> = Vec::new_in(alloc);
    let (mut emitted, mut codegen, mut test) = (false, false, false);
    let mut emits = Emits(0);
    let (mut crate_name, mut out): (Option<&'s str>, Option<&'s str>) = (None, None);
    let mut at = 0;
    while at < args.len() {
        let (arg, took) = read(args, at)?;
        let single = |slot: &mut Option<&'s str>, value: &'s str| -> Result<(), Refused> {
            if slot.replace(value).is_some() { Err(Refused { at, why: Why::Repeated }) } else { Ok(()) }
        };
        match arg {
            Arg::Input(snippet) => {
                if input.is_some() {
                    return Err(Refused { at, why: Why::TwoInputs });
                }
                input = Some((at, snippet));
            }
            Arg::Flag(flag, value) => match flag {
                Flag::Print => {
                    print.get_or_insert(value);
                }
                Flag::Version => version = true,
                Flag::Verbose => verbose = true,
                Flag::Quiet => {}
                Flag::Joined => {
                    // Each letter is a short flag without a value (`read`): `V`, `v`, `O`, `g`.
                    let letters = &value.as_bytes()[1..];
                    version |= letters.contains(&b'V');
                    verbose |= letters.contains(&b'v');
                    if letters.contains(&b'O') {
                        flags.push((b'O', ""));
                    }
                    if letters.contains(&b'g') {
                        flags.push((b'g', ""));
                    }
                }
                Flag::CrateName | Flag::Out | Flag::OutDir => {
                    let slot = match flag {
                        Flag::CrateName => 0,
                        Flag::Out => 1,
                        _ => 2,
                    };
                    if core::mem::replace(&mut once[slot], true) {
                        return Err(Refused { at, why: Why::Repeated });
                    }
                    match flag {
                        Flag::CrateName => crate_name = Some(value),
                        Flag::Out => out = Some(value),
                        _ => {}
                    }
                }
                Flag::Target => single(&mut target, value)?,
                Flag::Edition => single(&mut edition, value)?,
                // rustc takes `--cap-lints` again (thiserror 2.0.16's build/probe.rs passes
                // `--cap-lints=allow` twice, and its probe passes under cargo): the same level
                // again is the same question, keyed as one. Two levels are refused by name:
                // which one rustc applies is not read here (getopts' `opt_str` gives the first,
                // getopts 0.2.24 lib.rs:900; rustc's reading of it is not quoted), so no guess.
                Flag::CapLints => match cap_lints {
                    Some(held) if held == value => {}
                    Some(_) => return Err(Refused { at, why: Why::Differs }),
                    None => cap_lints = Some(value),
                },
                Flag::CrateType => crate_types.extend(value.split(',').filter(|kind| !kind.is_empty())),
                Flag::Cfg => cfgs.push(value),
                Flag::CheckCfg => check_cfgs.push(value),
                Flag::Emit => {
                    emitted = true;
                    for kind in value.split(',') {
                        let kind = kind.split_once('=').map_or(kind, |(kind, _)| kind);
                        let Some(bit) = EMIT_KINDS.iter().position(|k| *k == kind) else {
                            return Err(Refused { at, why: Why::Emit });
                        };
                        emits.0 |= 1 << bit;
                        codegen |= !matches!(kind, "metadata" | "dep-info");
                    }
                }
                Flag::Test => test = true,
                Flag::Codegen => flags.push((b'C', value)),
                Flag::Unstable => flags.push((b'Z', value)),
                Flag::Lint(level) => flags.push((level, value)),
                Flag::Extern => flags.push((b'e', value)),
                Flag::Search => flags.push((b'L', value)),
                Flag::Link => flags.push((b'l', value)),
                Flag::Sysroot => flags.push((b's', value)),
                Flag::Optimize => flags.push((b'O', "")),
                Flag::Debuginfo => flags.push((b'g', "")),
            },
        }
        at += took;
    }
    // rustc answers a print request, then a version, before it reads any input.
    if let Some(what) = print {
        return Ok(Ask::Print { what, target });
    }
    if version {
        return Ok(Ask::Version { verbose });
    }
    let Some((input_at, input)) = input else {
        return Err(Refused { at: args.len(), why: Why::NoInput });
    };
    Ok(Ask::Compile(Compile {
        input,
        input_at,
        target,
        edition,
        cap_lints,
        crate_types: set(crate_types),
        cfgs: set(cfgs),
        check_cfgs: set(check_cfgs),
        flags,
        codegen: codegen || !emitted,
        test,
        emits: if emitted { emits } else { Emits::LINK_ONLY },
        crate_name,
        out,
    }))
}

impl<'s> Compile<'_, 's> {
    /// The target the probe asks about: its `--target`, else the host (rustc's default).
    pub fn target_or<'t>(&self, host: &'t str) -> &'t str
    where
        's: 't,
    {
        self.target.unwrap_or(host)
    }

    /// The probe's key (module docs): what its verdict is a function of, normalized.
    pub fn key(&self, host: &str, snippet: &[u8]) -> ContentId {
        let mut h = Hasher::new(KEY_DOMAIN);
        h.part(RUSTC_V.as_bytes()).part(COMMIT_HASH.as_bytes()).part(self.target_or(host).as_bytes());
        let list = |h: &mut Hasher, label: &[u8], values: &[&str]| {
            h.part(label).update(&(values.len() as u64).to_le_bytes());
            for value in values {
                h.part(value.as_bytes());
            }
        };
        match self.crate_types.is_empty() {
            true => list(&mut h, b"crate types", &["bin"]),
            false => list(&mut h, b"crate types", &self.crate_types),
        }
        h.part(b"edition").part(self.edition.unwrap_or("2015").as_bytes());
        h.part(b"cap-lints").opt_part(self.cap_lints);
        h.update(&[u8::from(self.codegen), u8::from(self.test)]);
        list(&mut h, b"cfg", &self.cfgs);
        list(&mut h, b"check-cfg", &self.check_cfgs);
        h.part(b"flags").update(&(self.flags.len() as u64).to_le_bytes());
        for (letter, value) in self.flags.iter() {
            h.update(&[*letter]).part(value.as_bytes());
        }
        h.part(b"<snippet>").part(snippet);
        h.finish()
    }
}

/// The key of the fact "the pinned toolchain ships the crate `krate` for `target`": a row of
/// the answers table, made from the toolchain's sysroot of the side whose target it is.
pub fn ships_key(target: &str, krate: &str) -> ContentId {
    let mut h = Hasher::new(SHIPS_DOMAIN);
    h.part(RUSTC_V.as_bytes()).part(COMMIT_HASH.as_bytes()).part(target.as_bytes()).part(krate.as_bytes());
    h.finish()
}

/// The [`ships_key`] rows of `target` for each crate of `crates` (unsorted: the caller sorts
/// the whole table).
pub fn ships_rows<'a, 'c>(target: &str, crates: impl IntoIterator<Item = &'c str>, alloc: Alloc<'a>) -> AVec<'a, Row> {
    let mut rows = Vec::new_in(alloc);
    for krate in crates {
        rows.push(Row { key: ships_key(target, krate), accepted: true });
    }
    rows
}

/// The key of the fact "the crates a probe of `needs` loads for `target` declare the library
/// feature `name`" (rustc's `lib_features`): a row accepted for a declared name, rejected for
/// a name only mentioned in a macro's tokens or a body (known neither way).
pub fn feature_key(target: &str, needs: Needs, name: &str) -> ContentId {
    let mut h = Hasher::new(FEATURE_DOMAIN);
    h.part(RUSTC_V.as_bytes()).part(COMMIT_HASH.as_bytes()).part(target.as_bytes()).part(needs.krate().as_bytes());
    h.update(&[1]).part(name.as_bytes());
    h.finish()
}

/// The key of the fact "the library features of `needs` for `target` are listed": with it, a
/// name no [`feature_key`] row holds is declared by none of the crates loaded (E0635).
pub fn features_listed_key(target: &str, needs: Needs) -> ContentId {
    let mut h = Hasher::new(FEATURE_DOMAIN);
    h.part(RUSTC_V.as_bytes()).part(COMMIT_HASH.as_bytes()).part(target.as_bytes()).part(needs.krate().as_bytes());
    h.update(&[0]);
    h.finish()
}

/// The rows of one listing (unsorted: the caller sorts the whole table): the listed fact, each
/// `declared` name accepted, each `mentioned` one (never declared) rejected.
pub fn feature_rows<'a, 'n>(target: &str, needs: Needs, declared: &[&'n str], mentioned: &[&'n str], alloc: Alloc<'a>) -> AVec<'a, Row> {
    let mut rows = Vec::with_capacity_in(1 + declared.len() + mentioned.len(), alloc);
    rows.push(Row { key: features_listed_key(target, needs), accepted: true });
    for name in declared {
        rows.push(Row { key: feature_key(target, needs, name), accepted: true });
    }
    for name in mentioned {
        rows.push(Row { key: feature_key(target, needs, name), accepted: false });
    }
    rows
}

/// The crate a snippet with no items needs from the toolchain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Needs {
    /// No `#![no_std]`: the std prelude and `extern crate std`.
    Std,
    /// `#![no_std]`: `core` alone.
    Core,
}

impl Needs {
    pub fn krate(self) -> &'static str {
        match self {
            Needs::Std => "std",
            Needs::Core => "core",
        }
    }
}

/// Why a probe is refused by the compiler it asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rejection {
    /// `--edition` names no edition the pinned nightly has.
    Edition,
    /// A `bin` crate with no `fn main` (E0601).
    NoMain,
    /// A row of the table says the pinned nightly rejects it.
    Table,
    /// `#![feature(name)]` of a language feature the pinned nightly removed (E0557); `name` is
    /// the snippet's bytes `at..at + len`.
    RemovedFeature { at: u32, len: u32 },
    /// `#![feature(name)]` of a name that is no language feature and that no crate the probe
    /// loads declares for its target (E0635); `name` is the snippet's bytes `at..at + len`.
    UnknownFeature { at: u32, len: u32 },
}

impl Rejection {
    /// A short stderr naming the first error, as rustc's first line says it. `snippet` is the
    /// probe's, which a feature's range is in.
    pub fn stderr<'a>(self, compile: &Compile<'_, '_>, snippet: &[u8], alloc: Alloc<'a>) -> AString<'a> {
        let mut out = String::new_in(alloc);
        match self {
            Rejection::Edition => {
                out.push_str("error: argument for `--edition` must be one of: 2015|2018|2021|2024. (instead was `");
                out.push_str(compile.edition.unwrap_or(""));
                out.push_str("`)\n");
            }
            Rejection::NoMain => out.push_str("error[E0601]: `main` function not found in crate\n"),
            Rejection::Table => out.push_str("error: the pinned nightly rejects this probe (blaze's table of answers)\n"),
            Rejection::RemovedFeature { .. } => out.push_str("error[E0557]: feature has been removed\n"),
            Rejection::UnknownFeature { at, len } => {
                let name = snippet.get(at as usize..(at + len) as usize).and_then(|name| core::str::from_utf8(name).ok());
                out.push_str("error[E0635]: unknown feature `");
                out.push_str(name.expect("a feature's range is in the snippet decide read as UTF-8"));
                out.push_str("`\n");
            }
        }
        out
    }
}

/// Why [`decide`] leaves a probe to the table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Open {
    /// The snippet has an item (or anything but inner attributes and comments) at this byte:
    /// a names-and-types check of it needs the sysroot crates' scopes.
    Items { at: usize },
    /// An inner attribute whose effect it does not model (`cfg_attr`, `deny`, ...), or a
    /// `#![feature(..)]` it does not read (a name twice, a list past [`MOST_FEATURES`]).
    Attribute { at: usize },
    /// The library feature named at this byte is only mentioned in the loaded crates (inside a
    /// macro's tokens or a body: declared only once expanded), so known neither way.
    Feature { at: usize },
    /// A library feature is named and the table holds no listing of `needs`'s features for the
    /// target ([`features_listed_key`]).
    Features(Needs),
    /// A flag it does not model (`-C`, `-Z`, `-D`, `--extern`, `--check-cfg`, `--test`, ...).
    Flag,
    /// A crate type other than `lib`, `rlib` and `bin`, or `bin` under `no_std`.
    CrateType,
    /// The `future` edition (unstable: needs `-Z unstable-options`).
    Edition,
    /// The snippet is not UTF-8, or a comment does not end.
    Text,
    /// Decided accepted if the toolchain ships this crate for the target, and the table holds
    /// no [`ships_key`] row saying it does (no sysroot of that target was handed).
    Sysroot(Needs),
}

/// What [`decide`] makes of a probe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decided {
    /// Accepted if the toolchain ships `needs` for the target and, with `library`, the crates
    /// it loads declare each library feature `#![feature]` names ([`answer`] reads the table).
    Accepted { needs: Needs, library: bool },
    Rejected(Rejection),
    Open(Open),
}

/// Inner attributes blaze knows have no effect on whether a crate of no items compiles: lint
/// levels that only lower or keep (`allow`, `warn`, `expect`: an unknown lint name is the
/// `unknown_lints` warning; an unmet expectation is a warning).
const HARMLESS: &[&str] = &["allow", "expect", "warn"];

/// The probe's verdict from its own text, where that is enough (module docs).
pub fn decide(compile: &Compile<'_, '_>, snippet: &[u8]) -> Decided {
    // The edition is read with the options, before any input.
    match compile.edition {
        None | Some("2015" | "2018" | "2021" | "2024") => {}
        Some("future") => return Decided::Open(Open::Edition),
        Some(_) => return Decided::Rejected(Rejection::Edition),
    }
    if compile.test || !compile.check_cfgs.is_empty() || compile.flags.iter().any(|(letter, _)| !matches!(letter, b'A' | b'W')) {
        return Decided::Open(Open::Flag);
    }
    if !compile.cfgs.iter().all(|cfg| plain_cfg(cfg)) {
        return Decided::Open(Open::Flag);
    }
    let Ok(text) = core::str::from_utf8(snippet) else { return Decided::Open(Open::Text) };
    let inner = match inner_attributes(text) {
        Ok(inner) => inner,
        Err(open) => return Decided::Open(open),
    };
    let no_std = inner.no_std;
    // The features are read in expansion, before the crate type's E0601: a removed language
    // feature is the first error whatever else holds.
    let mut library = false;
    for &(at, name) in inner.features() {
        match blaze_surface::lang_features::lang(name) {
            Some(Lang::Removed) => return Decided::Rejected(Rejection::RemovedFeature { at: at as u32, len: name.len() as u32 }),
            Some(Lang::Accepted | Lang::Unstable) => {}
            None => library = true,
        }
    }
    let needs = if no_std { Needs::Core } else { Needs::Std };
    let types: &[&str] = if compile.crate_types.is_empty() { &["bin"] } else { &compile.crate_types };
    if types.iter().all(|kind| matches!(*kind, "lib" | "rlib")) {
        return Decided::Accepted { needs, library };
    }
    if types.contains(&"bin") && !no_std && types.iter().all(|kind| matches!(*kind, "lib" | "rlib" | "bin")) {
        return Decided::Rejected(Rejection::NoMain);
    }
    Decided::Open(Open::CrateType)
}

/// A `--cfg` value rustc takes: `name` or `name="value"`, the name an identifier.
fn plain_cfg(cfg: &str) -> bool {
    let (name, value) = match cfg.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (cfg, None),
    };
    let ident = !name.is_empty() && name.bytes().enumerate().all(|(at, b)| b == b'_' || b.is_ascii_alphabetic() || (at > 0 && b.is_ascii_digit()));
    ident && value.is_none_or(|v| v.len() >= 2 && v.starts_with('"') && v.ends_with('"') && !v[1..v.len() - 1].contains(['"', '\\']))
}

/// The most names the `#![feature(..)]` attributes of a probe blaze decides may give.
pub const MOST_FEATURES: usize = 32;

/// What a crate of inner attributes says.
struct Inner<'t> {
    no_std: bool,
    /// Each name `#![feature(..)]` gives, with its byte, in order, each once.
    features: [(usize, &'t str); MOST_FEATURES],
    count: usize,
}

impl<'t> Inner<'t> {
    fn features(&self) -> &[(usize, &'t str)] {
        &self.features[..self.count]
    }
}

/// Reads a crate made only of whitespace, comments and inner attributes `#![name]` or
/// `#![name(..)]`: whether it says `no_std`, and the features `#![feature(a, b)]` names. An
/// item, an attribute not `no_std`, `feature` or in [`HARMLESS`], or anything else is
/// [`Open`], at its byte.
fn inner_attributes(text: &str) -> Result<Inner<'_>, Open> {
    let bytes = text.as_bytes();
    let mut at = if text.starts_with('\u{feff}') { 3 } else { 0 };
    let mut inner = Inner { no_std: false, features: [(0, ""); MOST_FEATURES], count: 0 };
    loop {
        at = skip_trivia(bytes, at)?;
        if at == bytes.len() {
            return Ok(inner);
        }
        let start = at;
        if !bytes[at..].starts_with(b"#![") {
            return Err(Open::Items { at });
        }
        at += 3;
        let name_start = at;
        while at < bytes.len() && (bytes[at] == b'_' || bytes[at].is_ascii_alphanumeric()) {
            at += 1;
        }
        let name = &text[name_start..at];
        at = skip_trivia(bytes, at)?;
        let with_args = bytes.get(at) == Some(&b'(');
        let args_at = at;
        if with_args {
            at = balanced(bytes, at).ok_or(Open::Text)?;
        }
        let args_end = at;
        if with_args {
            at = skip_trivia(bytes, at)?;
        }
        if bytes.get(at) != Some(&b']') {
            return Err(Open::Attribute { at: start });
        }
        at += 1;
        match (name, with_args) {
            ("no_std", false) => inner.no_std = true,
            ("feature", true) => feature_names(text, args_at + 1, args_end - 1, start, &mut inner)?,
            (name, true) if HARMLESS.contains(&name) => {}
            _ => return Err(Open::Attribute { at: start }),
        }
    }
}

/// The names of `#![feature(..)]` (the attribute at `attr`), whose list is `text[from..to]`:
/// identifiers between commas (a trailing comma allowed), each new, into `inner`.
fn feature_names<'t>(text: &'t str, from: usize, to: usize, attr: usize, inner: &mut Inner<'t>) -> Result<(), Open> {
    let list = &text[from..to];
    let pieces = list.split(',').count();
    let mut offset = from;
    for (index, piece) in list.split(',').enumerate() {
        let name = piece.trim_matches(|c: char| c.is_ascii_whitespace());
        let at = offset + (piece.len() - piece.trim_start_matches(|c: char| c.is_ascii_whitespace()).len());
        offset += piece.len() + 1;
        if name.is_empty() && index + 1 == pieces {
            break;
        }
        let ident = !name.is_empty() && name.bytes().enumerate().all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()));
        if !ident || inner.count == MOST_FEATURES || inner.features().iter().any(|(_, seen)| *seen == name) {
            return Err(Open::Attribute { at: attr });
        }
        inner.features[inner.count] = (at, name);
        inner.count += 1;
    }
    Ok(())
}

/// Past whitespace and comments from `at`. A doc comment is an item's or the crate's
/// documentation: an outer one (`///`, `/**`) is [`Open::Items`]; an inner one is trivia.
fn skip_trivia(bytes: &[u8], mut at: usize) -> Result<usize, Open> {
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let rest = &bytes[at..];
        if rest.starts_with(b"//") {
            if rest.starts_with(b"///") && !rest.starts_with(b"////") {
                return Err(Open::Items { at });
            }
            at += rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
        } else if rest.starts_with(b"/*") {
            if rest.starts_with(b"/**") && !rest.starts_with(b"/**/") && !rest.starts_with(b"/***") {
                return Err(Open::Items { at });
            }
            let mut depth = 0usize;
            loop {
                if bytes[at..].starts_with(b"/*") {
                    depth += 1;
                    at += 2;
                } else if bytes[at..].starts_with(b"*/") {
                    depth -= 1;
                    at += 2;
                    if depth == 0 {
                        break;
                    }
                } else if at < bytes.len() {
                    at += 1;
                } else {
                    return Err(Open::Text);
                }
            }
        } else {
            return Ok(at);
        }
    }
}

/// Past the parenthesized group opening at `at` (strings inside read whole); `None` when it
/// does not close.
fn balanced(bytes: &[u8], mut at: usize) -> Option<usize> {
    let mut depth = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            b'"' => {
                at += 1;
                while at < bytes.len() && bytes[at] != b'"' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
            }
            _ => {}
        }
        at += 1;
    }
    None
}

/// Who gave a verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum By {
    /// [`decide`], with the table's [`ships_key`] fact.
    Decided,
    /// A row of the table.
    Table,
}

/// A try-compile's verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Accepted(By),
    Rejected(Rejection),
    /// Nobody can answer it yet: the runtime's `CannotAnswerYet`, naming the key.
    Unanswered(Open),
}

/// A try-compile answered: its key, the target it asked about, the verdict.
#[derive(Clone, Copy, Debug)]
pub struct Answered<'t> {
    pub key: ContentId,
    pub target: &'t str,
    pub verdict: Verdict,
}

/// The one place a try-compile is answered (module docs): `host` is the run's host, `table`
/// its answers (the pinned rows and the [`ships_key`] facts of its side).
pub fn answer<'t>(compile: &Compile<'_, 't>, host: &'t str, snippet: &[u8], table: &Table<'_>) -> Answered<'t> {
    let target = compile.target_or(host);
    let key = compile.key(host, snippet);
    let open = match decide(compile, snippet) {
        Decided::Accepted { needs, library } => match table.answer(&ships_key(target, needs.krate())) {
            Some(true) if !library => return Answered { key, target, verdict: Verdict::Accepted(By::Decided) },
            Some(true) => match library_features(snippet, target, needs, table) {
                Ok(()) => return Answered { key, target, verdict: Verdict::Accepted(By::Decided) },
                Err(Ok(rejection)) => return Answered { key, target, verdict: Verdict::Rejected(rejection) },
                Err(Err(open)) => open,
            },
            // No row says the toolchain ships it for this target: not known here.
            Some(false) | None => Open::Sysroot(needs),
        },
        Decided::Rejected(rejection) => return Answered { key, target, verdict: Verdict::Rejected(rejection) },
        Decided::Open(open) => open,
    };
    let verdict = match table.answer(&key) {
        Some(true) => Verdict::Accepted(By::Table),
        Some(false) => Verdict::Rejected(Rejection::Table),
        None => Verdict::Unanswered(open),
    };
    Answered { key, target, verdict }
}

/// Whether the crates a probe of `needs` loads for `target` declare each library feature (a
/// name no language feature) the snippet's `#![feature]` gives, by the table's listing: all
/// declared is `Ok`; the first name declared by none while the listing is there is E0635
/// (`Err(Ok)`), certain whatever the others are; else the first name known neither way
/// (`Err(Err)`).
fn library_features(snippet: &[u8], target: &str, needs: Needs, table: &Table<'_>) -> Result<(), Result<Rejection, Open>> {
    let text = core::str::from_utf8(snippet).map_err(|_| Err(Open::Text))?;
    let inner = inner_attributes(text).map_err(Err)?;
    let listed = table.answer(&features_listed_key(target, needs)) == Some(true);
    let mut open = None;
    for &(at, name) in inner.features() {
        if blaze_surface::lang_features::lang(name).is_some() {
            continue;
        }
        match table.answer(&feature_key(target, needs, name)) {
            Some(true) => {}
            Some(false) => {
                open.get_or_insert(Open::Feature { at });
            }
            None if listed => return Err(Ok(Rejection::UnknownFeature { at: at as u32, len: name.len() as u32 })),
            None => {
                open.get_or_insert(Open::Features(needs));
            }
        }
    }
    match open {
        Some(open) => Err(Err(open)),
        None => Ok(()),
    }
}
