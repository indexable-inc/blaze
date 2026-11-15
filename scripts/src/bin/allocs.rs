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
//! crates/blaze-lints/baselines/alloc-sites.txt only shrinks, except `global`, which may rise
//! against the base by the `// alloc:` lines the change converted and by the lines it added
//! naming `Global` where no allocator can be taken (`fn main`, a trait impl's method, outside
//! every function body). It runs with no compiler: a gate step of the `scripts` job
//! (.github/workflows/blaze.yaml), beside `domains`.
//!
//! The `signatures` column (2026-11-14) counts the function signatures that fix a caller's
//! allocator: a std collection or box with its allocator left to the default or given as
//! `Global`, or `String`. The rule it holds is `blaze::unwritten_allocator`'s: a function is
//! generic over `A: Allocator + Clone` and takes `alloc: A`, and the caller names `Global`. It is
//! the count the gate can hold before blaze checks blaze's own crates.
//!
//! The `test_signatures` column (2026-11-15) counts the same signatures in test code: the lint
//! does not exempt tests (crates/blaze-lints/src/blaze/test_code.rs), so a test helper takes its
//! allocator and the `#[test]` function names `Global`. Before it, a helper in a `#[cfg(test)]
//! mod` file, and a `global` that rose while any `// alloc:` line was left anywhere in the tree,
//! passed this gate where the lint refuses them.

#![feature(allocator_api)]

/// What the usage error prints.
const DOC: &str = r#"allocs: what is left of the global heap in each crate only goes down (AGENTS.md bump-allocate).

    blaze run -q -p scripts --bin allocs -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin allocs -- '{"command": "write"}'
    blaze run -q -p scripts --bin allocs -- selftest

    command   check   each count of every crate equals its row's in the list (a `comments`,
                      `expects`, `signatures` or `test_signatures` count over its row is
                      refused, with its new sites named given `base`; one under it must be
                      lowered; a `global` count off its row is written with `write`); a crate
                      with no row is held to 0; with `base`, no `comments`, `expects`,
                      `signatures` or `test_signatures` of a row is above the base's (a crate
                      with no row there counts 0), and its `global` is above the base's by no
                      more than the number its `comments` fell (a `// alloc:` line converted to
                      `Global`, one for one) plus its lines added since the base that name
                      `Global` in a place that cannot take an allocator (below) (CI)
              write   write the list from the tree as it is
    root      the checkout (".")
    base      the commit the change is against, or null: its list (`git show`) holds every
              row down, and the lines added since it (`git diff --unified=0`) name a crate's
              new sites

The list's row is `<crate><TAB><comments><TAB><expects><TAB><global><TAB><signatures><TAB>
<test_signatures>`, of lines of `.rs` files under crates/ (a directory named `target` is not
read); the first four outside test code, the last in it:
  comments  a line with `// alloc: <why>` (`//`, then `alloc:` and a reason that is not
            empty): the comment that once kept an allocator lint's finding and now keeps
            nothing; each is a site to convert, and the count goes to 0
  expects   a line that starts with an `allow` or `expect` attribute naming
            `blaze::implicit_allocator`: an item that says why it keeps the global heap
  global    a line whose code (not a comment, a string or a character, and not a `use` item)
            has the word `Global`: the global heap named where it is used, which moving to
            frames brings down. Its place can take an allocator when the innermost function
            around it (its body, or its signature from `fn` to the `{` or `;`) is a `fn` that
            is neither `fn main` nor a method directly in an `impl <Trait> for <Type>` block;
            a closure is its function's, and a `fn` nested in `fn main` can take one. Every
            other place cannot: `fn main`, a trait impl's method (its signature is the
            trait's), and code outside every function (a static, a field's type, an impl
            header). A `Global` in a function's own signature is in that function's place:
            `fn f(v: Vec<u8, Global>)` can take an allocator, the same in `fn main` or a trait
            impl's method cannot. A line with any `Global` in a place that can take an
            allocator is a line of such a place
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
  test_signatures
            the signatures `signatures` counts, in test code (below): a test helper's, as the
            lint does not exempt tests. The form is the same, and the `#[test]` function is
            the caller that names `Global`
A line can count in more than one column. The previous layouts, `<crate><TAB><count>`,
`<crate><TAB><comments><TAB><expects><TAB><global>` and the same with `<TAB><signatures>`, are
refused with a message to run `write`; a base whose list has one of them holds no row (the
columns begin with the change).

`signatures` is read from the text, so it differs from the lint at the edges: a crate's own type
named `Vec`, a type alias of a std collection, a function a macro makes, a `fn` in a
`macro_rules!` body, and whose trait an impl implements (no impl's method counts, its trait in
the crate or not). The lint `blaze::unwritten_allocator` is the definition; this column is the
count the gate can hold before blaze checks blaze's own crates, and it goes when that lint's
baseline joins the gate.

A site in test code counts in `test_signatures` when it is a signature, and in no column
otherwise. Test code is the code a crate's own tests compile, and nothing else is:
  - a test root: `tests/<name>.rs` or `tests/<name>/main.rs` in the crate's directory
    (`benches` alike), a crate root for module declarations;
  - a file named `tests.rs` or `test.rs`;
  - a file that says `#![cfg(test)]`;
  - the item after `#[cfg(test)]` or `#[cfg(all(test, ...))]`, from the attribute's line to
    the line of the `}` or `;` that ends the item (strings, characters and comments read as
    Rust reads them); an attribute whose item does not end before its block does (a match
    arm, a field) covers its own line only;
  - the file of a module declared out of line in test code (`#[cfg(test)] mod t;`, or any
    `mod m;` in a test file): `m.rs` or `m/mod.rs` beside a crate root (`lib.rs`, `main.rs`,
    `mod.rs`, `build.rs`, a file of a `bin` directory), else in the directory named for the
    declaring file, and so on down (a test root's `mod common;` is `tests/common.rs` or
    `tests/common/mod.rs`). A `#[path = "<file>"]` on the `mod`'s line, or on an attribute
    line before it, names its file instead, relative to the declaring file's directory.
Test data counts in no column: a file under a directory named `tests` or `benches` that no
test root or test module reaches (`tests/data/x.rs`, a UI test's input), and every file of a
crate whose directory is under one (a fixture project blaze builds, with its own Cargo.toml),
which has no row. Everything else counts, `*_tests.rs` files a test module does not declare
included.

A crate is the nearest directory above the file, under crates/, that holds a Cargo.toml, named
by its path under crates/ (`blaze-cargo`, `harness/harness-native`).
"#;

use std::{
    alloc::{Allocator, Global},
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_script::{
    TempDir, fail, git,
    input::{self, Fields, Input, InputError},
    read_text,
    regex::Regex,
    universal_newlines,
};

const SCRIPT: &str = "allocs";
/// The list, from the root.
const LIST: &str = "crates/blaze-lints/baselines/alloc-sites.txt";
/// The word of the comment that once kept an allocator lint's finding and keeps nothing now.
const TAG: &str = "alloc";
/// The global heap's name where it is used (`Vec::new_in(Global)`).
const GLOBAL: &str = "Global";
const TEST_DIRS: [&str; 2] = ["tests", "benches"];
const TEST_FILES: [&str; 2] = ["tests.rs", "test.rs"];
/// The files whose out-of-line modules are beside them, not in a directory named for them.
const ROOT_FILES: [&str; 4] = ["lib.rs", "main.rs", "mod.rs", "build.rs"];
/// What every message about a crate over its row ends with.
const RULE: &str = "AGENTS.md bump-allocate: don't allocate where a borrow, a fixed buffer or the caller's buffer does; \
                    where an allocation is needed, allocate by lifetime and name the allocator (`Vec::new_in(frame)`, or \
                    `Vec::new_in(Global)` for the global heap); never implied, and no comment stands in for it";
/// The list's first lines.
const HEADER: &str = "# What is left of the global heap, by crate (its directory under crates/). Outside test code:\n\
                      # `// alloc: <why>` lines, which keep nothing now and go to 0; `allow`/`expect`s of\n\
                      # `blaze::implicit_allocator`; lines whose code names `Global`; function signatures that fix\n\
                      # the caller's allocator (`blaze::unwritten_allocator`'s rule, read from the text). In test\n\
                      # code: the same signatures. Only shrinks, except `global`, which may rise by the `// alloc:`\n\
                      # lines converted and the lines added naming `Global` where no allocator can be taken (`fn main`,\n\
                      # a trait impl's method, outside every function); a crate with no row is held to 0\n\
                      # (AGENTS.md bump-allocate). Written by scripts/src/bin/allocs.rs (`{\"command\": \"write\"}`),\n\
                      # checked in CI.\n\
                      # crate\tcomments\texpects\tglobal\tsignatures\ttest_signatures\n";
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
            command: f.one_of("command", &["check", "write"], None)?,
            root: f.str("root", Some("."))?,
            base: f.str_or_null("base")?,
        })
    }
}

/// What a site is. Each counts in one of the list's columns ([`Kind::column`]); [`KINDS`] has
/// one kind per column, in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `// alloc: <why>`: kept nothing since 2026-11-13, a site to convert.
    Comment,
    /// An `allow` or `expect` of `blaze::implicit_allocator`.
    Expect,
    /// Code that names `Global`, in a place that can take an allocator ([`Place::Takes`]).
    Global,
    /// Code that names `Global` where no allocator can be taken ([`Place::Fixed`]): counted in
    /// `global`, and a line of it added since the base allows `global` one more.
    FixedGlobal,
    /// A function signature outside test code that fixes its caller's allocator.
    Signature,
    /// The same signature in test code.
    TestSignature,
}

/// One kind per column, in the list's order.
const KINDS: [Kind; 5] = [Kind::Comment, Kind::Expect, Kind::Global, Kind::Signature, Kind::TestSignature];

impl Kind {
    /// Its column in a row, from 0.
    const fn column(self) -> usize {
        match self {
            Kind::Comment => 0,
            Kind::Expect => 1,
            Kind::Global | Kind::FixedGlobal => 2,
            Kind::Signature => 3,
            Kind::TestSignature => 4,
        }
    }

    /// Its column's name in the header and in messages.
    const fn name(self) -> &'static str {
        match self {
            Kind::Comment => "comments",
            Kind::Expect => "expects",
            Kind::Global | Kind::FixedGlobal => "global",
            Kind::Signature => "signatures",
            Kind::TestSignature => "test_signatures",
        }
    }

    /// What its column's sites are, for a message: `<n> <what>`.
    const fn what(self) -> &'static str {
        match self {
            Kind::Comment => "`// alloc:` lines outside tests, which keep nothing now",
            Kind::Expect => "`allow`/`expect`s of `blaze::implicit_allocator` outside tests",
            Kind::Global | Kind::FixedGlobal => "lines that name `Global` outside tests",
            Kind::Signature => "function signatures that fix the caller's allocator outside tests (`blaze::unwritten_allocator`)",
            Kind::TestSignature => {
                "function signatures that fix the caller's allocator in test code (`blaze::unwritten_allocator`, which does not exempt tests)"
            }
        }
    }

    /// What to write instead, for a message about a count over its row.
    const fn instead(self) -> &'static str {
        match self {
            Kind::Comment => "no comment keeps an allocation: borrow or write into a buffer, or name the allocator where it is used",
            Kind::Signature => concat!(
                "make the function generic over `A: Allocator + Clone`, take `alloc: A` from its caller and write `A` in the type ",
                "(`Vec<T, A>`; `blaze_mem::String<A>` or a typed error for `String`); the caller names `Global` where the lifetime ",
                "is decided"
            ),
            Kind::TestSignature => concat!(
                "make the helper generic over `A: Allocator + Clone`, take `alloc: A` from its caller and write `A` in the type ",
                "(`Vec<T, A>`; `blaze_mem::String<A>` or a typed error for `String`); the `#[test]` function names `Global`"
            ),
            Kind::Expect | Kind::Global | Kind::FixedGlobal => {
                "borrow or write into a buffer, or give each value its allocator, from a parameter or the task's frame"
            }
        }
    }
}

/// A crate's counts, by [`Kind::column`].
type Row = [usize; 5];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Site {
    /// Relative to the root.
    path: String,
    line: usize,
    kind: Kind,
}

/// The counts of `sites`, by kind.
fn tally(sites: &[Site]) -> Row {
    let mut row = [0; 5];
    for it in sites {
        row[it.kind.column()] += 1;
    }
    row
}

struct Patterns {
    cfg_test: Regex,
    inner_cfg_test: Regex,
    /// `mod <name>;`, attributes before it on the line allowed.
    module: Regex,
    /// `#[path = "<file>"]` at the line's start: the file of the `mod <name>;` it is on or
    /// before.
    path: Regex,
    /// An `allow` or `expect` of `blaze::implicit_allocator`, at the line's start.
    allowed: Regex,
}

impl Patterns {
    fn new() -> Patterns {
        let re = |pattern: &str| Regex::new(pattern).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
        Patterns {
            cfg_test: re(r"\A\s*#\[cfg\((?:test|all\(test\b[^\]]*)\)\]"),
            inner_cfg_test: re(r"\A\s*#!\[cfg\((?:test|all\(test\b[^\]]*)\)\]"),
            module: re(r"\A\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?(\w+)\s*;"),
            path: re(r#"\A\s*#\[path\s*=\s*"([^"\\]*)"\s*\]"#),
            allowed: re(r"\A\s*#!?\[[^\]]*\b(?:allow|expect)\([^\]]*\bblaze::implicit_allocator\b"),
        }
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

/// What one file holds.
#[derive(Debug, Default, PartialEq, Eq)]
struct Scanned {
    /// The lines (from 1) that are sites, each with what it is, by line and then by kind (a line
    /// can be more than one): outside its test code any kind, in it [`Kind::TestSignature`].
    sites: Vec<(usize, Kind)>,
    /// Each `mod <name>;` it declares.
    mods: Vec<Declared>,
    /// `#![cfg(test)]`: the whole file is test code.
    test: bool,
}

/// A module a file declares out of line, `mod <name>;`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Declared {
    name: String,
    /// Whether test code declares it.
    test: bool,
    /// Its `#[path = ".."]`, on its line or an attribute line before it: relative to the
    /// declaring file's directory.
    path: Option<String>,
}

/// The sites of one file's text and the modules it declares out of line.
fn scan_file(p: &Patterns, text: &str) -> Scanned {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut test = vec![false; lines.len()];
    let mut out = Scanned::default();
    let code = code_lines(text, Global);
    let mut global = code.global.iter().peekable();
    let mut signatures = code.signatures.iter().peekable();
    // The byte where line `at` starts.
    let mut start = 0;
    // A `#[path = ".."]` on an attribute line, waiting for the `mod` its attributes are on.
    let mut path: Option<String> = None;
    for (at, line) in lines.iter().enumerate() {
        if p.inner_cfg_test.is_match(line) {
            out.test = true;
        }
        if let Some(found) = p.cfg_test.find(line) {
            // The item runs from after the attribute to its `}` or `;`, on the line of that byte.
            let last = match item_end(text, start + found.end()) {
                Some(end) => at + text[start..end - 1].matches('\n').count(),
                None => at,
            };
            for it in &mut test[at..=last] {
                *it = true;
            }
        }
        let path_here = p.path.captures(line).map(|it| it[1].to_owned());
        if let Some(found) = p.module.captures(line) {
            out.mods.push(Declared { name: found[1].to_owned(), test: test[at], path: path_here.or(path.take()) });
        } else if path_here.is_some() {
            path = path_here;
        } else if !matches!(line.trim_start().get(..2), None | Some("#[" | "//")) {
            // A line of code between: the attribute was another item's.
            path = None;
        }
        // `global` is in line order: take this line's entry, if it has one, test code or not.
        let named = global.next_if(|(line, _)| *line == at + 1).map(|(_, place)| match place {
            Place::Takes => Kind::Global,
            Place::Fixed => Kind::FixedGlobal,
        });
        // `signatures` likewise, each signature whose `fn` is on this line (two can share one).
        let mut signed: usize = 0;
        // Bounded: each turn takes one entry of the list.
        while signatures.next_if(|it| **it == at + 1).is_some() {
            signed += 1;
        }
        if !test[at] {
            let kinds = [(keeps(line), Kind::Comment), (p.allowed.is_match(line), Kind::Expect)];
            out.sites.extend(kinds.iter().filter(|(is, _)| *is).map(|(_, kind)| (at + 1, *kind)));
            out.sites.extend(named.map(|kind| (at + 1, kind)));
        }
        let signature = if test[at] { Kind::TestSignature } else { Kind::Signature };
        out.sites.extend(std::iter::repeat_n((at + 1, signature), signed));
        start += line.len() + 1;
    }
    if out.test {
        as_test_code(&mut out.sites);
        for it in &mut out.mods {
            it.test = true;
        }
    }
    out
}

/// Makes `sites`, a file's, those of test code: its signatures are [`Kind::TestSignature`], and
/// no other site counts in test code.
fn as_test_code(sites: &mut Vec<(usize, Kind)>) {
    sites.retain_mut(|(_, kind)| {
        let signature = matches!(kind, Kind::Signature | Kind::TestSignature);
        if signature {
            *kind = Kind::TestSignature;
        }
        signature
    });
}

/// The byte after the `}` or `;` that ends the item starting at byte `from` of `text`: the
/// first `;` outside brackets, or the `}` that closes its first brace; `None` when a closing
/// bracket comes first (the item is part of a larger one) or the text ends. Comments, strings
/// (raw and byte strings too) and character literals are skipped, so a brace in them counts not.
fn item_end(text: &str, from: usize) -> Option<usize> {
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
            b'{' | b'(' | b'[' => {
                depth += 1;
                at + 1
            }
            b'}' | b')' | b']' => {
                depth = depth.checked_sub(1)?;
                if byte == b'}' && depth == 0 {
                    return Some(at + 1);
                }
                at + 1
            }
            b';' if depth == 0 => return Some(at + 1),
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => word_end(text, at),
            _ => at + 1,
        };
    }
    None
}

/// Whether the place of a line naming `Global` can take an allocator in its stead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// In a `fn`'s body or signature, the `fn` neither `fn main` nor a method directly in an
    /// `impl <Trait> for <Type>` block: it can be made generic over its allocator.
    Takes,
    /// In `fn main`, in a trait impl's method (its signature is the trait's), or outside every
    /// function (a static, a field's type, an impl header).
    Fixed,
}

/// What a `{` opens, for [`code_lines`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    /// An `impl <Trait> for <Type>` block: its methods' signatures are the trait's.
    TraitImpl,
    /// A function's body, with whether that function can take an allocator.
    Body(Place),
    /// Any other: a module, a trait, an inherent impl, a struct, an expression's block.
    Other,
}

/// What [`code_lines`] finds in a file's text, each list in line order.
struct CodeLines<A: Allocator> {
    /// The lines (from 1, each once) whose code has the word `Global`, with their place: a line
    /// with any `Global` in a place that can take an allocator is [`Place::Takes`].
    global: Vec<(usize, Place), A>,
    /// The line of the `fn` of each function signature that fixes its caller's allocator; two
    /// signatures on a line are there twice.
    signatures: Vec<usize, A>,
}

/// The line (from 1) of a byte, counted forward: each call's byte is at or after the last's.
struct LineOf {
    line: usize,
    counted: usize,
}

impl LineOf {
    fn at(&mut self, text: &str, at: usize) -> usize {
        self.line += text[self.counted..at].matches('\n').count();
        self.counted = at;
        self.line
    }
}

/// The lines of `text`'s code that name `Global`, and of its signatures that fix the caller's
/// allocator, in one pass that knows each byte's function. Comments, strings (raw and byte
/// strings too) and character literals are skipped as [`item_end`] skips them, and a word is a
/// whole identifier (`MyGlobal` and `Global2` are not `Global`).
/// - `Global` in a `use` item, from the word `use` to its `;`, names nothing (its import is not
///   a use of the heap). Its [`Place`] is that of the innermost function around it: the one
///   whose signature it is in, else the one whose body it is in, a closure's or a block's being
///   its function's; none is [`Place::Fixed`].
/// - A signature counts when it [`fixes_allocator`] and its function can take an allocator: not
///   a method directly in an `impl <Trait> for <Type>` block (its signature is the trait's), not
///   `fn main`. A function pointer type (`fn` with no name after it) is no signature.
fn code_lines<A: Allocator + Clone>(text: &str, alloc: A) -> CodeLines<A> {
    let bytes = text.as_bytes();
    let mut out = CodeLines { global: Vec::new_in(alloc.clone()), signatures: Vec::new_in(alloc.clone()) };
    // What each `{` open opened, outermost first.
    let mut blocks = Vec::new_in(alloc);
    // From the word `impl` to the `{` or `;` after it: whether a `for` is in it (not `for<..>`,
    // a bound's).
    let mut header: Option<bool> = None;
    // The signature being read: the byte of the `{` or `;` that ends it, and its function's place.
    let mut signature: Option<(usize, Place)> = None;
    let mut in_use = false;
    let mut lines = LineOf { line: 1, counted: 0 };
    let mut at = 0;
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        if signature.is_some_and(|(end, _)| end < at) {
            signature = None;
        }
        at = match byte {
            b'/' if next == Some(b'/') => text[at..].find('\n').map_or(bytes.len(), |n| at + n),
            b'/' if next == Some(b'*') => comment_end(bytes, at),
            b'"' => string_end(bytes, at + 1),
            b'\'' => quote_end(text, at),
            b'{' => {
                let trait_impl = header.take() == Some(true);
                blocks.push(match signature {
                    Some((end, place)) if end == at => Block::Body(place),
                    _ if trait_impl => Block::TraitImpl,
                    _ => Block::Other,
                });
                at + 1
            }
            b'}' => {
                blocks.pop();
                at + 1
            }
            b';' => {
                header = None;
                in_use = false;
                at + 1
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let end = word_end(text, at);
                match &text[at..end] {
                    "use" => in_use = true,
                    "impl" => header = Some(false),
                    "for" if header.is_some() && !text[end..].trim_start().starts_with('<') => header = Some(true),
                    "fn" => {
                        if let Some(name) = fn_name(&text[end..])
                            && let Some(close) = signature_end(text, end)
                        {
                            let fixed = name == "main" || blocks.last() == Some(&Block::TraitImpl);
                            let place = if fixed { Place::Fixed } else { Place::Takes };
                            signature = Some((close, place));
                            if place == Place::Takes && fixes_allocator(&text[end..close]) {
                                out.signatures.push(lines.at(text, at));
                            }
                        }
                    }
                    GLOBAL if !in_use => {
                        let line = lines.at(text, at);
                        let place = match signature {
                            Some((_, place)) => place,
                            None => blocks
                                .iter()
                                .rev()
                                .find_map(|it| match it {
                                    Block::Body(place) => Some(*place),
                                    Block::TraitImpl | Block::Other => None,
                                })
                                .unwrap_or(Place::Fixed),
                        };
                        match out.global.last_mut() {
                            Some((last, seen)) if *last == line => {
                                if place == Place::Takes {
                                    *seen = Place::Takes;
                                }
                            }
                            _ => out.global.push((line, place)),
                        }
                    }
                    _ => {}
                }
                end
            }
            // A number's digits and suffix (`1u8`, `0x1F`): never the start of a word.
            b'0'..=b'9' => at + bytes[at..].iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count(),
            _ => at + 1,
        };
    }
    out
}

/// The name after a `fn` (`rest` is the text after the word), its `r#` dropped; `None` for a
/// function pointer type (`fn(u8)`) or a macro's `fn $name`.
fn fn_name(rest: &str) -> Option<&str> {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("r#").unwrap_or(rest);
    let len = rest.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
    let name = &rest[..len];
    name.bytes().next().is_some_and(|b| !b.is_ascii_digit()).then_some(name)
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
            b'0'..=b'9' => at + bytes[at..].iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count(),
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

/// The byte after the block comment that opens at `from`, comments nested in it included.
fn comment_end(bytes: &[u8], from: usize) -> usize {
    let mut depth: usize = 0;
    let mut at = from;
    while at + 1 < bytes.len() {
        match (bytes[at], bytes[at + 1]) {
            (b'/', b'*') => {
                depth += 1;
                at += 2;
            }
            (b'*', b'/') => {
                depth = depth.saturating_sub(1);
                at += 2;
                if depth == 0 {
                    return at;
                }
            }
            _ => at += 1,
        }
    }
    bytes.len()
}

/// The byte after the `"` that closes a string whose text starts at `from`.
fn string_end(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// The byte after the character literal at the `'` at `at`, or after the `'` alone when it opens
/// a lifetime or a label.
fn quote_end(text: &str, at: usize) -> usize {
    let rest = &text[at + 1..];
    if rest.starts_with('\\') {
        // An escape (`'\n'`, `'\''`, `'\u{7f}'`): closed by the next quote after the escaped one.
        return rest.get(2..).and_then(|it| it.find('\'')).map_or(text.len(), |n| at + 3 + n + 1);
    }
    match rest.chars().next() {
        Some(c) if rest[c.len_utf8()..].starts_with('\'') => at + 1 + c.len_utf8() + 1,
        _ => at + 1,
    }
}

/// The byte after the word at `at`, or after the literal it prefixes (`r#"..."#`, `b"..."`,
/// `br"..."`, `c"..."`, `b'x'`).
fn word_end(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    let end = at + bytes[at..].iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count();
    match (&text[at..end], bytes.get(end).copied()) {
        ("r" | "br" | "cr", Some(b'#' | b'"')) => raw_end(bytes, end),
        ("b" | "c", Some(b'"')) => string_end(bytes, end + 1),
        ("b", Some(b'\'')) => quote_end(text, end),
        _ => end,
    }
}

/// The byte after the raw string whose `#`s or `"` start at `from`; for a raw identifier
/// (`r#type`), the byte after its `#`s.
fn raw_end(bytes: &[u8], from: usize) -> usize {
    let hashes = bytes[from..].iter().take_while(|b| **b == b'#').count();
    let open = from + hashes;
    if bytes.get(open) != Some(&b'"') {
        return open;
    }
    let mut at = open + 1;
    while at < bytes.len() {
        if bytes[at] == b'"' && bytes[at + 1..].iter().take(hashes).filter(|b| **b == b'#').count() == hashes {
            return at + 1 + hashes;
        }
        at += 1;
    }
    bytes.len()
}

/// What a file's path says it is, given its crate (by its path under crates/).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathRole {
    /// The crate's code: test code only by what declares it or what it says.
    Source,
    /// Named `tests.rs` or `test.rs`: test code.
    TestFile,
    /// A crate root of the crate's own tests or benches: `tests/<name>.rs` or
    /// `tests/<name>/main.rs` in the crate's directory (`benches` alike). Test code.
    TestRoot,
    /// Any other file under a directory named `tests` or `benches`: data that blaze reads or
    /// builds, counted nowhere, unless a test root's module declarations reach it.
    UnderTestDir,
    /// A file of a crate whose directory is under a `tests` or `benches` directory: a fixture
    /// project blaze builds as input. Counted nowhere, and no row.
    Fixture,
}

fn path_role(parts: &[String], krate: &str) -> PathRole {
    let test_dir = |part: &str| TEST_DIRS.contains(&part);
    if krate.split('/').any(test_dir) {
        return PathRole::Fixture;
    }
    let (name, dirs) = parts.split_last().map_or(("", &[][..]), |(name, dirs)| (name.as_str(), dirs));
    // The parts after the crate's directory, `crates/<krate>`.
    let inside = parts.get(1 + krate.split('/').count()..).unwrap_or(&[]);
    match inside {
        [dir, _] if test_dir(dir) => PathRole::TestRoot,
        [dir, _, main] if test_dir(dir) && main == "main.rs" => PathRole::TestRoot,
        _ if dirs.iter().any(|it| test_dir(it)) => PathRole::UnderTestDir,
        _ if TEST_FILES.contains(&name) => PathRole::TestFile,
        _ => PathRole::Source,
    }
}

/// The two files the module `name`, declared out of line in the file `parts`, can be; `root`
/// says the file is a crate root whatever its name (a test root).
fn module_files(parts: &[String], name: &str, root: bool) -> [String; 2] {
    let Some((file, dirs)) = parts.split_last() else { return [String::new(), String::new()] };
    let mut base: Vec<&str> = dirs.iter().map(String::as_str).collect();
    let root = root || ROOT_FILES.contains(&file.as_str()) || dirs.last().is_some_and(|it| it == "bin");
    if !root {
        base.push(file.strip_suffix(".rs").unwrap_or(file));
    }
    let base = base.join("/");
    [format!("{base}/{name}.rs"), format!("{base}/{name}/mod.rs")]
}

/// The file a `#[path = "<file>"]` names, declared in the file `parts`: relative to its
/// directory, `.` and `..` resolved (a module out of line, not in an inline module's block).
fn path_file(parts: &[String], file: &str) -> String {
    let mut out: Vec<&str> = parts.split_last().map_or(Vec::new(), |(_, dirs)| dirs.iter().map(String::as_str).collect());
    for part in file.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            _ => out.push(part),
        }
    }
    out.join("/")
}

/// `(root / "crates").rglob("*.rs")`, sorted part by part, each as its parts from the root, none
/// under a directory named `target`. Directories that are links are not entered.
fn rust_files(root: &Path) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut todo = vec![vec!["crates".to_owned()]];
    // Bounded: each directory is listed once, and links to directories are not followed.
    while let Some(dir) = todo.pop() {
        let full: PathBuf = dir.iter().fold(root.to_path_buf(), |path, part| path.join(part));
        let Ok(listed) = std::fs::read_dir(&full) else { continue };
        for entry in listed.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut parts = dir.clone();
            parts.push(name.clone());
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && name != "target" {
                todo.push(parts);
            } else if !is_dir && name.ends_with(".rs") {
                out.push(parts);
            }
        }
    }
    out.sort();
    out
}

/// The crate of the file `parts`: the nearest directory above it, under crates/, with a
/// Cargo.toml, by its path under crates/; the first directory under crates/ when none has one.
fn crate_of(root: &Path, parts: &[String], manifests: &mut BTreeMap<Vec<String>, bool>) -> String {
    for end in (2..parts.len()).rev() {
        let dir = &parts[..end];
        let has = *manifests.entry(dir.to_vec()).or_insert_with(|| {
            dir.iter().fold(root.to_path_buf(), |path, part| path.join(part)).join("Cargo.toml").is_file()
        });
        if has {
            return parts[1..end].join("/");
        }
    }
    match parts {
        [_, first, _, ..] => first.clone(),
        _ => "crates".to_owned(),
    }
}

/// Each crate's sites, in file and line order (a test file's are [`as_test_code`]'s; a file
/// under a `tests` or `benches` directory that no test root reaches, and a fixture crate's, have
/// none); a crate with none is absent.
fn count(p: &Patterns, root: &Path) -> BTreeMap<String, Vec<Site>> {
    let files = rust_files(root);
    let known: BTreeSet<String> = files.iter().map(|it| it.join("/")).collect();
    let mut manifests: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    let mut scanned: Vec<(Vec<String>, String, Scanned)> = Vec::new();
    let mut test: BTreeSet<String> = BTreeSet::new();
    let mut data: BTreeSet<String> = BTreeSet::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for parts in files {
        let path = parts.join("/");
        let krate = crate_of(root, &parts, &mut manifests);
        let role = path_role(&parts, &krate);
        if role == PathRole::Fixture {
            continue;
        }
        let full = parts.iter().fold(root.to_path_buf(), |path, part| path.join(part));
        // `read_text(errors="replace")`: what is not UTF-8 becomes U+FFFD.
        let Ok(bytes) = std::fs::read(&full) else { fail(SCRIPT, &format!("{} could not be read", full.display())) };
        let text = universal_newlines(String::from_utf8_lossy(&bytes).into_owned());
        let found = scan_file(p, &text);
        match role {
            PathRole::UnderTestDir => {
                data.insert(path.clone());
            }
            PathRole::TestRoot | PathRole::TestFile => {
                test.insert(path.clone());
            }
            PathRole::Source | PathRole::Fixture => {}
        }
        if found.test {
            test.insert(path.clone());
        }
        for declared in &found.mods {
            let files = match &declared.path {
                Some(file) => vec![path_file(&parts, file)],
                None => module_files(&parts, &declared.name, role == PathRole::TestRoot).to_vec(),
            };
            for file in files {
                if !known.contains(&file) {
                    continue;
                }
                if declared.test {
                    test.insert(file.clone());
                }
                children.entry(path.clone()).or_default().push(file);
            }
        }
        scanned.push((parts, krate, found));
    }
    // A module that test code declares is test code, and so is every module it declares.
    let mut todo: Vec<String> = test.iter().cloned().collect();
    // Bounded: a file enters `test` once, and only a file that entered is taken again.
    while let Some(path) = todo.pop() {
        for child in children.get(&path).into_iter().flatten() {
            if test.insert(child.clone()) {
                todo.push(child.clone());
            }
        }
    }
    let mut out: BTreeMap<String, Vec<Site>> = BTreeMap::new();
    for (parts, krate, mut found) in scanned {
        let path = parts.join("/");
        if test.contains(&path) {
            as_test_code(&mut found.sites);
        } else if data.contains(&path) {
            continue;
        }
        if found.sites.is_empty() {
            continue;
        }
        out.entry(krate)
            .or_default()
            .extend(found.sites.iter().map(|(line, kind)| Site { path: path.clone(), line: *line, kind: *kind }));
    }
    out
}

/// A row's layout, for messages.
const LAYOUT: &str = "`<crate><TAB><comments><TAB><expects><TAB><global><TAB><signatures><TAB><test_signatures>`";

/// The list as `write` writes it: the header, then a [`LAYOUT`] row for each crate with a site,
/// sorted.
fn render(counts: &BTreeMap<String, Vec<Site>>) -> String {
    let mut out = HEADER.to_owned();
    for (krate, sites) in counts.iter().filter(|(_, sites)| !sites.is_empty()) {
        let [comments, expects, global, signatures, test_signatures] = tally(sites);
        out.push_str(&format!("{krate}\t{comments}\t{expects}\t{global}\t{signatures}\t{test_signatures}\n"));
    }
    out
}

/// The list's rows; `Err` names the line that is not a row, a comment or blank, and says to run
/// `write` for a row of a previous layout: `<crate><TAB><count>`, `<crate><TAB><comments><TAB>
/// <expects><TAB><global>`, or that with `<TAB><signatures>`.
fn parse_rows(text: &str) -> Result<BTreeMap<String, Row>, String> {
    let mut rows = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (krate, fields) = line.split_once('\t').unwrap_or((line, ""));
        let numbers: Option<Vec<usize>> = fields.split('\t').map(|it| it.parse::<usize>().ok()).collect();
        let row = match numbers {
            Some(numbers) if !krate.is_empty() => {
                if let Some(layout) = previous_row_layout(numbers.len()) {
                    return Err(format!("{LIST}:{}: {line:?} is a row of a previous layout, {layout}: run command `write`", at + 1));
                }
                <Row>::try_from(numbers.as_slice()).ok()
            }
            _ => None,
        };
        let Some(n) = row else {
            return Err(format!("{LIST}:{}: {line:?} is not {LAYOUT}", at + 1));
        };
        if rows.insert(krate.to_owned(), n).is_some() {
            return Err(format!("{LIST}:{}: `{krate}` has two rows", at + 1));
        }
    }
    Ok(rows)
}

/// The previous layout whose rows have `numbers` numbers after the crate, for a message; `None`
/// for any other count.
fn previous_row_layout(numbers: usize) -> Option<&'static str> {
    match numbers {
        1 => Some("`<crate><TAB><count>`"),
        3 => Some("`<crate><TAB><comments><TAB><expects><TAB><global>`, with no `signatures` or `test_signatures`"),
        4 => Some("`<crate><TAB><comments><TAB><expects><TAB><global><TAB><signatures>`, with no `test_signatures`"),
        _ => None,
    }
}

/// Whether `text` is a list of a previous layout ([`previous_row_layout`]), read from its first
/// row.
fn previous_layout(text: &str) -> bool {
    let first = text.split('\n').find(|line| !line.is_empty() && !line.starts_with('#'));
    first.is_some_and(|line| previous_row_layout(line.split('\t').count() - 1).is_some())
}

/// The lines each file has that `base` did not, from `git diff --unified=0`'s hunks: by path
/// from the root, the line numbers in the tree.
fn added_lines(diff: &str) -> Result<BTreeMap<String, BTreeSet<usize>>, String> {
    let mut out: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    let mut file: Option<String> = None;
    // The lines of the hunk being read, which are text, never a header (`+++ x` is a line `++ x`).
    let mut skip: usize = 0;
    for line in diff.split('\n') {
        if skip > 0 {
            skip -= 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            // `/dev/null` for a file deleted: no line of it is in the tree.
            file = path.strip_prefix("b/").map(str::to_owned);
            continue;
        }
        let Some(hunk) = line.strip_prefix("@@ ") else { continue };
        // `@@ -a,b +c,d @@`: `b` lines from `a` were the base's, `d` from `c` are the tree's
        // (one when `,b` or `,d` is left out).
        let range = |sign: char| -> Option<(usize, usize)> {
            let it = hunk.split(' ').find_map(|it| it.strip_prefix(sign))?;
            let (from, len) = it.split_once(',').unwrap_or((it, "1"));
            Some((from.parse().ok()?, len.parse().ok()?))
        };
        let (Some((_, removed)), Some((from, len))) = (range('-'), range('+')) else {
            return Err(format!("git diff's hunk {line:?} is not `@@ -<line>[,<n>] +<line>[,<n>] @@`"));
        };
        skip = removed + len;
        if let Some(path) = &file {
            out.entry(path.clone()).or_default().extend(from..from + len);
        }
    }
    Ok(out)
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
    Ok((rows, added_lines(&diff)?))
}

/// What is wrong: `counts` the tree's sites by crate, `rows` the list's, `base_rows` the base's
/// list (when it had one), `added` the lines added since the base (when one was given).
fn problems(
    counts: &BTreeMap<String, Vec<Site>>,
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
                out.push(format!("`{krate}` has {now} {what}, and its row's `{column}` in {LIST} is {held}: run command `write`"));
            } else if now > held {
                let shown = match row {
                    Some(_) => format!("its row's `{column}` {held}"),
                    None => "0, as a crate with no row is held".to_owned(),
                };
                let new = match added {
                    Some(added) => {
                        let new = added_sites(sites, added, |it| it.column() == kind.column());
                        if new.is_empty() {
                            "none is on a line added since the base: a site moved in from another crate, or a file git does not track".to_owned()
                        } else {
                            format!("new: {}", new.join(", "))
                        }
                    }
                    None => "give `base`, the commit the change is against, to have the new ones named".to_owned(),
                };
                out.push(format!("`{krate}` has {now} {what}, over {shown} in {LIST} ({new}): {} ({RULE})", kind.instead()));
            } else if now < held {
                let lower = match now_row {
                    [0, 0, 0, 0, 0] => "remove the row".to_owned(),
                    _ => format!("lower its `{column}` to {now}"),
                };
                out.push(format!(
                    "`{krate}` has {now} {what}, under its row's `{column}` {held} in {LIST}: {lower} (command `write`); the list only shrinks"
                ));
            }
        }
    }
    if let Some(base) = base_rows {
        for (krate, n) in rows {
            let was = base.get(krate).copied().unwrap_or([0; 5]);
            for kind in KINDS.into_iter().filter(|it| *it != Kind::Global) {
                let (now, then) = (n[kind.column()], was[kind.column()]);
                if now > then {
                    out.push(format!(
                        "{LIST}: the row `{krate}`'s `{}` is {now} and was {then} at the base: the list only shrinks ({RULE})",
                        kind.name()
                    ));
                }
            }
            let sites = counts.get(krate).map_or(&[][..], Vec::as_slice);
            out.extend(global_rise(krate, sites, *n, was, added));
        }
    }
    out
}

/// `path:line` of each of `sites` whose kind is `kind` on a line `added` since the base.
fn added_sites(sites: &[Site], added: &BTreeMap<String, BTreeSet<usize>>, kind: impl Fn(Kind) -> bool) -> Vec<String> {
    sites
        .iter()
        .filter(|it| kind(it.kind) && added.get(&it.path).is_some_and(|lines| lines.contains(&it.line)))
        .map(|it| format!("{}:{}", it.path, it.line))
        .collect()
}

/// The message when `krate`'s `global`, `now` in its row against `was` at the base, rose past
/// what the change allows: the base's, plus the `// alloc:` lines its `comments` fell by (each
/// converted to `Global`), plus its lines `added` since the base that name `Global` where no
/// allocator can be taken ([`Kind::FixedGlobal`] among `sites`, the crate's). It names the
/// added lines in a function that could take one.
fn global_rise(krate: &str, sites: &[Site], now: Row, was: Row, added: Option<&BTreeMap<String, BTreeSet<usize>>>) -> Option<String> {
    let (global, comments) = (Kind::Global.column(), Kind::Comment.column());
    let converted = was[comments].saturating_sub(now[comments]);
    let fixed = added.map_or(0, |added| added_sites(sites, added, |it| it == Kind::FixedGlobal).len());
    let allowed = was[global] + converted + fixed;
    if now[global] <= allowed {
        return None;
    }
    let open = added.map_or_else(Vec::new, |added| added_sites(sites, added, |it| it == Kind::Global));
    let new = if open.is_empty() {
        "none is on a line added since the base in a function that can take an allocator: a site moved in from another crate, or a file git does not track".to_owned()
    } else {
        format!("new in a function that can take an allocator: {}", open.join(", "))
    };
    Some(format!(
        "{LIST}: the row `{krate}`'s `global` is {}, {} at the base, over the {allowed} it may be: the {converted} `// alloc:` lines \
         converted and the {fixed} added lines naming `Global` where no allocator can be taken (`fn main`, a trait impl's \
         method, outside every function) allow that many more ({new}): make the function generic over `A: Allocator + Clone`, \
         take `alloc: A` from its caller, and let the caller that decides the lifetime name `Global` ({RULE})",
        now[global], was[global]
    ))
}

/// The tree's counts, by kind.
fn total(counts: &BTreeMap<String, Vec<Site>>) -> Row {
    let mut row = [0; 5];
    for sites in counts.values() {
        for (sum, n) in row.iter_mut().zip(tally(sites)) {
            *sum += n;
        }
    }
    row
}

/// `counts`' totals, for a line of output.
fn summary(counts: &BTreeMap<String, Vec<Site>>) -> String {
    let [comments, expects, global, signatures, test_signatures] = total(counts);
    format!(
        "{comments} `// alloc:` lines left to convert, {expects} `allow`/`expect`s of the lint, {global} lines naming `Global` \
         and {signatures} signatures fixing the caller's allocator outside tests, and {test_signatures} such signatures in \
         test code, in {} crates",
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
            "{LIST} is not as command `write` writes it (its header, then a sorted {LAYOUT} row per crate with a site): run it"
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
fn shown(counts: &BTreeMap<String, Vec<Site>>) -> String {
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
fn sites_of(path_lines: &[(&str, usize, Kind)]) -> Vec<Site> {
    path_lines.iter().map(|(path, line, kind)| Site { path: (*path).to_owned(), line: *line, kind: *kind }).collect()
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
    let global = code_lines(text, Global).global;
    let takes = |lines: &[usize]| -> Vec<(usize, Place)> { lines.iter().map(|it| (*it, Place::Takes)).collect() };
    expect(global.as_slice() == takes(&[18, 24, 29, 33]).as_slice(), &format!("the lines that name `Global`: {global:?}"));
    let found = scan_file(p, text);
    let want = [
        (3, Kind::Comment),
        (20, Kind::Comment),
        (21, Kind::Expect),
        (24, Kind::Comment),
        (24, Kind::Global),
        (29, Kind::Global),
        (33, Kind::Global),
    ];
    expect(found.sites == want, &format!("the sites outside test code (line 18 is a test's): {:?}", found.sites));
    expect(
        found.mods
            == [
                Declared { name: "t".to_owned(), test: true, path: None },
                Declared { name: "live".to_owned(), test: false, path: None },
            ],
        &format!("the modules declared out of line: {:?}", found.mods),
    );
    let inner = scan_file(p, "#![cfg(test)]\nmod deep;\nfn x() { Vec::new_in(Global) } // alloc: a test file's\n");
    expect(
        inner.test && inner.sites.is_empty() && inner.mods == [Declared { name: "deep".to_owned(), test: true, path: None }],
        &format!("{inner:?}"),
    );
    // A `#[path]` on the `mod`'s line or an attribute line before it is the module's file; one
    // that an item's code line separates from the `mod` is not.
    let pathed = scan_file(p, "#[cfg(test)]\n#[path = \"a/b.rs\"]\nmod b;\n#[path = \"x.rs\"] mod x;\n#[path = \"no.rs\"]\nfn f() {}\nmod c;\n");
    let paths: Vec<Option<&str>> = pathed.mods.iter().map(|it| it.path.as_deref()).collect();
    expect(paths == [Some("a/b.rs"), Some("x.rs"), None], &format!("the modules' `#[path]`s: {paths:?}"));
    let it: Vec<String> = "crates/a/tests/it.rs".split('/').map(str::to_owned).collect();
    expect(path_file(&it, "../tests/it/./x.rs") == "crates/a/tests/it/x.rs", "a `#[path]` is relative to its file's directory");
    // A field's attribute: the item ends with its struct, so the attribute's line alone is test.
    // A field's type is outside every function: no allocator can be taken there.
    let field = scan_file(p, "struct S {\n    #[cfg(test)]\n    a: Vec<u8, Global>, // alloc: counted\n}\n");
    expect(
        field.sites == [(3, Kind::Comment), (3, Kind::FixedGlobal)],
        &format!("a field under cfg(test) is counted: {:?}", field.sites),
    );
    expect(item_end("{ \"\\\"}\" }", 0) == Some(9), "an escaped quote does not end a string");
    expect(item_end("br##\"}\"#\"##;", 0) == Some(12), "a raw byte string with hashes");

    let parts = |path: &str| -> Vec<String> { path.split('/').map(str::to_owned).collect() };
    expect(
        module_files(&parts("crates/a/src/lib.rs"), "t", false) == ["crates/a/src/t.rs".to_owned(), "crates/a/src/t/mod.rs".to_owned()],
        "a crate root's module is beside it",
    );
    expect(
        module_files(&parts("crates/a/src/t.rs"), "deep", false)
            == ["crates/a/src/t/deep.rs".to_owned(), "crates/a/src/t/deep/mod.rs".to_owned()],
        "another file's module is in the directory named for it",
    );
    expect(module_files(&parts("crates/a/src/bin/x.rs"), "m", false)[0] == "crates/a/src/bin/m.rs", "a bin's module");
    expect(module_files(&parts("crates/a/tests/it.rs"), "common", true)[1] == "crates/a/tests/common/mod.rs", "a test root's module");

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
    expect(total(&counts) == [5, 1, 3, 0, 0], &format!("the totals: {:?}", total(&counts)));

    // The ratchet. As recorded, nothing is wrong, and the list reads back as it was written.
    let rows = parse_rows(&render(&counts)).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    expect(
        rows == BTreeMap::from([("a".to_owned(), [4, 1, 3, 0, 0]), ("b/c".to_owned(), [1, 0, 0, 0, 0])]),
        &format!("the rows read back: {rows:?}"),
    );
    expect(problems(&counts, &rows, Some(&rows), None).is_empty(), "the tree as recorded is right");
    // Planted: one `// alloc:` line added. Named by its crate, its column, its place (given the
    // base's diff, which also added the `Global` on that line: not named, it is another column)
    // and the rule.
    let mut fewer = rows.clone();
    fewer.insert("a".to_owned(), [3, 1, 3, 0, 0]);
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
    fewer_expects.insert("a".to_owned(), [4, 0, 3, 0, 0]);
    let said = problems(&counts, &fewer_expects, None, None);
    expect(said.len() == 1 && said[0].contains("`a` has 1 `allow`/`expect`s"), &format!("one expect added is caught: {said:?}"));
    // Planted: one site removed. Its column must come down.
    let mut more = rows.clone();
    more.insert("b/c".to_owned(), [2, 0, 0, 0, 0]);
    let said = problems(&counts, &more, None, None);
    expect(
        said.len() == 1 && said[0].contains("`b/c`") && said[0].contains("lower its `comments` to 1"),
        &format!("one site removed is caught: {said:?}"),
    );
    // Planted: `global` off its row, either way: the list is written again.
    for off in [2, 4] {
        let mut stale = rows.clone();
        stale.insert("a".to_owned(), [4, 1, off, 0, 0]);
        let said = problems(&counts, &stale, None, None);
        expect(
            said.len() == 1 && said[0].contains("`a` has 3 lines that name `Global`") && said[0].contains("run command `write`"),
            &format!("`global` off its row by {off} is caught: {said:?}"),
        );
    }
    // Planted: a crate with no row is held to 0; a row with no site goes; a row raised above
    // the base's is caught even where the tree matches it.
    let said = problems(&counts, &BTreeMap::from([("a".to_owned(), [4, 1, 3, 0, 0])]), None, None);
    expect(said.len() == 1 && said[0].contains("`b/c` has 1") && said[0].contains("held"), &format!("a new crate starts at 0: {said:?}"));
    let mut gone = rows.clone();
    gone.insert("old".to_owned(), [5, 0, 2, 0, 0]);
    let said = problems(&counts, &gone, None, None);
    expect(
        !said.is_empty() && said.iter().all(|it| it.contains("`old`")) && said.iter().any(|it| it.contains("remove the row")),
        &format!("a crate gone: {said:?}"),
    );
    let said = problems(&counts, &rows, Some(&fewer), None);
    expect(said.len() == 1 && said[0].contains("the row `a`'s `comments` is 4 and was 3 at the base"), &format!("a row raised: {said:?}"));
    // `global` rises with this crate's `// alloc:` lines converted, never with another crate's.
    let mut converting = rows.clone();
    converting.insert("a".to_owned(), [5, 1, 2, 0, 0]);
    expect(problems(&counts, &rows, Some(&converting), None).is_empty(), "`global` rises by the comments converted");
    let elsewhere = BTreeMap::from([("a".to_owned(), [4, 1, 2, 0, 0]), ("b/c".to_owned(), [9, 0, 0, 0, 0])]);
    let said = problems(&counts, &rows, Some(&elsewhere), None);
    expect(
        said.iter().any(|it| it.contains("the row `a`'s `global` is 3, 2 at the base, over the 2 it may be")),
        &format!("another crate's `// alloc:` lines left allow no rise: {said:?}"),
    );

    // The base's diff: hunks of added lines, a deleted file, a hunk that only removes.
    // The first hunk adds a line whose text starts `++ `: it is no file's header.
    let diff = "diff --git a/crates/a/src/lib.rs b/crates/a/src/lib.rs\n--- a/crates/a/src/lib.rs\n+++ b/crates/a/src/lib.rs\n\
        @@ -2,0 +3 @@ fn f() {\n+++ b/not/a/file\n@@ -9 +10,2 @@\n-old\n+y\n+z\n@@ -20 +21,0 @@\n-w\n\
        diff --git a/crates/a/src/gone.rs b/crates/a/src/gone.rs\n--- a/crates/a/src/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-v\n";
    let lines = added_lines(diff).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    expect(
        lines == BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([3, 10, 11]))]),
        &format!("the lines added: {lines:?}"),
    );
    expect(
        parse_rows("a\t1\t0\t0\t0\t0\nb 2\n").is_err()
            && parse_rows("a\t1\t0\t0\t0\t0\na\t2\t0\t0\t0\t0\n").is_err()
            && parse_rows("a\t1\t0\n").is_err()
            && parse_rows("a\t1\t0\t0\t0\tx\n").is_err()
            && parse_rows("a\t1\t0\t0\t0\t0\t0\n").is_err()
            && parse_rows("\t1\t0\t0\t0\t0\n").is_err(),
        "a bad row is refused",
    );
    expect(parse_rows("# h\na\t1\t0\t2\t3\t4\n") == Ok(BTreeMap::from([("a".to_owned(), [1, 0, 2, 3, 4])])), "a row is read");
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
    // The layout before `test_signatures`, five fields: refused with the same message, and a
    // base whose list has it holds no row.
    let five = parse_rows("# header\na\t1\t0\t2\t3\n");
    expect(
        five.as_ref().is_err_and(|e| e.contains("previous layout") && e.contains("no `test_signatures`") && e.contains("`write`")),
        &format!("a row of the five-field layout says to write the list: {five:?}"),
    );
    expect(
        previous_layout("# h\n\na\t3\n")
            && previous_layout("# h\na\t3\t0\t1\n")
            && previous_layout("# h\na\t3\t0\t1\t2\n")
            && !previous_layout("# h\na\t3\t0\t1\t2\t4\n")
            && !previous_layout("# h\n"),
        "the previous layouts",
    );
    signatures_selftest(p);
    test_signatures_selftest(p);
    global_rule_selftest(p);
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
/// - 28, `helper`, in `#[cfg(test)] mod tests`: a signature in test code, a `test_signatures`
///   site and not a `signatures` one.
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

/// The signature lines [`code_lines`] gives for `text`, in `alloc`.
fn signatures_of<A: Allocator + Clone>(text: &str, alloc: A) -> Vec<usize, A> {
    code_lines(text, alloc).signatures
}

/// The `signatures` column: what a signature is, what counts, its planted controls, and the
/// ratchet's refusal of a rise naming the site.
fn signatures_selftest(p: &Patterns) {
    let lines = signatures_of(SIGNATURES, Global);
    expect(lines == [2, 8, 9, 14, 17, 28], &format!("the signatures that fix the allocator: {lines:?}"));
    let found = scan_file(p, SIGNATURES);
    let sites: Vec<usize> = found.sites.iter().filter(|(_, kind)| *kind == Kind::Signature).map(|(line, _)| *line).collect();
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
    let found = scan_file(p, &planted);
    let sites: Vec<usize> = found.sites.iter().filter(|(_, kind)| *kind == Kind::Signature).map(|(line, _)| *line).collect();
    expect(sites == [2, 8, 9, 14, 17, 28], &format!("`helper` outside test code is a site: {sites:?}"));
    // `String` with no `<..>`, and one fewer argument than the allocator's form, at any depth.
    expect(fixes_allocator("f(x: &[Rc<u8>])") && fixes_allocator("f() -> Option<(u8, Box<dyn Fn(u8) -> u8>)>"), "at depth");
    expect(fixes_allocator("f(s: String)") && !fixes_allocator("f(s: blaze_mem::String<A>)"), "`String`");
    expect(!fixes_allocator("f(v: Vec<u8, A,>)") && fixes_allocator("f(v: Vec<u8,>)"), "a trailing comma adds no argument");
    expect(!fixes_allocator("f(v: MyVec<u8>, s: OsStrings)"), "a longer word is not the type");

    // The ratchet: a rise over the row is refused, with the new site named from the base's diff.
    let path = "crates/s/src/lib.rs";
    let found = scan_file(p, SIGNATURES);
    let sites: Vec<Site> =
        found.sites.iter().map(|(line, kind)| Site { path: path.to_owned(), line: *line, kind: *kind }).collect();
    let counts = BTreeMap::from([("s".to_owned(), sites)]);
    let row = tally(&counts["s"]);
    expect(row == [0, 0, 1, 5, 1], &format!("crate `s`'s row: {row:?}"));
    let rows = BTreeMap::from([("s".to_owned(), row)]);
    expect(problems(&counts, &rows, Some(&rows), None).is_empty(), "the tree as recorded is right");
    let fewer = BTreeMap::from([("s".to_owned(), [0, 0, 1, 4, 1])]);
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

/// The function a lane's diff added (2026-11-15) in a file that `#[cfg(test)] mod one_copy;`
/// declares, which fixes its caller's allocator and names `Global` itself: a `test_signatures`
/// site at line 3. The `#[test]` function's signature has no such type.
const ONE_COPY: &str = "use super::*;

fn job(lowered: &Lowered, units: &[usize]) -> Vec<hir::Crate> {
    let units: Vec<(bool, Option<base_db::Crate>)> =
        units.iter().map(|&at| (at == TESTED, lowered.krate(at))).collect();
    read_order(&units, std::alloc::Global)
}

#[test]
fn reads_in_order() {
    let _ = job(&Lowered::new(), &[0]);
}
";

/// [`ONE_COPY`]'s `job` in the form the rule asks for: no site.
const ONE_COPY_GENERIC: &str = "use super::*;

fn job<A: Allocator + Clone>(lowered: &Lowered, units: &[usize], alloc: A) -> Vec<hir::Crate, A> {
    read_order(lowered, units, alloc)
}
";

/// The `test_signatures` column: a helper's signature in each kind of test code is a site, the
/// generic form is none, and the column only shrinks.
fn test_signatures_selftest(p: &Patterns) {
    let tmp = TempDir::new("allocs-selftest-").unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    put("crates/t/Cargo.toml", "[package]\nname = \"t\"\n");
    // An inline `#[cfg(test)] mod` (line 9), a module file test code declares (`one_copy`), a
    // `#![cfg(test)]` file (`whole`, line 2), a file under `tests` (line 1); `live` has the
    // generic form outside test code, and `generic` it in test code: neither is a site.
    put(
        "crates/t/src/lib.rs",
        "#[cfg(test)]\nmod one_copy;\n#[cfg(test)]\nmod generic;\nmod live;\nmod whole;\n#[cfg(test)]\nmod inline {\n    fn helper() -> Vec<u8> { loop {} }\n}\n",
    );
    put("crates/t/src/one_copy.rs", ONE_COPY);
    put("crates/t/src/generic.rs", ONE_COPY_GENERIC);
    put("crates/t/src/live.rs", ONE_COPY_GENERIC);
    put("crates/t/src/whole.rs", "#![cfg(test)]\nfn made() -> Box<u8> { loop {} }\n");
    // The crate's own tests: a test root (line 4), the module files it declares
    // (`tests/common/mod.rs` and, by `#[path]`, `tests/support/walk.rs`, line 1 each), and a
    // test root in its own directory (`tests/suite/main.rs`, line 1).
    put(
        "crates/t/tests/it.rs",
        "mod common;\n#[path = \"support/walk.rs\"]\nmod walk;\nfn fixture() -> String { loop {} }\n#[test]\nfn it() {}\n",
    );
    put("crates/t/tests/common/mod.rs", "pub fn shared() -> Vec<u8> { loop {} }\n");
    put("crates/t/tests/support/walk.rs", "pub fn walked() -> Vec<u8> { loop {} }\n");
    put("crates/t/tests/suite/main.rs", "fn made() -> Box<u8> { loop {} }\n");
    // Test data, counted in no column: a file under `tests/` no test root reaches, and a fixture
    // crate under `tests/` with its own tests and `#[cfg(test)]` module.
    put("crates/t/tests/data/f.rs", "fn f() -> Vec<u8> { loop {} } // alloc: data\nfn g() { Vec::new_in(Global); }\n");
    put("crates/t/tests/fixture/Cargo.toml", "[package]\nname = \"fixture\"\n");
    put(
        "crates/t/tests/fixture/src/lib.rs",
        "pub fn g() -> Vec<u8> { loop {} } // alloc: a fixture's\n#[cfg(test)]\nmod tests {\n    fn h() -> Vec<u8> { loop {} }\n}\n",
    );
    put("crates/t/tests/fixture/tests/it.rs", "fn helper() -> Vec<u8> { loop {} }\n");
    let counts = count(p, tmp.path());
    let want = BTreeMap::from([(
        "t".to_owned(),
        sites_of(&[
            ("crates/t/src/lib.rs", 9, Kind::TestSignature),
            ("crates/t/src/one_copy.rs", 3, Kind::TestSignature),
            ("crates/t/src/whole.rs", 2, Kind::TestSignature),
            ("crates/t/tests/common/mod.rs", 1, Kind::TestSignature),
            ("crates/t/tests/it.rs", 4, Kind::TestSignature),
            ("crates/t/tests/suite/main.rs", 1, Kind::TestSignature),
            ("crates/t/tests/support/walk.rs", 1, Kind::TestSignature),
        ]),
    )]);
    expect(
        counts == want,
        &format!("a test helper's signature in each kind of test code, and none in test data: {}", shown(&counts)),
    );
    let rows = parse_rows(&render(&counts)).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    expect(rows == BTreeMap::from([("t".to_owned(), [0, 0, 0, 0, 7])]), &format!("crate `t`'s row: {rows:?}"));

    // Over its row: refused, naming the new helper; the message is about test code, and the
    // `#[test]` function is who names `Global`.
    let under = BTreeMap::from([("t".to_owned(), [0, 0, 0, 0, 6])]);
    let added = BTreeMap::from([("crates/t/src/one_copy.rs".to_owned(), BTreeSet::from([3, 4, 5, 6, 7]))]);
    let said = problems(&counts, &under, None, Some(&added));
    expect(
        said.len() == 1
            && said[0].contains("`t` has 7 function signatures that fix the caller's allocator in test code")
            && said[0].contains("over its row's `test_signatures` 6")
            && said[0].contains("new: crates/t/src/one_copy.rs:3)")
            && said[0].contains("the `#[test]` function names `Global`")
            && !said[0].contains("outside tests"),
        &format!("a helper added to a test module is caught and named: {said:?}"),
    );
    // A crate with no row is held to 0; a row above the base's is refused.
    let said = problems(&counts, &BTreeMap::new(), None, None);
    expect(said.len() == 1 && said[0].contains("`t` has 7") && said[0].contains("held"), &format!("no row: {said:?}"));
    let said = problems(&counts, &rows, Some(&under), None);
    expect(
        said.len() == 1 && said[0].contains("the row `t`'s `test_signatures` is 7 and was 6 at the base"),
        &format!("a row raised over the base's: {said:?}"),
    );
}

/// Where a line naming `Global` can take an allocator, line by line:
/// - 2, a plain function's body: can;
/// - 3, a closure in it, the function's: can;
/// - 6, `fn main`'s body: cannot;
/// - 8, a `fn` nested in `fn main`: can;
/// - 12, a method of `impl Clone for S`, its signature the trait's: cannot;
/// - 15, an inherent method: can;
/// - 17, a static, and 18, a field's type, outside every function: cannot;
/// - 19, the generic form, names no `Global`.
const PLACES: &str = "fn plain() {
    let v = Vec::new_in(Global);
    let f = || { Vec::<u8, Global>::new_in(Global) };
}
fn main() {
    let v = Vec::new_in(Global);
    fn nested() -> usize {
        Vec::new_in(Global).len()
    }
}
impl Clone for S {
    fn clone(&self) -> S { S(Vec::new_in(Global)) }
}
impl S {
    fn inherent(&self) -> S { S(Vec::new_in(Global)) }
}
static H: Vec<u8, Global> = Vec::new_in(Global);
struct F { a: Vec<u8, Global> }
fn generic<A: Allocator + Clone>(alloc: A) -> Vec<u8, A> { Vec::new_in(alloc) }
";

/// The lines of [`PLACES`] in a place that can take an allocator, and those in one that cannot.
const PLACES_TAKE: [usize; 4] = [2, 3, 8, 15];
const PLACES_FIXED: [usize; 4] = [6, 12, 17, 18];

/// [`PLACES`] as the sites of crate `g`.
fn places_counts(p: &Patterns) -> BTreeMap<String, Vec<Site>> {
    let path = "crates/g/src/lib.rs";
    let sites = scan_file(p, PLACES).sites.iter().map(|(line, kind)| Site { path: path.to_owned(), line: *line, kind: *kind }).collect();
    BTreeMap::from([("g".to_owned(), sites)])
}

/// The `global` rule: a rise is allowed by the crate's own `// alloc:` lines converted and by
/// the lines added where no allocator can be taken, and refused, naming the line, in a function
/// that can take one.
fn global_rule_selftest(p: &Patterns) {
    let found = scan_file(p, PLACES);
    let kinds: Vec<(usize, Kind)> = found.sites.clone();
    let mut want: Vec<(usize, Kind)> = PLACES_TAKE.iter().map(|it| (*it, Kind::Global)).collect();
    want.extend(PLACES_FIXED.iter().map(|it| (*it, Kind::FixedGlobal)));
    want.sort_by_key(|it| it.0);
    expect(kinds == want, &format!("each `Global`'s place: {kinds:?}"));

    let counts = places_counts(p);
    let now = [0, 0, 8, 0, 0];
    let rows = BTreeMap::from([("g".to_owned(), now)]);
    let path = "crates/g/src/lib.rs";
    for line in PLACES_TAKE.into_iter().chain(PLACES_FIXED) {
        let added = BTreeMap::from([(path.to_owned(), BTreeSet::from([line]))]);
        // One line naming `Global` added, no `// alloc:` line converted.
        let base = BTreeMap::from([("g".to_owned(), [0, 0, 7, 0, 0])]);
        let said = problems(&counts, &rows, Some(&base), Some(&added));
        if PLACES_FIXED.contains(&line) {
            expect(said.is_empty(), &format!("line {line}, where no allocator can be taken, may add one: {said:?}"));
        } else {
            expect(
                said.len() == 1
                    && said[0].contains("the row `g`'s `global` is 8, 7 at the base, over the 7 it may be")
                    && said[0].contains(&format!("new in a function that can take an allocator: {path}:{line})"))
                    && said[0].contains("generic over `A: Allocator + Clone`, take `alloc: A` from its caller")
                    && said[0].contains("let the caller that decides the lifetime name `Global`"),
                &format!("line {line}, in a function that can take an allocator, is refused and named: {said:?}"),
            );
        }
        // The same line with one of the crate's `// alloc:` lines gone: converted, allowed.
        let converted = BTreeMap::from([("g".to_owned(), [1, 0, 7, 0, 0])]);
        let said = problems(&counts, &rows, Some(&converted), Some(&added));
        expect(said.is_empty(), &format!("line {line} with a `// alloc:` line converted: {said:?}"));
    }
    // A crate with no row at the base counts 0: its four lines in places that cannot take an
    // allocator allow four, and the other four are refused, named.
    let every = BTreeMap::from([(path.to_owned(), BTreeSet::from_iter(PLACES_TAKE.into_iter().chain(PLACES_FIXED)))]);
    let said = problems(&counts, &rows, Some(&BTreeMap::new()), Some(&every));
    expect(
        said.len() == 1
            && said[0].contains("is 8, 0 at the base, over the 4 it may be")
            && said[0].contains(&format!("{path}:2, {path}:3, {path}:8, {path}:15)")),
        &format!("a new crate's `global`: {said:?}"),
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
        let signed = |text: &str| -> usize { scan_file(&p, text).sites.iter().filter(|(_, kind)| *kind == Kind::Signature).count() };
        assert_eq!(signed(in_test), 0);
        assert_eq!(signed(&in_test.replace("#[cfg(test)]\n", "\n")), 1);
    }

    /// A rise in `signatures` over the crate's row is refused, naming the site the base's diff
    /// added; the tree at its row is not.
    #[test]
    fn a_rise_in_signatures_is_refused_with_its_site() {
        let sites = [
            Site { path: "crates/s/src/lib.rs".to_owned(), line: 3, kind: Kind::Signature },
            Site { path: "crates/s/src/lib.rs".to_owned(), line: 9, kind: Kind::Signature },
        ];
        let counts = BTreeMap::from([("s".to_owned(), sites.to_vec())]);
        let at_row = BTreeMap::from([("s".to_owned(), [0, 0, 0, 2, 0])]);
        assert!(problems(&counts, &at_row, Some(&at_row), None).is_empty());
        let under = BTreeMap::from([("s".to_owned(), [0, 0, 0, 1, 0])]);
        let added = BTreeMap::from([("crates/s/src/lib.rs".to_owned(), BTreeSet::from([9]))]);
        let said = problems(&counts, &under, None, Some(&added));
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("over its row's `signatures` 1") && said[0].contains("new: crates/s/src/lib.rs:9)"), "{said:?}");
        // The layouts before this column and before `test_signatures` are refused, saying to
        // write the list.
        assert!(parse_rows("s\t0\t0\t0\n").is_err_and(|e| e.contains("previous layout") && e.contains("`write`")));
        assert!(parse_rows("s\t0\t0\t0\t2\n").is_err_and(|e| e.contains("previous layout") && e.contains("`write`")));
    }

    /// The lane's `job` in a test module fixes its caller's allocator: a `test_signatures` site.
    /// The generic form is none, and neither counts in `signatures`.
    #[test]
    fn a_test_helper_fixing_the_allocator_is_a_test_signature() {
        let p = Patterns::new();
        let in_module = |text: &str| -> Vec<(usize, Kind)> { scan_file(&p, &format!("#[cfg(test)]\nmod one_copy {{\n{text}}}\n")).sites };
        assert_eq!(in_module(ONE_COPY), [(5, Kind::TestSignature)]);
        assert!(in_module(ONE_COPY_GENERIC).is_empty());
        // The same file declared by `#[cfg(test)] mod one_copy;`, and its generic form.
        test_signatures_selftest(&p);
    }

    /// `global` rises by a line in `fn main`, a trait impl's method or outside every function,
    /// or by a `// alloc:` line of the crate converted; not by a line in a plain function.
    #[test]
    fn global_rises_only_by_conversions_and_places_without_an_allocator() {
        let p = Patterns::new();
        let counts = places_counts(&p);
        let rows = BTreeMap::from([("g".to_owned(), [0, 0, 8, 0, 0])]);
        let base = BTreeMap::from([("g".to_owned(), [0, 0, 7, 0, 0])]);
        let added = |line: usize| BTreeMap::from([("crates/g/src/lib.rs".to_owned(), BTreeSet::from([line]))]);
        let said = problems(&counts, &rows, Some(&base), Some(&added(2)));
        assert!(said.len() == 1 && said[0].contains("crates/g/src/lib.rs:2)"), "{said:?}");
        assert!(problems(&counts, &rows, Some(&base), Some(&added(6))).is_empty());
        let converted = BTreeMap::from([("g".to_owned(), [1, 0, 7, 0, 0])]);
        assert!(problems(&counts, &rows, Some(&converted), Some(&added(2))).is_empty());
        global_rule_selftest(&p);
    }
}
