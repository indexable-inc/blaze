//! untyped: the function signatures whose error is untyped (`String`, `&str`, `Box<dyn Error>`,
//! `anyhow`) only go down, per crate (AGENTS.md errors-name-the-thing, rule 19). `DOC` below is
//! what its usage error prints; it says what each count is, what the text scan misses, which
//! code is a test's, and the rule.
//!
//!     blaze run -q -p scripts --bin untyped -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin untyped -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin untyped -- selftest
//!
//! Why (2026-11-15): `blaze::untyped_error` (crates/blaze-lints/src/blaze/untyped_error.rs) is
//! the rule, but blaze cannot yet check its own crates, so its baseline says `unmeasured` and
//! nothing held the count: `Result<_, String>` kept spreading, and an error `String` also
//! cannot take an allocator, which blocks the allocator work. This holds the count from the
//! text, with no compiler, as `allocs` holds the global heap's: a gate step of the `scripts`
//! job (.github/workflows/blaze.yaml), beside `allocs`, until the lint's baseline joins the
//! gate.
//!
//! The scanning is a copy of allocs.rs's (`item_end`, `signature_end`, `comment_end`,
//! `string_end`, `quote_end`, `word_end`, `raw_end`, `test_path`, `module_files`,
//! `rust_files`, `crate_of`, `added_lines`, the file walk of `count`), kept self-contained
//! while allocs.rs changes in another pull request; the two go into one shared module of
//! blaze-script once it is in (AGENTS.md one-concept-one-home). `word_end` here differs: it
//! reads a raw identifier (`r#type`) whole, where allocs.rs reads its word on as a keyword.

/// What the usage error prints.
const DOC: &str = r#"untyped: the function signatures whose error is untyped only go down, per crate (AGENTS.md errors-name-the-thing, rule 19).

    blaze run -q -p scripts --bin untyped -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin untyped -- '{"command": "write"}'
    blaze run -q -p scripts --bin untyped -- selftest

    command   check   each count of every crate equals its row's in the list: one over its row
                      is refused (its new sites named given `base`), one under it must be
                      lowered with `write`; a crate with no row is held to 0; with `base`, no
                      count of a row is above the base's (CI)
              write   write the list from the tree as it is
    root      the checkout (".")
    base      the commit the change is against, or null: its list (`git show`) holds every
              row down, and the lines added since it (`git diff --unified=0`) name a crate's
              new sites

The list's row is `<crate><TAB><string><TAB><str><TAB><boxed><TAB><anyhow><TAB><aliases>`, of
`.rs` files under crates/ (a directory named `target` is not read).

A signature is a function's, from the word `fn` and its name to the `{` or `;` that ends it,
over as many lines as it spans (comments, strings and characters skipped): a free function, an
inherent method, a trait's declaration, a function nested in another's body. Its error is
untyped when its text (its return type, a parameter's type, a generic bound or a `where`
clause) names, at any depth (`Option<Result<(), String>>`), a `Result` (any path) whose last
top-level type argument, its error, is one of:
  string   `String` (any path, with or without `<..>`: `blaze_mem::String<A>` is a string too);
           `Result<Vec<(String, u32)>, String>` is one, `Result<String, Error>` none
  str      `&str`, any lifetime, `mut` or not (`&'static str`)
  boxed    `Box<dyn ..>` one of whose traits is named `Error` (any path, any auto traits and
           lifetimes beside: `Box<dyn std::error::Error + Send + Sync>`)
  anyhow   `anyhow::Error`, or `anyhow::Result<T>` with no error argument; anywhere in the
           signature, as the lint finds them
A signature counts once in each column it holds, at the line of its `fn`.
  aliases  a `type` item (`type Result<T> = std::result::Result<T, String>`, an associated
           type too) whose aliased type, after its `=`, holds one of the four: counted where it
           is defined, once, at the line of its `type`. The lint finds each signature that
           uses it, through the types; the text can't, so it counts the definition.

Not counted, as the lint `blaze::untyped_error` does not check them:
  - a method directly in an `impl <Trait> for <Type>` block: its signature is the trait's,
    counted at the trait's declaration (when the trait is in the workspace). The scan tells
    such a block by its header: from the word `impl` to its `{`, a word `for` that is not a
    bound's `for<'a>`;
  - in a binary's crate root, its `main` module, which may report to a person and exit: a
    signature or alias outside every inline `mod` block of `src/main.rs`, of a file of a `bin`
    or `examples` directory, or of a `main.rs` in a directory under one;
  - a build script (`build.rs` outside `src/`), and the modules it declares out of line;
  - a closure, and a function pointer type on its own (`fn(u8) -> ..`, no name);
  - test code (below).

The count is read from the text, with no types, so it differs from the lint:
  missed  an import renamed (`use anyhow::Result;` then `Result<T>`; `use String as Text`);
          a use of an alias (its definition is counted, in its crate; an alias outside the
          workspace is not seen at all); an error given only as an alias's default parameter
          (`type Result<T, E = String> = ..`); a trait impl's associated error (`type Err =
          String` of `FromStr`, which is no `Result`: the lint misses it too); a binary whose
          `[[bin]] path` is another file (its root counts as a library's); a function a macro
          makes from tokens
  over    a trait of a crate's own named `Error` in a `Box<dyn ..>`; a type of a crate's own
          named `Result` with two or more parameters whose last is a string; a `Result` in a
          generic bound or a `where` clause (`F: Fn() -> Result<(), String>`), which the lint,
          reading the parameter's type `F`, does not see
  either  a `fn` in a `macro_rules!` body counts once, at the template, where the lint counts
          each function the macro makes, at its call
The lint is the definition; this list is the count the gate can hold before blaze checks
blaze's own crates, and it goes when the lint's baseline joins the gate.

A site in test code is not counted. Test code is, and nothing else is:
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
    declaring file. A `#[path]` is not followed, so such a module counts.
Everything else counts, `*_tests.rs` files a test module does not declare included.

A crate is the nearest directory above the file, under crates/, that holds a Cargo.toml, named
by its path under crates/ (`blaze-cargo`, `harness/harness-native`).
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use blaze_script::{
    TempDir, fail, git,
    input::{self, Fields, Input, InputError},
    read_text,
    regex::Regex,
    universal_newlines,
};

const SCRIPT: &str = "untyped";
/// The list, from the root.
const LIST: &str = "tools/ratchets/untyped-errors.txt";
const TEST_DIRS: [&str; 2] = ["tests", "benches"];
const TEST_FILES: [&str; 2] = ["tests.rs", "test.rs"];
/// The files whose out-of-line modules are beside them, not in a directory named for them.
const ROOT_FILES: [&str; 4] = ["lib.rs", "main.rs", "mod.rs", "build.rs"];
/// The directories whose files are each a binary's crate root (and so is a `main.rs` in a
/// directory under one).
const BIN_DIRS: [&str; 2] = ["bin", "examples"];
/// What every message about a crate over its row ends with.
const RULE: &str = "AGENTS.md errors-name-the-thing (rule 19): an error names the item it is about and the rule that \
                    refused it, in a type the caller can match on";
/// What to write instead of an untyped error, for a message.
const INSTEAD: &str = "return an error type of the function's module instead: an enum (or a struct) with a variant per \
                       way the function fails, each naming the thing it failed on (the path, the item, the key) and \
                       keeping the error it came from as its source, with a `Display` that says it in a sentence. \
                       crates/hir-def/src/nameres/frozen.rs's `Unreadable` is one: it names its crate and what was \
                       frozen, and its `Display` says why the bytes do not read; where many call sites wrap one \
                       cause, `snafu` (a workspace dependency) writes the variants and their context. Never \
                       `String`, `&str`, `Box<dyn Error>` or `anyhow`, which the caller can't match on";
/// What to write instead of an alias of a `Result` whose error is untyped, for a message.
const INSTEAD_ALIAS: &str = "give the module its own error type (an enum or a struct naming what failed and the thing it \
                             failed on, with `Display`; crates/hir-def/src/nameres/frozen.rs's `Unreadable` is one) \
                             and make it the alias's error (`type Result<T, E = Error> = std::result::Result<T, E>`), \
                             or drop the alias and write `Result<T, ThatError>`";
/// The list's first lines.
const HEADER: &str = "# The function signatures whose error is untyped, outside test code, by crate (its directory\n\
                      # under crates/): a `Result` whose error is `String`, `&str`, `Box<dyn Error>` or `anyhow`\n\
                      # in a signature's return or parameter types (`blaze::untyped_error`'s rule, read from the\n\
                      # text), and the type aliases of such a `Result`, counted where they are defined. Only\n\
                      # shrinks; a crate with no row is held to 0 (AGENTS.md errors-name-the-thing). Written by\n\
                      # scripts/src/bin/untyped.rs (`{\"command\": \"write\"}`), checked in CI.\n\
                      # crate\tstring\tstr\tboxed\tanyhow\taliases\n";

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

/// What a site is: the list's columns, in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A signature with a `Result` whose error is `String`.
    String,
    /// A signature with a `Result` whose error is `&str`.
    Str,
    /// A signature with a `Result` whose error is `Box<dyn Error>`.
    Boxed,
    /// A signature with `anyhow::Error` or `anyhow::Result<T>`.
    Anyhow,
    /// A `type` item aliasing a type that holds one of the four.
    Alias,
}

const KINDS: [Kind; 5] = [Kind::String, Kind::Str, Kind::Boxed, Kind::Anyhow, Kind::Alias];
/// The kinds a signature's error can be, by [`Kind::column`]: the first four columns.
const ERRORS: [Kind; 4] = [Kind::String, Kind::Str, Kind::Boxed, Kind::Anyhow];

impl Kind {
    /// Its column in a row, from 0.
    const fn column(self) -> usize {
        match self {
            Kind::String => 0,
            Kind::Str => 1,
            Kind::Boxed => 2,
            Kind::Anyhow => 3,
            Kind::Alias => 4,
        }
    }

    /// Its column's name in the header and in messages.
    const fn name(self) -> &'static str {
        match self {
            Kind::String => "string",
            Kind::Str => "str",
            Kind::Boxed => "boxed",
            Kind::Anyhow => "anyhow",
            Kind::Alias => "aliases",
        }
    }

    /// What its sites are, for a message: `<n> <what>`.
    const fn what(self) -> &'static str {
        match self {
            Kind::String => "function signatures whose error is a `String`",
            Kind::Str => "function signatures whose error is a `&str`",
            Kind::Boxed => "function signatures whose error is a `Box<dyn Error>`",
            Kind::Anyhow => "function signatures whose error is `anyhow`'s",
            Kind::Alias => "type aliases of a `Result` whose error is untyped",
        }
    }

    /// What to write instead, for a message.
    const fn instead(self) -> &'static str {
        match self {
            Kind::Alias => INSTEAD_ALIAS,
            _ => INSTEAD,
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

/// The list read wrong: where, and what is wrong there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListError {
    /// A line (from 1) that is not a row, a comment or blank.
    NotARow { line: usize, text: String },
    /// A crate's second row, at its line (from 1).
    TwoRows { line: usize, krate: String },
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListError::NotARow { line, text } => write!(
                f,
                "{LIST}:{line}: {text:?} is not `<crate><TAB><string><TAB><str><TAB><boxed><TAB><anyhow><TAB><aliases>`"
            ),
            ListError::TwoRows { line, krate } => write!(f, "{LIST}:{line}: `{krate}` has two rows"),
        }
    }
}

/// The base could not be read: which base, and what of it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BaseError {
    /// `base` names no commit of the checkout; `why` is git's message.
    NoCommit { base: String, why: String },
    /// A git command about `base` failed: `what` it was to do, and git's message.
    Git { base: String, what: &'static str, why: String },
    /// The base's list does not read.
    List { base: String, error: ListError },
    /// A hunk header of git's diff that is not one.
    Hunk { text: String },
}

impl fmt::Display for BaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaseError::NoCommit { base, why } => write!(f, "the base {base} is no commit here (fetch it): {why}"),
            BaseError::Git { base, what, why } => write!(f, "git could not {what} at the base {base}: {why}"),
            BaseError::List { base, error } => write!(f, "at the base {base}: {error}"),
            BaseError::Hunk { text } => write!(f, "git diff's hunk {text:?} is not `@@ -<line>[,<n>] +<line>[,<n>] @@`"),
        }
    }
}

impl std::error::Error for ListError {}
impl std::error::Error for BaseError {}

struct Patterns {
    cfg_test: Regex,
    /// `#[test]`, or an attribute whose path ends in the word `test` (`#[tokio::test]`).
    test_fn: Regex,
    inner_cfg_test: Regex,
    /// `mod <name>;`, attributes before it on the line allowed.
    module: Regex,
}

impl Patterns {
    fn new() -> Patterns {
        let re = |pattern: &str| Regex::new(pattern).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
        Patterns {
            cfg_test: re(r"\A\s*#\[cfg\((?:test|all\(test\b[^\]]*)\)\]"),
            // To the attribute's `]`, so that the item read after it starts after the attribute.
            test_fn: re(r"\A\s*#\[(?:\w+::)*test\b[^\]]*\]"),
            inner_cfg_test: re(r"\A\s*#!\[cfg\((?:test|all\(test\b[^\]]*)\)\]"),
            module: re(r"\A\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?(\w+)\s*;"),
        }
    }
}

/// What one file holds.
#[derive(Debug, Default, PartialEq, Eq)]
struct Scanned {
    /// The sites outside its test code (line from 1, what it is), in line order and then by
    /// column (a signature can be in more than one).
    sites: Vec<(usize, Kind)>,
    /// Each `mod <name>;` it declares, and whether test code declares it.
    mods: Vec<(String, bool)>,
    /// `#![cfg(test)]`: the whole file is test code.
    test: bool,
}

/// The sites of one file's text and the modules it declares out of line; `bin_root` when the
/// file is a binary's crate root, whose `main` module is not counted.
fn scan_file(p: &Patterns, text: &str, bin_root: bool) -> Scanned {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut test = vec![false; lines.len()];
    let mut out = Scanned::default();
    // The byte where line `at` starts.
    let mut start = 0;
    for (at, line) in lines.iter().enumerate() {
        if p.inner_cfg_test.is_match(line) {
            out.test = true;
        }
        if let Some(found) = p.cfg_test.find(line).or_else(|| p.test_fn.find(line)) {
            // The item runs from after the attribute to its `}` or `;`, on the line of that byte.
            let last = match item_end(text, start + found.end()) {
                Some(end) => at + text[start..end - 1].matches('\n').count(),
                None => at,
            };
            for it in &mut test[at..=last] {
                *it = true;
            }
        }
        if let Some(found) = p.module.captures(line) {
            out.mods.push((found[1].to_owned(), test[at]));
        }
        start += line.len() + 1;
    }
    if out.test {
        for it in &mut out.mods {
            it.1 = true;
        }
        return out;
    }
    out.sites = untyped_items(text, bin_root).into_iter().filter(|(line, _)| !test[line - 1]).collect();
    out
}

/// A `{` block open, as [`untyped_items`] tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    /// An `impl <Type>` block, or the block after any other `impl` (a body after `-> impl X`).
    Impl,
    /// An `impl <Trait> for <Type>` block: its methods' signatures are the trait's.
    TraitImpl,
    /// An inline `mod <name> { .. }`.
    Module,
    /// Any other.
    Other,
}

/// The function signatures and type aliases of `text` that hold an untyped error, each as the
/// line (from 1) of its `fn` or `type` and its kind, in text order (a signature in each column
/// it holds, in column order), test code included: [`scan_file`] takes it out. Comments,
/// strings and characters are skipped as [`item_end`] skips them. Not a method directly in an
/// `impl <Trait> for <Type>` block, not a function pointer type (`fn` with no name after it),
/// and, when `bin_root`, nothing outside every inline `mod` block.
fn untyped_items(text: &str, bin_root: bool) -> Vec<(usize, Kind)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    // Each `{` open, outermost first.
    let mut blocks: Vec<Block> = Vec::new();
    // From the word `impl` or `mod` to the `{` or `;` after it: the block it opens.
    let mut header: Option<Block> = None;
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
                blocks.push(header.take().unwrap_or(Block::Other));
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
                // In a binary's `main` module: outside every inline `mod`.
                let main_module = bin_root && !blocks.contains(&Block::Module);
                match &text[at..end] {
                    "impl" => header = Some(Block::Impl),
                    "for" if header == Some(Block::Impl) && !text[end..].trim_start().starts_with('<') => {
                        header = Some(Block::TraitImpl);
                    }
                    "mod" => header = Some(Block::Module),
                    "fn" if blocks.last() != Some(&Block::TraitImpl) && !main_module => {
                        let rest = text[end..].trim_start();
                        let rest = rest.strip_prefix("r#").unwrap_or(rest);
                        let named = rest.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
                        if named && let Some(close) = signature_end(text, end) {
                            let found = untyped_errors(&text[end..close]);
                            if found.contains(&true) {
                                line += text[counted..at].matches('\n').count();
                                counted = at;
                                out.extend(ERRORS.iter().filter(|kind| found[kind.column()]).map(|kind| (line, *kind)));
                            }
                        }
                    }
                    "type" if !main_module => {
                        if let Some(close) = signature_end(text, end)
                            && let Some(eq) = alias_eq(&text[end..close])
                            && untyped_errors(&text[end + eq + 1..close]).contains(&true)
                        {
                            line += text[counted..at].matches('\n').count();
                            counted = at;
                            out.push((line, Kind::Alias));
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

/// Which untyped errors the signature text `signature` holds, by [`Kind::column`] of the first
/// four: a `Result<..>` (any path, any depth) whose last top-level type argument is one
/// ([`error_kind`]), `anyhow::Error`, or `anyhow::Result` with no second argument. Comments,
/// strings and characters are skipped.
fn untyped_errors(signature: &str) -> [bool; 4] {
    let bytes = signature.as_bytes();
    let mut found = [false; 4];
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
                let after = signature[end..].trim_start();
                match &signature[at..end] {
                    "anyhow" => {
                        if let Some(rest) = after.strip_prefix("::") {
                            let rest = rest.trim_start();
                            let len = ident_len(rest.as_bytes());
                            let then = rest[len..].trim_start();
                            let anyhow = match &rest[..len] {
                                "Error" => true,
                                // `anyhow::Result<T, E>` with its own error is that error's,
                                // which the `Result` word after reads.
                                "Result" => !then.starts_with('<') || type_args(then).is_some_and(|args| args.len() == 1),
                                _ => false,
                            };
                            if anyhow {
                                found[Kind::Anyhow.column()] = true;
                            }
                        }
                    }
                    "Result" if after.starts_with('<') => {
                        if let Some(args) = type_args(after)
                            && args.len() >= 2
                            && let Some(kind) = args.last().copied().and_then(error_kind)
                        {
                            found[kind.column()] = true;
                        }
                    }
                    _ => {}
                }
                end
            }
            b'0'..=b'9' => at + ident_len(&bytes[at..]),
            _ => at + 1,
        };
    }
    found
}

/// The kind of untyped error the type text `error` is, if it is one: `&str` (any lifetime,
/// `mut` or not), `String` (any path, any `<..>`), `anyhow::Error`, or `Box<dyn ..>` with a
/// trait named `Error`.
fn error_kind(error: &str) -> Option<Kind> {
    let error = error.trim();
    if let Some(rest) = error.strip_prefix('&') {
        let rest = rest.trim_start();
        let rest = match rest.strip_prefix('\'') {
            Some(lifetime) => &lifetime[ident_len(lifetime.as_bytes())..],
            None => rest,
        };
        let rest = rest.trim_start();
        let rest = rest.strip_prefix("mut").filter(|it| it.starts_with(char::is_whitespace)).unwrap_or(rest);
        return (rest.trim() == "str").then_some(Kind::Str);
    }
    let (path, args) = match error.find('<') {
        Some(at) => (&error[..at], Some(&error[at..])),
        None => (error, None),
    };
    match last_segment(path) {
        "String" => Some(Kind::String),
        "Error" if path.split("::").any(|it| it.trim() == "anyhow") => Some(Kind::Anyhow),
        "Box" => {
            let first = *type_args(args?)?.first()?;
            let traits = first.strip_prefix("dyn")?;
            let named = traits.starts_with(char::is_whitespace)
                && traits.split('+').any(|it| last_segment(it.split('<').next().unwrap_or(it)) == "Error");
            named.then_some(Kind::Boxed)
        }
        _ => None,
    }
}

/// The last `::` segment of the path text `path`, trimmed.
fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path).trim()
}

/// The top-level arguments of the `<..>` that `text` starts with, each trimmed: commas count
/// only at its own level (`(`, `[` and `<` nest; the `>` of `->` closes none), and a trailing
/// comma adds none. `None` when it does not close.
fn type_args(text: &str) -> Option<Vec<&str>> {
    let bytes = text.as_bytes();
    let mut depth: usize = 0;
    let mut args = Vec::new();
    let mut from = 1;
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' | b')' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let tail = text[from..at].trim();
                    if !tail.is_empty() {
                        args.push(tail);
                    }
                    return Some(args);
                }
            }
            b',' if depth == 1 => {
                args.push(text[from..at].trim());
                from = at + 1;
            }
            _ => {}
        }
    }
    None
}

/// The byte of the `=` of a `type` item whose text after the word `type` is `item` (to its `;`,
/// not included): the first outside brackets (a generic parameter's default, `E = Error`, is
/// inside `<..>`). `None` for a `type` with none (a trait's declaration, `type Error;`).
fn alias_eq(item: &str) -> Option<usize> {
    let bytes = item.as_bytes();
    let mut depth: usize = 0;
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' | b')' | b']' | b'}' => depth = depth.checked_sub(1)?,
            b'=' if depth == 0 && !matches!(bytes.get(at + 1), Some(b'=' | b'>')) => return Some(at),
            _ => {}
        }
    }
    None
}

// The scanning below is allocs.rs's (`item_end`, `signature_end`, `comment_end`, `string_end`,
// `quote_end`, `raw_end`; `word_end` reads a raw identifier whole): one shared module once
// allocs.rs's open change is in.

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

/// The byte of the `{` or `;` that ends the signature (or `type` item) whose text after its
/// keyword starts at byte `from` of `text`: the first outside brackets (`(`, `[`, `<`, and `{`
/// of a const argument; the `>` of `->` closes none); `None` when a bracket closes that it did
/// not open, or the text ends.
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

/// The length of the identifier (or number) `bytes` starts with: ASCII letters, digits and `_`.
fn ident_len(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count()
}

/// The byte after the word at `at`, after the literal it prefixes (`r#"..."#`, `b"..."`,
/// `br"..."`, `c"..."`, `b'x'`), or after the raw identifier it starts (`r#type`, read whole so
/// that its word is no keyword).
fn word_end(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    let end = at + ident_len(&bytes[at..]);
    match (&text[at..end], bytes.get(end).copied()) {
        ("r" | "br" | "cr", Some(b'#' | b'"')) => raw_end(bytes, end),
        ("b" | "c", Some(b'"')) => string_end(bytes, end + 1),
        ("b", Some(b'\'')) => quote_end(text, end),
        _ => end,
    }
}

/// The byte after the raw string whose `#`s or `"` start at `from`; for a raw identifier
/// (`r#type`), the byte after its identifier.
fn raw_end(bytes: &[u8], from: usize) -> usize {
    let hashes = bytes[from..].iter().take_while(|b| **b == b'#').count();
    let open = from + hashes;
    if bytes.get(open) != Some(&b'"') {
        return open + ident_len(&bytes[open..]);
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

/// Whether the file `parts` is test code by its path.
fn test_path(parts: &[String]) -> bool {
    let (name, dirs) = parts.split_last().map_or(("", &[][..]), |(name, dirs)| (name.as_str(), dirs));
    dirs.iter().any(|part| TEST_DIRS.contains(&part.as_str())) || TEST_FILES.contains(&name)
}

/// Whether the file `parts` is a build script: `build.rs`, not under a `src` directory. The lint
/// checks no build script target.
fn build_script(parts: &[String]) -> bool {
    matches!(parts, [.., dir, name] if name == "build.rs" && dir != "src")
}

/// Whether the file `parts` is a binary's crate root: `src/main.rs`, a file of a `bin` or
/// `examples` directory, or a `main.rs` in a directory under one.
fn bin_root(parts: &[String]) -> bool {
    match parts {
        [.., up, _, name] if name == "main.rs" && BIN_DIRS.contains(&up.as_str()) => true,
        [.., dir, name] if name == "main.rs" && dir == "src" => true,
        [.., dir, name] => name.ends_with(".rs") && BIN_DIRS.contains(&dir.as_str()),
        _ => false,
    }
}

/// The two files the module `name`, declared out of line in the file `parts`, can be.
fn module_files(parts: &[String], name: &str) -> [String; 2] {
    let Some((file, dirs)) = parts.split_last() else { return [String::new(), String::new()] };
    let mut base: Vec<&str> = dirs.iter().map(String::as_str).collect();
    let root = ROOT_FILES.contains(&file.as_str()) || dirs.last().is_some_and(|it| it == "bin");
    if !root {
        base.push(file.strip_suffix(".rs").unwrap_or(file));
    }
    let base = base.join("/");
    [format!("{base}/{name}.rs"), format!("{base}/{name}/mod.rs")]
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

/// Each crate's sites outside test code and build scripts, in file and line order; a crate
/// with none is absent.
fn count(p: &Patterns, root: &Path) -> BTreeMap<String, Vec<Site>> {
    let files = rust_files(root);
    let known: BTreeSet<String> = files.iter().map(|it| it.join("/")).collect();
    let mut scanned: Vec<(Vec<String>, Scanned)> = Vec::new();
    // The files not counted: test code and build scripts, and the modules they declare.
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for parts in files {
        let path = parts.join("/");
        let full = parts.iter().fold(root.to_path_buf(), |path, part| path.join(part));
        // `read_text(errors="replace")`: what is not UTF-8 becomes U+FFFD.
        let Ok(bytes) = std::fs::read(&full) else { fail(SCRIPT, &format!("{} could not be read", full.display())) };
        let text = universal_newlines(String::from_utf8_lossy(&bytes).into_owned());
        let found = scan_file(p, &text, bin_root(&parts));
        if test_path(&parts) || build_script(&parts) || found.test {
            skipped.insert(path.clone());
        }
        for (name, in_test) in &found.mods {
            for file in module_files(&parts, name) {
                if !known.contains(&file) {
                    continue;
                }
                if *in_test {
                    skipped.insert(file.clone());
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
    let mut manifests: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    let mut out: BTreeMap<String, Vec<Site>> = BTreeMap::new();
    for (parts, found) in scanned {
        let path = parts.join("/");
        if found.sites.is_empty() || skipped.contains(&path) {
            continue;
        }
        let krate = crate_of(root, &parts, &mut manifests);
        out.entry(krate)
            .or_default()
            .extend(found.sites.iter().map(|(line, kind)| Site { path: path.clone(), line: *line, kind: *kind }));
    }
    out
}

/// The list as `write` writes it: the header, then `<crate><TAB><string><TAB><str><TAB><boxed>
/// <TAB><anyhow><TAB><aliases>` for each crate with a site, sorted.
fn render(counts: &BTreeMap<String, Vec<Site>>) -> String {
    let mut out = HEADER.to_owned();
    for (krate, sites) in counts.iter().filter(|(_, sites)| !sites.is_empty()) {
        let [string, borrowed, boxed, anyhow, aliases] = tally(sites);
        out.push_str(&format!("{krate}\t{string}\t{borrowed}\t{boxed}\t{anyhow}\t{aliases}\n"));
    }
    out
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
            [krate, rest @ ..] if !krate.is_empty() && rest.len() == 5 => {
                let mut n: Row = [0; 5];
                let read = rest.iter().zip(&mut n).all(|(field, slot)| field.parse::<usize>().map(|it| *slot = it).is_ok());
                read.then_some((*krate, n))
            }
            _ => None,
        };
        let Some((krate, n)) = row else {
            return Err(ListError::NotARow { line: at + 1, text: line.to_owned() });
        };
        if rows.insert(krate.to_owned(), n).is_some() {
            return Err(ListError::TwoRows { line: at + 1, krate: krate.to_owned() });
        }
    }
    Ok(rows)
}

/// The lines each file has that `base` did not, from `git diff --unified=0`'s hunks: by path
/// from the root, the line numbers in the tree.
fn added_lines(diff: &str) -> Result<BTreeMap<String, BTreeSet<usize>>, BaseError> {
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
            return Err(BaseError::Hunk { text: line.to_owned() });
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
fn at_base(root: &Path, base: &str) -> Result<(Option<BTreeMap<String, Row>>, BTreeMap<String, BTreeSet<usize>>), BaseError> {
    let root_arg = root.to_string_lossy();
    let dir = root_arg.as_ref();
    let commit = format!("{base}^{{commit}}");
    git(root, &["-C", dir, "rev-parse", "--verify", "--quiet", commit.as_str()])
        .map_err(|why| BaseError::NoCommit { base: base.to_owned(), why })?;
    let failed = |what: &'static str| move |why: String| BaseError::Git { base: base.to_owned(), what, why };
    let listed = git(root, &["-C", dir, "ls-tree", "--name-only", base, "--", LIST]).map_err(failed("list its tree"))?;
    let rows = if listed.trim().is_empty() {
        None
    } else {
        let spec = format!("{base}:{LIST}");
        let text = git(root, &["-C", dir, "show", spec.as_str()]).map_err(failed("show the list"))?;
        Some(parse_rows(&text).map_err(|error| BaseError::List { base: base.to_owned(), error })?)
    };
    let diff = git(root, &["-C", dir, "diff", "--no-color", "--no-ext-diff", "--no-renames", "--unified=0", base, "--", "crates"])
        .map_err(failed("diff the tree"))?;
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
            if now > held {
                let shown = match row {
                    Some(_) => format!("its row's `{column}` {held}"),
                    None => "0, as a crate with no row is held".to_owned(),
                };
                let new = match added {
                    Some(added) => {
                        let new: Vec<String> = sites
                            .iter()
                            .filter(|it| it.kind == kind && added.get(&it.path).is_some_and(|lines| lines.contains(&it.line)))
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
                out.push(format!("`{krate}` has {now} {what} outside tests, over {shown} in {LIST} ({new}): {} ({RULE})", kind.instead()));
            } else if now < held {
                let lower = match now_row {
                    [0, 0, 0, 0, 0] => "remove the row".to_owned(),
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
            let was = base.get(krate).copied().unwrap_or([0; 5]);
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
    let [string, borrowed, boxed, anyhow, aliases] = total(counts);
    format!(
        "{string} signatures with a `String` error, {borrowed} with a `&str`, {boxed} with a `Box<dyn Error>`, {anyhow} with \
         `anyhow`'s and {aliases} aliases of such a `Result`, outside tests, in {} crates",
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
    if wrong.is_empty() && listed != text {
        wrong.push(format!(
            "{LIST} is not as command `write` writes it (its header, then a sorted `<crate><TAB><string><TAB><str><TAB><boxed><TAB><anyhow><TAB><aliases>` row per crate with a site): run it"
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

/// One file's sites, for a selftest's expectation.
fn sites_of(path: &str, lines: &[(usize, Kind)]) -> Vec<Site> {
    lines.iter().map(|(line, kind)| Site { path: path.to_owned(), line: *line, kind: *kind }).collect()
}

/// The selftest's file of crate `a`. Line by line, what each item is:
/// - 2-6, `spans`: a signature over three lines, `String` error: counted at 2, its `fn`;
/// - 7, `last`: `Result<Vec<(String, u32)>, String>`, the error the last top-level argument:
///   one `string`;
/// - 8, `typed`: `Result<String, Error>`, a typed error: not counted;
/// - 9, `nested`: `Option<Result<(), String>>`: counted;
/// - 10-15, `where_after`: a `where` clause after the return type: counted at 10;
/// - 16, `text`: `Result<(), &'static str>`: `str`;
/// - 17, `boxed`: `Box<dyn std::error::Error + Send + Sync>`: `boxed`;
/// - 18, `any`: an `anyhow::Result<u8>` parameter: `anyhow`;
/// - 19, `Fallible`: an alias of `Result<T, String>`: `aliases`, where it is defined;
/// - 21, `load` declared in `trait Load`: counted;
/// - 24, `load` in `impl Load for Disk`, its trait's signature: not counted;
/// - 26-27, `inherent` in `impl<T> Disk<T> where T: for<'a> ..` (the bound's `for` is no trait
///   impl's): its `fn() -> Result<(), String>` parameter is counted;
/// - 29, a closure's `-> Result<u8, String>`: no signature;
/// - 30, `raw`: `r#type` is a parameter, no alias; `alloc::string::String`: `string`;
/// - 31, `quoted`: a signature in a string and in a comment: not counted;
/// - 34, `helper` in `#[cfg(test)] mod tests`: test code;
/// - 37, `a_test` after `#[test]`: test code;
/// - 38, `both`: a `String` and a `&str`: once in each column;
/// - 39, `Plain`: an alias of a typed `Result`: not counted;
/// - 40, `mapped`: `blaze_mem::String<A>` is a string still: counted.
const FIXTURE: &str = r#"use std::fmt;
pub fn spans(
    a: u8,
) -> Result<u8, String> {
    Ok(a)
}
pub fn last(v: u8) -> Result<Vec<(String, u32)>, String> { loop {} }
pub fn typed() -> Result<String, Error> { loop {} }
pub fn nested() -> Option<Result<(), String>> { None }
pub fn where_after<T>(t: T) -> Result<T, String>
where
    T: Clone,
{
    Ok(t)
}
pub fn text() -> Result<(), &'static str> { Ok(()) }
pub fn boxed() -> Result<(), Box<dyn std::error::Error + Send + Sync>> { loop {} }
pub fn any(r: anyhow::Result<u8>) -> u8 { loop {} }
pub type Fallible<T> = std::result::Result<T, String>;
pub trait Load {
    fn load(&self) -> Result<u8, String>;
}
impl Load for Disk {
    fn load(&self) -> Result<u8, String> { loop {} }
}
impl<T> Disk<T> where T: for<'a> Fn(&'a u8) {
    fn inherent(&self, f: fn() -> Result<(), String>) {}
}
fn closure() { let _ = |x: u8| -> Result<u8, String> { Ok(x) }; }
fn raw(r#type: u8) -> Result<u8, alloc::string::String> { loop {} }
fn quoted() -> &'static str { "fn x() -> Result<(), String> {" } // fn y() -> Result<(), String> {}
#[cfg(test)]
mod tests {
    fn helper() -> Result<(), String> { Ok(()) }
}
#[test]
fn a_test() -> Result<(), String> { Ok(()) }
fn both(a: Result<u8, String>) -> Result<u8, &str> { a }
pub type Plain = Result<u8, Error>;
fn mapped() -> Result<u8, blaze_mem::String<A>> { loop {} }
"#;

/// The sites of [`FIXTURE`] outside its test code, by line and column.
const FIXTURE_SITES: [(usize, Kind); 14] = [
    (2, Kind::String),
    (7, Kind::String),
    (9, Kind::String),
    (10, Kind::String),
    (16, Kind::Str),
    (17, Kind::Boxed),
    (18, Kind::Anyhow),
    (19, Kind::Alias),
    (21, Kind::String),
    (27, Kind::String),
    (30, Kind::String),
    (38, Kind::String),
    (38, Kind::Str),
    (40, Kind::String),
];

/// A binary's crate root: its `main` module is not counted, a module inside it is.
const BIN_ROOT: &str = "fn main() -> anyhow::Result<()> { loop {} }
type R = anyhow::Result<()>;
mod cli {
    pub fn go() -> anyhow::Result<()> { loop {} }
    pub type R = Result<(), String>;
}
";

/// Two lists of sites for a selftest's message: what was found, what was wanted, and the
/// difference (`extra`: found and not wanted; `missing`: wanted and not found).
fn compared(found: &[(usize, Kind)], want: &[(usize, Kind)]) -> String {
    let extra: Vec<&(usize, Kind)> = found.iter().filter(|it| !want.contains(it)).collect();
    let missing: Vec<&(usize, Kind)> = want.iter().filter(|it| !found.contains(it)).collect();
    format!("\n  found   {found:?}\n  wanted  {want:?}\n  extra   {extra:?}\n  missing {missing:?}")
}

/// The lines of [`scan_file`]'s sites of `text` (not a binary's root) that are of `kind`.
fn lines_of(p: &Patterns, text: &str, kind: Kind) -> Vec<usize> {
    scan_file(p, text, false).sites.iter().filter(|(_, it)| *it == kind).map(|(line, _)| *line).collect()
}

fn selftest(p: &Patterns) {
    // What an error is: the last top-level argument of a `Result`, at any depth.
    let errors = untyped_errors;
    expect(errors("f() -> Result<Vec<(String, u32)>, String>") == [true, false, false, false], "the error is the last argument");
    expect(errors("f() -> Result<String, Error>") == [false; 4], "a `String` that is no error");
    expect(errors("f(r: io::Result<u8>, s: fmt::Result) -> MyString") == [false; 4], "one argument, or none, holds no error");
    expect(errors("f() -> anyhow::Result<u8, Typed>") == [false; 4], "`anyhow::Result` with a typed error");
    expect(errors("f() -> Result<u8, anyhow::Error>") == [false, false, false, true], "`anyhow::Error` as the error");
    expect(errors("f() -> Result<(), &'a mut str>") == [false, true, false, false], "`&'a mut str`");
    expect(errors("f() -> Result<(), Box<dyn Error + Send + Sync + 'static>>") == [false, false, true, false], "boxed, auto traits");
    expect(errors("f() -> Result<(), Box<dyn Fn() -> Result<u8, Typed>>>") == [false; 4], "a boxed closure is no boxed error");
    expect(errors("f(cb: impl Fn() -> Result<(), String>)") == [true, false, false, false], "in a parameter's type");
    expect(errors("f(x: Result<u8, String,>)") == [true, false, false, false], "a trailing comma adds no argument");
    expect(errors("f() /* -> Result<(), String> */ -> u8") == [false; 4], "a comment holds no type");
    let item = " Result<T, E = Error> = std::result::Result<T, E>";
    expect(
        alias_eq(item).is_some_and(|eq| item[eq + 1..].trim() == "std::result::Result<T, E>"),
        "a default's `=` is no alias's",
    );
    expect(alias_eq(" Error").is_none(), "a declaration has no `=`");

    // One file: every case of [`FIXTURE`], test code out.
    let all: Vec<(usize, Kind)> = untyped_items(FIXTURE, false);
    let mut want: Vec<(usize, Kind)> = FIXTURE_SITES.to_vec();
    want.extend([(34, Kind::String), (37, Kind::String)]);
    want.sort_by_key(|(line, kind)| (*line, kind.column()));
    expect(all == want, &format!("the fixture's items, test code in: {}", compared(&all, &want)));
    let found = scan_file(p, FIXTURE, false);
    expect(found.sites == FIXTURE_SITES, &format!("the fixture's sites outside test code: {}", compared(&found.sites, &FIXTURE_SITES)));
    // `#[test]`'s item starts after its `]`; with arguments too.
    let attributed = "#[tokio::test(flavor = \"x\")]\nfn t() -> Result<(), String> { loop {} }\n";
    expect(scan_file(p, attributed, false).sites.is_empty(), "a test attribute with arguments covers its function");
    // Planted: the trait impl made inherent, its `load` is counted, at line 24.
    let lines = lines_of(p, &FIXTURE.replace("impl Load for Disk {", "impl Disk {"), Kind::String);
    expect(lines.contains(&24), &format!("an inherent impl's method is counted: {lines:?}"));
    // Planted: the test module's attribute, then the test's, removed: each is a site.
    let lines = lines_of(p, &FIXTURE.replace("#[cfg(test)]\n", "\n"), Kind::String);
    expect(lines.contains(&34), &format!("`helper` outside test code is a site: {lines:?}"));
    let lines = lines_of(p, &FIXTURE.replace("#[test]\n", "\n"), Kind::String);
    expect(lines.contains(&37), &format!("`a_test` without `#[test]` is a site: {lines:?}"));
    // Planted: the bound's `for<'a>` made a trait's `for`, `inherent` is a trait impl's.
    let lines = lines_of(p, &FIXTURE.replace("where T: for<'a> Fn(&'a u8) {", "for Holder {"), Kind::String);
    expect(!lines.contains(&27), &format!("a trait impl's method is not counted: {lines:?}"));
    // A binary's root: only what is in an inline module counts.
    let bin = scan_file(p, BIN_ROOT, true);
    expect(bin.sites == [(4, Kind::Anyhow), (5, Kind::Alias)], &format!("a binary's root: {}", compared(&bin.sites, &[(4, Kind::Anyhow), (5, Kind::Alias)])));
    let lib = scan_file(p, BIN_ROOT, false);
    expect(lib.sites.len() == 4, &format!("the same file as a library's: {:?}", lib.sites));
    let parts = |path: &str| -> Vec<String> { path.split('/').map(str::to_owned).collect() };
    expect(
        bin_root(&parts("crates/a/src/main.rs"))
            && bin_root(&parts("crates/a/src/bin/x.rs"))
            && bin_root(&parts("crates/a/src/bin/x/main.rs"))
            && bin_root(&parts("crates/a/examples/e.rs"))
            && !bin_root(&parts("crates/a/src/lib.rs"))
            && !bin_root(&parts("crates/a/src/bin/x/util.rs"))
            && !bin_root(&parts("crates/a/src/cli/main.rs")),
        "the binaries' crate roots",
    );
    expect(build_script(&parts("crates/a/build.rs")) && !build_script(&parts("crates/a/src/build.rs")), "a build script");

    // A tree: crates, a binary's root, test files by path and by declaration, a build script,
    // a nested crate.
    let tmp = TempDir::new("untyped-selftest-").unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    let string_fn = "pub fn f() -> Result<(), String> { Ok(()) }\n";
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    put("crates/a/src/lib.rs", FIXTURE);
    put("crates/a/src/live.rs", string_fn);
    put("crates/a/src/main.rs", BIN_ROOT);
    put("crates/a/src/x.rs", "#[cfg(test)]\nmod t;\n");
    put("crates/a/src/x/t.rs", string_fn);
    put("crates/a/build.rs", string_fn);
    put("crates/a/tests/it.rs", string_fn);
    put("crates/a/target/debug/build/out.rs", string_fn);
    put("crates/b/Cargo.toml", "[workspace]\n");
    put("crates/b/c/Cargo.toml", "[package]\nname = \"c\"\n");
    put("crates/b/c/src/lib.rs", "pub fn q() -> Result<(), Box<dyn Error>> { loop {} }\n");
    let counts = count(p, tmp.path());
    let mut a = sites_of("crates/a/src/lib.rs", &FIXTURE_SITES);
    a.extend(sites_of("crates/a/src/live.rs", &[(1, Kind::String)]));
    a.extend(sites_of("crates/a/src/main.rs", &[(4, Kind::Anyhow), (5, Kind::Alias)]));
    let want = BTreeMap::from([("a".to_owned(), a), ("b/c".to_owned(), sites_of("crates/b/c/src/lib.rs", &[(1, Kind::Boxed)]))]);
    expect(counts == want, &format!("the tree's sites: {}", shown(&counts)));
    expect(total(&counts) == [10, 2, 2, 2, 2], &format!("the totals: {:?}", total(&counts)));

    // The ratchet. As recorded, nothing is wrong, and the list reads back as it was written.
    let rows = parse_rows(&render(&counts)).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    expect(
        rows == BTreeMap::from([("a".to_owned(), [10, 2, 1, 2, 2]), ("b/c".to_owned(), [0, 0, 1, 0, 0])]),
        &format!("the rows read back: {rows:?}"),
    );
    expect(problems(&counts, &rows, Some(&rows), None).is_empty(), "the tree as recorded is right");
    // Planted: one `String` signature added. Named by its crate, its column, its place (given
    // the base's diff, which also added the `&str` on that line: not named, another column),
    // what to write instead and the rule.
    let lib = "crates/a/src/lib.rs";
    let mut fewer = rows.clone();
    fewer.insert("a".to_owned(), [9, 2, 1, 2, 2]);
    let added = BTreeMap::from([(lib.to_owned(), BTreeSet::from([38]))]);
    let said = problems(&counts, &fewer, None, Some(&added));
    expect(
        said.len() == 1
            && said[0].contains("`a` has 10 function signatures whose error is a `String`")
            && said[0].contains("over its row's `string` 9")
            && said[0].contains("new: crates/a/src/lib.rs:38)")
            && said[0].contains("an enum (or a struct)")
            && said[0].contains("`Unreadable`")
            && said[0].contains("errors-name-the-thing"),
        &format!("one signature added is caught and named: {said:?}"),
    );
    // Planted: an alias added; the column and its own advice are named.
    let mut fewer_aliases = rows.clone();
    fewer_aliases.insert("a".to_owned(), [10, 2, 1, 2, 1]);
    let said = problems(&counts, &fewer_aliases, None, None);
    expect(
        said.len() == 1 && said[0].contains("`a` has 2 type aliases") && said[0].contains("make it the alias's error"),
        &format!("one alias added is caught: {said:?}"),
    );
    // Planted: one site removed. Its column must come down; a crate with none left goes.
    let mut more = rows.clone();
    more.insert("b/c".to_owned(), [0, 0, 2, 0, 0]);
    let said = problems(&counts, &more, None, None);
    expect(
        said.len() == 1 && said[0].contains("`b/c`") && said[0].contains("lower its `boxed` to 1"),
        &format!("one site removed is caught: {said:?}"),
    );
    let mut gone = rows.clone();
    gone.insert("old".to_owned(), [1, 0, 0, 0, 0]);
    let said = problems(&counts, &gone, None, None);
    expect(said.len() == 1 && said[0].contains("`old`") && said[0].contains("remove the row"), &format!("a crate gone: {said:?}"));
    // Planted: a crate with no row is held to 0; a row raised above the base's is caught even
    // where the tree matches it.
    let said = problems(&counts, &BTreeMap::from([("a".to_owned(), [10, 2, 1, 2, 2])]), None, None);
    expect(said.len() == 1 && said[0].contains("`b/c` has 1") && said[0].contains("held"), &format!("a new crate starts at 0: {said:?}"));
    let said = problems(&counts, &rows, Some(&fewer), None);
    expect(said.len() == 1 && said[0].contains("the row `a`'s `string` is 10 and was 9 at the base"), &format!("a row raised: {said:?}"));

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
    expect(added_lines("@@ nonsense\n").is_err_and(|e| matches!(e, BaseError::Hunk { .. })), "a hunk that is none is refused");
    expect(
        parse_rows("a\t1\t0\t0\t0\t0\nb 2\n").is_err_and(|e| e == ListError::NotARow { line: 2, text: "b 2".to_owned() })
            && parse_rows("a\t1\t0\t0\t0\t0\na\t2\t0\t0\t0\t0\n").is_err_and(|e| matches!(e, ListError::TwoRows { line: 2, .. }))
            && parse_rows("a\t1\t0\t0\t0\n").is_err()
            && parse_rows("a\t1\t0\t0\t0\tx\n").is_err()
            && parse_rows("\t1\t0\t0\t0\t0\n").is_err(),
        "a bad row is refused, naming its line",
    );
    println!("{SCRIPT} selftest: ok");
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

    /// The fixture's cases, each with its control: what is counted and what is not.
    #[test]
    fn untyped_signatures_are_counted() {
        let p = Patterns::new();
        assert_eq!(scan_file(&p, FIXTURE, false).sites, FIXTURE_SITES);
        // Multi-line: the site is the `fn`'s line, 1, not the `{`'s, 3.
        assert_eq!(untyped_items("pub fn spans(\n    a: u8,\n) -> Result<u8, String> {\n}\n", false), [(1, Kind::String)]);
        assert!(untyped_items("pub fn spans(\n    a: u8,\n) -> Result<u8, Typed> {\n}\n", false).is_empty());
        // A trait's declaration counts; its impl's method does not; an inherent one does.
        let decl = "fn load(&self) -> Result<u8, &'static str>;";
        assert_eq!(untyped_items(&format!("trait Load {{\n    {decl}\n}}\n"), false), [(2, Kind::Str)]);
        assert!(untyped_items(&format!("impl Load for Disk {{\n    {decl}\n}}\n"), false).is_empty());
        assert_eq!(untyped_items(&format!("impl Disk {{\n    {decl}\n}}\n"), false), [(2, Kind::Str)]);
        // An alias counts where it is defined, an associated type too; a declaration does not.
        assert_eq!(untyped_items("type R<T> = Result<T, String>;\n", false), [(1, Kind::Alias)]);
        assert_eq!(untyped_items("impl I for X {\n    type Item = Result<u8, String>;\n}\n", false), [(2, Kind::Alias)]);
        assert!(untyped_items("trait T {\n    type Error;\n}\ntype R<T, E = Typed> = Result<T, E>;\n", false).is_empty());
        // Test code is not a site; the same file without its attribute is.
        let in_test = "#[cfg(test)]\nmod tests {\n    fn helper() -> Result<(), String> { loop {} }\n}\n";
        assert!(scan_file(&p, in_test, false).sites.is_empty());
        assert_eq!(scan_file(&p, &in_test.replace("#[cfg(test)]\n", "\n"), false).sites.len(), 1);
    }

    /// A rise over the crate's row is refused, naming the site the base's diff added; the tree
    /// at its row is not.
    #[test]
    fn a_rise_is_refused_with_its_site() {
        let sites = [
            Site { path: "crates/s/src/lib.rs".to_owned(), line: 3, kind: Kind::String },
            Site { path: "crates/s/src/lib.rs".to_owned(), line: 9, kind: Kind::String },
        ];
        let counts = BTreeMap::from([("s".to_owned(), sites.to_vec())]);
        let at_row = BTreeMap::from([("s".to_owned(), [2, 0, 0, 0, 0])]);
        assert!(problems(&counts, &at_row, Some(&at_row), None).is_empty());
        let under = BTreeMap::from([("s".to_owned(), [1, 0, 0, 0, 0])]);
        let added = BTreeMap::from([("crates/s/src/lib.rs".to_owned(), BTreeSet::from([9]))]);
        let said = problems(&counts, &under, None, Some(&added));
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("over its row's `string` 1") && said[0].contains("new: crates/s/src/lib.rs:9)"), "{said:?}");
    }
}
