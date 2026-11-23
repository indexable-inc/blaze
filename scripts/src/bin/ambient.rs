//! ambient: what each crate takes from Rust's `std` and `alloc`, and from their collections, only
//! goes down (docs/v2/plan/design/bottom.md: blaze's own code uses no `std`, no `alloc` and no
//! third-party crate; AGENTS.md nothing-ambient, rule 75). `DOC` below is what its usage error
//! prints; it says what each count is, what the text scan misses, which code is a test's, and
//! the rule.
//!
//!     blaze run -q -p scripts --bin ambient -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin ambient -- selftest
//!
//! Why (2026-11-15): the new item layer's crates are `#![no_std]` on blaze-own's collections,
//! and nothing held the rest of the tree from taking more of `std`: a crate's root lost its
//! `#![no_std]`, or a file reached for `std::collections::HashMap`, and only a compile of that
//! crate under the new rules would say so. This holds the count from the text, with no
//! compiler, as `untyped` and `allocs` hold theirs: a gate step of the `scripts` job
//! (.github/workflows/blaze.yaml), beside them. thirdparty.rs holds the third-party crates.
//!
//! Test code, the text scanner primitives (`item_end`, `comment_end`, `string_end`, `quote_end`,
//! `word_end`, `ident_len`, `is_ident`), path/crate helpers (`test_path`, `module_files`,
//! `crate_of`, `Members`), `Hit`, `BaseError` and `added_lines` are `blaze_script::ratchet`'s
//! (AGENTS.md one-concept-one-home). A manifest is read by the small TOML reader here.

/// What the usage error prints.
const DOC: &str = r#"ambient: what each crate takes from `std` and `alloc`, and from their collections, only goes down (docs/v2/plan/design/bottom.md; AGENTS.md nothing-ambient, rule 75).

    blaze run -q -p scripts --bin ambient -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin ambient -- '{"command": "write"}'
    blaze run -q -p scripts --bin ambient -- selftest

    command   check   each count of every crate equals its row's in the list: one over its row
                      is refused (its new sites named given `base`), one under it must be
                      lowered with `write`; a crate with no row is held to 0 in every column;
                      with `base`, no count of a row is above the base's (CI)
              write   write the list from the tree as it is
              findings  print every site as a `blaze_script::finding` document (for `advise`)
              tests-check, tests-write, tests-report  the tests' count (D72), at the end
    root     the checkout (".")
    base      the commit the change is against, or null: its list (`git show`) holds every
              row down, and the lines added since it (`git diff --unified=0`) name a crate's
              new sites

The list's row is `<crate><TAB><std><TAB><is_std><TAB><alloc><TAB><vec_box>`, of `.rs` files
under crates/ (a directory named `target` is not read; scripts/ and tools/ are not either). A
line is read as code: comments, strings (raw and byte strings too) and characters are skipped.
Each column counts a line once, however many sites it holds:
  std      a line that names `std` as a path's first segment (`std::fs`, `::std::io`), or after
           `use` or `extern crate` (`use std;`, `extern crate std;`)
  is_std   1 when the crate's root does not say `#![no_std]` (or `#![cfg_attr(not(test),
           no_std)]`) on a line of its own, else 0. The root is the manifest's `[lib] path`,
           else `src/lib.rs`, else the first `[[bin]] path`, else `src/main.rs`; a manifest
           with no `[package]` (a workspace's) is no crate, nor is a package under a directory
           named `tests` or `benches` (a test's fixture)
  alloc    a line that names `alloc` as a path's first segment (`alloc::vec::Vec`), or after
           `use` or `extern crate`; `core::alloc::Layout` and `std::alloc::Global` are not it
           (the second is `std`'s). A use of `alloc` that names no path is not seen: in a crate
           on `std`, the prelude's `Vec`, `Box` and `String` are `alloc`'s, and so is what
           `vec!`, `format!` and `.to_owned()` make; the first three count in `vec_box`, the
           rest nowhere
  vec_box  a line that names `Vec`, `Box`, `Rc`, `Arc`, `BTreeMap`, `BTreeSet`, `VecDeque`,
           `BinaryHeap`, `HashMap` or `HashSet` followed by `<` or `::` (`Vec<u8>`,
           `Vec::new()`), or the word `String`, where it is `std`'s or `alloc`'s: written bare,
           or under a path whose first segment is `std` or `alloc`. Not counted:
             - under any other path (`blaze_own::Vec<u8, A>`, `Kind::String`, `Self::Box`);
             - in a `use` item (an import is no use) or as the name an item defines
               (`struct Vec<T, A>`);
             - bare, in a file whose `use` items import the name from `blaze_own`, `own`,
               `blaze_mem` or `blaze_hash` (a glob of one of them imports every name), or from
               `crate`, `self` or `super` (not by a glob), or that defines it (`struct`,
               `enum`, `union`, `trait` or `type` of that name)
Test code is not counted (below).

The count is read from the text, with no names resolved, so it misses and it over-counts:
  missed  a rename (`use std::vec::Vec as V;` then `V<u8>`; `use std as s;` then `s::fs`); a
          std collection re-exported by a crate of the tree and imported from it by `crate::`
          (`pub use std::vec::Vec;` counts once, in `std`, where it is written); a local alias
          of one (`type Vec<T> = std::vec::Vec<T>;` defines the name); `vec!`, `format!`,
          `.to_string()`, `.collect()` with no type written; any other std collection
          (`LinkedList`, `Cow`, `Mutex`), and every std item named through a `use` alone
          (`use std::fs::File;` counts in `std` once, its uses nowhere); a crate root that
          a `#[path]` or a manifest's inline table (`lib = { path = .. }`) names
  over    an enum variant or a type of the crate's own named `String` (or one of the others)
          and written bare, outside a file that defines it (`use Kind::*;` then `String`); a
          module of the crate's own named `std` or `alloc` named relative to where it is
          (`mod alloc;` then `alloc::Frame`); a name a `use super::*` brings from the parent
          module's own types
  either  a `macro_rules!` body counts once, at the template, where the expansions count
          nowhere
blaze's lints are the definition (`blaze::implicit_allocator`, and a crate's root check once
blaze checks its own crates); this list is the count the gate can hold before then.

A site in test code is not counted here (the tests' count, at the end, counts it). Test code
is, and nothing else is:
  - a file under a directory named `tests` or `benches`, or named `tests.rs` or `test.rs`;
  - a file that says `#![cfg(test)]`;
  - the item after `#[cfg(test)]` or `#[cfg(all(test, ...))]`, or after `#[test]` or another
    attribute whose path ends in the word `test` (`#[tokio::test]`, not `#[test_case]`), from
    the attribute's line to the line of the `}` or `;` that ends the item (strings,
    characters and comments read as Rust reads them); an attribute whose item does not end
    before its block does (a match arm, a field) covers its own line only;
  - the file of a module declared out of line in test code (`#[cfg(test)] mod t;`, or any
    `mod m;` in a test file): `m.rs` or `m/mod.rs` beside a crate root (`lib.rs`, `main.rs`,
    `mod.rs`, `build.rs`, a file of a `bin` directory), else in the directory named for the
    declaring file; a `#[path = ".."]` on the declaration (its line or an attribute line above)
    names the file instead, relative to the declaring file's directory.
Everything else counts: build scripts, binaries and `*_tests.rs` files a test module does not
declare included. `is_std` is the root's, test code or not.

A crate is the nearest directory above the file, under crates/, that holds a Cargo.toml, named
by its path under crates/ (`blaze-own`, `harness/harness-native`).
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use blaze_script::{
    TempDir, fail,
    finding::{self, Finding},
    git,
    input::{self, Fields, Input, InputError},
    ratchet::{
        BaseError, Hit, Members, Tests, added_lines, comment_end, crate_of, ident_len, is_ident, item_end, module_files, quote_end,
        string_end, test_path, word_end,
    },
    read_text, universal_newlines,
};

/// The tests' count (decision D72): the code this count skips, read by the same walk.
#[path = "ambient/in_tests.rs"]
mod in_tests;
use in_tests::Effect;

/// The heap's ratchet (decision D102): calls that reach the global heap from nowhere, read by
/// the same walk.
#[path = "ambient/heap.rs"]
mod heap;
use heap::HeapUse;

/// The reach ratchet (rule 75, D104): calls that reach the clock, the environment, randomness,
/// exit or a whole world from nowhere, read by the same walk.
#[path = "ambient/reach.rs"]
mod reach;
use reach::ReachUse;

/// The producer-string ratchet (D105): typed-in producer versions in hash domains, read by the
/// same walk.
#[path = "ambient/producers.rs"]
mod producers;
use producers::ProducerUse;

/// The sans-IO gate (D170, C7): per crate, `std` and `alloc` paths, `Global`, `Alloc::global()`,
/// implicit allocations, statics with state, ambient reads, direct IO, the root's `#![no_std]`
/// and its `#![blaze::sans_io]` or `#![blaze::driver]`, and blocking calls in sans-IO code, read
/// from the tree's tokens (blaze-lex) over the same walk.
#[path = "ambient/sansio.rs"]
mod sansio;

const SCRIPT: &str = "ambient";
/// The list, from the root.
const LIST: &str = "tools/ratchets/std-alloc.txt";
const TEST_DIRS: [&str; 2] = ["tests", "benches"];
/// The crates whose collections are blaze's own: a name imported from one is not `std`'s.
const OWN: [&str; 4] = ["blaze_own", "own", "blaze_mem", "blaze_hash"];
/// The first segments of a path inside the crate: a name imported by one is the crate's.
const LOCAL: [&str; 3] = ["crate", "self", "super"];
/// The first segments under which a collection is `std`'s or `alloc`'s.
const STD_HEADS: [&str; 2] = ["std", "alloc"];
/// `std`'s and `alloc`'s collections and owning pointers, as `vec_box` reads them.
const COLLECTIONS: [&str; 11] =
    ["Vec", "Box", "String", "Rc", "Arc", "BTreeMap", "BTreeSet", "VecDeque", "BinaryHeap", "HashMap", "HashSet"];
/// The one of [`COLLECTIONS`] that counts written alone, with no `<` or `::` after it.
const TEXT: &str = "String";
/// The words that define the name after them.
const DEFINING: [&str; 5] = ["struct", "enum", "union", "trait", "type"];
/// The two forms of a crate root's attribute that make it `#![no_std]`.
const NO_STD: [&str; 2] = ["#![no_std]", "#![cfg_attr(not(test), no_std)]"];
/// What every message about a crate over its row ends with.
const RULE: &str = "docs/v2/plan/design/bottom.md: blaze's own code uses no `std`, no `alloc` and no third-party crate; \
                    AGENTS.md nothing-ambient (rule 75): the allocator is passed in and named";
/// What to write instead, for each column's message.
const INSTEAD_STD: &str = "take the item from `core` (`core::fmt`, `core::mem`), or from blaze's own crate for it \
                           (blaze-own's collections, blaze-hash's maps, blaze-sys for the system), or keep it in a \
                           crate that is still on `std` and pass its result in";
const INSTEAD_IS_STD: &str = "put `#![no_std]` at the crate's root, with `extern crate std;` under `#[cfg(test)]` if its \
                              tests need it: a new crate starts on `core` alone";
const INSTEAD_ALLOC: &str = "use blaze-own's collections over `core::alloc::Allocator` (`blaze_own::Vec<T, A>`, \
                             `Vec::with_capacity_in(n, alloc)`), the allocator passed in by the caller";
const INSTEAD_VEC_BOX: &str = "use blaze-own's (`blaze_own::Vec<T, A>`, `Box<T, A>`, `String<A>`, `Rc`, `Arc`, \
                               `SortedMap`, `VecDeque`) or blaze-hash's maps, each in the allocator the caller \
                               passes: `use blaze_own::{Vec, Box};` makes the bare names blaze-own's in the file";
/// The list's first lines.
const HEADER: &str = "# What each crate takes from Rust's `std` and `alloc`, outside test code, by crate (its\n\
                      # directory under crates/): lines naming `std` as a path's first segment; 1 when the crate's\n\
                      # root is not `#![no_std]`; lines naming `alloc` so; lines naming a `std` or `alloc`\n\
                      # collection or owning pointer (`Vec`, `Box`, `String`, `Rc`, `Arc`, `BTreeMap`, `BTreeSet`,\n\
                      # `VecDeque`, `BinaryHeap`, `HashMap`, `HashSet`) not imported from blaze's own crates. Only\n\
                      # shrinks; a crate with no row is held to 0 in every column (docs/v2/plan/design/bottom.md).\n\
                      # Written by scripts/src/bin/ambient.rs (`{\"command\": \"write\"}`), checked in CI.\n\
                      # crate\tstd\tis_std\talloc\tvec_box\n";

struct Args {
    command: String,
    root: String,
    base: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "base"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of(
                "command",
                &[
                    "check",
                    "write",
                    "findings",
                    "tests-check",
                    "tests-write",
                    "tests-report",
                    "heap-check",
                    "heap-write",
                    "heap-report",
                    "heap-sites",
                    "reach-check",
                    "reach-write",
                    "reach-report",
                    "reach-sites",
                    "producers-check",
                    "producers-write",
                    "producers-sites",
                    "sansio-check",
                    "sansio-write",
                    "sansio-report",
                    "sansio-sites",
                ],
                None,
            )?,
            root: f.str("root", Some("."))?,
            base: f.str_or_null("base")?,
        })
    }
}

/// What a site is: the list's columns, in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// A line naming `std` as a path's first segment.
    Std,
    /// A crate root with no `#![no_std]`, at its line 1.
    IsStd,
    /// A line naming `alloc` as a path's first segment.
    Alloc,
    /// A line naming a collection of `std` or `alloc`.
    VecBox,
}

const KINDS: [Kind; 4] = [Kind::Std, Kind::IsStd, Kind::Alloc, Kind::VecBox];

impl Kind {
    /// Its column in a row, from 0.
    const fn column(self) -> usize {
        match self {
            Kind::Std => 0,
            Kind::IsStd => 1,
            Kind::Alloc => 2,
            Kind::VecBox => 3,
        }
    }

    /// Its column's name in the header and in messages.
    const fn name(self) -> &'static str {
        match self {
            Kind::Std => "std",
            Kind::IsStd => "is_std",
            Kind::Alloc => "alloc",
            Kind::VecBox => "vec_box",
        }
    }

    /// What its sites are, for a message: `<n> <what>`.
    const fn what(self) -> &'static str {
        match self {
            Kind::Std => "lines naming `std`",
            Kind::IsStd => "crate root (or none) without `#![no_std]`",
            Kind::Alloc => "lines naming `alloc`",
            Kind::VecBox => "lines naming a collection of `std` or `alloc`",
        }
    }

    /// What to write instead, for a message.
    const fn instead(self) -> &'static str {
        match self {
            Kind::Std => INSTEAD_STD,
            Kind::IsStd => INSTEAD_IS_STD,
            Kind::Alloc => INSTEAD_ALLOC,
            Kind::VecBox => INSTEAD_VEC_BOX,
        }
    }

    /// The kind as a finding (`blaze_script::finding`), for `advise`, which refines a line's by
    /// tools/std-map.json's class of what it names.
    const fn finding(self) -> finding::Kind {
        finding::Kind { name: self.name(), label: self.what(), rule: "75 nothing-ambient", instead: self.instead(), mechanical: false }
    }
}

/// A crate's counts, by [`Kind::column`].
type Row = [usize; 4];

type Site = Hit<Kind>;

/// The counts of `sites`, by kind.
fn tally(sites: &[Site]) -> Row {
    let mut row = [0; 4];
    for it in sites {
        row[it.what.column()] += 1;
    }
    row
}

/// The list read wrong: where, and what is wrong there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListError {
    /// A line (from 1) that is not a row, a comment or blank.
    NotARow { line: usize, text: String },
    /// A crate's second row, at its line (from 1).
    TwoRows { line: usize, krate: String },
    /// A row whose `is_std` is neither 0 nor 1, at its line (from 1).
    NotABit { line: usize, krate: String, found: usize },
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListError::NotARow { line, text } => {
                write!(f, "{LIST}:{line}: {text:?} is not `<crate><TAB><std><TAB><is_std><TAB><alloc><TAB><vec_box>`")
            }
            ListError::TwoRows { line, krate } => write!(f, "{LIST}:{line}: `{krate}` has two rows"),
            ListError::NotABit { line, krate, found } => {
                write!(f, "{LIST}:{line}: `{krate}`'s `is_std` is {found}, where a crate's root is one: 0 or 1")
            }
        }
    }
}

/// The base could not be read: a git/diff error, or the base's list did not read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AtBaseError {
    Base(BaseError),
    List { base: String, error: ListError },
}

impl fmt::Display for AtBaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AtBaseError::Base(error) => write!(f, "{error}"),
            AtBaseError::List { base, error } => write!(f, "at the base {base}: {error}"),
        }
    }
}

/// A crate's manifest or root could not be read: which file, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeError {
    /// A file under crates/ that could not be read, from the root, and the system's message.
    Unreadable { path: String, why: String },
    /// A package's manifest, from the root, that names no root this script finds: no `[lib]
    /// path`, `src/lib.rs`, `[[bin]] path` or `src/main.rs`.
    NoRoot { manifest: String },
    /// A `path` key of a manifest's `[lib]` or `[[bin]]` that is not one basic string, at its
    /// line (from 1).
    NotAPath { manifest: String, line: usize },
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TreeError::Unreadable { path, why } => write!(f, "{path} could not be read: {why}"),
            TreeError::NoRoot { manifest } => write!(
                f,
                "{manifest} is a package with no root this script finds (a `[lib] path`, src/lib.rs, a `[[bin]] path` or \
                 src/main.rs), so whether it is `#![no_std]` is not known: give it one, or teach scripts/src/bin/ambient.rs \
                 its form"
            ),
            TreeError::NotAPath { manifest, line } => {
                write!(f, "{manifest}:{line}: a target's `path` that is not one string in double quotes")
            }
        }
    }
}

impl std::error::Error for ListError {}
impl std::error::Error for AtBaseError {}
impl std::error::Error for TreeError {}

/// The byte of `line` after the attribute it starts with when that attribute is
/// `#[cfg(any(test, feature = "testing"))]` (either order, any spacing): a module only tests
/// and other crates' tests reach (blaze-solve `testing/`, blaze-consteval `fixture.rs`). It is
/// a test's root for the HEAP columns only (D102, the coordinator's answer of 2026-11-16): it
/// takes `blaze_own::testing::heap()`. The `std` and effect counts still read it as code.
fn cfg_testing_end(line: &str, inner: bool) -> Option<usize> {
    let trimmed = line.trim_start();
    let at = line.len() - trimmed.len();
    let open = if inner { "#![cfg(" } else { "#[cfg(" };
    let rest = trimmed.strip_prefix(open)?;
    let close = rest.find(']')?;
    let cond: String = rest[..close].chars().filter(|c| !c.is_whitespace()).collect();
    let testing = matches!(cond.as_str(), "any(test,feature=\"testing\"))" | "any(feature=\"testing\",test))");
    testing.then(|| at + open.len() + close + 1)
}

/// The name of the module `line` declares out of line (`mod m;`, after attributes and `pub` or
/// `pub(..)`): untyped.rs's `module`,
/// `\A\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?(\w+)\s*;`.
fn module_decl(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    // Bounded: each turn takes one attribute off the front.
    while let Some(attr) = rest.strip_prefix("#[") {
        rest = attr[attr.find(']')? + 1..].trim_start();
    }
    if let Some(after) = rest.strip_prefix("pub") {
        let after = match after.strip_prefix('(') {
            Some(scope) => &scope[scope.find(')')? + 1..],
            None => after,
        };
        if !after.starts_with(char::is_whitespace) {
            return None;
        }
        rest = after.trim_start();
    }
    let after = rest.strip_prefix("mod")?;
    if !after.starts_with(char::is_whitespace) {
        return None;
    }
    let after = after.trim_start();
    let after = after.strip_prefix("r#").unwrap_or(after);
    let len = ident_len(after.as_bytes());
    (len > 0 && after[len..].trim_start().starts_with(';')).then(|| &after[..len])
}

/// Whether the crate root `text` says `#![no_std]` (or `#![cfg_attr(not(test), no_std)]`) at
/// the start of a line: nostd.rs's pattern, `(?m)^#!\[(?:no_std|cfg_attr\(not\(test\), no_std\))\]`.
fn declares_no_std(text: &str) -> bool {
    text.split('\n').any(|line| NO_STD.iter().any(|it| line.starts_with(it)))
}

/// What one file holds.
#[derive(Debug, Default, PartialEq, Eq)]
struct Scanned {
    /// The sites outside its test code (line from 1, what it is), in line order and then by
    /// column.
    sites: Vec<(usize, Kind)>,
    /// Each `mod <name>;` it declares.
    mods: Vec<ModDecl>,
    /// `#![cfg(test)]`: the whole file is test code.
    test: bool,
    /// The sites on its test code's lines (every line when `test`): the other side of the same
    /// line, for the tests' count (in_tests.rs).
    test_sites: Vec<(usize, Kind)>,
    /// The ambient effects ([`Effect`]) outside its test code, and on it.
    effects: Vec<(usize, Effect)>,
    test_effects: Vec<(usize, Effect)>,
    /// The calls that reach the heap with no caller handing it ([`HeapUse`], heap.rs),
    /// outside its test code, and on it.
    heap: Vec<(usize, HeapUse)>,
    test_heap: Vec<(usize, HeapUse)>,
    /// `#![cfg(any(test, feature = "testing"))]`: the whole file is a test's root for the heap
    /// columns ([`cfg_testing_end`]); its heap uses are in `test_heap`.
    testing: bool,
    /// The calls that reach the clock, the environment, randomness, exit or a whole world with
    /// no caller handing it ([`ReachUse`], reach.rs), outside its test code, and on it.
    reach: Vec<(usize, ReachUse)>,
    test_reach: Vec<(usize, ReachUse)>,
    /// The typed-in producer strings in its key code ([`ProducerUse`], producers.rs), outside
    /// its test code, and on it.
    producers: Vec<(usize, ProducerUse)>,
    test_producers: Vec<(usize, ProducerUse)>,
    /// Its text says `#![no_std]` (read for a test target's root).
    no_std: bool,
}

/// One `mod <name>;` a file declares out of line.
#[derive(Debug, PartialEq, Eq)]
struct ModDecl {
    name: String,
    /// Its `#[path = ".."]`, on its line or on an attribute line above it: the file, relative
    /// to the declaring file's directory.
    path: Option<String>,
    /// Test code declares it.
    in_test: bool,
    /// Test code or a `cfg(any(test, feature = "testing"))` item declares it: a test's root for
    /// the heap columns ([`cfg_testing_end`]).
    in_testing: bool,
}

/// The file a `#[path = "<file>"]` on `line` names, if any.
fn path_attr(line: &str) -> Option<&str> {
    let at = line.find("#[path")?;
    let rest = line[at + "#[path".len()..].trim_start().strip_prefix('=')?.trim_start().strip_prefix('"')?;
    Some(&rest[..rest.find('"')?])
}

/// The sites of one file's text and the modules it declares out of line.
fn scan_file(text: &str) -> Scanned {
    scan_file_with(&Tests::new(SCRIPT), text)
}

fn scan_file_with(tests: &Tests, text: &str) -> Scanned {
    let file_tests = tests.of(text);
    let lines: Vec<&str> = text.split('\n').collect();
    let test = file_tests.test;
    // The lines under `cfg(any(test, feature = "testing"))`: test code for the heap columns.
    let mut testing = vec![false; lines.len()];
    let mut out = Scanned { test: file_tests.whole, ..Scanned::default() };
    // A `#[path]` on an attribute line, waiting for the `mod` it is on.
    let mut path: Option<&str> = None;
    // The byte where line `at` starts.
    let mut start = 0;
    for (at, line) in lines.iter().enumerate() {
        if cfg_testing_end(line, true).is_some() {
            out.testing = true;
        }
        if let Some(end) = cfg_testing_end(line, false) {
            let last = match item_end(text, start + end) {
                Some(end) => at + text[start..end - 1].matches('\n').count(),
                None => at,
            };
            for it in &mut testing[at..=last] {
                *it = true;
            }
        }
        let named = path_attr(line).or(path);
        if let Some(name) = module_decl(line) {
            let in_test = test[at];
            out.mods.push(ModDecl { name: name.to_owned(), path: named.map(str::to_owned), in_test, in_testing: in_test || testing[at] });
            path = None;
        } else if line.trim_start().starts_with("#[") {
            path = named;
        } else if !line.trim().is_empty() {
            path = None;
        }
        start += line.len() + 1;
    }
    let whole = out.test;
    if whole {
        for it in &mut out.mods {
            it.in_test = true;
        }
    }
    let whole_testing = whole || out.testing;
    if whole_testing {
        for it in &mut out.mods {
            it.in_testing = true;
        }
    }
    // One scan; each site is on test code's side of the line or on the other, by `test`.
    let (sites, effects) = all_sites(text);
    (out.test_sites, out.sites) = sites.into_iter().partition(|(line, _)| whole || test[line - 1]);
    (out.test_effects, out.effects) = effects.into_iter().partition(|(line, _)| whole || test[line - 1]);
    (out.test_heap, out.heap) =
        heap::heap_uses(text).into_iter().partition(|(line, _)| whole_testing || test[line - 1] || testing[line - 1]);
    (out.test_reach, out.reach) = reach::reach_uses(text).into_iter().partition(|(line, _)| whole || test[line - 1]);
    (out.test_producers, out.producers) =
        producers::producer_uses(text).into_iter().partition(|(line, _)| whole || test[line - 1]);
    out.no_std = declares_no_std(text);
    out
}

/// The names a file makes its own, so that a bare use of one is not `std`'s: each imported from
/// one of [`OWN`] or [`LOCAL`], and each it defines; or all of them, after a glob of [`OWN`].
#[derive(Debug, Default, PartialEq, Eq)]
struct Own {
    names: BTreeSet<String>,
    all: bool,
}

impl Own {
    fn holds(&self, name: &str) -> bool {
        self.all || self.names.contains(name)
    }

    /// Takes in the names of the `use` item whose tree (after `use`, to its `;`) is `tree`.
    fn take_use(&mut self, tree: &str) {
        let mut leaves = Vec::new();
        use_leaves(&without_comments(tree), None, "", &mut leaves);
        for (head, leaf) in leaves {
            let own = OWN.contains(&head.as_str());
            match leaf.as_str() {
                "*" if own => self.all = true,
                "*" => {}
                _ if own || LOCAL.contains(&head.as_str()) => {
                    self.names.insert(leaf);
                }
                _ => {}
            }
        }
    }
}

/// `text` with its comments taken out (a `use` tree may hold some), strings and characters
/// skipped as [`item_end`] skips them.
fn without_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        let next = bytes.get(at + 1).copied();
        let end = match bytes[at] {
            b'/' if next == Some(b'/') => {
                at = text[at..].find('\n').map_or(bytes.len(), |n| at + n);
                continue;
            }
            b'/' if next == Some(b'*') => {
                at = comment_end(bytes, at);
                out.push(' ');
                continue;
            }
            b'"' => string_end(bytes, at + 1),
            b'\'' => quote_end(text, at),
            _ => at + text[at..].chars().next().map_or(1, char::len_utf8),
        };
        out.push_str(&text[at..end]);
        at = end;
    }
    out
}

/// Pushes onto `out` each name the `use` tree `tree` imports, with the first segment of its
/// path: `head` when an enclosing `{..}` gave one, `parent` the last segment before that `{`
/// (what `self` in it names). A glob is the name `*`; a rename, its new name.
fn use_leaves(tree: &str, head: Option<&str>, parent: &str, out: &mut Vec<(String, String)>) {
    let tree = tree.trim();
    let tree = tree.strip_prefix("::").unwrap_or(tree).trim_start();
    if let Some(open) = tree.find('{') {
        let path = tree[..open].trim().trim_end_matches(':').trim();
        let segments: Vec<&str> = path.split("::").map(str::trim).filter(|it| !it.is_empty()).collect();
        let first = head.or(segments.first().copied());
        let last = segments.last().copied().unwrap_or(parent);
        let Some(close) = tree.rfind('}') else { return };
        for item in top_level(&tree[open + 1..close.max(open + 1)]) {
            use_leaves(item, first, last, out);
        }
        return;
    }
    if tree.is_empty() {
        return;
    }
    let (path, alias) = match tree.split_once(" as ") {
        Some((path, alias)) => (path.trim(), Some(alias.trim())),
        None => (tree, None),
    };
    let segments: Vec<&str> = path.split("::").map(str::trim).filter(|it| !it.is_empty()).collect();
    let Some(&last) = segments.last() else { return };
    let first = head.unwrap_or(segments[0]);
    let leaf = match (alias, last) {
        (Some(alias), _) => alias,
        (None, "self") if segments.len() == 1 => parent,
        (None, "self") => segments[segments.len() - 2],
        (None, last) => last,
    };
    out.push((first.to_owned(), leaf.to_owned()));
}

/// `text` split at its commas outside `{..}`, `(..)`, `[..]` and `<..>`.
fn top_level(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth: usize = 0;
    let mut from = 0;
    for (at, byte) in text.bytes().enumerate() {
        match byte {
            b'{' | b'(' | b'[' | b'<' => depth += 1,
            b'}' | b')' | b']' | b'>' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                out.push(&text[from..at]);
                from = at + 1;
            }
            _ => {}
        }
    }
    out.push(&text[from..]);
    out
}

/// The word that ends `text` after its trailing spaces, empty when it ends in no word.
fn trailing_word(text: &str) -> &str {
    let text = text.trim_end();
    let len = text.bytes().rev().take_while(|b| is_ident(*b)).count();
    &text[text.len() - len..]
}

/// The first segment of the path whose last segment is the word at byte `at` of `text`, or
/// `None` when that word is the path's first segment (or a path's after a leading `::`, or
/// after a `>::` of a qualified path, which is read as no path). A word after `.` is a field
/// or method, never a path's: `Some("")`.
fn path_head(text: &str, at: usize) -> Option<&str> {
    let mut head = None;
    let mut to = at;
    // Bounded: each turn moves `to` back by one segment, to the text's start at most.
    loop {
        let before = text[..to].trim_end();
        if head.is_none() && before.ends_with('.') && !before.ends_with("..") {
            return Some("");
        }
        let Some(rest) = before.strip_suffix("::") else { return head };
        let word = trailing_word(rest);
        if word.is_empty() {
            return head;
        }
        head = Some(word);
        to = rest.trim_end().len() - word.len();
    }
}

/// The sites of `text`, test code included ([`scan_file`] takes it out): each line (from 1) in
/// each column it holds, by line and then column. Comments, strings and characters are skipped
/// as [`item_end`] skips them.
fn code_sites(text: &str) -> Vec<(usize, Kind)> {
    all_sites(text).0
}

/// [`code_sites`], and in the same pass each line in each ambient effect it names
/// ([`in_tests::effect_of`]; a `std` `HashMap` or `HashSet` where `vec_box` reads it as
/// `std`'s), by line and then effect: the tests' count reads both (in_tests.rs).
fn all_sites(text: &str) -> (Vec<(usize, Kind)>, Vec<(usize, Effect)>) {
    let bytes = text.as_bytes();
    let mut sites: BTreeSet<(usize, Kind)> = BTreeSet::new();
    let mut effects: BTreeSet<(usize, Effect)> = BTreeSet::new();
    // The bare collection names, each with its line: whether they are `std`'s waits for the
    // file's imports, which may come after them.
    let mut bare: Vec<(usize, &str)> = Vec::new();
    let mut own = Own::default();
    // From the word `use` to its `;`: an import, whose names are no use.
    let mut use_from: Option<usize> = None;
    // The word before was `struct`, `enum`, `union`, `trait` or `type`: this one is defined.
    let mut defining = false;
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
            b';' => {
                if let Some(from) = use_from.take() {
                    own.take_use(&text[from..at]);
                }
                at + 1
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let end = word_end(text, at);
                let word = &text[at..end];
                let after = text[end..].trim_start();
                // The line of this word: `counted` only moves forward, so the text is read once.
                line += text[counted..at].matches('\n').count();
                counted = at;
                if let Some(effect) = in_tests::effect_of(text, at, end) {
                    effects.insert((line, effect));
                }
                let was_defining = std::mem::replace(&mut defining, DEFINING.contains(&word));
                match word {
                    "use" if use_from.is_none() => use_from = Some(end),
                    "std" | "alloc" if path_head(text, at).is_none() => {
                        let before = trailing_word(&text[..at]);
                        if after.starts_with("::") || before == "use" || before == "crate" {
                            sites.insert((line, if word == "std" { Kind::Std } else { Kind::Alloc }));
                        }
                    }
                    _ if COLLECTIONS.contains(&word) && use_from.is_none() && !was_defining => {
                        let shaped = word == TEXT || after.starts_with('<') || after.starts_with("::");
                        match path_head(text, at) {
                            _ if !shaped => {}
                            None => bare.push((line, word)),
                            Some(head) if STD_HEADS.contains(&head) => {
                                sites.insert((line, Kind::VecBox));
                                if in_tests::HASHES.contains(&word) {
                                    effects.insert((line, Effect::StdHash));
                                }
                            }
                            Some(_) => {}
                        }
                    }
                    _ if was_defining && COLLECTIONS.contains(&word) => {
                        own.names.insert(word.to_owned());
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
    for (line, name) in bare {
        if !own.holds(name) {
            sites.insert((line, Kind::VecBox));
            if in_tests::HASHES.contains(&name) {
                effects.insert((line, Effect::StdHash));
            }
        }
    }
    (sites.into_iter().collect(), effects.into_iter().collect())
}

/// The file a `#[path = "<named>"]` on a module the file `parts` declares names: relative to
/// that file's directory, `.` and `..` taken.
fn path_file(parts: &[String], named: &str) -> String {
    let mut out: Vec<&str> = parts.split_last().map_or(&[][..], |(_, dirs)| dirs).iter().map(String::as_str).collect();
    for part in named.split('/') {
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

/// The `.rs` files and the manifests under crates/, each sorted part by part, each as its parts
/// from the root, none under a directory named `target`. Directories that are links are not
/// entered.
fn tree_files(root: &Path) -> Result<(Vec<Vec<String>>, Vec<Vec<String>>), TreeError> {
    let manifest = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).map_err(|e| TreeError::Unreadable { path: "Cargo.toml".to_owned(), why: e.to_string() })?;
    let members = Members::of(&text);
    let (mut sources, mut manifests) = (Vec::new(), Vec::new());
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
            // A directory right under crates/ that the root workspace does not hold (its own
            // workspace, or a parked crate) is not the repository's code: not counted.
            if is_dir && parts.len() == 2 && !members.holds(&parts.join("/")) {
                continue;
            }
            if is_dir && name != "target" {
                todo.push(parts);
            } else if !is_dir && name.ends_with(".rs") {
                sources.push(parts);
            } else if !is_dir && name == "Cargo.toml" {
                manifests.push(parts);
            }
        }
    }
    sources.sort();
    manifests.sort();
    Ok((sources, manifests))
}

/// What a manifest's text says of its crate's root, read by sections and `path = ".."` keys:
/// `None` when it has no `[package]` (a workspace's), else its `[lib] path` and the first
/// `[[bin]] path`, either absent. `manifest` names it for an error.
fn manifest_targets(text: &str, manifest: &str) -> Result<Option<(Option<String>, Option<String>)>, TreeError> {
    let (mut package, mut lib, mut bin) = (false, None, None);
    let mut section = String::new();
    let mut bins = 0;
    for (at, line) in text.split('\n').enumerate() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("[[").and_then(|it| it.split("]]").next()) {
            section = format!("[[{}]]", name.trim());
            bins += usize::from(name.trim() == "bin");
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|it| it.split(']').next()) {
            section = name.trim().to_owned();
            package |= section == "package";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim() != "path" {
            continue;
        }
        let wanted = match section.as_str() {
            "lib" => &mut lib,
            "[[bin]]" if bins == 1 => &mut bin,
            _ => continue,
        };
        let value = value.split('#').next().unwrap_or(value).trim();
        let Some(path) = value.strip_prefix('"').and_then(|it| it.strip_suffix('"')) else {
            return Err(TreeError::NotAPath { manifest: manifest.to_owned(), line: at + 1 });
        };
        *wanted = Some(path.to_owned());
    }
    Ok(package.then_some((lib, bin)))
}

/// The root of the package whose manifest is `parts`, from the root, or `None` when the
/// manifest is a workspace's: its `[lib] path`, `src/lib.rs`, the first `[[bin]] path`, or
/// `src/main.rs`, whichever is first there.
fn crate_root(root: &Path, parts: &[String]) -> Result<Option<String>, TreeError> {
    let manifest = parts.join("/");
    let full = parts.iter().fold(root.to_path_buf(), |path, part| path.join(part));
    let text = std::fs::read_to_string(&full)
        .map_err(|e| TreeError::Unreadable { path: manifest.clone(), why: e.to_string() })?;
    let Some((lib, bin)) = manifest_targets(&universal_newlines(text), &manifest)? else { return Ok(None) };
    let dir = parts[..parts.len() - 1].join("/");
    let candidates = [lib, Some("src/lib.rs".to_owned()), bin, Some("src/main.rs".to_owned())];
    for path in candidates.into_iter().flatten() {
        let path = format!("{dir}/{}", path.trim_start_matches("./"));
        if root.join(&path).is_file() {
            return Ok(Some(path));
        }
    }
    Err(TreeError::NoRoot { manifest })
}

/// The tree as both counts read it: each `.rs` file under crates/ scanned, the files that are
/// test code by path, by `#![cfg(test)]` or by a declaration in test code (the ONE definition:
/// `count` skips them, in_tests.rs counts them), the modules each file declares, and the
/// packages' manifests.
struct Walked {
    scanned: Vec<(Vec<String>, Scanned)>,
    skipped: BTreeSet<String>,
    /// `skipped`, and the files a `cfg(any(test, feature = "testing"))` module holds, with
    /// every module they declare: a test's root for the heap columns only (heap.rs).
    testing: BTreeSet<String>,
    children: BTreeMap<String, Vec<String>>,
    packages: Vec<Vec<String>>,
}

/// Each crate's sites outside test code, in file and line order, its root's `is_std` first; a
/// crate with none is absent.
fn count(root: &Path) -> Result<BTreeMap<String, Vec<Site>>, TreeError> {
    let Walked { scanned, skipped, packages, .. } = walk(root)?;
    let mut out: BTreeMap<String, Vec<Site>> = BTreeMap::new();
    // A package under a `tests` or `benches` directory is a test's fixture, not a crate.
    for parts in packages.iter().filter(|it| !test_path(it)) {
        let Some(path) = crate_root(root, parts)? else { continue };
        let text = std::fs::read(root.join(&path)).map_err(|e| TreeError::Unreadable { path: path.clone(), why: e.to_string() })?;
        if !declares_no_std(&universal_newlines(String::from_utf8_lossy(&text).into_owned())) {
            let krate = parts[1..parts.len() - 1].join("/");
            out.entry(krate).or_default().push(Site { path, line: 1, what: Kind::IsStd });
        }
    }
    let mut manifests: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    for (parts, found) in scanned {
        let path = parts.join("/");
        if found.sites.is_empty() || skipped.contains(&path) {
            continue;
        }
        let krate = crate_of(root, &parts, &mut manifests);
        out.entry(krate)
            .or_default()
            .extend(found.sites.iter().map(|(line, kind)| Site { path: path.clone(), line: *line, what: *kind }));
    }
    Ok(out)
}

/// The tree's files scanned, and which are test code (see [`Walked`]).
fn walk(root: &Path) -> Result<Walked, TreeError> {
    let tests = Tests::new(SCRIPT);
    let (files, packages) = tree_files(root)?;
    let known: BTreeSet<String> = files.iter().map(|it| it.join("/")).collect();
    let mut scanned: Vec<(Vec<String>, Scanned)> = Vec::new();
    // The files not counted: test code, and the modules it declares.
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    let mut testing: BTreeSet<String> = BTreeSet::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for parts in files {
        let path = parts.join("/");
        let full = parts.iter().fold(root.to_path_buf(), |path, part| path.join(part));
        // What is not UTF-8 becomes U+FFFD, as untyped.rs reads it.
        let bytes = std::fs::read(&full).map_err(|e| TreeError::Unreadable { path: path.clone(), why: e.to_string() })?;
        let text = universal_newlines(String::from_utf8_lossy(&bytes).into_owned());
        let found = scan_file_with(&tests, &text);
        if test_path(&parts) || found.test {
            skipped.insert(path.clone());
        }
        if found.testing {
            testing.insert(path.clone());
        }
        for decl in &found.mods {
            let files = match &decl.path {
                Some(named) => vec![path_file(&parts, named)],
                None => module_files(&parts, &decl.name).into(),
            };
            for file in files {
                if !known.contains(&file) {
                    continue;
                }
                if decl.in_test {
                    skipped.insert(file.clone());
                }
                if decl.in_testing {
                    testing.insert(file.clone());
                }
                children.entry(path.clone()).or_default().push(file);
            }
        }
        scanned.push((parts, found));
    }
    // A module that skipped code declares is skipped, and so is every module it declares.
    let mut todo: Vec<String> = skipped.iter().cloned().collect();
    // Bounded: a file enters `skipped` once, and only a file that entered is taken again.
    while let Some(path) = todo.pop() {
        for child in children.get(&path).into_iter().flatten() {
            if skipped.insert(child.clone()) {
                todo.push(child.clone());
            }
        }
    }
    // The same for the heap's wider test code: what skipped code is, and the `testing` modules.
    testing.extend(skipped.iter().cloned());
    let mut todo: Vec<String> = testing.iter().cloned().collect();
    // Bounded: as above.
    while let Some(path) = todo.pop() {
        for child in children.get(&path).into_iter().flatten() {
            if testing.insert(child.clone()) {
                todo.push(child.clone());
            }
        }
    }
    Ok(Walked { scanned, skipped, testing, children, packages })
}

/// The list as `write` writes it: the header, then `<crate><TAB><std><TAB><is_std><TAB><alloc>
/// <TAB><vec_box>` for each crate with a site, sorted.
fn render(counts: &BTreeMap<String, Vec<Site>>) -> String {
    let mut out = HEADER.to_owned();
    for (krate, sites) in counts.iter().filter(|(_, sites)| !sites.is_empty()) {
        let [named_std, is_std, alloc, vec_box] = tally(sites);
        out.push_str(&format!("{krate}\t{named_std}\t{is_std}\t{alloc}\t{vec_box}\n"));
    }
    out
}

/// What command `write` writes, and what it found.
struct Written {
    /// Each crate's row: where it had one, the lower of the row and its count in each column
    /// (`write` never raises a row, so "over its row" always means a raise); where it had none,
    /// its count.
    rows: BTreeMap<String, Row>,
    /// The crates that had no row and got one.
    fresh: Vec<String>,
    /// The crates over their rows in some column.
    over: Vec<String>,
}

fn written_rows(counts: &BTreeMap<String, Vec<Site>>, rows: &BTreeMap<String, Row>) -> Written {
    let mut out = Written { rows: BTreeMap::new(), fresh: Vec::new(), over: Vec::new() };
    let crates: BTreeSet<&String> = counts.keys().chain(rows.keys()).collect();
    for krate in crates {
        let now = counts.get(krate).map_or([0; 4], |sites| tally(sites));
        let row = match rows.get(krate) {
            Some(held) => {
                if now.iter().zip(held).any(|(n, h)| n > h) {
                    out.over.push(krate.clone());
                }
                core::array::from_fn(|at| now[at].min(held[at]))
            }
            None => {
                if now != [0; 4] {
                    out.fresh.push(krate.clone());
                }
                now
            }
        };
        if row != [0; 4] {
            out.rows.insert(krate.clone(), row);
        }
    }
    out
}

/// The list with `rows`: the header, then each row that is not all zero, sorted, each under the
/// comment lines `notes` keeps for it (why a row was raised by hand).
fn render_rows(rows: &BTreeMap<String, Row>, notes: &BTreeMap<String, Vec<String>>) -> String {
    let mut out = HEADER.to_owned();
    for (krate, [named_std, is_std, alloc, vec_box]) in rows {
        for note in notes.get(krate).into_iter().flatten() {
            out.push_str(note);
            out.push('\n');
        }
        out.push_str(&format!("{krate}\t{named_std}\t{is_std}\t{alloc}\t{vec_box}\n"));
    }
    out
}

/// The comment lines of the list `text` past its `header`, by the row they stand above.
fn row_notes(text: &str, header: &str) -> BTreeMap<String, Vec<String>> {
    let header = header.split('\n').filter(|it| !it.is_empty()).count();
    let mut notes = BTreeMap::new();
    let mut pending = Vec::new();
    for line in text.split('\n').skip(header) {
        if line.starts_with('#') {
            pending.push(line.to_owned());
        } else if let Some(krate) = line.split('\t').next().filter(|it| !it.is_empty()) {
            if !pending.is_empty() {
                notes.insert(krate.to_owned(), std::mem::take(&mut pending));
            }
        }
    }
    notes
}

/// The list's rows.
fn parse_rows(text: &str) -> Result<BTreeMap<String, Row>, ListError> {
    let mut rows = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let row = match fields.as_slice() {
            [krate, rest @ ..] if !krate.is_empty() && rest.len() == 4 => {
                let mut n: Row = [0; 4];
                let read = rest.iter().zip(&mut n).all(|(field, slot)| field.parse::<usize>().map(|it| *slot = it).is_ok());
                read.then_some((*krate, n))
            }
            _ => None,
        };
        let Some((krate, n)) = row else {
            return Err(ListError::NotARow { line: at + 1, text: line.to_owned() });
        };
        if n[Kind::IsStd.column()] > 1 {
            return Err(ListError::NotABit { line: at + 1, krate: krate.to_owned(), found: n[Kind::IsStd.column()] });
        }
        if rows.insert(krate.to_owned(), n).is_some() {
            return Err(ListError::TwoRows { line: at + 1, krate: krate.to_owned() });
        }
    }
    Ok(rows)
}

/// What `base` says: its rows (none when it has no list, before the ratchet began), and the lines
/// added since it under crates/.
type AtBase = (Option<BTreeMap<String, Row>>, BTreeMap<String, BTreeSet<usize>>);

fn at_base(root: &Path, base: &str) -> Result<AtBase, AtBaseError> {
    let root_arg = root.to_string_lossy();
    let dir = root_arg.as_ref();
    let commit = format!("{base}^{{commit}}");
    git(root, &["-C", dir, "rev-parse", "--verify", "--quiet", commit.as_str()])
        .map_err(|why| AtBaseError::Base(BaseError::NoCommit { base: base.to_owned(), why }))?;
    let failed = |what: &'static str| move |why: String| AtBaseError::Base(BaseError::Git { base: base.to_owned(), what, why });
    let listed = git(root, &["-C", dir, "ls-tree", "--name-only", base, "--", LIST]).map_err(failed("list its tree"))?;
    let rows = if listed.trim().is_empty() {
        None
    } else {
        let spec = format!("{base}:{LIST}");
        let text = git(root, &["-C", dir, "show", spec.as_str()]).map_err(failed("show the list"))?;
        Some(parse_rows(&text).map_err(|error| AtBaseError::List { base: base.to_owned(), error })?)
    };
    let diff = git(root, &["-C", dir, "diff", "--no-color", "--no-ext-diff", "--no-renames", "--unified=0", base, "--", "crates"])
        .map_err(failed("diff the tree"))?;
    Ok((rows, added_lines(&diff).map_err(AtBaseError::Base)?))
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
            if now > held {
                let shown = match row {
                    Some(_) => format!("its row's `{column}` {held}"),
                    None => "0, as a crate with no row is held".to_owned(),
                };
                let of_kind = sites.iter().filter(|it| it.what == kind);
                let new = match (kind, added) {
                    // The root is the site whatever the diff says: it is one line of one file.
                    (Kind::IsStd, _) => {
                        let roots: Vec<&str> = of_kind.map(|it| it.path.as_str()).collect();
                        format!("its root {} has no `#![no_std]`", roots.join(", "))
                    }
                    (_, Some(added)) => {
                        let new: Vec<String> = of_kind
                            .filter(|it| added.get(&it.path).is_some_and(|lines| lines.contains(&it.line)))
                            .map(|it| format!("{}:{}", it.path, it.line))
                            .collect();
                        if new.is_empty() {
                            "none is on a line added since the base: a site moved in from another crate, or a file git does not track".to_owned()
                        } else {
                            format!("new: {}", new.join(", "))
                        }
                    }
                    (_, None) => "give `base`, the commit the change is against, to have the new ones named".to_owned(),
                };
                out.push(format!(
                    "`{krate}` has {now} {what} outside tests, over {shown} in {LIST} by {} ({new}): {} ({RULE})",
                    now - held,
                    kind.instead()
                ));
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
        for (krate, n) in rows {
            let was = base.get(krate).copied().unwrap_or([0; 4]);
            for kind in KINDS {
                let (now, then) = (n[kind.column()], was[kind.column()]);
                if now > then {
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
fn total(counts: &BTreeMap<String, Vec<Site>>) -> Row {
    let mut row = [0; 4];
    for sites in counts.values() {
        for (sum, n) in row.iter_mut().zip(tally(sites)) {
            *sum += n;
        }
    }
    row
}

/// `counts`' totals, for a line of output.
fn summary(counts: &BTreeMap<String, Vec<Site>>) -> String {
    let [named_std, is_std, alloc, vec_box] = total(counts);
    format!(
        "{named_std} lines naming `std`, {is_std} crate roots without `#![no_std]`, {alloc} lines naming `alloc` and \
         {vec_box} naming a `std` or `alloc` collection, outside tests, in {} crates",
        counts.len()
    )
}

fn main_with(it: &Args) {
    // `Path(root).resolve()`.
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    if !root.join("crates").is_dir() {
        fail(SCRIPT, &format!("{} has no crates/ directory", root.display()));
    }
    if it.command.starts_with("tests-") {
        in_tests::main_with(&it.command, it.base.as_deref(), &root);
        return;
    }
    if it.command.starts_with("heap-") {
        heap::main_with(&it.command, it.base.as_deref(), &root);
        return;
    }
    if it.command.starts_with("reach-") {
        reach::main_with(&it.command, it.base.as_deref(), &root);
        return;
    }
    if it.command.starts_with("producers-") {
        producers::main_with(&it.command, it.base.as_deref(), &root);
        return;
    }
    if it.command.starts_with("sansio-") {
        sansio::main_with(&it.command, it.base.as_deref(), &root);
        return;
    }
    let counts = count(&root).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    if it.command == "findings" {
        let found = counts.iter().flat_map(|(krate, sites)| {
            sites.iter().map(|it| Finding { krate: krate.clone(), file: it.path.clone(), line: it.line, kind: it.what.name(), detail: None })
        });
        finding::print(SCRIPT, &KINDS.map(Kind::finding), found.collect());
        return;
    }
    if it.command == "write" {
        let listed = read_text(&root.join(LIST)).unwrap_or_default();
        let rows = parse_rows(&listed).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
        let written = written_rows(&counts, &rows);
        if let Err(e) = std::fs::write(root.join(LIST), render_rows(&written.rows, &row_notes(&listed, HEADER))) {
            fail(SCRIPT, &format!("{LIST} is not written: {e}"));
        }
        println!("{SCRIPT}: wrote {LIST}: {}", summary(&counts));
        if !written.fresh.is_empty() {
            println!("{SCRIPT}: {} crates had no row and got one at today's count: {}", written.fresh.len(), written.fresh.join(", "));
        }
        if !written.over.is_empty() {
            println!(
                "{SCRIPT}: {} crates are over their rows, which stay (a row never rises by `write`; raise one by hand, named in the change): {}",
                written.over.len(),
                written.over.join(", ")
            );
        }
        return;
    }
    let Some(listed) = read_text(&root.join(LIST)) else {
        fail(SCRIPT, &format!("{LIST} could not be read: every crate is held to its row there (command `write` makes it)"))
    };
    let rows = parse_rows(&listed).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let (base_rows, added) = match &it.base {
        Some(base) => {
            let (rows, added) = at_base(&root, base).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
            if rows.is_none() {
                println!("{SCRIPT}: {base} has no {LIST}: the list begins with this change, so no row is compared with the base");
            }
            (rows, Some(added))
        }
        None => (None, None),
    };
    let mut wrong = problems(&counts, &rows, base_rows.as_ref(), added.as_ref());
    if wrong.is_empty() && listed != render_rows(&written_rows(&counts, &rows).rows, &row_notes(&listed, HEADER)) {
        wrong.push(format!(
            "{LIST} is not as command `write` writes it (its header, then a sorted `<crate><TAB><std><TAB><is_std><TAB><alloc><TAB><vec_box>` row per crate with a site): run it"
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
            let at: Vec<String> = sites.iter().map(|it| format!("{}:{}:{}", it.path, it.line, it.what.name())).collect();
            format!("{krate}: {}", at.join(", "))
        })
        .collect();
    crates.join("; ")
}

/// One file's sites, for a selftest's expectation.
fn sites_of(path: &str, lines: &[(usize, Kind)]) -> Vec<Site> {
    lines.iter().map(|(line, kind)| Site { path: path.to_owned(), line: *line, what: *kind }).collect()
}

/// The selftest's file of crate `a`, which imports nothing of blaze's own. Line by line:
/// - 1, `use std::fmt;`: `std` (an import names `std`, and holds no use of a collection);
/// - 2, `core::alloc::Layout`: no `alloc` (not the first segment);
/// - 3, `extern crate alloc;`: `alloc`;
/// - 4, `Vec<u8>` and `Box<str>`: one `vec_box`, the line once;
/// - 5, `String`, twice: one `vec_box`;
/// - 6, `alloc::vec::Vec<u8>`: `alloc` and `vec_box`;
/// - 7, `blaze_own::Vec<u8, A>`: nothing;
/// - 8, `::std::sync::Arc<u8>`: `std` and `vec_box`;
/// - 9, `std::fs Vec<u8>` in a string and `HashMap<u8, u8> std::io` in a comment: nothing;
/// - 10, `Kind::String`: nothing (a variant's path);
/// - 11, `HashMap<..>` and `Rc<u8>` in a struct: one `vec_box`;
/// - 12, `Vec::new()`: `vec_box`;
/// - 13, `crate::std::Thing`: nothing (`std` not the first segment);
/// - 14-18, `#[cfg(test)] mod tests`: test code;
/// - 19-20, `#[test]`: test code;
/// - 21, a parameter named `alloc`: nothing;
/// - 22, five more collections: one `vec_box`;
/// - 23, `blaze_std::Thing`: nothing (another word);
/// - 24, `std::x Vec<u8>` in a raw string: nothing;
/// - 25, `self.alloc` and `.Box` after a `.`: nothing.
const FIXTURE: &str = r##"use std::fmt;
use core::alloc::Layout;
extern crate alloc;
pub fn a(v: Vec<u8>) -> Box<str> { loop {} }
pub fn b() -> String { String::new() }
pub fn c() -> alloc::vec::Vec<u8> { loop {} }
pub fn d() -> blaze_own::Vec<u8, A> { loop {} }
pub fn e() -> ::std::sync::Arc<u8> { loop {} }
fn f() -> &'static str { "std::fs Vec<u8>" } // HashMap<u8, u8> std::io
fn g(m: Kind) -> bool { matches!(m, Kind::String) }
struct Holder { map: HashMap<u32, u32>, rc: Rc<u8> }
fn h() { let _ = Vec::new(); }
fn i(x: crate::std::Thing) {}
#[cfg(test)]
mod tests {
    use std::vec::Vec;
    fn t() -> Vec<u8> { Vec::new() }
}
#[test]
fn a_test() { let _: Vec<u8> = std::vec::Vec::new(); }
fn alloc_var(alloc: A) -> A { alloc }
fn big(s: BTreeSet<u8>, d: VecDeque<u8>, h: BinaryHeap<u8>, x: HashSet<u8>, y: BTreeMap<u8, u8>) {}
fn mystd() -> blaze_std::Thing { loop {} }
fn raw() -> &'static str { r#"std::x Vec<u8>"# }
fn field(&self) -> u8 { self.alloc.Box }
"##;

/// The sites of [`FIXTURE`] outside its test code, by line and column.
const FIXTURE_SITES: [(usize, Kind); 11] = [
    (1, Kind::Std),
    (3, Kind::Alloc),
    (4, Kind::VecBox),
    (5, Kind::VecBox),
    (6, Kind::Alloc),
    (6, Kind::VecBox),
    (8, Kind::Std),
    (8, Kind::VecBox),
    (11, Kind::VecBox),
    (12, Kind::VecBox),
    (22, Kind::VecBox),
];

/// The sites of [`FIXTURE`]'s test code: its test module's `use std::vec::Vec;` and the
/// `Vec`s it makes (an import from `std` makes no name blaze's own), and the test function's.
const FIXTURE_TEST_SITES: [(usize, Kind); 4] = [(16, Kind::Std), (17, Kind::VecBox), (20, Kind::Std), (20, Kind::VecBox)];

/// A file that takes its collections from blaze-own and from its own crate: only line 4's `Rc`
/// is `std`'s. Line 2's `alloc` is the crate's own module, named under `crate::`.
const OWN_FIXTURE: &str = "use blaze_own::{Vec, Box as B};\n\
                           use crate::alloc::String;\n\
                           fn f(v: Vec<u8>, b: B<u8>, s: String) {}\n\
                           fn g() -> Rc<u8> { loop {} }\n";

/// A file that takes every name of blaze-own by a glob.
const GLOB_FIXTURE: &str = "use blaze_own::*;\nfn f(v: Vec<u8>) -> HashMap<u8, u8> { loop {} }\n";

/// A file that defines its own `Vec`: only line 3's `Box` is `std`'s.
const DEFINES_FIXTURE: &str = "pub struct Vec<T, A> { a: A, t: T }\nimpl<T, A> Vec<T, A> {}\nfn f() -> Box<u8> { loop {} }\n";

fn compared(found: &[(usize, Kind)], want: &[(usize, Kind)]) -> String {
    let extra: Vec<&(usize, Kind)> = found.iter().filter(|it| !want.contains(it)).collect();
    let missing: Vec<&(usize, Kind)> = want.iter().filter(|it| !found.contains(it)).collect();
    format!("\n  found   {found:?}\n  wanted  {want:?}\n  extra   {extra:?}\n  missing {missing:?}")
}

/// Ends the selftest with `e`, which a step of it did not expect.
fn failed(e: &dyn fmt::Display) -> ! {
    fail(&format!("{SCRIPT} selftest"), &e.to_string())
}

fn selftest() {
    // Test attributes and `cfg(test)` matchers, through `blaze_script::ratchet::Tests`.
    let tests = Tests::new(SCRIPT);
    expect(
        tests.of("#[test]\nfn t() {}\n").test == [true, true, false]
            && tests.of("  #[tokio::test(flavor = \"x\")]\nfn t() {}\n").test == [true, true, false],
        "test attributes",
    );
    expect(
        tests.of("#[test_case(1)]\nfn t() {}\n").test == [false, false, false]
            && tests.of("#[testing]\nfn t() {}\n").test == [false, false, false],
        "an attribute named for no test",
    );
    expect(
        tests.of("#[cfg(test)]\nmod t {}\n").test == [true, true, false]
            && tests.of("#[cfg(all(test, feature = \"x\"))]\nmod t {}\n").test == [true, true, false]
            && tests.of("#[cfg(testing)]\nmod t {}\n").test == [false, false, false]
            && tests.of("#![cfg(test)]\n").whole
            && tests.of("#[cfg(not(test))]\nmod t {}\n").test == [false, false, false],
        "`cfg(test)` and `cfg(all(test, ..))`",
    );
    expect(
        module_decl("pub(crate) mod a;") == Some("a")
            && module_decl("#[cfg(test)] mod t ;") == Some("t")
            && module_decl("mod r#type;") == Some("type")
            && module_decl("mod a {").is_none()
            && module_decl("model;").is_none(),
        "out-of-line module declarations",
    );
    expect(
        declares_no_std("//! doc\n#![no_std]\n")
            && declares_no_std("#![cfg_attr(not(test), no_std)]\n")
            && !declares_no_std("// #![no_std]\n")
            && !declares_no_std("#![cfg_attr(feature = \"x\", no_std)]\n"),
        "a crate root's `#![no_std]`",
    );
    let mut leaves = Vec::new();
    use_leaves("::blaze_own::{self, vec::{Vec as V, self}, *}", None, "", &mut leaves);
    let pairs: Vec<(&str, &str)> = leaves.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    expect(
        pairs == [("blaze_own", "blaze_own"), ("blaze_own", "V"), ("blaze_own", "vec"), ("blaze_own", "*")],
        &format!("a use tree's leaves: {pairs:?}"),
    );

    // One file: every case of [`FIXTURE`], test code in, then out.
    let all = code_sites(FIXTURE);
    let mut want: Vec<(usize, Kind)> = FIXTURE_SITES.iter().chain(&FIXTURE_TEST_SITES).copied().collect();
    want.sort();
    expect(all == want, &format!("the fixture's sites, test code in: {}", compared(&all, &want)));
    let found = scan_file(FIXTURE);
    expect(found.sites == FIXTURE_SITES, &format!("the fixture's sites outside test code: {}", compared(&found.sites, &FIXTURE_SITES)));
    expect(found.mods.is_empty() && !found.test, "the fixture declares no module out of line");
    // Planted: the test module's attribute, then the test's, removed: their lines are sites.
    let lines = scan_file(&FIXTURE.replace("#[cfg(test)]\n", "\n")).sites;
    expect(lines.contains(&(16, Kind::Std)) && lines.contains(&(17, Kind::VecBox)), &format!("the test module unmarked counts: {lines:?}"));
    let lines = scan_file(&FIXTURE.replace("#[test]\n", "\n")).sites;
    expect(lines.contains(&(20, Kind::Std)) && lines.contains(&(20, Kind::VecBox)), &format!("the test function unmarked counts: {lines:?}"));
    expect(scan_file("#![cfg(test)]\nuse std::fs;\n").sites.is_empty(), "a file that is test code holds no site");

    // Imports and definitions make a name the file's own; planted: the same names from another
    // crate are `std`'s again.
    let own = code_sites(OWN_FIXTURE);
    expect(own == [(4, Kind::VecBox)], &format!("names imported from blaze-own and the crate: {own:?}"));
    let other = code_sites(&OWN_FIXTURE.replace("blaze_own", "hashbrown"));
    expect(other == [(3, Kind::VecBox), (4, Kind::VecBox)], &format!("the same names from another crate: {other:?}"));
    let glob = code_sites(GLOB_FIXTURE);
    expect(glob.is_empty(), &format!("a glob of blaze-own: {glob:?}"));
    let glob = code_sites(&GLOB_FIXTURE.replace("blaze_own", "std::collections"));
    expect(glob == [(1, Kind::Std), (2, Kind::VecBox)], &format!("a glob of std: {glob:?}"));
    let defines = code_sites(DEFINES_FIXTURE);
    expect(defines == [(3, Kind::VecBox)], &format!("a file that defines `Vec`: {defines:?}"));

    // A manifest's targets.
    let package = manifest_targets("[package]\nname = \"a\"\n[lib]\npath = \"mod.rs\" # here\n[[bin]]\npath = \"b.rs\"\n[[bin]]\npath = \"c.rs\"\n[dependencies.x]\npath = \"../x\"\n", "m");
    expect(
        package == Ok(Some((Some("mod.rs".to_owned()), Some("b.rs".to_owned())))),
        &format!("a package's `[lib]` and first `[[bin]]` path: {package:?}"),
    );
    expect(manifest_targets("[workspace]\nmembers = []\n", "m") == Ok(None), "a workspace's manifest is no crate");
    expect(
        manifest_targets("[package]\n[lib]\npath = mod.rs\n", "m") == Err(TreeError::NotAPath { manifest: "m".to_owned(), line: 3 }),
        "a path that is no string is refused, naming its line",
    );

    // A tree: a crate on `std` with a build script, test files by path and by declaration and a
    // `target`; a workspace holding a `#![no_std]` crate whose root is a `[lib] path`; a binary
    // on `cfg_attr(not(test), no_std)` with no site; a test fixture's package.
    let tmp = TempDir::new("ambient-selftest-").unwrap_or_else(|e| failed(&e));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    let uses_std = "use std::fs;\n";
    // The root workspace: `crates/*`, less a parked crate; `crates/own` is its own workspace,
    // excluded too. Neither is counted: planted below with a `std` use each.
    put("Cargo.toml", "[package]\nname = \"root\"\n\n[workspace]\nmembers = [\n    \"crates/*\", # all\n    \"scripts\",\n]\nexclude = [\n    \"crates/parked\",\n    \"crates/own\",\n]\n");
    put("crates/parked/Cargo.toml", "[package]\nname = \"parked\"\n");
    put("crates/parked/src/lib.rs", uses_std);
    put("crates/own/Cargo.toml", "[workspace]\n");
    put("crates/own/host/src/lib.rs", uses_std);
    let read = Members::of(&std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap_or_else(|e| failed(&e)));
    expect(
        read.globs == ["crates/*", "scripts"] && read.exclude == BTreeSet::from(["crates/parked".to_owned(), "crates/own".to_owned()]),
        &format!("the root workspace read: {read:?}"),
    );
    expect(
        read.holds("crates/a") && read.holds("scripts") && !read.holds("crates/parked") && !read.holds("crates/a/b") && !read.holds("tools/x"),
        "membership",
    );
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    put("crates/a/src/lib.rs", FIXTURE);
    put("crates/a/src/x.rs", "#[cfg(test)]\nmod t;\n");
    put("crates/a/src/x/t.rs", uses_std);
    // A test module declared with a `#[path]`: its file is test code, not the default `y/t.rs`.
    put("crates/a/src/y.rs", "#[cfg(test)]\n#[path = \"y_tests.rs\"]\nmod t;\n");
    put("crates/a/src/y_tests.rs", uses_std);
    put("crates/a/build.rs", uses_std);
    put("crates/a/tests/it.rs", uses_std);
    put("crates/a/tests/fixture/Cargo.toml", "[package]\nname = \"fixture\"\n");
    put("crates/a/tests/fixture/src/lib.rs", uses_std);
    put("crates/a/target/debug/build/out.rs", uses_std);
    put("crates/b/Cargo.toml", "[workspace]\n");
    put("crates/b/c/Cargo.toml", "[package]\nname = \"c\"\n[lib]\npath = \"mod.rs\"\n");
    put("crates/b/c/mod.rs", "#![no_std]\nextern crate alloc;\n");
    put("crates/d/Cargo.toml", "[package]\nname = \"d\"\n");
    put("crates/d/src/main.rs", "#![cfg_attr(not(test), no_std)]\nfn main() {}\n");
    let counts = count(tmp.path()).unwrap_or_else(|e| failed(&e));
    let lib = "crates/a/src/lib.rs";
    let mut a = sites_of(lib, &[(1, Kind::IsStd)]);
    a.extend(sites_of("crates/a/build.rs", &[(1, Kind::Std)]));
    a.extend(sites_of(lib, &FIXTURE_SITES));
    let want = BTreeMap::from([("a".to_owned(), a), ("b/c".to_owned(), sites_of("crates/b/c/mod.rs", &[(2, Kind::Alloc)]))]);
    expect(counts == want, &format!("the tree's sites: {}", shown(&counts)));
    expect(total(&counts) == [3, 1, 3, 7], &format!("the totals: {:?}", total(&counts)));
    // `write` never raises a row: `a` over its `std` 0 keeps 0 there and is named; `b/c`, with
    // no row, gets one at its count.
    let written = written_rows(&counts, &BTreeMap::from([("a".to_owned(), [0, 9, 9, 9])]));
    expect(
        written.over == ["a"] && written.fresh == ["b/c"] && written.rows.get("a").is_some_and(|it| it[0] == 0 && it[1] == 1),
        &format!("write's rows: {:?}, fresh {:?}, over {:?}", written.rows, written.fresh, written.over),
    );
    // A hand-raised row's comment stays above it through `write`.
    let notes = BTreeMap::from([("b/c".to_owned(), vec!["# b/c raised by hand: why".to_owned()])]);
    let listed = render_rows(&written.rows, &notes);
    expect(row_notes(&listed, HEADER) == notes && listed.contains("# b/c raised by hand: why\nb/c\t"), &format!("the notes kept: {listed}"));

    // The ratchet. As recorded, nothing is wrong, and the list reads back as it was written.
    let rows = parse_rows(&render(&counts)).unwrap_or_else(|e| failed(&e));
    expect(
        rows == BTreeMap::from([("a".to_owned(), [3, 1, 2, 7]), ("b/c".to_owned(), [0, 0, 1, 0])]),
        &format!("the rows read back: {rows:?}"),
    );
    expect(problems(&counts, &rows, Some(&rows), None).is_empty(), "the tree as recorded is right");
    // Planted: one collection added, named by its crate, column and place given the base's
    // diff, with what to write instead and the rule.
    let mut fewer = rows.clone();
    fewer.insert("a".to_owned(), [3, 1, 2, 6]);
    let added = BTreeMap::from([(lib.to_owned(), BTreeSet::from([4]))]);
    let said = problems(&counts, &fewer, None, Some(&added));
    expect(
        said.len() == 1
            && said[0].contains("`a` has 7 lines naming a collection of `std` or `alloc`")
            && said[0].contains("over its row's `vec_box` 6")
            && said[0].contains("new: crates/a/src/lib.rs:4)")
            && said[0].contains("blaze_own::Vec<T, A>")
            && said[0].contains("rule 75"),
        &format!("one collection added is caught and named: {said:?}"),
    );
    // Planted: the root's `#![no_std]` taken away, named by the root.
    let mut no_std = rows.clone();
    no_std.insert("a".to_owned(), [3, 0, 2, 7]);
    let said = problems(&counts, &no_std, None, None);
    expect(
        said.len() == 1 && said[0].contains("`is_std` 0") && said[0].contains("its root crates/a/src/lib.rs has no `#![no_std]`"),
        &format!("a root on `std` is caught: {said:?}"),
    );
    // Planted: one site removed. Its column must come down; a crate with none left goes.
    let mut more = rows.clone();
    more.insert("b/c".to_owned(), [0, 0, 2, 0]);
    let said = problems(&counts, &more, None, None);
    expect(said.len() == 1 && said[0].contains("`b/c`") && said[0].contains("lower its `alloc` to 1"), &format!("one site removed: {said:?}"));
    let mut gone = rows.clone();
    gone.insert("old".to_owned(), [1, 0, 0, 0]);
    let said = problems(&counts, &gone, None, None);
    expect(said.len() == 1 && said[0].contains("`old`") && said[0].contains("remove the row"), &format!("a crate gone: {said:?}"));
    // Planted: a crate with no row is held to 0; a row raised above the base's is caught even
    // where the tree matches it.
    let said = problems(&counts, &BTreeMap::from([("a".to_owned(), [3, 1, 2, 7])]), None, None);
    expect(said.len() == 1 && said[0].contains("`b/c` has 1") && said[0].contains("held"), &format!("a new crate starts at 0: {said:?}"));
    let said = problems(&counts, &rows, Some(&fewer), None);
    expect(said.len() == 1 && said[0].contains("the row `a`'s `vec_box` is 7 and was 6 at the base"), &format!("a row raised: {said:?}"));

    // A package with no root this script finds is refused, naming its manifest.
    put("crates/e/Cargo.toml", "[package]\nname = \"e\"\n");
    let refused = count(tmp.path());
    expect(
        refused == Err(TreeError::NoRoot { manifest: "crates/e/Cargo.toml".to_owned() }),
        &format!("a package with no root: {:?}", refused.err()),
    );

    // The base's diff: hunks of added lines, a deleted file, a hunk that only removes.
    // The first hunk adds a line whose text starts `++ `: it is no file's header.
    let diff = "diff --git a/crates/a/src/lib.rs b/crates/a/src/lib.rs\n--- a/crates/a/src/lib.rs\n+++ b/crates/a/src/lib.rs\n\
        @@ -2,0 +3 @@ fn f() {\n+++ b/not/a/file\n@@ -9 +10,2 @@\n-old\n+y\n+z\n@@ -20 +21,0 @@\n-w\n\
        diff --git a/crates/a/src/gone.rs b/crates/a/src/gone.rs\n--- a/crates/a/src/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-v\n";
    let lines = added_lines(diff).unwrap_or_else(|e| failed(&e));
    expect(lines == BTreeMap::from([(lib.to_owned(), BTreeSet::from([3, 10, 11]))]), &format!("the lines added: {lines:?}"));
    expect(added_lines("@@ nonsense\n").is_err_and(|e| matches!(e, BaseError::Hunk { .. })), "a hunk that is none is refused");
    expect(
        parse_rows("a\t1\t0\t0\t0\nb 2\n").is_err_and(|e| e == ListError::NotARow { line: 2, text: "b 2".to_owned() })
            && parse_rows("a\t1\t0\t0\t0\na\t2\t0\t0\t0\n").is_err_and(|e| matches!(e, ListError::TwoRows { line: 2, .. }))
            && parse_rows("a\t0\t2\t0\t0\n").is_err_and(|e| matches!(e, ListError::NotABit { line: 1, found: 2, .. }))
            && parse_rows("a\t1\t0\t0\n").is_err()
            && parse_rows("a\t1\t0\t0\tx\n").is_err()
            && parse_rows("\t1\t0\t0\t0\n").is_err(),
        "a bad row is refused, naming its line",
    );
    in_tests::selftest();
    heap::selftest();
    reach::selftest();
    producers::selftest();
    sansio::selftest();
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!(
                "usage: {SCRIPT} '<json>' | selftest\n\n{DOC}{}{}{}{}{}",
                in_tests::TESTS_DOC,
                heap::HEAP_DOC,
                reach::REACH_DOC,
                producers::PRODUCERS_DOC,
                sansio::SANS_DOC
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script's own cases run under `blaze test` and `cargo test` too: one that fails ends
    /// the test process with status 1, which fails the run.
    #[test]
    fn the_selftest_holds() {
        selftest();
    }
}
