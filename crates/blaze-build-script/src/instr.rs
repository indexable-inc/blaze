//! The instruction lines a build script prints, read into typed values.
//!
//! The statement is the Cargo Book's "Build Scripts" chapter, "Outputs of the Build Script":
//! a line `cargo::KEY=VALUE` (Cargo 1.77 and later) or `cargo:KEY=VALUE` (every Cargo) is an
//! instruction; every other line is the script's own and is not read. The keys:
//!
//! | key | value |
//! |---|---|
//! | `rustc-cfg` | `NAME` or `NAME="VALUE"` |
//! | `rustc-check-cfg` | `cfg(...)`, as rustc's `--check-cfg` takes it |
//! | `rustc-env` | `VAR=VALUE` |
//! | `rustc-link-lib` | `[KIND[:MODIFIERS]=]NAME[:RENAME]` |
//! | `rustc-link-search` | `[KIND=]PATH`, KIND one of `dependency crate native framework all` |
//! | `rustc-link-arg`, `-bins`, `-tests`, `-examples`, `-benches`, `rustc-cdylib-link-arg` (and `rustc-link-arg-cdylib`) | `FLAG` |
//! | `rustc-link-arg-bin` | `BIN=FLAG` |
//! | `rustc-flags` | `-l NAME` and `-L PATH` (the only flags it may carry) |
//! | `rerun-if-changed`, `rerun-if-env-changed` | a path, a variable |
//! | `warning`, `error` | a message |
//! | `metadata` (new syntax only) | `KEY=VALUE` |
//!
//! In the old syntax a key that is none of these is `links` metadata (`cargo:KEY=VALUE`); in
//! the new syntax it is [`Line::Unknown`], a typed warning. As Cargo reads it, the value is
//! everything after the first `=`, with trailing whitespace (a `\r`) taken off.
//!
//! [`lines`] walks a whole output once, refusing a line longer than its limit and an output
//! longer than its own ([`OutputError`]): what a hostile script prints is bounded before it is
//! read.

#![blaze::sans_io]

/// Which spelling a line used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Syntax {
    /// `cargo:KEY=VALUE`.
    Old,
    /// `cargo::KEY=VALUE`.
    New,
}

/// The kind of a `rustc-link-search` path.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum SearchKind {
    /// No kind written: rustc's `all`.
    Unspecified = 0,
    Dependency = 1,
    Crate = 2,
    Native = 3,
    Framework = 4,
    All = 5,
}

impl SearchKind {
    pub fn from_u8(byte: u8) -> Option<SearchKind> {
        Some(match byte {
            0 => SearchKind::Unspecified,
            1 => SearchKind::Dependency,
            2 => SearchKind::Crate,
            3 => SearchKind::Native,
            4 => SearchKind::Framework,
            5 => SearchKind::All,
            _ => return None,
        })
    }
}

/// Which of the package's targets a link argument is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkTo<'a> {
    /// `rustc-link-arg`.
    All,
    /// `rustc-cdylib-link-arg`, `rustc-link-arg-cdylib`.
    Cdylib,
    /// `rustc-link-arg-bins`.
    Bins,
    /// `rustc-link-arg-bin=BIN=FLAG`.
    Bin(&'a str),
    /// `rustc-link-arg-tests`.
    Tests,
    /// `rustc-link-arg-examples`.
    Examples,
    /// `rustc-link-arg-benches`.
    Benches,
}

impl LinkTo<'_> {
    /// Its byte in the outputs record.
    pub fn code(self) -> u8 {
        match self {
            LinkTo::All => 0,
            LinkTo::Cdylib => 1,
            LinkTo::Bins => 2,
            LinkTo::Bin(_) => 3,
            LinkTo::Tests => 4,
            LinkTo::Examples => 5,
            LinkTo::Benches => 6,
        }
    }
}

/// One instruction, its values borrowed from the line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Instr<'a> {
    Cfg { name: &'a str, value: Option<&'a str> },
    CheckCfg(&'a str),
    Env { name: &'a str, value: &'a str },
    LinkLib(&'a str),
    LinkSearch { kind: SearchKind, path: &'a str },
    LinkArg { to: LinkTo<'a>, arg: &'a str },
    Metadata { key: &'a str, value: &'a str },
    /// Advice for a builder that cannot see what a script reads; blaze tracks the reads.
    RerunIfChanged(&'a str),
    /// The same, for a variable.
    RerunIfEnvChanged(&'a str),
    Warning(&'a str),
    /// The script says it failed.
    Error(&'a str),
}

/// Why an instruction line is not one Cargo accepts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// No `=` after the key.
    NoValue,
    /// The line is not UTF-8.
    NotUtf8,
    /// `rustc-cfg`'s name is not an identifier, or its value is not one quoted string.
    Cfg,
    /// `rustc-env` without `VAR=`, or with an empty name.
    Env,
    /// `rustc-link-search` with a kind Cargo does not know, or an empty path.
    LinkSearch,
    /// An empty `rustc-link-lib`.
    LinkLib,
    /// `rustc-link-arg-bin` without `BIN=`.
    LinkArgBin,
    /// `metadata` without `KEY=`.
    Metadata,
    /// `rustc-flags` with a flag other than `-l` and `-L`, or one without its value.
    Flags,
}

/// One line of output, read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Line<'a> {
    /// Not an instruction: the script's own output.
    Plain,
    Instr(Syntax, Instr<'a>),
    /// `rustc-flags=...`: [`flags`] reads its value.
    Flags(Syntax, &'a str),
    /// A `cargo::` key Cargo does not document: a typed warning.
    Unknown { key: &'a str },
    Malformed { key: &'a str, why: Malformed },
}

/// Reads one line, without its line break.
pub fn parse(line: &[u8]) -> Line<'_> {
    let (syntax, rest) = match line.strip_prefix(b"cargo::") {
        Some(rest) => (Syntax::New, rest),
        None => match line.strip_prefix(b"cargo:") {
            Some(rest) => (Syntax::Old, rest),
            None => return Line::Plain,
        },
    };
    let Ok(rest) = core::str::from_utf8(rest) else {
        return Line::Malformed { key: "", why: Malformed::NotUtf8 };
    };
    let Some((key, value)) = rest.split_once('=') else {
        return Line::Malformed { key: rest.trim_end(), why: Malformed::NoValue };
    };
    let value = value.trim_end();
    let instr = match key {
        "rustc-cfg" => match cfg(value) {
            Some((name, value)) => Instr::Cfg { name, value },
            None => return Line::Malformed { key, why: Malformed::Cfg },
        },
        "rustc-check-cfg" => Instr::CheckCfg(value),
        "rustc-env" => match value.split_once('=') {
            Some((name, value)) if !name.is_empty() => Instr::Env { name, value },
            _ => return Line::Malformed { key, why: Malformed::Env },
        },
        "rustc-link-lib" => match value.is_empty() {
            true => return Line::Malformed { key, why: Malformed::LinkLib },
            false => Instr::LinkLib(value),
        },
        "rustc-link-search" => match search(value) {
            Some((kind, path)) => Instr::LinkSearch { kind, path },
            None => return Line::Malformed { key, why: Malformed::LinkSearch },
        },
        "rustc-link-arg" => Instr::LinkArg { to: LinkTo::All, arg: value },
        "rustc-cdylib-link-arg" | "rustc-link-arg-cdylib" => Instr::LinkArg { to: LinkTo::Cdylib, arg: value },
        "rustc-link-arg-bins" => Instr::LinkArg { to: LinkTo::Bins, arg: value },
        "rustc-link-arg-tests" => Instr::LinkArg { to: LinkTo::Tests, arg: value },
        "rustc-link-arg-examples" => Instr::LinkArg { to: LinkTo::Examples, arg: value },
        "rustc-link-arg-benches" => Instr::LinkArg { to: LinkTo::Benches, arg: value },
        "rustc-link-arg-bin" => match value.split_once('=') {
            Some((bin, arg)) if !bin.is_empty() => Instr::LinkArg { to: LinkTo::Bin(bin), arg },
            _ => return Line::Malformed { key, why: Malformed::LinkArgBin },
        },
        "rustc-flags" => return Line::Flags(syntax, value),
        "rerun-if-changed" => Instr::RerunIfChanged(value),
        "rerun-if-env-changed" => Instr::RerunIfEnvChanged(value),
        "warning" => Instr::Warning(value),
        "error" => Instr::Error(value),
        "metadata" if syntax == Syntax::New => match value.split_once('=') {
            Some((key, value)) if !key.is_empty() => Instr::Metadata { key, value },
            _ => return Line::Malformed { key, why: Malformed::Metadata },
        },
        _ => match syntax {
            Syntax::Old => Instr::Metadata { key, value },
            Syntax::New => return Line::Unknown { key },
        },
    };
    Line::Instr(syntax, instr)
}

fn is_ident(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_') && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `NAME` or `NAME="VALUE"` (a `r#` raw name allowed, as rustc's `--cfg` takes it).
fn cfg(text: &str) -> Option<(&str, Option<&str>)> {
    let (name, value) = match text.split_once('=') {
        None => (text, None),
        Some((name, quoted)) => {
            let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
            // One string: no unescaped quote inside.
            let mut escaped = false;
            for byte in inner.bytes() {
                match (escaped, byte) {
                    (false, b'\\') => escaped = true,
                    (false, b'"') => return None,
                    _ => escaped = false,
                }
            }
            (name, Some(inner))
        }
    };
    let bare = name.strip_prefix("r#").unwrap_or(name);
    is_ident(bare).then_some((name, value))
}

fn search(text: &str) -> Option<(SearchKind, &str)> {
    let (kind, path) = match text.split_once('=') {
        Some(("dependency", path)) => (SearchKind::Dependency, path),
        Some(("crate", path)) => (SearchKind::Crate, path),
        Some(("native", path)) => (SearchKind::Native, path),
        Some(("framework", path)) => (SearchKind::Framework, path),
        Some(("all", path)) => (SearchKind::All, path),
        // A kind is a lowercase word; anything else before `=` is part of a path.
        Some((kind, _)) if !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_lowercase()) => return None,
        _ => (SearchKind::Unspecified, text),
    };
    (!path.is_empty()).then_some((kind, path))
}

/// The instructions of a `rustc-flags` value: `-l NAME`, `-lNAME`, `-L PATH`, `-LPATH`, as
/// Cargo reads it (whitespace-separated; any other flag refused).
pub fn flags(value: &str) -> Flags<'_> {
    Flags { words: value.split_ascii_whitespace() }
}

/// What [`flags`] gives.
pub struct Flags<'a> {
    words: core::str::SplitAsciiWhitespace<'a>,
}

impl<'a> Iterator for Flags<'a> {
    type Item = Result<Instr<'a>, Malformed>;

    fn next(&mut self) -> Option<Self::Item> {
        let word = self.words.next()?;
        let (flag, inline) = match word.get(..2) {
            Some(flag @ ("-l" | "-L")) => (flag, &word[2..]),
            _ => return Some(Err(Malformed::Flags)),
        };
        let value = match inline.is_empty() {
            true => match self.words.next() {
                Some(value) => value,
                None => return Some(Err(Malformed::Flags)),
            },
            false => inline,
        };
        Some(Ok(match flag {
            "-l" => Instr::LinkLib(value),
            _ => match search(value) {
                Some((kind, path)) => Instr::LinkSearch { kind, path },
                None => return Some(Err(Malformed::LinkSearch)),
            },
        }))
    }
}

/// How much output is read.
#[derive(Clone, Copy, Debug)]
pub struct OutputLimits {
    /// The longest line, in bytes.
    pub line: usize,
    /// The whole output, in bytes.
    pub output: usize,
}

impl OutputLimits {
    /// 256 KiB a line (tempo's longest instruction is a 237-byte JSON), 16 MiB in all.
    pub const DEFAULT: OutputLimits = OutputLimits { line: 256 << 10, output: 16 << 20 };
}

/// Why an output is refused before any of it is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputError {
    /// Line `line` (from 1) is `len` bytes, over `limit`.
    LineTooLong { line: usize, len: usize, limit: usize },
    /// The output is `len` bytes, over `limit`.
    TooLarge { len: usize, limit: usize },
}

/// Every line of `output` (split at `\n`), each paired with its number from 1, after checking
/// the whole against `limits` in one pass.
pub fn lines(output: &[u8], limits: OutputLimits) -> Result<Lines<'_>, OutputError> {
    if output.len() > limits.output {
        return Err(OutputError::TooLarge { len: output.len(), limit: limits.output });
    }
    for (at, line) in output.split(newline as fn(&u8) -> bool).enumerate() {
        if line.len() > limits.line {
            return Err(OutputError::LineTooLong { line: at + 1, len: line.len(), limit: limits.line });
        }
    }
    Ok(Lines { rest: output.split(newline as fn(&u8) -> bool).enumerate() })
}

fn newline(byte: &u8) -> bool {
    *byte == b'\n'
}

/// What [`lines`] gives.
pub struct Lines<'a> {
    rest: core::iter::Enumerate<core::slice::Split<'a, u8, fn(&u8) -> bool>>,
}

impl<'a> Iterator for Lines<'a> {
    type Item = (usize, Line<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let (at, line) = self.rest.next()?;
        Some((at + 1, parse(line)))
    }
}
