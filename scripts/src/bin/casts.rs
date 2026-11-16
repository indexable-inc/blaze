//! casts: the integer `as` casts only go down, per crate (AGENTS.md correct-by-construction,
//! rule 76; type-then-lint-then-rule, rule 73). `DOC` below is what its usage error prints; it
//! says what a cast is, what the text scan misses, which code is a test's, and the rule.
//!
//!     blaze run -q -p scripts --bin casts -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin casts -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin casts -- selftest
//!
//! Why (2026-11-15): `x as u32` is one of five conversions (widen, narrow and hope, truncate on
//! purpose, reinterpret the sign, or a mix), and the reader cannot tell which; a narrowing that
//! drops bits it should not is silent. crates/blaze-int names each (after Zig's `@intCast`,
//! `@truncate`): `widen_to` only where nothing is lost, `narrow` checked with a typed error,
//! `narrow_or_stop` where failure is a bug, `truncate_to` and `saturate` on purpose, and
//! `core`'s `cast_signed`/`cast_unsigned`. This holds the count of what is left from the text,
//! with no compiler, as `allocs` and `untyped` hold theirs, until a blaze lint with a fix
//! (`blaze::int_cast`, rule 73) replaces it.
//!
//! The scanning is a copy of untyped.rs's (`item_end`, `comment_end`, `string_end`,
//! `quote_end`, `word_end`, `raw_end`, `test_path`, `module_files`, `rust_files`, `crate_of`,
//! `added_lines`, the file walk of `count`), the third copy of one scanner: it belongs in one
//! shared module of blaze-script (AGENTS.md one-concept-one-home), which a change of its own
//! makes for all three.

/// What the usage error prints.
const DOC: &str = r#"casts: the integer `as` casts only go down, per crate (AGENTS.md correct-by-construction, rule 76).

    blaze run -q -p scripts --bin casts -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin casts -- '{"command": "write"}'
    blaze run -q -p scripts --bin casts -- selftest

    command   check   each crate's count equals its row's in the list: one over its row is
                      refused (its new casts named given `base`), one under it must be lowered
                      with `write`; a crate with no row is held to 0, so a new crate starts at
                      0; with `base`, no row is above the base's (CI)
              write   write the list from the tree as it is
    root      the checkout (".")
    base      the commit the change is against, or null: its list (`git show`) holds every
              row down, and the lines added since it (`git diff --unified=0`) name a crate's
              new casts

The list's row is `<crate><TAB><casts>`, of `.rs` files under crates/ (a directory named
`target` is not read).

A cast is the word `as` followed (after spaces or line breaks) by the name of a primitive
integer type, `u8` to `u128`, `usize`, `i8` to `i128` or `isize`, as a whole word; counted at
the line of its `as`, once per `as` (`x as u32 as usize` is two). Comments, strings (raw and
byte strings too) and character literals are skipped. Not counted:
  - `as` in a `use` item, from the word `use` to its `;` (`use a::B as C`: a rename);
  - `as` before any other type: `<T as Trait>`, `as char`, `as f64`, `as *const u8`, `as Self`;
  - test code (below).
The count is read from the text, with no types, so it differs from a lint's:
  missed  `as _` and `as T` where `_` or `T` is an integer (an inferred type, an alias, a
          type parameter); a path to the type (`as core::primitive::u32`); a cast a macro
          makes from tokens (`$e as $t`)
  over    a crate's own type named like a primitive integer (`struct u8;`), which no crate has
`as` in a `macro_rules!` body to a written integer type (`$e as i64`) counts once, at the
template.

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
Everything else counts, build scripts and binaries included.

A crate is the nearest directory above the file, under crates/, that holds a Cargo.toml, named
by its path under crates/ (`blaze-cargo`, `harness/harness-native`). crates/blaze-int, the one
home of the conversions, has a row of its own: its few casts are the conversions themselves.
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

const SCRIPT: &str = "casts";
/// The list, from the root.
const LIST: &str = "tools/ratchets/casts.txt";
const TEST_DIRS: [&str; 2] = ["tests", "benches"];
const TEST_FILES: [&str; 2] = ["tests.rs", "test.rs"];
/// The files whose out-of-line modules are beside them, not in a directory named for them.
const ROOT_FILES: [&str; 4] = ["lib.rs", "main.rs", "mod.rs", "build.rs"];
/// The primitive integer types: a cast to one of them is a site.
const INTEGERS: [&str; 12] = ["u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize"];
/// What every message about a crate over its row ends with.
const RULE: &str = "AGENTS.md correct-by-construction (rule 76): a conversion says what happens to a value that does \
                    not fit; `as` says nothing, and drops bits silently";
/// What to write instead, for a message.
const INSTEAD: &str = "write the conversion that says what it does, from blaze-int (`use blaze_int::Int`): \
                       `x.widen_to::<u64>()` where no value can be lost (it compiles only then), `x.narrow::<u32>()?` \
                       returning its `Narrowed` where the function can fail, `x.narrow_or_stop::<u32>()` with a comment \
                       stating the invariant that makes it fit, `x.truncate_to::<u8>()` where dropping the high bits is \
                       the point (hashing, codecs), `x.saturate::<u8>()` to clamp; `cast_signed()`/`cast_unsigned()` for \
                       a sign change of equal width; `blaze_int::idx(id)` for an id as an index, `blaze_int::len32(len)` \
                       for a length as a `u32`; `Cast::<S, T>::..` in a `const`";
/// The list's first lines.
const HEADER: &str = "# The integer `as` casts outside test code, by crate (its directory under crates/): `as`\n\
                      # followed by a primitive integer type (`u8`..`u128`, `usize`, `i8`..`i128`, `isize`), not in a\n\
                      # `use` item, read from the text. Only shrinks; a crate with no row is held to 0, so a new crate\n\
                      # starts at 0 (AGENTS.md correct-by-construction; the conversions are crates/blaze-int's).\n\
                      # Written by scripts/src/bin/casts.rs (`{\"command\": \"write\"}`), checked in CI.\n\
                      # crate\tcasts\n";

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

/// One cast: its file and the line of its `as`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Site {
    /// Relative to the root.
    path: String,
    line: usize,
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
            ListError::NotARow { line, text } => write!(f, "{LIST}:{line}: {text:?} is not `<crate><TAB><casts>`"),
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
    /// The lines (from 1) of its casts outside its test code, in order, a line once per cast.
    sites: Vec<usize>,
    /// Each `mod <name>;` it declares, and whether test code declares it.
    mods: Vec<(String, bool)>,
    /// `#![cfg(test)]`: the whole file is test code.
    test: bool,
}

/// The casts of one file's text and the modules it declares out of line.
fn scan_file(p: &Patterns, text: &str) -> Scanned {
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
    out.sites = cast_lines(text).into_iter().filter(|line| !test[line - 1]).collect();
    out
}

/// The line (from 1) of each integer cast's `as` in `text`, in text order, once per cast, test
/// code included ([`scan_file`] takes it out): comments, strings and characters skipped as
/// [`item_end`] skips them, and a `use` item, from the word `use` to its `;`, casts nothing.
fn cast_lines(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
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
                    "as" if !in_use && casts_to_integer(text, end) => {
                        line += text[counted..at].matches('\n').count();
                        counted = at;
                        out.push(line);
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

/// Whether the text after an `as` that ends at byte `from` is, after spaces and line breaks, a
/// primitive integer type's name as a whole word, not the start of a path (`u32::MAX`).
fn casts_to_integer(text: &str, from: usize) -> bool {
    let rest = text[from..].trim_start();
    let len = ident_len(rest.as_bytes());
    INTEGERS.contains(&&rest[..len]) && !rest[len..].starts_with("::")
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
/// `br"..."`, `c"..."`, `b'x'`), or after the raw identifier it starts (`r#as`, read whole so
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
/// (`r#as`), the byte after its identifier.
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

/// Each crate's casts outside test code, in file and line order; a crate with none is absent.
fn count(p: &Patterns, root: &Path) -> BTreeMap<String, Vec<Site>> {
    let files = rust_files(root);
    let known: BTreeSet<String> = files.iter().map(|it| it.join("/")).collect();
    let mut scanned: Vec<(Vec<String>, Scanned)> = Vec::new();
    let mut test: BTreeSet<String> = BTreeSet::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for parts in files {
        let path = parts.join("/");
        let full = parts.iter().fold(root.to_path_buf(), |path, part| path.join(part));
        // `read_text(errors="replace")`: what is not UTF-8 becomes U+FFFD.
        let Ok(bytes) = std::fs::read(&full) else { fail(SCRIPT, &format!("{} could not be read", full.display())) };
        let text = universal_newlines(String::from_utf8_lossy(&bytes).into_owned());
        let found = scan_file(p, &text);
        if test_path(&parts) || found.test {
            test.insert(path.clone());
        }
        for (name, in_test) in &found.mods {
            for file in module_files(&parts, name) {
                if !known.contains(&file) {
                    continue;
                }
                if *in_test {
                    test.insert(file.clone());
                }
                children.entry(path.clone()).or_default().push(file);
            }
        }
        scanned.push((parts, found));
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
    let mut manifests: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    let mut out: BTreeMap<String, Vec<Site>> = BTreeMap::new();
    for (parts, found) in scanned {
        let path = parts.join("/");
        if found.sites.is_empty() || test.contains(&path) {
            continue;
        }
        let krate = crate_of(root, &parts, &mut manifests);
        out.entry(krate).or_default().extend(found.sites.iter().map(|line| Site { path: path.clone(), line: *line }));
    }
    out
}

/// The list as `write` writes it: the header, then `<crate><TAB><casts>` for each crate with a
/// cast, sorted.
fn render(counts: &BTreeMap<String, Vec<Site>>) -> String {
    let mut out = HEADER.to_owned();
    for (krate, sites) in counts.iter().filter(|(_, sites)| !sites.is_empty()) {
        out.push_str(&format!("{krate}\t{}\n", sites.len()));
    }
    out
}

/// The list's rows.
fn parse_rows(text: &str) -> Result<BTreeMap<String, usize>, ListError> {
    let mut rows = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let row = match line.split('\t').collect::<Vec<&str>>().as_slice() {
            [krate, n] if !krate.is_empty() => n.parse::<usize>().ok().map(|n| (*krate, n)),
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
fn at_base(root: &Path, base: &str) -> Result<(Option<BTreeMap<String, usize>>, BTreeMap<String, BTreeSet<usize>>), BaseError> {
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

/// What is wrong: `counts` the tree's casts by crate, `rows` the list's, `base_rows` the base's
/// list (when it had one), `added` the lines added since the base (when one was given).
fn problems(
    counts: &BTreeMap<String, Vec<Site>>,
    rows: &BTreeMap<String, usize>,
    base_rows: Option<&BTreeMap<String, usize>>,
    added: Option<&BTreeMap<String, BTreeSet<usize>>>,
) -> Vec<String> {
    let mut out = Vec::new();
    let crates: BTreeSet<&String> = counts.keys().chain(rows.keys()).collect();
    for krate in crates {
        let sites = counts.get(krate).map_or(&[][..], Vec::as_slice);
        let row = rows.get(krate).copied();
        let now = sites.len();
        let held = row.unwrap_or(0);
        if now > held {
            let shown = match row {
                Some(_) => format!("its row's {held}"),
                None => "0, as a crate with no row is held".to_owned(),
            };
            let new = match added {
                Some(added) => {
                    let new: Vec<String> = sites
                        .iter()
                        .filter(|it| added.get(&it.path).is_some_and(|lines| lines.contains(&it.line)))
                        .map(|it| format!("{}:{}", it.path, it.line))
                        .collect();
                    if new.is_empty() {
                        "none is on a line added since the base: a cast moved in from another crate, or a file git does not track".to_owned()
                    } else {
                        format!("new: {}", new.join(", "))
                    }
                }
                None => "give `base`, the commit the change is against, to have the new ones named".to_owned(),
            };
            out.push(format!("`{krate}` has {now} integer `as` casts outside tests, over {shown} in {LIST} ({new}): {INSTEAD} ({RULE})"));
        } else if now < held {
            let lower = if now == 0 { "remove the row".to_owned() } else { format!("lower it to {now}") };
            out.push(format!(
                "`{krate}` has {now} integer `as` casts outside tests, under its row's {held} in {LIST}: {lower} (command `write`); the list only shrinks"
            ));
        }
    }
    if let Some(base) = base_rows {
        for (krate, now) in rows {
            let then = base.get(krate).copied().unwrap_or(0);
            if *now > then {
                out.push(format!("{LIST}: the row `{krate}` is {now} and was {then} at the base: the list only shrinks ({RULE})"));
            }
        }
    }
    out
}

/// `counts`' total, for a line of output.
fn summary(counts: &BTreeMap<String, Vec<Site>>) -> String {
    let total: usize = counts.values().map(Vec::len).sum();
    format!("{total} integer `as` casts outside tests, in {} crates", counts.len())
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
            "{LIST} is not as command `write` writes it (its header, then a sorted `<crate><TAB><casts>` row per crate with a cast): run it"
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

/// The casts of the counted crates, as `crate: path:line, ...`, for a selftest's message.
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

/// One file's casts, for a selftest's expectation.
fn sites_of(path: &str, lines: &[usize]) -> Vec<Site> {
    lines.iter().map(|line| Site { path: path.to_owned(), line: *line }).collect()
}

/// The selftest's file of crate `a`. Line by line, what each cast is:
/// - 1, `use crate::Thing as u8;`: a rename in a `use` item, not counted;
/// - 2, `x as u32`: counted;
/// - 3, `(x as u32) as usize`: two;
/// - 4, `as char`, 5, `as f64`: not integers;
/// - 6, `<T as Into<u8>>`: a trait cast, not counted;
/// - 7, a cast in a string and in a comment: not counted;
/// - 8, one in a block comment, not counted, then `p as usize`: counted;
/// - 9, `$e as $t` in a `macro_rules!` body, not counted; `$e as i64`: counted at the template;
/// - 10, `as _`: no type written, not counted;
/// - 11-14, `x as` and `u64` on the next line: counted at 12, the `as`'s line;
/// - 15, `r#as` is an identifier, then `r#as as u16`: one;
/// - 18, in `#[cfg(test)] mod tests`: test code;
/// - 21, after `#[test]`: test code;
/// - 22, `as usize}`: counted;
/// - 23, `as core::primitive::u32`: a path, not counted (a miss DOC names); `u32::MAX as u64`:
///   counted, once.
const FIXTURE: &str = r#"use crate::Thing as u8;
fn a(x: u64) -> u32 { x as u32 }
fn b(x: u64) -> usize { (x as u32) as usize }
fn c(x: u8) -> char { x as char }
fn d(x: u32) -> f64 { x as f64 }
fn e<T: Into<u8>>(t: T) -> u8 { <T as Into<u8>>::into(t) }
const S: &str = "x as u32"; // y as u64
/* z as i8 */ fn f(p: *const u8) -> usize { p as usize }
macro_rules! m { ($e:expr, $t:ty) => { $e as $t }; ($e:expr) => { $e as i64 }; }
fn g(x: i32) -> u32 { x as _ }
fn h(x: u32) -> u64 {
    x as
        u64
}
fn raw() { let r#as = 1u8; let _ = r#as as u16; }
#[cfg(test)]
mod tests {
    fn t(x: u64) -> u8 { x as u8 }
}
#[test]
fn a_test() { let _ = 1u64 as u8; }
fn ab(x: u32) -> usize { x as usize}
fn path(x: u64) -> (u32, u64) { (x as core::primitive::u32, u32::MAX as u64) }
"#;

/// The lines of [`FIXTURE`]'s casts outside its test code.
const FIXTURE_SITES: [usize; 9] = [2, 3, 3, 8, 9, 12, 15, 22, 23];

fn selftest(p: &Patterns) {
    // What a cast is: `as` and an integer type's name as a whole word, not a path's start.
    expect(casts_to_integer(" u32 }", 0) && casts_to_integer("\n\t usize)", 0), "an integer after spaces and line breaks");
    expect(!casts_to_integer(" u32x", 0) && !casts_to_integer(" u32::MAX", 0) && !casts_to_integer(" char", 0), "not a longer word, a path or another type");

    // One file: every case of [`FIXTURE`], test code in, then out.
    let all = cast_lines(FIXTURE);
    let mut want: Vec<usize> = FIXTURE_SITES.to_vec();
    want.extend([18, 21]);
    want.sort();
    expect(all == want, &format!("the fixture's casts, test code in: found {all:?}, wanted {want:?}"));
    let found = scan_file(p, FIXTURE);
    expect(found.sites == FIXTURE_SITES, &format!("the fixture's casts outside test code: {:?}", found.sites));
    // Planted: the test module's attribute, then the test's, removed: each is a site.
    let planted = scan_file(p, &FIXTURE.replace("#[cfg(test)]\n", "\n")).sites;
    expect(planted.contains(&18), &format!("`t` outside test code is a site: {planted:?}"));
    let planted = scan_file(p, &FIXTURE.replace("#[test]\n", "\n")).sites;
    expect(planted.contains(&21), &format!("`a_test` without `#[test]` is a site: {planted:?}"));
    // Planted: the `use` item's `;` gone, the rename reads on into line 2 and hides its cast.
    let planted = cast_lines(&FIXTURE.replace("use crate::Thing as u8;", "use crate::Thing as u8"));
    expect(!planted.contains(&2), &format!("a `use` item casts nothing up to its `;`: {planted:?}"));
    // Planted: `as _` given its type is counted.
    let planted = cast_lines(&FIXTURE.replace("x as _", "x as u32"));
    expect(planted.contains(&10), &format!("`as u32` in place of `as _` is counted: {planted:?}"));

    // A tree: crates, test files by path and by declaration, a nested crate.
    let tmp = TempDir::new("casts-selftest-").unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    let one = "pub fn f(x: u64) -> u8 { x as u8 }\n";
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    put("crates/a/src/lib.rs", FIXTURE);
    put("crates/a/src/live.rs", one);
    put("crates/a/src/x.rs", "#[cfg(test)]\nmod t;\n");
    put("crates/a/src/x/t.rs", one);
    put("crates/a/build.rs", one);
    put("crates/a/tests/it.rs", one);
    put("crates/a/target/debug/build/out.rs", one);
    put("crates/b/Cargo.toml", "[workspace]\n");
    put("crates/b/c/Cargo.toml", "[package]\nname = \"c\"\n");
    put("crates/b/c/src/lib.rs", one);
    let counts = count(p, tmp.path());
    let mut a = sites_of("crates/a/build.rs", &[1]);
    a.extend(sites_of("crates/a/src/lib.rs", &FIXTURE_SITES));
    a.extend(sites_of("crates/a/src/live.rs", &[1]));
    let want = BTreeMap::from([("a".to_owned(), a), ("b/c".to_owned(), sites_of("crates/b/c/src/lib.rs", &[1]))]);
    expect(counts == want, &format!("the tree's casts: {}", shown(&counts)));

    // The ratchet. As recorded, nothing is wrong, and the list reads back as it was written.
    let rows = parse_rows(&render(&counts)).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    expect(rows == BTreeMap::from([("a".to_owned(), 11), ("b/c".to_owned(), 1)]), &format!("the rows read back: {rows:?}"));
    expect(problems(&counts, &rows, Some(&rows), None).is_empty(), "the tree as recorded is right");
    // Planted: one cast added. Named by its crate, its place (given the base's diff), what to
    // write instead and the rule.
    let mut fewer = rows.clone();
    fewer.insert("a".to_owned(), 10);
    let added = BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([22]))]);
    let said = problems(&counts, &fewer, None, Some(&added));
    expect(
        said.len() == 1
            && said[0].contains("`a` has 11 integer `as` casts outside tests, over its row's 10")
            && said[0].contains("new: crates/a/src/lib.rs:22)")
            && said[0].contains("narrow_or_stop")
            && said[0].contains("correct-by-construction"),
        &format!("one cast added is caught and named: {said:?}"),
    );
    // Planted: one cast removed. The row must come down; a crate with none left goes.
    let mut more = rows.clone();
    more.insert("b/c".to_owned(), 2);
    let said = problems(&counts, &more, None, None);
    expect(said.len() == 1 && said[0].contains("`b/c`") && said[0].contains("lower it to 1"), &format!("one cast removed is caught: {said:?}"));
    let mut gone = rows.clone();
    gone.insert("old".to_owned(), 3);
    let said = problems(&counts, &gone, None, None);
    expect(said.len() == 1 && said[0].contains("`old`") && said[0].contains("remove the row"), &format!("a crate gone: {said:?}"));
    // Planted: a crate with no row is held to 0; a row raised above the base's is caught even
    // where the tree matches it.
    let said = problems(&counts, &BTreeMap::from([("a".to_owned(), 11)]), None, None);
    expect(said.len() == 1 && said[0].contains("`b/c` has 1") && said[0].contains("held"), &format!("a new crate starts at 0: {said:?}"));
    let said = problems(&counts, &rows, Some(&fewer), None);
    expect(said.len() == 1 && said[0].contains("the row `a` is 11 and was 10 at the base"), &format!("a row raised: {said:?}"));

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
        parse_rows("a\t1\nb 2\n").is_err_and(|e| e == ListError::NotARow { line: 2, text: "b 2".to_owned() })
            && parse_rows("a\t1\na\t2\n").is_err_and(|e| matches!(e, ListError::TwoRows { line: 2, .. }))
            && parse_rows("a\t1\t0\n").is_err()
            && parse_rows("a\tx\n").is_err()
            && parse_rows("\t1\n").is_err(),
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

    /// A rise over the crate's row is refused, naming the cast the base's diff added; the tree
    /// at its row is not.
    #[test]
    fn a_rise_is_refused_with_its_site() {
        let counts = BTreeMap::from([("s".to_owned(), sites_of("crates/s/src/lib.rs", &[3, 9]))]);
        let at_row = BTreeMap::from([("s".to_owned(), 2)]);
        assert!(problems(&counts, &at_row, Some(&at_row), None).is_empty());
        let under = BTreeMap::from([("s".to_owned(), 1)]);
        let added = BTreeMap::from([("crates/s/src/lib.rs".to_owned(), BTreeSet::from([9]))]);
        let said = problems(&counts, &under, None, Some(&added));
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("over its row's 1") && said[0].contains("new: crates/s/src/lib.rs:9)"), "{said:?}");
    }
}
