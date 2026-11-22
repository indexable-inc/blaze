//! The pinned toolchain's facts as constants (D162, docs/v2/plan/design/end-state.md): the one
//! Rust reader of the repository's `pins/` folder (`pins/README.md` says what each file is).
//!
//! Every fact is made at compile time: the files are `include_str!`ed here, the only include of
//! a file outside its own crate's folder, and parsed by the `const fn`s below, so a malformed or
//! missing pin stops the build with an error naming the file and the line. Facts that two files
//! both state are checked to agree here too, at compile time (the nightly in `pins/toolchain`
//! and `pins/rust.txt`; rustc's version line and commit in `pins/rust.txt` and in the recorded
//! answers): nothing is copied, so no test holds copies equal.
//!
//! - [`RUST`]: `pins/rust.txt`, written by `harness/toolchain_pins.py` from the nightly's channel
//!   manifest: the toolchain's name ([`TOOLCHAIN`]), `rustc -V` ([`RUSTC_V`]) and its parts
//!   ([`RUSTC_VERSION`], [`RELEASE`], [`COMMIT_HASH`], [`COMMIT_DATE`]), `cargo -V` ([`CARGO_V`],
//!   [`CARGO_VERSION`]), and [`ARCHIVES`]: each file blaze downloads with its SHA-256 and size.
//! - [`RUSTC_LLVM`]: the `LLVM version:` of the pinned rustc's `-vV`, from the recorded answers
//!   (`pins/answers/rustc.json`, written by `harness/answers.py`); only those bytes are kept, not
//!   the 400 KB file.
//! - [`LLVM_JSON`], [`CLANG_JSON`]: the LLVM blaze links and the C toolchain native build scripts
//!   run with, whose readers (vendor/llvm-sys, blaze-build-script `toolchain`) parse them; their
//!   top-level [`LLVM_VERSION`], [`LLVM_COMMIT`] and [`CLANG_VERSION`] are read here.
//! - The `*_PATH` constants: each file's path from the repository's root, for messages and for
//!   the tools that read a checkout's files by path (scripts/src/bin/toolchain_cut.rs).
//!
//! No dependency, so that every crate can take the facts from here: blaze-manifest is below
//! blaze-toolchain (blaze-toolchain -> blaze-fetch -> blaze-manifest), so the facts cannot live
//! in blaze-toolchain itself.
//!
//! The one stop (`panic!`) is [`Msg::fail`]'s, the build error: every caller of it is a `const`
//! item's evaluation, so it stops a build, never a program. Bytes are read through [`at`] and
//! written through [`put`], which cannot index out of range.

#![no_std]

/// `pins/toolchain`: the nightly's name and a line feed, what rustup and the shell read.
pub const TOOLCHAIN_PATH: &str = "pins/toolchain";
/// `pins/rust.txt`: the nightly's version lines, commit and archives.
pub const RUST_PATH: &str = "pins/rust.txt";
/// `pins/llvm.json`: the LLVM blaze links.
pub const LLVM_PATH: &str = "pins/llvm.json";
/// `pins/clang.json`: the C toolchain, shelf programs and host tools native build scripts run with.
pub const CLANG_PATH: &str = "pins/clang.json";
/// `pins/answers/rustc.json`: what the pinned rustc answers, per target.
pub const ANSWERS_PATH: &str = "pins/answers/rustc.json";
/// `pins/answers/own_targets.json`: the same answers for blaze's own targets, which no rustc has.
pub const OWN_TARGETS_PATH: &str = "pins/answers/own_targets.json";
/// `pins/answers/clang.json`: what the pinned clang answers to a build script's compiler probes.
pub const CLANG_ANSWERS_PATH: &str = "pins/answers/clang.json";

const TOOLCHAIN_TEXT: &str = include_str!("../../../pins/toolchain");
/// `pins/rust.txt` as checked in.
pub const RUST_TXT: &str = include_str!("../../../pins/rust.txt");
/// `pins/llvm.json` as checked in.
pub const LLVM_JSON: &str = include_str!("../../../pins/llvm.json");
/// `pins/clang.json` as checked in.
pub const CLANG_JSON: &str = include_str!("../../../pins/clang.json");
/// `pins/answers/clang.json` as checked in: read by blaze-build-script `c_tasks_probe`, whose
/// digest is in every wasm build script's base key.
pub const CLANG_ANSWERS_JSON: &str = include_str!("../../../pins/answers/clang.json");
/// `pins/base-tools.json`: the pinned base tool set a sandboxed build step runs (D192).
pub const BASE_TOOLS_PATH: &str = "pins/base-tools.json";
/// `pins/base-tools.json` as checked in: read by blaze-base-tools `pins`, whose set's digest is
/// in the key of every step that ran one of its programs.
pub const BASE_TOOLS_JSON: &str = include_str!("../../../pins/base-tools.json");
/// Read at compile time only; never in a value a program keeps (the facts below copy out the
/// few bytes they need).
const ANSWERS_JSON: &str = include_str!("../../../pins/answers/rustc.json");

/// A downloaded file's pin: its SHA-256, 64 lower-case hex digits, and its size in bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FilePin {
    pub sha256: &'static str,
    pub bytes: u64,
}

/// One file of the nightly blaze downloads for a host: `<host> <package> <target> <sha256> <bytes>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Archive {
    pub host: &'static str,
    pub package: &'static str,
    pub target: &'static str,
    pub file: FilePin,
}

/// `pins/rust.txt`, read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rust {
    /// `nightly-2026-10-27`: the `release` line's first field.
    pub toolchain: &'static str,
    /// `2026-10-27`.
    pub date: &'static str,
    /// `nightly`.
    pub channel: &'static str,
    /// The `rustc` line whole: what `rustc -V` prints.
    pub rustc_v: &'static str,
    /// `rustc_v` after `rustc `.
    pub rustc_version: &'static str,
    /// The first word of `rustc_version`: `1.101.0-nightly`.
    pub release: &'static str,
    /// The date in `rustc_version`'s parentheses: the commit's date.
    pub commit_date: &'static str,
    /// The `cargo` line whole: what `cargo -V` prints.
    pub cargo_v: &'static str,
    /// `cargo_v` after `cargo `.
    pub cargo_version: &'static str,
    /// The rust-lang/rust commit, 40 hex digits.
    pub commit: &'static str,
    /// The channel manifest's own pin.
    pub manifest: FilePin,
}

/// `pins/rust.txt`, read at compile time.
pub const RUST: Rust = parse_rust(RUST_PATH, RUST_TXT);
/// The pinned nightly's name, `nightly-YYYY-MM-DD`.
pub const TOOLCHAIN: &str = RUST.toolchain;
/// The nightly's date.
pub const DATE: &str = RUST.date;
/// The nightly's channel, `nightly`.
pub const CHANNEL: &str = RUST.channel;
/// What `rustc -V` of the pinned nightly prints (without the line feed).
pub const RUSTC_V: &str = RUST.rustc_v;
/// [`RUSTC_V`] after `rustc `.
pub const RUSTC_VERSION: &str = RUST.rustc_version;
/// `release:` of `rustc -vV`.
pub const RELEASE: &str = RUST.release;
/// `commit-hash:` of `rustc -vV`.
pub const COMMIT_HASH: &str = RUST.commit;
/// `commit-date:` of `rustc -vV`.
pub const COMMIT_DATE: &str = RUST.commit_date;
/// What `cargo -V` of the pinned nightly prints (without the line feed).
pub const CARGO_V: &str = RUST.cargo_v;
/// [`CARGO_V`] after `cargo `: the form cargo names itself by in its messages.
pub const CARGO_VERSION: &str = RUST.cargo_version;

/// How many archive rows `pins/rust.txt` has.
pub const ARCHIVE_COUNT: usize = count_archives(RUST_TXT);
/// Every archive row of `pins/rust.txt`, in the file's order.
pub const ARCHIVES: [Archive; ARCHIVE_COUNT] = archives::<ARCHIVE_COUNT>(RUST_PATH, RUST_TXT);

/// The archive of `package` for `target` that `host` downloads, if `pins/rust.txt` pins one.
pub const fn archive(host: &str, package: &str, target: &str) -> Option<Archive> {
    let mut rest: &[Archive] = &ARCHIVES;
    while let Some((row, tail)) = rest.split_first() {
        if eq(row.host, host) && eq(row.package, package) && eq(row.target, target) {
            return Some(*row);
        }
        rest = tail;
    }
    None
}

/// `version` of `pins/llvm.json`.
pub const LLVM_VERSION: &str = json_str(LLVM_PATH, LLVM_JSON, 2, "version", false);
/// `commit` of `pins/llvm.json`: the fork's commit.
pub const LLVM_COMMIT: &str = json_str(LLVM_PATH, LLVM_JSON, 2, "commit", false);
/// `version` of `pins/clang.json`.
pub const CLANG_VERSION: &str = json_str(CLANG_PATH, CLANG_JSON, 2, "version", false);

/// The answers' top-level `version` and `verbose` (raw JSON string bodies, escapes kept): they
/// are the two last keys, so they are found from the end.
const ANSWERED_VERSION: &str = json_str(ANSWERS_PATH, ANSWERS_JSON, 1, "version", true);
const ANSWERED_VERBOSE: &str = json_str(ANSWERS_PATH, ANSWERS_JSON, 1, "verbose", true);
const RUSTC_LLVM_BYTES: Small = Small::of(ANSWERS_PATH, verbose_field(ANSWERED_VERBOSE, "LLVM version: "));
/// `LLVM version:` of `rustc -vV` of the pinned nightly, as the recorded answers hold it.
pub const RUSTC_LLVM: &str = Small::as_str(&RUSTC_LLVM_BYTES);

// The facts two files state agree, or the build stops naming both.
const _: () = {
    let text = TOOLCHAIN_TEXT.as_bytes();
    let name = RUST.toolchain.as_bytes();
    if !(text.len() == name.len() + 1 && at(text, name.len()) == b'\n' && starts(text, 0, name)) {
        Msg::new()
            .s(TOOLCHAIN_PATH)
            .s(":1: is not `")
            .s(RUST.toolchain)
            .s("` and a line feed, the nightly ")
            .s(RUST_PATH)
            .s("'s `release` line names: harness/toolchain_pins.py '{\"write\": true}' writes ")
            .s(RUST_PATH)
            .s(" from ")
            .s(TOOLCHAIN_PATH)
            .fail();
    }
    if !eq(ANSWERED_VERSION, RUST.rustc_version) {
        Msg::new()
            .s(ANSWERS_PATH)
            .s(": `version` is `")
            .s(ANSWERED_VERSION)
            .s("`, and ")
            .s(RUST_PATH)
            .s(" pins rustc `")
            .s(RUST.rustc_version)
            .s("`: the answers are of another nightly; run harness/answers.py --write on each host")
            .fail();
    }
    let commit = verbose_field(ANSWERED_VERBOSE, "commit-hash: ");
    if !eq(commit, RUST.commit) {
        Msg::new()
            .s(ANSWERS_PATH)
            .s(": `verbose` says commit-hash ")
            .s(commit)
            .s(", and ")
            .s(RUST_PATH)
            .s(" pins commit ")
            .s(RUST.commit)
            .s(": run harness/answers.py --write on each host")
            .fail();
    }
};

/// `pins/rust.txt`'s form: `#` lines and empty lines are comments; `release <toolchain> <date>
/// <channel>`, `rustc <rustc -V>`, `cargo <cargo -V>`, `commit <40 hex>` and `manifest <sha256>
/// <bytes>` each once; every other line an archive row `<host> <package> <target> <sha256>
/// <bytes>`. A line of no form stops the build, naming `path` and the line.
pub const fn parse_rust(path: &str, text: &'static str) -> Rust {
    let bytes = text.as_bytes();
    let mut toolchain: Option<&'static str> = None;
    let mut date = "";
    let mut channel = "";
    let mut rustc_v: Option<&'static str> = None;
    let mut cargo_v: Option<&'static str> = None;
    let mut commit: Option<&'static str> = None;
    let mut manifest: Option<FilePin> = None;
    let mut start = 0;
    let mut line = 1;
    while start < bytes.len() {
        let end = line_end(bytes, start);
        if end > start && at(bytes, start) != b'#' {
            let Some((key_start, key_end)) = word(bytes, start, end) else { bad(path, line, "a line of spaces") };
            let key = slice(path, text, key_start, key_end);
            if eq(key, "release") {
                if toolchain.is_some() {
                    bad(path, line, "a second `release` line")
                }
                if words(bytes, start, end) != 4 {
                    bad(path, line, "`release` takes <toolchain> <date> <channel>")
                }
                let Some((a, b)) = word(bytes, key_end, end) else { bad(path, line, "no toolchain") };
                let Some((c, d)) = word(bytes, b, end) else { bad(path, line, "no date") };
                let Some((e, f)) = word(bytes, d, end) else { bad(path, line, "no channel") };
                let name = slice(path, text, a, b);
                date = slice(path, text, c, d);
                channel = slice(path, text, e, f);
                if !is_date(date) {
                    bad(path, line, "the date is not YYYY-MM-DD")
                }
                // The toolchain is `<channel>-<date>`.
                let named = name.as_bytes();
                if !(named.len() == channel.len() + 1 + date.len()
                    && starts(named, 0, channel.as_bytes())
                    && at(named, channel.len()) == b'-'
                    && starts(named, channel.len() + 1, date.as_bytes()))
                {
                    bad(path, line, "the toolchain is not `<channel>-<date>`")
                }
                toolchain = Some(name);
            } else if eq(key, "rustc") {
                if rustc_v.is_some() {
                    bad(path, line, "a second `rustc` line")
                }
                rustc_v = Some(slice(path, text, start, end));
            } else if eq(key, "cargo") {
                if cargo_v.is_some() {
                    bad(path, line, "a second `cargo` line")
                }
                cargo_v = Some(slice(path, text, start, end));
            } else if eq(key, "commit") {
                if commit.is_some() {
                    bad(path, line, "a second `commit` line")
                }
                let Some((a, b)) = word(bytes, key_end, end) else { bad(path, line, "no commit") };
                let hash = slice(path, text, a, b);
                if words(bytes, start, end) != 2 || !is_hex(hash, 40) {
                    bad(path, line, "`commit` takes 40 lower-case hex digits")
                }
                commit = Some(hash);
            } else if eq(key, "manifest") {
                if manifest.is_some() {
                    bad(path, line, "a second `manifest` line")
                }
                if words(bytes, start, end) != 3 {
                    bad(path, line, "`manifest` takes <sha256> <bytes>")
                }
                manifest = Some(file_pin(path, line, text, key_end, end));
            } else {
                // An archive row, read whole by `archives`; here its form is checked.
                archive_row(path, line, text, start, end);
            }
        }
        start = end + 1;
        line += 1;
    }
    let Some(toolchain) = toolchain else { missing(path, "release") };
    let Some(rustc_v) = rustc_v else { missing(path, "rustc") };
    let Some(cargo_v) = cargo_v else { missing(path, "cargo") };
    let Some(commit) = commit else { missing(path, "commit") };
    let Some(manifest) = manifest else { missing(path, "manifest") };
    let rustc_version = version_after(path, rustc_v, "rustc ");
    let cargo_version = version_after(path, cargo_v, "cargo ");
    // `<release> (<short commit> <date>)`, as `rustc -V` prints it.
    let rb = rustc_version.as_bytes();
    let Some((r0, r1)) = word(rb, 0, rb.len()) else { missing(path, "rustc") };
    let release = slice(path, rustc_version, r0, r1);
    let Some((s0, s1)) = word(rb, r1, rb.len()) else { missing(path, "rustc") };
    let Some((d0, d1)) = word(rb, s1, rb.len()) else { missing(path, "rustc") };
    if !(at(rb, s0) == b'(' && at(rb, d1 - 1) == b')' && d1 == rb.len()) {
        Msg::new().s(path).s(": the `rustc` line is not `rustc <release> (<commit> <date>)`").fail()
    }
    let short = slice(path, rustc_version, s0 + 1, s1);
    let commit_date = slice(path, rustc_version, d0, d1 - 1);
    if short.is_empty() || !starts(commit.as_bytes(), 0, short.as_bytes()) || !is_date(commit_date) {
        Msg::new()
            .s(path)
            .s(": the `rustc` line's `(")
            .s(short)
            .s(" ")
            .s(commit_date)
            .s(")` is not a prefix of the `commit` line's ")
            .s(commit)
            .s(" and a date")
            .fail()
    }
    // cargo is built with the tree's release: `cargo -V` names it first.
    let cb = cargo_version.as_bytes();
    if !(cb.len() > release.len() && starts(cb, 0, release.as_bytes()) && at(cb, release.len()) == b' ') {
        Msg::new().s(path).s(": `cargo -V` says `").s(cargo_version).s("`, not rustc's release `").s(release).s("` first").fail()
    }
    Rust { toolchain, date, channel, rustc_v, rustc_version, release, commit_date, cargo_v, cargo_version, commit, manifest }
}

/// How many archive rows `text` (`pins/rust.txt`'s form) has.
pub const fn count_archives(text: &str) -> usize {
    let bytes = text.as_bytes();
    let (mut start, mut count) = (0, 0);
    while start < bytes.len() {
        let end = line_end(bytes, start);
        if end > start && at(bytes, start) != b'#' && is_archive_line(bytes, start, end) {
            count += 1;
        }
        start = end + 1;
    }
    count
}

/// The `N` archive rows of `text`, in order; a row of no form stops the build naming the line.
pub const fn archives<const N: usize>(path: &str, text: &'static str) -> [Archive; N] {
    const NONE: Archive = Archive { host: "", package: "", target: "", file: FilePin { sha256: "", bytes: 0 } };
    let bytes = text.as_bytes();
    let mut out = [NONE; N];
    let (mut start, mut line, mut filled) = (0, 1, 0);
    while start < bytes.len() {
        let end = line_end(bytes, start);
        if end > start && at(bytes, start) != b'#' && is_archive_line(bytes, start, end) {
            let row = archive_row(path, line, text, start, end);
            let (_, rest) = out.as_mut_slice().split_at_mut(if filled < N { filled } else { N });
            let Some(slot) = rest.first_mut() else { bad(path, line, "more archive rows than counted") };
            *slot = row;
            filled += 1;
        }
        start = end + 1;
        line += 1;
    }
    if filled != N {
        Msg::new().s(path).s(": fewer archive rows than counted").fail()
    }
    out
}

/// Whether the line `[start, end)` is an archive row: its first word is none of the keys.
const fn is_archive_line(bytes: &[u8], start: usize, end: usize) -> bool {
    let Some((a, b)) = word(bytes, start, end) else { return false };
    let key = part(bytes, a, b);
    !(eq_bytes(key, b"release") || eq_bytes(key, b"rustc") || eq_bytes(key, b"cargo") || eq_bytes(key, b"commit") || eq_bytes(key, b"manifest"))
}

const fn archive_row(path: &str, line: usize, text: &'static str, start: usize, end: usize) -> Archive {
    let bytes = text.as_bytes();
    if words(bytes, start, end) != 5 {
        bad(path, line, "an archive row is `<host> <package> <target> <sha256> <bytes>`")
    }
    let Some((a, b)) = word(bytes, start, end) else { bad(path, line, "no host") };
    let Some((c, d)) = word(bytes, b, end) else { bad(path, line, "no package") };
    let Some((e, f)) = word(bytes, d, end) else { bad(path, line, "no target") };
    Archive {
        host: slice(path, text, a, b),
        package: slice(path, text, c, d),
        target: slice(path, text, e, f),
        file: file_pin(path, line, text, f, end),
    }
}

/// `<sha256> <bytes>` from `from` to `end`.
const fn file_pin(path: &str, line: usize, text: &'static str, from: usize, end: usize) -> FilePin {
    let bytes = text.as_bytes();
    let Some((a, b)) = word(bytes, from, end) else { bad(path, line, "no sha256") };
    let Some((c, d)) = word(bytes, b, end) else { bad(path, line, "no size") };
    let sha256 = slice(path, text, a, b);
    if !is_hex(sha256, 64) {
        bad(path, line, "the sha256 is not 64 lower-case hex digits")
    }
    let Some(size) = decimal(slice(path, text, c, d)) else { bad(path, line, "the size is not a decimal number of bytes") };
    FilePin { sha256, bytes: size }
}

/// `line` after `prefix`, which it must start with.
const fn version_after(path: &str, line: &'static str, prefix: &str) -> &'static str {
    if !starts(line.as_bytes(), 0, prefix.as_bytes()) || line.len() == prefix.len() {
        Msg::new().s(path).s(": the line `").s(line).s("` is not `").s(prefix).s("<version>`").fail()
    }
    slice(path, line, prefix.len(), line.len())
}

/// The string value of the top-level `key` of the JSON `text`, written as its writers write it
/// (one key a line, `indent` spaces before a top-level key: `\n<indent>"key": "`), as its raw
/// body (escapes kept). Found from the end when `from_end` (for keys near the end of a large
/// file). A file without it stops the build naming `path`.
pub const fn json_str(path: &str, text: &'static str, indent: usize, key: &str, from_end: bool) -> &'static str {
    let mut needle = Small::EMPTY.push(path, b'\n');
    let mut spaces = 0;
    while spaces < indent {
        needle = needle.push(path, b' ');
        spaces += 1;
    }
    needle = needle.push(path, b'"');
    let key_bytes = key.as_bytes();
    let mut k = 0;
    while k < key_bytes.len() {
        needle = needle.push(path, at(key_bytes, k));
        k += 1;
    }
    needle = needle.push(path, b'"').push(path, b':').push(path, b' ').push(path, b'"');
    let needle = part(needle.bytes.as_slice(), 0, needle.len);
    let bytes = text.as_bytes();
    let found = if from_end { rfind(bytes, needle) } else { find(bytes, 0, needle) };
    let Some(found) = found else { Msg::new().s(path).s(": no top-level string `").s(key).s("`").fail() };
    let start = found + needle.len();
    let mut end = start;
    while end < bytes.len() && at(bytes, end) != b'"' {
        if at(bytes, end) == b'\n' {
            Msg::new().s(path).s(":").n(line_of(bytes, end)).s(": `").s(key).s("` ends at the line's end, not at a quote").fail()
        }
        end += if at(bytes, end) == b'\\' { 2 } else { 1 };
    }
    if end >= bytes.len() {
        Msg::new().s(path).s(": `").s(key).s("` has no closing quote").fail()
    }
    slice(path, text, start, end)
}

/// In the raw JSON body of rustc's `-vV` answer, the value of the line starting `name` (up to
/// the escaped line feed `\n`).
const fn verbose_field(verbose: &'static str, name: &str) -> &'static str {
    let bytes = verbose.as_bytes();
    let Some(found) = find(bytes, 0, name.as_bytes()) else {
        Msg::new().s(ANSWERS_PATH).s(": `verbose` has no `").s(name).s("` line").fail()
    };
    let start = found + name.len();
    let mut end = start;
    while end < bytes.len() && at(bytes, end) != b'\\' {
        end += 1;
    }
    slice(ANSWERS_PATH, verbose, start, end)
}

/// A short text held by value, so a fact read from a large file keeps only its own bytes.
#[derive(Clone, Copy)]
struct Small {
    bytes: [u8; 64],
    len: usize,
}

impl Small {
    const EMPTY: Small = Small { bytes: [0; 64], len: 0 };

    const fn of(path: &str, text: &str) -> Small {
        let text = text.as_bytes();
        let mut out = Small::EMPTY;
        let mut k = 0;
        while k < text.len() {
            out = out.push(path, at(text, k));
            k += 1;
        }
        out
    }

    const fn push(mut self, path: &str, byte: u8) -> Small {
        if !put(&mut self.bytes, self.len, byte) {
            Msg::new().s(path).s(": a fact over 64 bytes").fail()
        }
        self.len += 1;
        self
    }

    const fn as_str(&self) -> &str {
        match core::str::from_utf8(part(self.bytes.as_slice(), 0, self.len)) {
            Ok(text) => text,
            Err(_) => Msg::new().s("a pin's fact is not UTF-8").fail(),
        }
    }
}

/// A build error's text, made at compile time (a `const` panic takes one `&str`).
struct Msg {
    bytes: [u8; 512],
    len: usize,
}

impl Msg {
    const fn new() -> Msg {
        Msg { bytes: [0; 512], len: 0 }
    }

    const fn s(mut self, text: &str) -> Msg {
        let text = text.as_bytes();
        let mut k = 0;
        while k < text.len() && put(&mut self.bytes, self.len, at(text, k)) {
            self.len += 1;
            k += 1;
        }
        self
    }

    const fn n(mut self, mut value: usize) -> Msg {
        let mut digits = [0u8; 20];
        let mut count = 0;
        loop {
            put(&mut digits, count, at(b"0123456789", value % 10));
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        while count > 0 && put(&mut self.bytes, self.len, at(&digits, count - 1)) {
            count -= 1;
            self.len += 1;
        }
        self
    }

    /// The build error. A message cut inside a character (a value over the buffer) is cut back
    /// to the last whole one.
    const fn fail(self) -> ! {
        let mut len = self.len;
        let text = loop {
            match core::str::from_utf8(part(self.bytes.as_slice(), 0, len)) {
                Ok(text) => break text,
                Err(_) => len -= 1,
            }
        };
        panic!("{}", text)
    }
}

/// Stops the build: `path:line: what`.
const fn bad(path: &str, line: usize, what: &str) -> ! {
    Msg::new().s(path).s(":").n(line).s(": ").s(what).fail()
}

/// Stops the build: `path` has no `key` line.
const fn missing(path: &str, key: &str) -> ! {
    Msg::new().s(path).s(": no `").s(key).s("` line").fail()
}

/// The byte at `k`, or 0 past the end (every caller has checked `k` against the length; 0 is
/// no byte any form looks for).
const fn at(bytes: &[u8], k: usize) -> u8 {
    if k >= bytes.len() {
        return 0;
    }
    let (_, rest) = bytes.split_at(k);
    match rest.first() {
        Some(byte) => *byte,
        None => 0,
    }
}

/// Writes `byte` at `k`; false when `k` is past the end.
const fn put(bytes: &mut [u8], k: usize, byte: u8) -> bool {
    if k >= bytes.len() {
        return false;
    }
    let (_, rest) = bytes.split_at_mut(k);
    match rest.first_mut() {
        Some(slot) => {
            *slot = byte;
            true
        }
        None => false,
    }
}

/// `bytes[start..end]`, clamped to the slice.
const fn part(bytes: &[u8], start: usize, end: usize) -> &[u8] {
    let end = if end > bytes.len() { bytes.len() } else { end };
    let start = if start > end { end } else { start };
    let (head, _) = bytes.split_at(end);
    let (_, part) = head.split_at(start);
    part
}

/// `text[start..end]` (at ASCII boundaries: every field ends at a space, a quote or a line).
const fn slice(path: &str, text: &'static str, start: usize, end: usize) -> &'static str {
    match core::str::from_utf8(part(text.as_bytes(), start, end)) {
        Ok(part) => part,
        Err(_) => Msg::new().s(path).s(":").n(line_of(text.as_bytes(), start)).s(": a field cut inside a character").fail(),
    }
}

const fn line_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && at(bytes, end) != b'\n' {
        end += 1;
    }
    end
}

const fn line_of(bytes: &[u8], pos: usize) -> usize {
    let (mut k, mut line) = (0, 1);
    while k < pos && k < bytes.len() {
        if at(bytes, k) == b'\n' {
            line += 1;
        }
        k += 1;
    }
    line
}

/// The first space-separated word in `[from, end)`.
const fn word(bytes: &[u8], from: usize, end: usize) -> Option<(usize, usize)> {
    let mut start = from;
    while start < end && at(bytes, start) == b' ' {
        start += 1;
    }
    if start >= end {
        return None;
    }
    let mut stop = start;
    while stop < end && at(bytes, stop) != b' ' {
        stop += 1;
    }
    Some((start, stop))
}

const fn words(bytes: &[u8], start: usize, end: usize) -> usize {
    let (mut from, mut count) = (start, 0);
    while let Some((_, stop)) = word(bytes, from, end) {
        count += 1;
        from = stop;
    }
    count
}

const fn starts(hay: &[u8], from: usize, needle: &[u8]) -> bool {
    if from + needle.len() > hay.len() {
        return false;
    }
    let mut k = 0;
    while k < needle.len() {
        if at(hay, from + k) != at(needle, k) {
            return false;
        }
        k += 1;
    }
    true
}

const fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let mut k = from;
    while k + needle.len() <= hay.len() {
        if starts(hay, k, needle) {
            return Some(k);
        }
        k += 1;
    }
    None
}

const fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    let mut k = hay.len() - needle.len();
    loop {
        if starts(hay, k, needle) {
            return Some(k);
        }
        if k == 0 {
            return None;
        }
        k -= 1;
    }
}

const fn eq_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && starts(a, 0, b)
}

/// Whether two texts are equal, at compile time.
pub const fn eq(a: &str, b: &str) -> bool {
    eq_bytes(a.as_bytes(), b.as_bytes())
}

const fn is_hex(text: &str, len: usize) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != len {
        return false;
    }
    let mut k = 0;
    while k < len {
        if !matches!(at(bytes, k), b'0'..=b'9' | b'a'..=b'f') {
            return false;
        }
        k += 1;
    }
    true
}

const fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || at(bytes, 4) != b'-' || at(bytes, 7) != b'-' {
        return false;
    }
    let mut k = 0;
    while k < 10 {
        if k != 4 && k != 7 && !at(bytes, k).is_ascii_digit() {
            return false;
        }
        k += 1;
    }
    true
}

const fn decimal(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let (mut k, mut value) = (0, 0u64);
    while k < bytes.len() {
        let digit: u64 = match at(bytes, k) {
            b'0' => 0,
            b'1' => 1,
            b'2' => 2,
            b'3' => 3,
            b'4' => 4,
            b'5' => 5,
            b'6' => 6,
            b'7' => 7,
            b'8' => 8,
            b'9' => 9,
            _ => return None,
        };
        let Some(next) = value.checked_mul(10) else { return None };
        let Some(next) = next.checked_add(digit) else { return None };
        value = next;
        k += 1;
    }
    Some(value)
}
