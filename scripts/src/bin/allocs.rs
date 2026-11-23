//! allocs: what is left of the global heap in each crate only goes down (AGENTS.md
//! bump-allocate). `DOC` below is what its usage error prints; it says what each count is,
//! which code is a test's, and the rule.
//!
//!     blaze run -q -p scripts --bin allocs -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin allocs -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin allocs -- selftest
//!
//! The rule (2026-11-13): don't allocate where a borrow or a buffer does; where an allocation is
//! needed, name its allocator, the global heap as `Global` (`Vec::new_in(Global)`); no comment
//! stands in for it. `blaze::implicit_allocator` and `blaze::hot_alloc` no longer read
//! `// alloc: <why>` (crates/blaze-lints/src/blaze/exempt.rs), and the lint's fix writes the
//! explicit form. This counts, per crate, what is left to convert: the `// alloc:` lines (to 0,
//! after which a crate with no row is refused any), the `allow`/`expect`s of the lint, and the
//! lines that name `Global` (the number moving to frames brings down later). Each column of
//! tools/ratchets/alloc-sites.txt only shrinks, except `global` while any
//! `// alloc:` line is left: converting those to `Global` raises it. It runs with no
//! compiler: a gate step of the `scripts` job (.github/workflows/blaze.yaml), beside `domains`.
//!
//! The `signatures` column (2026-11-14) counts the function signatures that fix a caller's
//! allocator: a std collection or box with its allocator left to the default or given as
//! `Global`, or `String`. The rule it holds is `blaze::unwritten_allocator`'s: a function is
//! generic over `A: Allocator + Clone` and takes `alloc: A`, and the caller names `Global`. It is
//! the count the gate can hold before blaze checks blaze's own crates.

#![feature(allocator_api)]

/// What the usage error prints.
const DOC: &str = r#"allocs: what is left of the global heap in each crate only goes down (AGENTS.md bump-allocate).

    blaze run -q -p scripts --bin allocs -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin allocs -- '{"command": "write"}'
    blaze run -q -p scripts --bin allocs -- selftest

    command   check   each count of every crate equals its row's in the list (a `comments`,
                      `expects` or `signatures` count over its row is refused, with its new
                      sites named given `base`; one under it must be lowered; a `global` count
                      off its row is written with `write`); a crate with no row is held to 0;
                      with `base`, no `comments`, `expects` or `signatures` of a row is above
                      the base's, and no `global` either once the base has no `// alloc:` line
                      left in any crate (while some are, converting them to `Global` raises it)
                      (CI)
              write   write the list from the tree as it is
              findings  print every site as a `blaze_script::finding` document (for `advise`)
    root      the checkout (".")
    base      the commit the change is against, or null: its list (`git show`) holds every
              row down, and the lines added since it (`git diff --unified=0`) name a crate's
              new sites

The list's row is `<crate><TAB><comments><TAB><expects><TAB><global><TAB><signatures>`, of lines
of `.rs` files under crates/ (a directory named `target` is not read):
  comments  a line with `// alloc: <why>` (`//`, then `alloc:` and a reason that is not
            empty): the comment that once kept an allocator lint's finding and now keeps
            nothing; each is a site to convert, and the count goes to 0
  expects   a line that starts with an `allow` or `expect` attribute naming
            `blaze::implicit_allocator`: an item that says why it keeps the global heap
  global    a line whose code (not a comment, a string or a character, and not a `use` item)
            has the word `Global`: the global heap named where it is used, which moving to
            frames brings down
  signatures
            a function signature, from the word `fn` and its name to the `{` or `;` that ends
            it, over as many lines as it spans (comments, strings and characters skipped),
            that names `Vec`, `VecDeque`, `Box`, `Rc`, `Arc`, `BTreeSet`, `BinaryHeap` or
            `LinkedList` with one type argument (`BTreeMap` with two), that is, with its
            allocator left to its default, or with the allocator given as `Global` (`Vec<T,
            Global>`, `Vec<T, std::alloc::Global>`), or `String` with no `<..>` after it; once
            a signature however many such types it names, at the line of its `fn`. Not
            counted: a method of an `impl <Trait> for <Type>` block (its signature is the
            trait's, found at the trait's declaration, which counts), a function pointer type
            (`fn(u8)`, no name) and `fn main`. The form to write instead: the function generic
            over `A: Allocator + Clone`, taking `alloc: A`, `Vec<T, A>` in its signature, and
            the caller naming `Global`
A line can count in more than one column. The previous layouts, `<crate><TAB><count>` and
`<crate><TAB><comments><TAB><expects><TAB><global>`, are refused with a message to run `write`;
a base whose list has one of them holds no row (the columns begin with the change).

`signatures` is read from the text, so it differs from the lint at the edges: a crate's own type
named `Vec`, a type alias of a std collection, a function a macro makes, a `fn` in a
`macro_rules!` body, and whose trait an impl implements (no impl's method counts, its trait in
the crate or not). The lint `blaze::unwritten_allocator` is the definition; this column is the
count the gate can hold before blaze checks blaze's own crates, and it goes when that lint's
baseline joins the gate.

A site in test code is not counted. Test code is, and nothing else is:
  - a file under a directory named `tests` or `benches`, or named `tests.rs` or `test.rs`;
  - a file that says `#![cfg(test)]`;
  - the item after `#[cfg(test)]` or `#[cfg(all(test, ...))]`, from the attribute's line to
    the line of the `}` or `;` that ends the item (strings, characters and comments read as
    Rust reads them); an attribute whose item does not end before its block does (a match
    arm, a field) covers its own line only;
  - the file of a module declared out of line in test code (`#[cfg(test)] mod t;`, or any
    `mod m;` in a test file): `m.rs` or `m/mod.rs` beside a crate root (`lib.rs`, `main.rs`,
    `mod.rs`, `build.rs`, a file of a `bin` directory), else in the directory named for the
    declaring file. A `#[path]` is not followed, so such a module counts.
Everything else counts, `*_tests.rs` files a test module does not declare included.

A crate is the nearest directory above the file, under crates/, that holds a Cargo.toml, named
by its path under crates/ (`blaze-cargo`, `harness/harness-native`).
"#;

use std::{
    alloc::{Allocator, Global},
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_script::{
    TempDir, fail,
    finding::{self, Finding},
    git,
    input::{self, Fields, Input, InputError},
    ratchet::{
        self, CrateScope, FileTests, Hit, Tests, added_lines, comment_end, ident_len, item_end, module_files, quote_end, string_end, word_end,
    },
    read_text,
    regex::Regex,
};

const SCRIPT: &str = "allocs";
/// The list, from the root.
const LIST: &str = "tools/ratchets/alloc-sites.txt";
/// The word of the comment that once kept an allocator lint's finding and keeps nothing now.
const TAG: &str = "alloc";
/// The global heap's name where it is used (`Vec::new_in(Global)`).
const GLOBAL: &str = "Global";
/// What every message about a crate over its row ends with.
const RULE: &str = "AGENTS.md bump-allocate: don't allocate where a borrow, a fixed buffer or the caller's buffer does; \
                    where an allocation is needed, allocate by lifetime and name the allocator (`Vec::new_in(frame)`, or \
                    `Vec::new_in(Global)` for the global heap); never implied, and no comment stands in for it";
/// The list's first lines.
const HEADER: &str = "# What is left of the global heap outside test code, by crate (its directory under crates/):\n\
                      # `// alloc: <why>` lines, which keep nothing now and go to 0; `allow`/`expect`s of\n\
                      # `blaze::implicit_allocator`; lines whose code names `Global`; function signatures that fix\n\
                      # the caller's allocator (`blaze::unwritten_allocator`'s rule, read from the text). Only shrinks\n\
                      # (`global` may rise while any `// alloc:` line is left); a crate with no row is held to 0\n\
                      # (AGENTS.md bump-allocate). Written by scripts/src/bin/allocs.rs (`{\"command\": \"write\"}`),\n\
                      # checked in CI.\n\
                      # crate\tcomments\texpects\tglobal\tsignatures\n";
/// The std types that take an allocator, each with the number of type arguments of its form
/// with one: a signature that writes one fewer leaves the allocator to its default.
const ALLOCATING: [(&str, usize); 9] = [
    ("Vec", 2),
    ("VecDeque", 2),
    ("Box", 2),
    ("Rc", 2),
    ("Arc", 2),
    ("BTreeMap", 3),
    ("BTreeSet", 2),
    ("BinaryHeap", 2),
    ("LinkedList", 2),
];
/// std's text type, which takes no allocator; `blaze_mem::String<A>` (written with `<..>`) does.
const TEXT: &str = "String";

struct Args {
    command: String,
    root: String,
    base: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "base"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["check", "write", "findings"], None)?,
            root: f.str("root", Some("."))?,
            base: f.str_or_null("base")?,
        })
    }
}

/// What a site is: the list's columns, in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `// alloc: <why>`: kept nothing since 2026-11-13, a site to convert.
    Comment,
    /// An `allow` or `expect` of `blaze::implicit_allocator`.
    Expect,
    /// Code that names `Global`.
    Global,
    /// A function signature that fixes its caller's allocator.
    Signature,
}

const KINDS: [Kind; 4] = [Kind::Comment, Kind::Expect, Kind::Global, Kind::Signature];

impl Kind {
    /// Its column in a row, from 0.
    const fn column(self) -> usize {
        match self {
            Kind::Comment => 0,
            Kind::Expect => 1,
            Kind::Global => 2,
            Kind::Signature => 3,
        }
    }

    /// Its column's name in the header and in messages.
    const fn name(self) -> &'static str {
        match self {
            Kind::Comment => "comments",
            Kind::Expect => "expects",
            Kind::Global => "global",
            Kind::Signature => "signatures",
        }
    }

    /// What its sites are, for a message: `<n> <what>`.
    const fn what(self) -> &'static str {
        match self {
            Kind::Comment => "`// alloc:` lines, which keep nothing now",
            Kind::Expect => "`allow`/`expect`s of `blaze::implicit_allocator`",
            Kind::Global => "lines that name `Global`",
            Kind::Signature => "function signatures that fix the caller's allocator (`blaze::unwritten_allocator`)",
        }
    }

    /// The kind as a finding (`blaze_script::finding`), for `advise`.
    const fn finding(self) -> finding::Kind {
        let instead = match self {
            Kind::Comment => "delete the comment and name the allocator in the code it kept: `new_in(frame)`, or `new_in(Global)`",
            Kind::Expect => "name the allocator where the item allocates, then drop the `allow`/`expect`",
            Kind::Global => "the task's frame where the value dies with the task; `Global` only where it outlives it",
            Kind::Signature => "make the function generic over `A: Allocator + Clone`, take `alloc: A`, and let the caller name `Global`",
        };
        finding::Kind { name: self.name(), label: self.what(), rule: "11 bump-allocate", instead, mechanical: false }
    }
}

/// A crate's counts, by [`Kind::column`].
type Row = [usize; 4];

/// The counts of `sites`, by kind.
fn tally(sites: &[Hit<Kind>]) -> Row {
    let mut row = [0; 4];
    for it in sites {
        row[it.what.column()] += 1;
    }
    row
}

struct Patterns {
    tests: Tests,
    /// An `allow` or `expect` of `blaze::implicit_allocator`, at the line's start.
    allowed: Regex,
}

impl Patterns {
    fn new() -> Patterns {
        let allowed = Regex::new(r"\A\s*#!?\[[^\]]*\b(?:allow|expect)\([^\]]*\bblaze::implicit_allocator\b")
            .unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
        Patterns { tests: Tests::cfg_test_only(SCRIPT), allowed }
    }
}

/// Whether `line` has `// alloc: <reason>` with a reason that isn't empty: exempt.rs's `keeps`.
fn keeps(line: &str) -> bool {
    let mut rest = line;
    while let Some(at) = rest.find("//") {
        rest = &rest[at + 2..];
        let reason = rest.trim_start().strip_prefix(TAG).and_then(|it| it.strip_prefix(':')).map(str::trim_start);
        if reason.is_some_and(|it| !it.is_empty()) {
            return true;
        }
    }
    false
}

/// Emits every site in `text`, in line and kind order, test code included (`walk_with` drops
/// test lines and test files).
fn scan_sites(p: &Patterns, text: &str, emit: &mut dyn FnMut(usize, Kind)) {
    let global = global_lines(text);
    let mut global = global.iter().peekable();
    let mut signatures = Vec::new_in(Global);
    signature_lines(text, &mut signatures);
    let mut signatures = signatures.iter().peekable();
    for (at, line) in text.split('\n').enumerate() {
        // `global` is in line order: take this line's entry, if it has one.
        let names_global = global.next_if(|it| **it == at + 1).is_some();
        // `signatures` likewise, each signature whose `fn` is on this line (two can share one).
        let mut signed: usize = 0;
        // Bounded: each turn takes one entry of the list.
        while signatures.next_if(|it| **it == at + 1).is_some() {
            signed += 1;
        }
        for (is, kind) in [(keeps(line), Kind::Comment), (p.allowed.is_match(line), Kind::Expect), (names_global, Kind::Global)] {
            if is {
                emit(at + 1, kind);
            }
        }
        for _ in 0..signed {
            emit(at + 1, Kind::Signature);
        }
    }
}

/// The test code and sites of one file's text outside test code (for selftest).
fn scan_file(p: &Patterns, text: &str) -> (FileTests, Vec<(usize, Kind)>) {
    let found = p.tests.of(text);
    let mut sites = Vec::new();
    if !found.whole {
        scan_sites(p, text, &mut |line, kind| {
            if !found.test.get(line - 1).copied().unwrap_or(false) {
                sites.push((line, kind));
            }
        });
    }
    (found, sites)
}

/// The lines (from 1, ascending, each once) whose code has the word `Global`: comments, strings
/// (raw and byte strings too) and character literals are skipped as [`item_end`] skips them, a
/// word is a whole identifier (`MyGlobal` and `Global2` are not it), and a `use` item, from the
/// word `use` to its `;`, names nothing (its import is not a use of the heap).
fn global_lines(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut out: Vec<usize> = Vec::new();
    // The line of byte `counted`, kept up to date as `at` moves on.
    let mut line = 1;
    let mut counted = 0;
    let mut in_use = false;
    let mut at = 0;
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        at = match byte {
            b'/' if next == Some(b'/') => text[at..].find('\n').map_or(bytes.len(), |n| at + n),
            b'/' if next == Some(b'*') => comment_end(bytes, at),
            b'"' => string_end(bytes, at + 1),
            b'\'' => quote_end(text, at),
            b';' => {
                in_use = false;
                at + 1
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let end = word_end(text, at);
                match &text[at..end] {
                    "use" => in_use = true,
                    GLOBAL if !in_use => {
                        line += text[counted..at].matches('\n').count();
                        counted = at;
                        if out.last() != Some(&line) {
                            out.push(line);
                        }
                    }
                    _ => {}
                }
                end
            }
            // A number's digits and suffix (`1u8`, `0x1F`): never the start of a word.
            b'0'..=b'9' => at + ident_len(&bytes[at..]),
            _ => at + 1,
        };
    }
    out
}

/// Pushes onto `out` the line (from 1, ascending) of the `fn` of each function signature in
/// `text` that fixes its caller's allocator ([`fixes_allocator`]): comments, strings and
/// characters skipped as [`item_end`] skips them. Not a method directly in an `impl <Trait> for
/// <Type>` block (its signature is the trait's), not a function pointer type (`fn` with no name
/// after it), not `fn main`.
fn signature_lines<A: Allocator>(text: &str, out: &mut Vec<usize, A>) {
    let bytes = text.as_bytes();
    // For each `{` open, outermost first: whether it opens an `impl <Trait> for <Type>` block.
    let mut blocks = Vec::new_in(Global);
    // From the word `impl` to the `{` or `;` after it: whether a `for` is in it (not `for<..>`,
    // a bound's).
    let mut header: Option<bool> = None;
    // The line of byte `counted`, kept up to date as `at` moves on.
    let mut line = 1;
    let mut counted = 0;
    let mut at = 0;
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        at = match byte {
            b'/' if next == Some(b'/') => text[at..].find('\n').map_or(bytes.len(), |n| at + n),
            b'/' if next == Some(b'*') => comment_end(bytes, at),
            b'"' => string_end(bytes, at + 1),
            b'\'' => quote_end(text, at),
            b'{' => {
                blocks.push(header.take().unwrap_or(false));
                at + 1
            }
            b'}' => {
                blocks.pop();
                at + 1
            }
            b';' => {
                header = None;
                at + 1
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let end = word_end(text, at);
                match &text[at..end] {
                    "impl" => header = Some(false),
                    "for" if header.is_some() && !text[end..].trim_start().starts_with('<') => header = Some(true),
                    "fn" if blocks.last() != Some(&true) => {
                        let rest = text[end..].trim_start();
                        let rest = rest.strip_prefix("r#").unwrap_or(rest);
                        let len = ident_len(rest.as_bytes());
                        let name = &rest[..len];
                        let named = name.bytes().next().is_some_and(|b| !b.is_ascii_digit());
                        if named
                            && name != "main"
                            && let Some(close) = signature_end(text, end)
                            && fixes_allocator(&text[end..close])
                        {
                            line += text[counted..at].matches('\n').count();
                            counted = at;
                            out.push(line);
                        }
                    }
                    _ => {}
                }
                end
            }
            // A number's digits and suffix (`1u8`, `0x1F`): never the start of a word.
            b'0'..=b'9' => at + ident_len(&bytes[at..]),
            _ => at + 1,
        };
    }
}

/// The byte of the `{` or `;` that ends the signature whose text (after the word `fn`) starts
/// at byte `from` of `text`: the first outside brackets (`(`, `[`, `<`, and `{` of a const
/// argument; the `>` of `->` closes none); `None` when a bracket closes that it did not open, or
/// the text ends.
fn signature_end(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth: usize = 0;
    let mut at = from;
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        at = match byte {
            b'/' if next == Some(b'/') => text[at..].find('\n').map_or(bytes.len(), |n| at + n),
            b'/' if next == Some(b'*') => comment_end(bytes, at),
            b'"' => string_end(bytes, at + 1),
            b'\'' => quote_end(text, at),
            b'{' | b';' if depth == 0 => return Some(at),
            b'(' | b'[' | b'<' | b'{' => {
                depth += 1;
                at + 1
            }
            b'>' if at > 0 && bytes[at - 1] == b'-' => at + 1,
            b')' | b']' | b'>' | b'}' => {
                depth = depth.checked_sub(1)?;
                at + 1
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => word_end(text, at),
            _ => at + 1,
        };
    }
    None
}

/// Whether the signature text `signature` names a type of [`ALLOCATING`] with one type argument
/// fewer than its form with an allocator, or with `Global` as its last argument, or [`TEXT`]
/// with no `<..>` after it. Comments, strings and characters are skipped.
fn fixes_allocator(signature: &str) -> bool {
    let bytes = signature.as_bytes();
    let mut at = 0;
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        at = match byte {
            b'/' if next == Some(b'/') => signature[at..].find('\n').map_or(bytes.len(), |n| at + n),
            b'/' if next == Some(b'*') => comment_end(bytes, at),
            b'"' => string_end(bytes, at + 1),
            b'\'' => quote_end(signature, at),
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let end = word_end(signature, at);
                let word = &signature[at..end];
                let after = signature[end..].trim_start();
                if word == TEXT && !after.starts_with('<') {
                    return true;
                }
                if let Some(&(_, arity)) = ALLOCATING.iter().find(|(name, _)| *name == word)
                    && after.starts_with('<')
                    && let Some((count, last)) = type_args(after)
                    && (count + 1 == arity || (count == arity && (last == GLOBAL || last.ends_with("::Global"))))
                {
                    return true;
                }
                end
            }
            b'0'..=b'9' => at + ident_len(&bytes[at..]),
            _ => at + 1,
        };
    }
    false
}

/// The number of arguments of the `<..>` that `text` starts with, and its last argument's text,
/// trimmed: commas count only at its own level (`(`, `[` and `<` nest; the `>` of `->` closes
/// none), and a trailing comma adds none. `None` when it does not close.
fn type_args(text: &str) -> Option<(usize, &str)> {
    let bytes = text.as_bytes();
    let mut depth: usize = 0;
    let mut count = 1;
    let mut last = 1;
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' | b')' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let tail = text[last..at].trim();
                    return Some(if tail.is_empty() { (count - 1, text[..last - 1].rsplit(',').next()?.trim()) } else { (count, tail) });
                }
            }
            b',' if depth == 1 => {
                count += 1;
                last = at + 1;
            }
            _ => {}
        }
    }
    None
}

/// Each crate's sites outside test code, in file and line order; a crate with none is absent.
fn count(p: &Patterns, root: &Path) -> BTreeMap<String, Vec<Hit<Kind>>> {
    ratchet::walk_with(SCRIPT, root, CrateScope::AllDirs, &p.tests, |_| false, |file, emit| {
        scan_sites(p, file.text, emit);
    })
}

/// The list as `write` writes it: the header, then `<crate><TAB><comments><TAB><expects><TAB>
/// <global><TAB><signatures>` for each crate with a site, sorted.
fn render(counts: &BTreeMap<String, Vec<Hit<Kind>>>) -> String {
    let mut out = HEADER.to_owned();
    for (krate, sites) in counts.iter().filter(|(_, sites)| !sites.is_empty()) {
        let [comments, expects, global, signatures] = tally(sites);
        out.push_str(&format!("{krate}\t{comments}\t{expects}\t{global}\t{signatures}\n"));
    }
    out
}

/// The list's rows; `Err` names the line that is not a row, a comment or blank, and says to run
/// `write` for a row of a previous layout, `<crate><TAB><count>` or `<crate><TAB><comments>
/// <TAB><expects><TAB><global>`.
fn parse_rows(text: &str) -> Result<BTreeMap<String, Row>, String> {
    let mut rows = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let number = |it: &str| it.parse::<usize>().ok();
        let row = match fields.as_slice() {
            [krate, n] if !krate.is_empty() && number(*n).is_some() => {
                return Err(format!(
                    "{LIST}:{}: {line:?} is a row of a previous layout, `<crate><TAB><count>`: run command `write`",
                    at + 1
                ));
            }
            [krate, rest @ ..] if !krate.is_empty() && rest.len() == 3 && rest.iter().all(|it| number(*it).is_some()) => {
                return Err(format!(
                    "{LIST}:{}: {line:?} is a row of a previous layout, `<crate><TAB><comments><TAB><expects><TAB><global>`, \
                     with no `signatures`: run command `write`",
                    at + 1
                ));
            }
            [krate, comments, expects, global, signatures] if !krate.is_empty() => {
                match (number(*comments), number(*expects), number(*global), number(*signatures)) {
                    (Some(comments), Some(expects), Some(global), Some(signatures)) => {
                        Some((*krate, [comments, expects, global, signatures]))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let Some((krate, n)) = row else {
            return Err(format!(
                "{LIST}:{}: {line:?} is not `<crate><TAB><comments><TAB><expects><TAB><global><TAB><signatures>`",
                at + 1
            ));
        };
        if rows.insert(krate.to_owned(), n).is_some() {
            return Err(format!("{LIST}:{}: `{krate}` has two rows", at + 1));
        }
    }
    Ok(rows)
}

/// Whether `text` is a list of a previous layout: its first row is `<crate><TAB><count>` or
/// `<crate><TAB><comments><TAB><expects><TAB><global>`.
fn previous_layout(text: &str) -> bool {
    let first = text.split('\n').find(|line| !line.is_empty() && !line.starts_with('#'));
    first.is_some_and(|line| matches!(line.split('\t').count(), 2 | 4))
}

/// What `base` says: its rows (none when it has no list, before the ratchet began), and the lines
/// added since it under crates/.
fn at_base(root: &Path, base: &str) -> Result<(Option<BTreeMap<String, Row>>, BTreeMap<String, BTreeSet<usize>>), String> {
    let root_arg = root.to_string_lossy();
    let dir = root_arg.as_ref();
    let commit = format!("{base}^{{commit}}");
    git(root, &["-C", dir, "rev-parse", "--verify", "--quiet", commit.as_str()])
        .map_err(|e| format!("the base {base} is no commit here (fetch it): {e}"))?;
    let listed = git(root, &["-C", dir, "ls-tree", "--name-only", base, "--", LIST])?;
    let rows = if listed.trim().is_empty() {
        None
    } else {
        let spec = format!("{base}:{LIST}");
        let text = git(root, &["-C", dir, "show", spec.as_str()])?;
        // A list of a previous layout has a column fewer, or one number the columns split:
        // nothing to hold the columns to, as before the ratchet began.
        if previous_layout(&text) { None } else { Some(parse_rows(&text).map_err(|e| format!("at {base}: {e}"))?) }
    };
    let diff = git(root, &["-C", dir, "diff", "--no-color", "--no-ext-diff", "--no-renames", "--unified=0", base, "--", "crates"])?;
    Ok((rows, added_lines(&diff).map_err(|e| e.to_string())?))
}

/// What is wrong: `counts` the tree's sites by crate, `rows` the list's, `base_rows` the base's
/// list (when it had one), `added` the lines added since the base (when one was given).
fn problems(
    counts: &BTreeMap<String, Vec<Hit<Kind>>>,
    rows: &BTreeMap<String, Row>,
    base_rows: Option<&BTreeMap<String, Row>>,
    added: Option<&BTreeMap<String, BTreeSet<usize>>>,
) -> Vec<String> {
    let mut out = Vec::new();
    let crates: BTreeSet<&String> = counts.keys().chain(rows.keys()).collect();
    for krate in crates {
        let sites = counts.get(krate).map_or(&[][..], Vec::as_slice);
        let row = rows.get(krate).copied();
        let now_row = tally(sites);
        for kind in KINDS {
            let now = now_row[kind.column()];
            let held = row.map_or(0, |it| it[kind.column()]);
            let what = kind.what();
            let column = kind.name();
            if kind == Kind::Global && now != held {
                // The base, not the list, holds `global` down: the list only has to say it.
                out.push(format!(
                    "`{krate}` has {now} {what} outside tests, and its row's `{column}` in {LIST} is {held}: run command `write`"
                ));
            } else if now > held {
                let shown = match row {
                    Some(_) => format!("its row's `{column}` {held}"),
                    None => "0, as a crate with no row is held".to_owned(),
                };
                let new = match added {
                    Some(added) => {
                        let new: Vec<String> = sites
                            .iter()
                            .filter(|it| it.what == kind && added.get(&it.path).is_some_and(|lines| lines.contains(&it.line)))
                            .map(|it| format!("{}:{}", it.path, it.line))
                            .collect();
                        if new.is_empty() {
                            "none is on a line added since the base: a site moved in from another crate, or a file git does not track".to_owned()
                        } else {
                            format!("new: {}", new.join(", "))
                        }
                    }
                    None => "give `base`, the commit the change is against, to have the new ones named".to_owned(),
                };
                let instead = match kind {
                    Kind::Comment => "no comment keeps an allocation: borrow or write into a buffer, or name the allocator where it is used",
                    Kind::Signature => {
                        "make the function generic over `A: Allocator + Clone`, take `alloc: A` from its caller and write `A` \
                         in the type (`Vec<T, A>`; `blaze_mem::String<A>` or a typed error for `String`); the caller names \
                         `Global` where the lifetime is decided"
                    }
                    _ => "borrow or write into a buffer, or give each value its allocator, from a parameter or the task's frame",
                };
                out.push(format!("`{krate}` has {now} {what} outside tests, over {shown} in {LIST} ({new}): {instead} ({RULE})"));
            } else if now < held {
                let lower = match now_row {
                    [0, 0, 0, 0] => "remove the row".to_owned(),
                    _ => format!("lower its `{column}` to {now}"),
                };
                out.push(format!(
                    "`{krate}` has {now} {what} outside tests, under its row's `{column}` {held} in {LIST}: {lower} (command `write`); the list only shrinks"
                ));
            }
        }
    }
    if let Some(base) = base_rows {
        // `global` rises while `// alloc:` lines are converted to it, and only shrinks once the
        // base has none left.
        let converting = base.values().any(|it| it[Kind::Comment.column()] > 0);
        for (krate, n) in rows {
            let was = base.get(krate).copied().unwrap_or([0; 4]);
            for kind in KINDS {
                let (now, then) = (n[kind.column()], was[kind.column()]);
                if now > then && !(kind == Kind::Global && converting) {
                    out.push(format!(
                        "{LIST}: the row `{krate}`'s `{}` is {now} and was {then} at the base: the list only shrinks ({RULE})",
                        kind.name()
                    ));
                }
            }
        }
    }
    out
}

/// The tree's counts, by kind.
fn total(counts: &BTreeMap<String, Vec<Hit<Kind>>>) -> Row {
    let mut row = [0; 4];
    for sites in counts.values() {
        for (sum, n) in row.iter_mut().zip(tally(sites)) {
            *sum += n;
        }
    }
    row
}

/// `counts`' totals, for a line of output.
fn summary(counts: &BTreeMap<String, Vec<Hit<Kind>>>) -> String {
    let [comments, expects, global, signatures] = total(counts);
    format!(
        "{comments} `// alloc:` lines left to convert, {expects} `allow`/`expect`s of the lint, {global} lines naming `Global` \
         and {signatures} signatures fixing the caller's allocator, outside tests, in {} crates",
        counts.len()
    )
}

fn main_with(p: &Patterns, it: &Args) {
    // `Path(root).resolve()`.
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    if !root.join("crates").is_dir() {
        fail(SCRIPT, &format!("{} has no crates/ directory", root.display()));
    }
    let counts = count(p, &root);
    if it.command == "findings" {
        let found = counts.iter().flat_map(|(krate, sites)| {
            sites.iter().map(|it| Finding { krate: krate.clone(), file: it.path.clone(), line: it.line, kind: it.what.name(), detail: None })
        });
        finding::print(SCRIPT, &KINDS.map(Kind::finding), found.collect());
        return;
    }
    let text = render(&counts);
    if it.command == "write" {
        if let Err(e) = std::fs::write(root.join(LIST), &text) {
            fail(SCRIPT, &format!("{LIST} is not written: {e}"));
        }
        println!("{SCRIPT}: wrote {LIST}: {}", summary(&counts));
        return;
    }
    let Some(listed) = read_text(&root.join(LIST)) else {
        fail(SCRIPT, &format!("{LIST} could not be read: every crate is held to its row there (command `write` makes it)"))
    };
    let rows = parse_rows(&listed).unwrap_or_else(|e| fail(SCRIPT, &e));
    let (base_rows, added) = match &it.base {
        Some(base) => {
            let (rows, added) = at_base(&root, base).unwrap_or_else(|e| fail(SCRIPT, &e));
            if rows.is_none() {
                println!(
                    "{SCRIPT}: {base} has no {LIST} with these columns: they begin with this change, so no row is compared with the base"
                );
            }
            (rows, Some(added))
        }
        None => (None, None),
    };
    let mut wrong = problems(&counts, &rows, base_rows.as_ref(), added.as_ref());
    if wrong.is_empty() && listed != text {
        wrong.push(format!(
            "{LIST} is not as command `write` writes it (its header, then a sorted `<crate><TAB><comments><TAB><expects><TAB><global><TAB><signatures>` row per crate with a site): run it"
        ));
    }
    if !wrong.is_empty() {
        let lines: Vec<String> = wrong.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
    println!("{SCRIPT}: {}, each crate at its row in {LIST}", summary(&counts));
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

/// The sites of the counted crates, as `crate: path:line, ...`, for a selftest's message.
fn shown(counts: &BTreeMap<String, Vec<Hit<Kind>>>) -> String {
    let crates: Vec<String> = counts
        .iter()
        .map(|(krate, sites)| {
            let at: Vec<String> = sites.iter().map(|it| format!("{}:{}", it.path, it.line)).collect();
            format!("{krate}: {}", at.join(", "))
        })
        .collect();
    crates.join("; ")
}

/// One crate's sites, for a selftest's expectation.
fn sites_of(path_lines: &[(&str, usize, Kind)]) -> Vec<Hit<Kind>> {
    path_lines.iter().map(|(path, line, what)| Hit { path: (*path).to_owned(), line: *line, what: *what }).collect()
}

fn selftest(p: &Patterns) {
    // What is a site: the comment as exempt.rs reads it, and the attributes.
    expect(keeps("let v = Vec::new(); // alloc: once, at start"), "a comment with a reason keeps");
    expect(keeps("//alloc:x"), "no space is needed");
    expect(!keeps("let v = Vec::new(); // alloc:"), "an empty reason keeps nothing");
    expect(!keeps("/// alloc: a doc line"), "a doc comment is not the comment");
    expect(!keeps("let v = Vec::new(); // allocates: no"), "another word is not the comment");
    expect(p.allowed.is_match("#[expect(blaze::implicit_allocator, reason = \"x\")]"), "an expect is a site");
    expect(p.allowed.is_match("    #![allow(clippy::x, blaze::implicit_allocator)]"), "an inner allow is a site");
    expect(!p.allowed.is_match("let s = \"#[expect(blaze::implicit_allocator)]\";"), "a string that names one is not");

    // Test code in a file: the item after `#[cfg(test)]`, to its end, braces in strings,
    // characters, raw strings and comments not counted.
    let text = "use x;\n\
        fn f() {\n\
        \x20   let v = Vec::new(); // alloc: once, at start\n\
        \x20   let w = Vec::new(); // alloc:\n\
        }\n\
        /// alloc: a doc line names the comment and keeps nothing\n\
        #[cfg(test)]\n\
        mod t;\n\
        mod live;\n\
        #[cfg(test)]\n\
        mod tests {\n\
        \x20   const S: &str = \"}\"; // alloc: a test's\n\
        \x20   const R: &str = r#\"}\"#; // alloc: a test's\n\
        \x20   fn g<'a>(_: &'a str) { let _ = '}'; /* } */ } // alloc: a test's\n\
        }\n\
        #[cfg(all(test, unix))]\n\
        fn helper() {\n\
        \x20   let v = Vec::new_in(Global); // alloc: a test's\n\
        }\n\
        fn h() {} // alloc: after the tests, counted\n\
        #[expect(blaze::implicit_allocator, reason = \"counted too\")]\n\
        fn k() {}\n\
        use std::alloc::Global;\n\
        fn m() { let v = Vec::new_in(Global); } // alloc: the comment left over from before\n\
        const G: &str = \"Global\"; // Global, in a comment\n\
        struct MyGlobal; fn n(_: &dyn GlobalAlloc, _: Global2) {}\n\
        /* Global\n\
        \x20  Global */ fn o() { let _ = r#\"Global\"#; let _ = 'G'; }\n\
        fn q() { let _ = Box::new_in(1, Global); let _ = Vec::<u8, Global>::new_in(Global); }\n\
        use std::{\n\
        \x20   alloc::Global,\n\
        };\n\
        fn s(_: u8) {} fn t(_: Global) {}\n";
    // `Global` in code, once a line: not in a `use` item, a string, a comment, or a longer word.
    let global = global_lines(text);
    expect(global == [18, 24, 29, 33], &format!("the lines that name `Global`: {global:?}"));
    let (found, sites) = scan_file(p, text);
    let want = [
        (3, Kind::Comment),
        (20, Kind::Comment),
        (21, Kind::Expect),
        (24, Kind::Comment),
        (24, Kind::Global),
        (29, Kind::Global),
        (33, Kind::Global),
    ];
    expect(sites == want, &format!("the sites outside test code (line 18 is a test's): {sites:?}"));
    expect(
        found.mods == [("t".to_owned(), true), ("live".to_owned(), false)],
        &format!("the modules declared out of line: {:?}", found.mods),
    );
    let (inner, inner_sites) = scan_file(p, "#![cfg(test)]\nmod deep;\nfn x() { Vec::new_in(Global) } // alloc: a test file's\n");
    expect(inner.whole && inner_sites.is_empty() && inner.mods == [("deep".to_owned(), true)], &format!("{inner:?}"));
    // A field's attribute: the item ends with its struct, so the attribute's line alone is test.
    let (_, field_sites) = scan_file(p, "struct S {\n    #[cfg(test)]\n    a: Vec<u8, Global>, // alloc: counted\n}\n");
    expect(
        field_sites == [(3, Kind::Comment), (3, Kind::Global)],
        &format!("a field under cfg(test) is counted: {field_sites:?}"),
    );
    expect(item_end("{ \"\\\"}\" }", 0) == Some(9), "an escaped quote does not end a string");
    expect(item_end("br##\"}\"#\"##;", 0) == Some(12), "a raw byte string with hashes");

    let parts = |path: &str| -> Vec<String> { path.split('/').map(str::to_owned).collect() };
    expect(
        module_files(&parts("crates/a/src/lib.rs"), "t") == ["crates/a/src/t.rs".to_owned(), "crates/a/src/t/mod.rs".to_owned()],
        "a crate root's module is beside it",
    );
    expect(
        module_files(&parts("crates/a/src/t.rs"), "deep")
            == ["crates/a/src/t/deep.rs".to_owned(), "crates/a/src/t/deep/mod.rs".to_owned()],
        "another file's module is in the directory named for it",
    );
    expect(module_files(&parts("crates/a/src/bin/x.rs"), "m")[0] == "crates/a/src/bin/m.rs", "a bin's module");

    // A tree: crates, test files by path and by declaration (transitively), a nested crate.
    let tmp = TempDir::new("allocs-selftest-").unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    put("crates/a/src/lib.rs", text);
    put("crates/a/src/t.rs", "mod deep;\nfn x() {} // alloc: a test module's file\n");
    put("crates/a/src/t/deep.rs", "fn y() {} // alloc: a test module's module\n");
    put("crates/a/src/live.rs", "fn z() {} // alloc: counted\n");
    put("crates/a/tests/it.rs", "fn i() {} // alloc: an integration test\n");
    put("crates/a/target/debug/build/out.rs", "fn o() {} // alloc: built, not source\n");
    put("crates/b/Cargo.toml", "[workspace]\n");
    put("crates/b/c/Cargo.toml", "[package]\nname = \"c\"\n");
    put("crates/b/c/src/lib.rs", "fn q() {} // alloc: a nested crate\n");
    let counts = count(p, tmp.path());
    let lib = "crates/a/src/lib.rs";
    let want = BTreeMap::from([
        (
            "a".to_owned(),
            sites_of(&[
                (lib, 3, Kind::Comment),
                (lib, 20, Kind::Comment),
                (lib, 21, Kind::Expect),
                (lib, 24, Kind::Comment),
                (lib, 24, Kind::Global),
                (lib, 29, Kind::Global),
                (lib, 33, Kind::Global),
                ("crates/a/src/live.rs", 1, Kind::Comment),
            ]),
        ),
        ("b/c".to_owned(), sites_of(&[("crates/b/c/src/lib.rs", 1, Kind::Comment)])),
    ]);
    expect(counts == want, &format!("the tree's sites: {}", shown(&counts)));
    expect(total(&counts) == [5, 1, 3, 0],&format!("the totals: {:?}", total(&counts)));

    // The ratchet. As recorded, nothing is wrong, and the list reads back as it was written.
    let rows = parse_rows(&render(&counts)).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    expect(
        rows == BTreeMap::from([("a".to_owned(), [4, 1, 3, 0]), ("b/c".to_owned(), [1, 0, 0, 0])]),
        &format!("the rows read back: {rows:?}"),
    );
    expect(problems(&counts, &rows, Some(&rows), None).is_empty(), "the tree as recorded is right");
    // Planted: one `// alloc:` line added. Named by its crate, its column, its place (given the
    // base's diff, which also added the `Global` on that line: not named, it is another column)
    // and the rule.
    let mut fewer = rows.clone();
    fewer.insert("a".to_owned(), [3, 1, 3, 0]);
    let added = BTreeMap::from([(lib.to_owned(), BTreeSet::from([24]))]);
    let said = problems(&counts, &fewer, None, Some(&added));
    expect(
        said.len() == 1
            && said[0].contains("`a` has 4 `// alloc:` lines")
            && said[0].contains("new: crates/a/src/lib.rs:24)")
            && said[0].contains("no comment keeps an allocation")
            && said[0].contains("bump-allocate"),
        &format!("one comment added is caught: {said:?}"),
    );
    // Planted: one `expect` added; the column is named.
    let mut fewer_expects = rows.clone();
    fewer_expects.insert("a".to_owned(), [4, 0, 3, 0]);
    let said = problems(&counts, &fewer_expects, None, None);
    expect(said.len() == 1 && said[0].contains("`a` has 1 `allow`/`expect`s"), &format!("one expect added is caught: {said:?}"));
    // Planted: one site removed. Its column must come down.
    let mut more = rows.clone();
    more.insert("b/c".to_owned(), [2, 0, 0, 0]);
    let said = problems(&counts, &more, None, None);
    expect(
        said.len() == 1 && said[0].contains("`b/c`") && said[0].contains("lower its `comments` to 1"),
        &format!("one site removed is caught: {said:?}"),
    );
    // Planted: `global` off its row, either way: the list is written again.
    for off in [2, 4] {
        let mut stale = rows.clone();
        stale.insert("a".to_owned(), [4, 1, off, 0]);
        let said = problems(&counts, &stale, None, None);
        expect(
            said.len() == 1 && said[0].contains("`a` has 3 lines that name `Global`") && said[0].contains("run command `write`"),
            &format!("`global` off its row by {off} is caught: {said:?}"),
        );
    }
    // Planted: a crate with no row is held to 0; a row with no site goes; a row raised above
    // the base's is caught even where the tree matches it.
    let said = problems(&counts, &BTreeMap::from([("a".to_owned(), [4, 1, 3, 0])]), None, None);
    expect(said.len() == 1 && said[0].contains("`b/c` has 1") && said[0].contains("held"), &format!("a new crate starts at 0: {said:?}"));
    let mut gone = rows.clone();
    gone.insert("old".to_owned(), [5, 0, 2, 0]);
    let said = problems(&counts, &gone, None, None);
    expect(
        !said.is_empty() && said.iter().all(|it| it.contains("`old`")) && said.iter().any(|it| it.contains("remove the row")),
        &format!("a crate gone: {said:?}"),
    );
    let said = problems(&counts, &rows, Some(&fewer), None);
    expect(said.len() == 1 && said[0].contains("the row `a`'s `comments` is 4 and was 3 at the base"), &format!("a row raised: {said:?}"));
    // `global` rises while the base has `// alloc:` lines left to convert, and not after.
    let mut converting = rows.clone();
    converting.insert("a".to_owned(), [5, 1, 2, 0]);
    expect(problems(&counts, &rows, Some(&converting), None).is_empty(), "`global` rises while converting");
    let converted = BTreeMap::from([("a".to_owned(), [0, 1, 2, 0])]);
    let said = problems(&counts, &rows, Some(&converted), None);
    expect(
        said.iter().any(|it| it.contains("the row `a`'s `global` is 3 and was 2 at the base")),
        &format!("`global` only shrinks once no `// alloc:` line is left: {said:?}"),
    );

    // The base's diff: hunks of added lines, a deleted file, a hunk that only removes.
    // The first hunk adds a line whose text starts `++ `: it is no file's header.
    let diff = "diff --git a/crates/a/src/lib.rs b/crates/a/src/lib.rs\n--- a/crates/a/src/lib.rs\n+++ b/crates/a/src/lib.rs\n\
        @@ -2,0 +3 @@ fn f() {\n+++ b/not/a/file\n@@ -9 +10,2 @@\n-old\n+y\n+z\n@@ -20 +21,0 @@\n-w\n\
        diff --git a/crates/a/src/gone.rs b/crates/a/src/gone.rs\n--- a/crates/a/src/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-v\n";
    let lines = added_lines(diff).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    expect(
        lines == BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([3, 10, 11]))]),
        &format!("the lines added: {lines:?}"),
    );
    expect(
        parse_rows("a\t1\t0\t0\t0\nb 2\n").is_err()
            && parse_rows("a\t1\t0\t0\t0\na\t2\t0\t0\t0\n").is_err()
            && parse_rows("a\t1\t0\n").is_err()
            && parse_rows("a\t1\t0\t0\tx\n").is_err()
            && parse_rows("\t1\t0\t0\t0\n").is_err(),
        "a bad row is refused",
    );
    let old = parse_rows("# header\na\t1\n");
    expect(
        old.as_ref().is_err_and(|e| e.contains("previous layout") && e.contains("`write`")),
        &format!("a row of the previous layout says to write the list: {old:?}"),
    );
    // The layout before `signatures`, four fields: refused with the same message.
    let four = parse_rows("# header\na\t1\t0\t2\n");
    expect(
        four.as_ref().is_err_and(|e| e.contains("previous layout") && e.contains("no `signatures`") && e.contains("`write`")),
        &format!("a row of the four-field layout says to write the list: {four:?}"),
    );
    expect(
        previous_layout("# h\n\na\t3\n")
            && previous_layout("# h\na\t3\t0\t1\n")
            && !previous_layout("# h\na\t3\t0\t1\t2\n")
            && !previous_layout("# h\n"),
        "the previous layouts",
    );
    signatures_selftest(p);
    println!("{SCRIPT} selftest: ok");
}

/// The `signatures` fixture, a file of crate `s`. Line by line, what each signature is:
/// - 2-4, `spans`: a signature over three lines whose `Vec<u8>` leaves its allocator to the
///   default: counted, at line 2, its `fn`;
/// - 7, `handed`: `Vec<u8, A>`, generic and handed its allocator: not counted;
/// - 8, `named`: `Vec<u8, std::alloc::Global>`, `Global` written into a signature: counted;
/// - 9, `pages`, the owner's signature, `Arc<dyn PageSource>` two levels down and `String`:
///   counted once;
/// - 11, the same signature in `impl ImageStore for Store`, a trait impl's method: not counted;
/// - 14, `made`, a declaration in `pub trait Local`: counted;
/// - 17, `inherent`, `BTreeMap<u8, u8>` in an inherent impl: counted;
/// - 18, `mapped`, `BTreeMap<u8, u8, A>`: not counted;
/// - 20, `fn main`: not counted;
/// - 21, `text`: `blaze_mem::String<A>` takes its allocator; the `String` and `Vec` after it
///   are a string's and a comment's: not counted;
/// - 22, `pointer`: `fn(u8) -> u8` is a pointer type, no signature, and `pointer`'s own has no
///   such type: not counted;
/// - 24, `from` in `impl<T> From<T> for Kept where T: for<'a> Fn(..)`, a trait impl's method
///   (the bound's `for<'a>` is no impl's `for`): not counted;
/// - 28, `helper`, in `#[cfg(test)] mod tests`: a signature, but test code, so not a site.
const SIGNATURES: &str = "use std::alloc::Global;
pub fn spans(
    a: u8,
) -> Vec<u8> {
    Vec::new()
}
pub fn handed<A: Allocator>(v: Vec<u8, A>) -> Vec<u8, A> { v }
pub fn named(v: Vec<u8, std::alloc::Global>) {}
fn pages(&self) -> Result<Option<std::sync::Arc<dyn PageSource>>, String> { loop {} }
impl ImageStore for Store {
    fn pages(&self) -> Result<Option<std::sync::Arc<dyn PageSource>>, String> { loop {} }
}
pub trait Local {
    fn made(&self) -> Box<u8>;
}
impl Store {
    fn inherent(&self) -> BTreeMap<u8, u8> { loop {} }
    fn mapped<A: Allocator + Clone>(&self, alloc: A) -> BTreeMap<u8, u8, A> { loop {} }
}
fn main() -> Vec<u8> { Vec::new() }
fn text(s: blaze_mem::String<A>) -> &str { \"fn x() -> String {\" } // fn y() -> Vec<u8> {}
fn pointer(f: fn(u8) -> u8) {}
impl<T> From<T> for Kept where T: for<'a> Fn(&'a u8) {
    fn from(t: T) -> Vec<u8> { loop {} }
}
#[cfg(test)]
mod tests {
    fn helper() -> Vec<u8> { Vec::new() }
}
";

/// The lines [`signature_lines`] gives for `text`, in `alloc`.
fn signatures_of<A: Allocator>(text: &str, alloc: A) -> Vec<usize, A> {
    let mut out = Vec::new_in(alloc);
    signature_lines(text, &mut out);
    out
}

/// The `signatures` column: what a signature is, what counts, its planted controls, and the
/// ratchet's refusal of a rise naming the site.
fn signatures_selftest(p: &Patterns) {
    let lines = signatures_of(SIGNATURES, Global);
    expect(lines == [2, 8, 9, 14, 17, 28], &format!("the signatures that fix the allocator: {lines:?}"));
    let (_, found_sites) = scan_file(p, SIGNATURES);
    let sites: Vec<usize> = found_sites.iter().filter(|(_, kind)| *kind == Kind::Signature).map(|(line, _)| *line).collect();
    expect(sites == [2, 8, 9, 14, 17], &format!("the sites outside test code (28 is a test's): {sites:?}"));

    // Planted: `Global` where `handed` takes `A` is counted, at line 7.
    let planted = SIGNATURES.replace("Vec<u8, A>", "Vec<u8, Global>");
    let lines = signatures_of(&planted, Global);
    expect(lines == [2, 7, 8, 9, 14, 17, 28], &format!("`Global` in `handed` is counted: {lines:?}"));
    // Planted: the trait impl made inherent, its `pages` is counted, at line 11.
    let planted = SIGNATURES.replace("impl ImageStore for Store {", "impl Store {");
    let lines = signatures_of(&planted, Global);
    expect(lines == [2, 8, 9, 11, 14, 17, 28], &format!("an inherent impl's method is counted: {lines:?}"));
    // Planted: `From`'s impl made inherent, its `from` is counted, at line 24.
    let planted = SIGNATURES.replace("impl<T> From<T> for Kept where T: for<'a> Fn(&'a u8) {", "impl<T> Kept<T> {");
    let lines = signatures_of(&planted, Global);
    expect(lines == [2, 8, 9, 14, 17, 24, 28], &format!("an inherent impl's `from` is counted: {lines:?}"));
    // Planted: the test module's attribute removed, `helper` is a site.
    let planted = SIGNATURES.replace("#[cfg(test)]\n", "\n");
    let (_, found_sites) = scan_file(p, &planted);
    let sites: Vec<usize> = found_sites.iter().filter(|(_, kind)| *kind == Kind::Signature).map(|(line, _)| *line).collect();
    expect(sites == [2, 8, 9, 14, 17, 28], &format!("`helper` outside test code is a site: {sites:?}"));
    // `String` with no `<..>`, and one fewer argument than the allocator's form, at any depth.
    expect(fixes_allocator("f(x: &[Rc<u8>])") && fixes_allocator("f() -> Option<(u8, Box<dyn Fn(u8) -> u8>)>"), "at depth");
    expect(fixes_allocator("f(s: String)") && !fixes_allocator("f(s: blaze_mem::String<A>)"), "`String`");
    expect(!fixes_allocator("f(v: Vec<u8, A,>)") && fixes_allocator("f(v: Vec<u8,>)"), "a trailing comma adds no argument");
    expect(!fixes_allocator("f(v: MyVec<u8>, s: OsStrings)"), "a longer word is not the type");

    // The ratchet: a rise over the row is refused, with the new site named from the base's diff.
    let path = "crates/s/src/lib.rs";
    let (_, found_sites) = scan_file(p, SIGNATURES);
    let sites: Vec<Hit<Kind>> =
        found_sites.iter().map(|(line, what)| Hit { path: path.to_owned(), line: *line, what: *what }).collect();
    let counts = BTreeMap::from([("s".to_owned(), sites)]);
    let row = tally(&counts["s"]);
    expect(row == [0, 0, 1, 5], &format!("crate `s`'s row: {row:?}"));
    let rows = BTreeMap::from([("s".to_owned(), row)]);
    expect(problems(&counts, &rows, Some(&rows), None).is_empty(), "the tree as recorded is right");
    let fewer = BTreeMap::from([("s".to_owned(), [0, 0, 1, 4])]);
    let added = BTreeMap::from([(path.to_owned(), BTreeSet::from([9]))]);
    let said = problems(&counts, &fewer, None, Some(&added));
    expect(
        said.len() == 1
            && said[0].contains("`s` has 5 function signatures that fix the caller's allocator")
            && said[0].contains("over its row's `signatures` 4")
            && said[0].contains("new: crates/s/src/lib.rs:9)")
            && said[0].contains("generic over `A: Allocator + Clone`"),
        &format!("one signature added is caught and named: {said:?}"),
    );
    // A crate with no row is held to 0; a row above the base's is refused.
    let said = problems(&counts, &BTreeMap::new(), None, None);
    expect(said.iter().any(|it| it.contains("`s` has 5 function signatures") && it.contains("held")), &format!("no row: {said:?}"));
    let said = problems(&counts, &rows, Some(&fewer), None);
    expect(
        said.len() == 1 && said[0].contains("the row `s`'s `signatures` is 5 and was 4 at the base"),
        &format!("a row raised over the base's: {said:?}"),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let p = Patterns::new();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(&p),
        [one] => main_with(&p, &input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern compiles with the regex features the lock builds, so a missing feature fails
    /// `blaze test`, not the script's first run.
    #[test]
    fn the_patterns_compile() {
        let _ = Patterns::new();
    }

    /// The script's own cases run under `blaze test` and `cargo test` too: one that fails ends
    /// the test process with status 1, which fails the run.
    #[test]
    fn the_selftest_holds() {
        selftest(&Patterns::new());
    }

    /// The `signatures` column's cases, each with its control: a multi-line signature, the
    /// generic form, the owner's signature once, a trait impl's method, a trait's declaration
    /// and test code (the fixture [`SIGNATURES`] says what each line is).
    #[test]
    fn signatures_fixing_the_allocator_are_counted() {
        assert_eq!(signatures_of(SIGNATURES, Global), [2, 8, 9, 14, 17, 28]);
        // Multi-line: the site is the `fn`'s line, 1, not the `{`'s, 3.
        assert_eq!(signatures_of("pub fn spans(\n    a: u8,\n) -> Vec<u8> {\n}\n", Global), [1]);
        assert!(signatures_of("pub fn spans(\n    a: u8,\n) -> Vec<u8, A> {\n}\n", Global).is_empty());
        // `Vec<T, A>` is not counted; `Global` given is.
        assert!(signatures_of("fn f<A: Allocator>(v: Vec<u8, A>) {}\n", Global).is_empty());
        assert_eq!(signatures_of("fn f(v: Vec<u8, Global>) {}\n", Global), [1]);
        // The owner's signature: two such types, counted once.
        let owner = "fn pages(&self) -> Result<Option<std::sync::Arc<dyn PageSource>>, String> {}\n";
        assert_eq!(signatures_of(owner, Global), [1]);
        // A trait impl's method is not counted; the same method in an inherent impl is.
        assert!(signatures_of(&format!("impl ImageStore for Store {{\n    {owner}}}\n"), Global).is_empty());
        assert_eq!(signatures_of(&format!("impl Store {{\n    {owner}}}\n"), Global), [2]);
        // A declaration in a trait block is counted.
        assert_eq!(signatures_of("pub trait Local {\n    fn made(&self) -> Box<u8>;\n}\n", Global), [2]);
        // Test code is not a site; the same file without its attribute is.
        let p = Patterns::new();
        let in_test = "#[cfg(test)]\nmod tests {\n    fn helper() -> Vec<u8> { loop {} }\n}\n";
        let signed = |text: &str| -> usize { scan_file(&p, text).1.iter().filter(|(_, kind)| *kind == Kind::Signature).count() };
        assert_eq!(signed(in_test), 0);
        assert_eq!(signed(&in_test.replace("#[cfg(test)]\n", "\n")), 1);
    }

    /// A rise in `signatures` over the crate's row is refused, naming the site the base's diff
    /// added; the tree at its row is not.
    #[test]
    fn a_rise_in_signatures_is_refused_with_its_site() {
        let sites = [
            Hit { path: "crates/s/src/lib.rs".to_owned(), line: 3, what: Kind::Signature },
            Hit { path: "crates/s/src/lib.rs".to_owned(), line: 9, what: Kind::Signature },
        ];
        let counts = BTreeMap::from([("s".to_owned(), sites.to_vec())]);
        let at_row = BTreeMap::from([("s".to_owned(), [0, 0, 0, 2])]);
        assert!(problems(&counts, &at_row, Some(&at_row), None).is_empty());
        let under = BTreeMap::from([("s".to_owned(), [0, 0, 0, 1])]);
        let added = BTreeMap::from([("crates/s/src/lib.rs".to_owned(), BTreeSet::from([9]))]);
        let said = problems(&counts, &under, None, Some(&added));
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("over its row's `signatures` 1") && said[0].contains("new: crates/s/src/lib.rs:9)"), "{said:?}");
        // The layout before this column is refused, saying to write the list.
        assert!(parse_rows("s\t0\t0\t0\n").is_err_and(|e| e.contains("previous layout") && e.contains("`write`")));
    }
}
