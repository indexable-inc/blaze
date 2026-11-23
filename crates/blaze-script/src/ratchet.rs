//! What a per-crate ratchet of scripts/ shares: reading Rust as text (comments, strings and
//! characters skipped), which code is a test's, the walk of crates/, the list of rows and the
//! comparison with a base (AGENTS.md one-concept-one-home). `recursion`, `stops`, `casts`,
//! `clones`, `allocs` and `untyped` are built on it; `ambient` still holds its own walk.
//!
//! What test code is, in every ratchet that uses this:
//!   - a file under a directory named `tests` or `benches`, or named `tests.rs` or `test.rs`;
//!   - a file that says `#![cfg(test)]`;
//!   - the item after `#[cfg(test)]` or `#[cfg(all(test, ...))]`, or after `#[test]` or another
//!     attribute whose path ends in the word `test` (`#[tokio::test]`, not `#[test_case]`), from
//!     the attribute's line to the line of the `}` or `;` that ends the item; an attribute whose
//!     item does not end before its block does (a match arm, a field) covers its own line only;
//!   - the file of a module declared out of line in test code (`#[cfg(test)] mod t;`, or any
//!     `mod m;` in a test file): `m.rs` or `m/mod.rs` beside a crate root (`lib.rs`, `main.rs`,
//!     `mod.rs`, `build.rs`, a file of a `bin` directory), else in the directory named for the
//!     declaring file. A `#[path]` is not followed.
//! A crate is the nearest directory above a file, under crates/, that holds a Cargo.toml, named
//! by its path under crates/; a directory named `target` is not read.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use crate::{fail, git, regex::Regex, universal_newlines};

const TEST_DIRS: [&str; 2] = ["tests", "benches"];
const TEST_FILES: [&str; 2] = ["tests.rs", "test.rs"];
/// The files whose out-of-line modules are beside them, not in a directory named for them.
const ROOT_FILES: [&str; 4] = ["lib.rs", "main.rs", "mod.rs", "build.rs"];

/// The patterns that find test code, compiled once.
pub struct Tests {
    cfg_test: Regex,
    /// `#[test]`, or an attribute whose path ends in the word `test` (`#[tokio::test]`), when
    /// enabled (`None` for [`Tests::cfg_test_only`]).
    test_fn: Option<Regex>,
    inner_cfg_test: Regex,
    /// `mod <name>;`, attributes before it on the line allowed.
    module: Regex,
}

/// What one file says of test code.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileTests {
    /// Whether each line (from 0) is test code.
    pub test: Vec<bool>,
    /// Each `mod <name>;` it declares, and whether test code declares it.
    pub mods: Vec<(String, bool)>,
    /// `#![cfg(test)]`: the whole file is test code.
    pub whole: bool,
}

impl Tests {
    pub fn new(script: &str) -> Tests {
        Tests::make(script, true)
    }

    /// As [`Tests::new`], without matching `#[test]` / `#[*::test]` attributes (only
    /// `#[cfg(test)]`, `#[cfg(all(test, ...))]` and `#![cfg(test)]`).
    pub fn cfg_test_only(script: &str) -> Tests {
        Tests::make(script, false)
    }

    fn make(script: &str, test_fns: bool) -> Tests {
        let re = |pattern: &str| Regex::new(pattern).unwrap_or_else(|e| fail(script, &e.to_string()));
        Tests {
            cfg_test: re(r"\A\s*#\[cfg\((?:test|all\(test\b[^\]]*)\)\]"),
            // To the attribute's `]`, so that the item read after it starts after the attribute.
            test_fn: test_fns.then(|| re(r"\A\s*#\[(?:\w+::)*test\b[^\]]*\]")),
            inner_cfg_test: re(r"\A\s*#!\[cfg\((?:test|all\(test\b[^\]]*)\)\]"),
            module: re(r"\A\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?(\w+)\s*;"),
        }
    }

    /// The test code of one file's text.
    pub fn of(&self, text: &str) -> FileTests {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut out = FileTests { test: vec![false; lines.len()], ..FileTests::default() };
        // The byte where line `at` starts.
        let mut start = 0;
        for (at, line) in lines.iter().enumerate() {
            if self.inner_cfg_test.is_match(line) {
                out.whole = true;
            }
            if let Some(found) = self.cfg_test.find(line).or_else(|| self.test_fn.as_ref().and_then(|re| re.find(line))) {
                // The item runs from after the attribute to its `}` or `;`, on the line of that byte.
                let last = match item_end(text, start + found.end()) {
                    Some(end) => at + text[start..end - 1].matches('\n').count(),
                    None => at,
                };
                for it in &mut out.test[at..=last] {
                    *it = true;
                }
            }
            if let Some(found) = self.module.captures(line) {
                out.mods.push((found[1].to_owned(), out.test[at]));
            }
            start += line.len() + 1;
        }
        if out.whole {
            for it in &mut out.mods {
                it.1 = true;
            }
        }
        out
    }
}

/// One counted site: its file from the root, its line from 1, and what the ratchet says of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit<T> {
    pub path: String,
    pub line: usize,
    pub what: T,
}

/// Which directories under `crates/` a walk reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrateScope {
    /// Only members of the root `Cargo.toml`'s `[workspace]` (skipping excluded/parked dirs).
    Members,
    /// Every directory under `crates/` (excluding `target`).
    AllDirs,
}

/// Each crate's counted sites outside test code, in file and line order.
pub type Counts<T> = BTreeMap<String, Vec<Hit<T>>>;

/// One `.rs` file visited by [`walk_with`].
pub struct File<'a> {
    /// Path components from the checkout root (e.g. `["crates", "a", "src", "lib.rs"]`).
    pub parts: &'a [String],
    /// The file's text (universal newlines, invalid UTF-8 replaced).
    pub text: &'a str,
    /// Test lines and out-of-line modules found in this file.
    pub tests: &'a FileTests,
}

/// Each crate's sites outside test code, in file and line order; a crate with none is absent.
/// `scan` is given each file's text and returns its sites, as (line from 1, what), test code
/// included: the sites on a test line or in a test file are dropped here.
pub fn walk<T>(script: &str, root: &Path, tests: &Tests, mut scan: impl FnMut(&str) -> Vec<(usize, T)>) -> BTreeMap<String, Vec<Hit<T>>> {
    walk_with(script, root, CrateScope::Members, tests, |_| false, |file, emit| {
        for (line, what) in scan(file.text) {
            emit(line, what);
        }
    })
}

/// Walks `.rs` files under `crates/` according to `scope`, skips test files, any file for
/// which `skip` returns true, and transitively any out-of-line module they declare, and
/// groups the remaining sites by crate.
///
/// `scan` is called only for files that are not already skipped; it emits `(line, what)` pairs
/// through `emit` (lines that are test code are dropped automatically).
pub fn walk_with<T>(
    script: &str,
    root: &Path,
    scope: CrateScope,
    tests: &Tests,
    mut skip: impl FnMut(&File<'_>) -> bool,
    mut scan: impl FnMut(&File<'_>, &mut dyn FnMut(usize, T)),
) -> Counts<T> {
    let files = match scope {
        // Only the root workspace's code: a directory under crates/ it does not hold (its own
        // workspace, or a parked crate) is not built and not counted.
        CrateScope::Members => {
            let manifest = root.join("Cargo.toml");
            let Ok(text) = std::fs::read_to_string(&manifest) else { fail(script, &format!("{} could not be read", manifest.display())) };
            let members = Members::of(&text);
            rust_files(root).into_iter().filter(|parts| parts.len() < 2 || members.holds(&parts[..2].join("/"))).collect()
        }
        CrateScope::AllDirs => rust_files(root),
    };
    let known: BTreeSet<String> = files.iter().map(|it| it.join("/")).collect();
    let mut scanned: Vec<(Vec<String>, Vec<(usize, T)>)> = Vec::new();
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for parts in files {
        let path = parts.join("/");
        let full = parts.iter().fold(root.to_path_buf(), |path, part| path.join(part));
        // What is not UTF-8 becomes U+FFFD.
        let Ok(bytes) = std::fs::read(&full) else { fail(script, &format!("{} could not be read", full.display())) };
        let text = universal_newlines(String::from_utf8_lossy(&bytes).into_owned());
        let found = tests.of(&text);
        let file = File { parts: &parts, text: &text, tests: &found };
        let is_skipped = test_path(&parts) || found.whole || skip(&file);
        if is_skipped {
            skipped.insert(path.clone());
        }
        for (name, in_test) in &found.mods {
            for child in module_files(&parts, name) {
                if !known.contains(&child) {
                    continue;
                }
                if *in_test {
                    skipped.insert(child.clone());
                }
                children.entry(path.clone()).or_default().push(child);
            }
        }
        let mut sites = Vec::new();
        if !is_skipped {
            scan(&file, &mut |line, what| {
                if !found.test.get(line - 1).copied().unwrap_or(false) {
                    sites.push((line, what));
                }
            });
        }
        scanned.push((parts, sites));
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
    let mut out: Counts<T> = Default::default();
    for (parts, sites) in scanned {
        let path = parts.join("/");
        if sites.is_empty() || skipped.contains(&path) {
            continue;
        }
        let krate = crate_of(root, &parts, &mut manifests);
        out.entry(krate).or_default().extend(sites.into_iter().map(|(line, what)| Hit { path: path.clone(), line, what }));
    }
    out
}

pub fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The byte after the `}` or `;` that ends the item starting at byte `from` of `text`: the
/// first `;` outside brackets, or the `}` that closes its first brace; `None` when a closing
/// bracket comes first (the item is part of a larger one) or the text ends. Comments, strings
/// (raw and byte strings too) and character literals are skipped.
pub fn item_end(text: &str, from: usize) -> Option<usize> {
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

/// The byte of the bracket that closes the one at `open` (`(`, `[` or `{`), brackets of every
/// kind nested, comments, strings and characters skipped; `None` when the text ends first.
pub fn close_of(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth: usize = 0;
    let mut at = open;
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
                if depth == 0 {
                    return Some(at);
                }
                at + 1
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => word_end(text, at),
            _ => at + 1,
        };
    }
    None
}

/// The byte after the block comment that opens at `from`, comments nested in it included.
pub fn comment_end(bytes: &[u8], from: usize) -> usize {
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
pub fn string_end(bytes: &[u8], from: usize) -> usize {
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
pub fn quote_end(text: &str, at: usize) -> usize {
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
pub fn ident_len(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|b| is_ident(**b)).count()
}

/// The byte after the word at `at`, after the literal it prefixes (`r#"..."#`, `b"..."`,
/// `br"..."`, `c"..."`, `b'x'`), or after the raw identifier it starts (`r#fn`, read whole so
/// that its word is no keyword).
pub fn word_end(text: &str, at: usize) -> usize {
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
pub fn raw_end(bytes: &[u8], from: usize) -> usize {
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
pub fn test_path(parts: &[String]) -> bool {
    let (name, dirs) = parts.split_last().map_or(("", &[][..]), |(name, dirs)| (name.as_str(), dirs));
    dirs.iter().any(|part| TEST_DIRS.contains(&part.as_str())) || TEST_FILES.contains(&name)
}

/// The two files the module `name`, declared out of line in the file `parts`, can be.
pub fn module_files(parts: &[String], name: &str) -> [String; 2] {
    let Some((file, dirs)) = parts.split_last() else { return [String::new(), String::new()] };
    let mut base: Vec<&str> = dirs.iter().map(String::as_str).collect();
    let root = ROOT_FILES.contains(&file.as_str()) || dirs.last().is_some_and(|it| it == "bin");
    if !root {
        base.push(file.strip_suffix(".rs").unwrap_or(file));
    }
    let base = base.join("/");
    [format!("{base}/{name}.rs"), format!("{base}/{name}/mod.rs")]
}

/// The `.rs` files under crates/, sorted part by part, each as its parts from the root, none
/// under a directory named `target`. Directories that are links are not entered.
/// The root manifest's `[workspace]` `members` and `exclude` (each a path from the root; a
/// member may end in `/*`, one directory level), read from the quoted strings of the two arrays.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Members {
    pub globs: Vec<String>,
    pub exclude: BTreeSet<String>,
}

impl Members {
    pub fn of(manifest: &str) -> Members {
        let mut out = Members::default();
        // Which array the lines are in: none, `members` (true) or `exclude`; only under `[workspace]`.
        let (mut in_workspace, mut array): (bool, Option<bool>) = (false, None);
        for line in manifest.split('\n') {
            let code = line.split('#').next().unwrap_or("").trim();
            if code.starts_with('[') && array.is_none() {
                in_workspace = code == "[workspace]";
                continue;
            }
            if !in_workspace {
                continue;
            }
            let rest = match array {
                Some(_) => code,
                None => match code.split_once('=') {
                    Some((key, value)) if matches!(key.trim(), "members" | "exclude") => {
                        array = Some(key.trim() == "members");
                        value.trim()
                    }
                    _ => continue,
                },
            };
            let Some(members) = array else { continue };
            for (at, quoted) in rest.split('"').enumerate() {
                if at % 2 == 1 {
                    match members {
                        true => out.globs.push(quoted.to_owned()),
                        false => {
                            out.exclude.insert(quoted.to_owned());
                        }
                    }
                }
            }
            if rest.contains(']') {
                array = None;
            }
        }
        out
    }

    /// The workspace holds the directory `dir` (a path from the root): a member and not excluded.
    pub fn holds(&self, dir: &str) -> bool {
        !self.exclude.contains(dir)
            && self.globs.iter().any(|glob| match glob.strip_suffix("/*") {
                Some(parent) => dir.rsplit_once('/').is_some_and(|(above, _)| above == parent),
                None => glob == dir,
            })
    }
}

pub fn rust_files(root: &Path) -> Vec<Vec<String>> {
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
pub fn crate_of(root: &Path, parts: &[String], manifests: &mut BTreeMap<Vec<String>, bool>) -> String {
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

/// A list read wrong: which list, where, and what is wrong there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListError {
    /// A line (from 1) that is not a row, a comment or blank.
    NotARow { list: String, line: usize, text: String },
    /// A crate's second row, at its line (from 1).
    TwoRows { list: String, line: usize, krate: String },
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListError::NotARow { list, line, text } => write!(f, "{list}:{line}: {text:?} is not `<crate><TAB><count>`"),
            ListError::TwoRows { list, line, krate } => write!(f, "{list}:{line}: `{krate}` has two rows"),
        }
    }
}

/// The base could not be read: which base, and what of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseError {
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

/// The list as `write` writes it: `header`, then `<crate><TAB><count>` for each crate with a
/// site, sorted, less the crates of `at_zero` (held at 0, so never given a row).
pub fn render<T>(header: &str, counts: &BTreeMap<String, Vec<Hit<T>>>, at_zero: &[&str]) -> String {
    let mut out = header.to_owned();
    for (krate, sites) in counts.iter().filter(|(krate, sites)| !sites.is_empty() && !at_zero.contains(&krate.as_str())) {
        out.push_str(&format!("{krate}\t{}\n", sites.len()));
    }
    out
}

/// The rows of the list `list` (its path, for an error), from its text.
pub fn parse_rows(list: &str, text: &str) -> Result<BTreeMap<String, usize>, ListError> {
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
            return Err(ListError::NotARow { list: list.to_owned(), line: at + 1, text: line.to_owned() });
        };
        if rows.insert(krate.to_owned(), n).is_some() {
            return Err(ListError::TwoRows { list: list.to_owned(), line: at + 1, krate: krate.to_owned() });
        }
    }
    Ok(rows)
}

/// The lines each file has that a base did not, from `git diff --unified=0`'s hunks: by path
/// from the root, the line numbers in the tree.
pub fn added_lines(diff: &str) -> Result<BTreeMap<String, BTreeSet<usize>>, BaseError> {
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

/// What `base` says of the list `list`: its rows (none when it has no list, before the ratchet
/// began), and the lines added since it under crates/.
pub fn at_base(root: &Path, base: &str, list: &str) -> Result<(Option<BTreeMap<String, usize>>, BTreeMap<String, BTreeSet<usize>>), BaseError> {
    let root_arg = root.to_string_lossy();
    let dir = root_arg.as_ref();
    let commit = format!("{base}^{{commit}}");
    git(root, &["-C", dir, "rev-parse", "--verify", "--quiet", commit.as_str()])
        .map_err(|why| BaseError::NoCommit { base: base.to_owned(), why })?;
    let failed = |what: &'static str| move |why: String| BaseError::Git { base: base.to_owned(), what, why };
    let listed = git(root, &["-C", dir, "ls-tree", "--name-only", base, "--", list]).map_err(failed("list its tree"))?;
    let rows = if listed.trim().is_empty() {
        None
    } else {
        let spec = format!("{base}:{list}");
        let text = git(root, &["-C", dir, "show", spec.as_str()]).map_err(failed("show the list"))?;
        Some(parse_rows(list, &text).map_err(|error| BaseError::List { base: base.to_owned(), error })?)
    };
    let diff = git(root, &["-C", dir, "diff", "--no-color", "--no-ext-diff", "--no-renames", "--unified=0", base, "--", "crates"])
        .map_err(failed("diff the tree"))?;
    Ok((rows, added_lines(&diff)?))
}

/// How a ratchet's list is held: the list's path, what a site is (plural, for a message), what
/// to write instead and the rule, and the crates held at 0 whatever the list says.
pub struct Policy<'a> {
    pub list: &'a str,
    pub noun: &'a str,
    pub instead: &'a str,
    pub rule: &'a str,
    pub at_zero: &'a [&'a str],
}

/// What is wrong: `counts` the tree's sites by crate, `rows` the list's, `base_rows` the base's
/// list (when it had one), `added` the lines added since the base (when one was given), and
/// `named` a site as a message names it. A crate with no row is held to 0; a crate of
/// `policy.at_zero` is held to 0 whatever its row, every site named.
pub fn problems<T>(
    policy: &Policy<'_>,
    counts: &BTreeMap<String, Vec<Hit<T>>>,
    rows: &BTreeMap<String, usize>,
    base_rows: Option<&BTreeMap<String, usize>>,
    added: Option<&BTreeMap<String, BTreeSet<usize>>>,
    named: impl Fn(&Hit<T>) -> String,
) -> Vec<String> {
    let Policy { list, noun, instead, rule, at_zero } = policy;
    let mut out = Vec::new();
    let crates: BTreeSet<&String> = counts.keys().chain(rows.keys()).collect();
    for krate in crates {
        let sites = counts.get(krate).map_or(&[][..], Vec::as_slice);
        let now = sites.len();
        if at_zero.contains(&krate.as_str()) {
            if now > 0 {
                let each: Vec<String> = sites.iter().map(&named).collect();
                out.push(format!("`{krate}` is held at 0 {noun} and has {now}: {}: {instead} ({rule})", each.join(", ")));
            }
            if rows.contains_key(krate) {
                out.push(format!("{list}: `{krate}` is held at 0 and has no row: remove it"));
            }
            continue;
        }
        let row = rows.get(krate).copied();
        let held = row.unwrap_or(0);
        if now > held {
            let shown = match row {
                Some(_) => format!("its row's {held}"),
                None => "0, as a crate with no row is held".to_owned(),
            };
            let new = match added {
                Some(added) => {
                    let new: Vec<String> =
                        sites.iter().filter(|it| added.get(&it.path).is_some_and(|lines| lines.contains(&it.line))).map(&named).collect();
                    if new.is_empty() {
                        "none is on a line added since the base: a site moved in from another crate, or a file git does not track".to_owned()
                    } else {
                        format!("new: {}", new.join(", "))
                    }
                }
                None => "give `base`, the commit the change is against, to have the new ones named".to_owned(),
            };
            out.push(format!("`{krate}` has {now} {noun} outside tests, over {shown} in {list} ({new}): {instead} ({rule})"));
        } else if now < held {
            let lower = if now == 0 { "remove the row".to_owned() } else { format!("lower it to {now}") };
            out.push(format!("`{krate}` has {now} {noun} outside tests, under its row's {held} in {list}: {lower} (command `write`); the list only shrinks"));
        }
    }
    if let Some(base) = base_rows {
        for (krate, now) in rows {
            let then = base.get(krate).copied().unwrap_or(0);
            if *now > then {
                out.push(format!("{list}: the row `{krate}` is {now} and was {then} at the base: the list only shrinks ({rule})"));
            }
        }
    }
    out
}

/// One run of a ratchet's `write` or `check`: the script, its policy and list header, the root,
/// which command, the base (for `check`), and the line that sums the tree up.
pub struct Run<'a> {
    pub script: &'a str,
    pub policy: &'a Policy<'a>,
    pub header: &'a str,
    pub root: &'a Path,
    pub write: bool,
    pub base: Option<&'a str>,
    pub summary: &'a str,
}

/// `write` and `check` of a ratchet over `counts`, as every one of them runs them: `write`
/// writes [`render`] to the list; `check` compares the list (and the base's, when `base` is
/// given) and ends the process with status 1, every problem printed, when one is found.
pub fn write_or_check<T>(run: &Run<'_>, counts: &BTreeMap<String, Vec<Hit<T>>>, named: impl Fn(&Hit<T>) -> String) {
    let Run { script, policy, header, root, write, base, summary } = *run;
    let list = policy.list;
    let text = render(header, counts, policy.at_zero);
    if write {
        if let Err(e) = std::fs::write(root.join(list), &text) {
            fail(script, &format!("{list} is not written: {e}"));
        }
        println!("{script}: wrote {list}: {summary}");
        return;
    }
    let Some(listed) = crate::read_text(&root.join(list)) else {
        fail(script, &format!("{list} could not be read: every crate is held to its row there (command `write` makes it)"))
    };
    let rows = parse_rows(list, &listed).unwrap_or_else(|e| fail(script, &e.to_string()));
    let (base_rows, added) = match base {
        Some(base) => {
            let (rows, added) = at_base(root, base, list).unwrap_or_else(|e| fail(script, &e.to_string()));
            if rows.is_none() {
                println!("{script}: {base} has no {list}: the list begins with this change, so no row is compared with the base");
            }
            (rows, Some(added))
        }
        None => (None, None),
    };
    let mut wrong = problems(policy, counts, &rows, base_rows.as_ref(), added.as_ref(), named);
    if wrong.is_empty() && listed != text {
        wrong.push(format!("{list} is not as command `write` writes it (its header, then a sorted `<crate><TAB><count>` row per crate with a site): run it"));
    }
    if !wrong.is_empty() {
        let lines: Vec<String> = wrong.iter().map(|w| format!("{script}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        crate::exit(1);
    }
    println!("{script}: {summary}, each crate at its row in {list}");
}
