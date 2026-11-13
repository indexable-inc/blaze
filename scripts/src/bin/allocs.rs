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
//! crates/blaze-lints/baselines/alloc-sites.txt only shrinks, except `global` while any
//! `// alloc:` line is left: converting those to `Global` raises it. It runs with no
//! compiler: a gate step of the `scripts` job (.github/workflows/blaze.yaml), beside `domains`.

/// What the usage error prints.
const DOC: &str = r#"allocs: what is left of the global heap in each crate only goes down (AGENTS.md bump-allocate).

    blaze run -q -p scripts --bin allocs -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin allocs -- '{"command": "write"}'
    blaze run -q -p scripts --bin allocs -- selftest

    command   check   each count of every crate equals its row's in the list (a `comments` or
                      `expects` count over its row is refused, with its new sites named given
                      `base`; one under it must be lowered; a `global` count off its row is
                      written with `write`); a crate with no row is held to 0; with `base`, no
                      `comments` or `expects` of a row is above the base's, and no `global`
                      either once the base has no `// alloc:` line left in any crate (while
                      some are, converting them to `Global` raises it) (CI)
              write   write the list from the tree as it is
    root      the checkout (".")
    base      the commit the change is against, or null: its list (`git show`) holds every
              row down, and the lines added since it (`git diff --unified=0`) name a crate's
              new sites

The list's row is `<crate><TAB><comments><TAB><expects><TAB><global>`, of lines of `.rs` files
under crates/ (a directory named `target` is not read):
  comments  a line with `// alloc: <why>` (`//`, then `alloc:` and a reason that is not
            empty): the comment that once kept an allocator lint's finding and now keeps
            nothing; each is a site to convert, and the count goes to 0
  expects   a line that starts with an `allow` or `expect` attribute naming
            `blaze::implicit_allocator`: an item that says why it keeps the global heap
  global    a line whose code (not a comment, a string or a character, and not a `use` item)
            has the word `Global`: the global heap named where it is used, which moving to
            frames brings down
A line can count in more than one column. The previous layout, `<crate><TAB><count>`, is
refused with a message to run `write`.

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
const HEADER: &str = "# What is left of the global heap outside test code, by crate (its directory under crates/):\n\
                      # `// alloc: <why>` lines, which keep nothing now and go to 0; `allow`/`expect`s of\n\
                      # `blaze::implicit_allocator`; lines whose code names `Global`. Only shrinks (`global` may rise\n\
                      # while any `// alloc:` line is left); a crate with no row is held to 0 (AGENTS.md bump-allocate).\n\
                      # Written by scripts/src/bin/allocs.rs (`{\"command\": \"write\"}`), checked in CI.\n\
                      # crate\tcomments\texpects\tglobal\n";

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
    /// `// alloc: <why>`: kept nothing since 2026-11-13, a site to convert.
    Comment,
    /// An `allow` or `expect` of `blaze::implicit_allocator`.
    Expect,
    /// Code that names `Global`.
    Global,
}

const KINDS: [Kind; 3] = [Kind::Comment, Kind::Expect, Kind::Global];

impl Kind {
    /// Its column in a row, from 0.
    const fn column(self) -> usize {
        match self {
            Kind::Comment => 0,
            Kind::Expect => 1,
            Kind::Global => 2,
        }
    }

    /// Its column's name in the header and in messages.
    const fn name(self) -> &'static str {
        match self {
            Kind::Comment => "comments",
            Kind::Expect => "expects",
            Kind::Global => "global",
        }
    }

    /// What its sites are, for a message: `<n> <what>`.
    const fn what(self) -> &'static str {
        match self {
            Kind::Comment => "`// alloc:` lines, which keep nothing now",
            Kind::Expect => "`allow`/`expect`s of `blaze::implicit_allocator`",
            Kind::Global => "lines that name `Global`",
        }
    }
}

/// A crate's counts, by [`Kind::column`].
type Row = [usize; 3];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Site {
    /// Relative to the root.
    path: String,
    line: usize,
    kind: Kind,
}

/// The counts of `sites`, by kind.
fn tally(sites: &[Site]) -> Row {
    let mut row = [0; 3];
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
    /// The lines (from 1) that are sites outside its test code, each with what it is, by line
    /// and then by kind (a line can be more than one).
    sites: Vec<(usize, Kind)>,
    /// Each `mod <name>;` it declares, and whether test code declares it.
    mods: Vec<(String, bool)>,
    /// `#![cfg(test)]`: the whole file is test code.
    test: bool,
}

/// The sites of one file's text and the modules it declares out of line.
fn scan_file(p: &Patterns, text: &str) -> Scanned {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut test = vec![false; lines.len()];
    let mut out = Scanned::default();
    let global = global_lines(text);
    let mut global = global.iter().peekable();
    // The byte where line `at` starts.
    let mut start = 0;
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
        if let Some(found) = p.module.captures(line) {
            out.mods.push((found[1].to_owned(), test[at]));
        }
        // `global` is in line order: take this line's entry, if it has one, test code or not.
        let names_global = global.next_if(|it| **it == at + 1).is_some();
        if !test[at] {
            let kinds = [(keeps(line), Kind::Comment), (p.allowed.is_match(line), Kind::Expect), (names_global, Kind::Global)];
            out.sites.extend(kinds.iter().filter(|(is, _)| *is).map(|(_, kind)| (at + 1, *kind)));
        }
        start += line.len() + 1;
    }
    if out.test {
        out.sites.clear();
        for it in &mut out.mods {
            it.1 = true;
        }
    }
    out
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
            b'0'..=b'9' => at + bytes[at..].iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count(),
            _ => at + 1,
        };
    }
    out
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

/// Each crate's sites outside test code, in file and line order; a crate with none is absent.
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
        out.entry(krate)
            .or_default()
            .extend(found.sites.iter().map(|(line, kind)| Site { path: path.clone(), line: *line, kind: *kind }));
    }
    out
}

/// The list as `write` writes it: the header, then `<crate><TAB><comments><TAB><expects><TAB>
/// <global>` for each crate with a site, sorted.
fn render(counts: &BTreeMap<String, Vec<Site>>) -> String {
    let mut out = HEADER.to_owned();
    for (krate, sites) in counts.iter().filter(|(_, sites)| !sites.is_empty()) {
        let [comments, expects, global] = tally(sites);
        out.push_str(&format!("{krate}\t{comments}\t{expects}\t{global}\n"));
    }
    out
}

/// The list's rows; `Err` names the line that is not a row, a comment or blank, and says to run
/// `write` for a row of the previous layout, `<crate><TAB><count>`.
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
                    "{LIST}:{}: {line:?} is a row of the previous layout, `<crate><TAB><count>`: run command `write`",
                    at + 1
                ));
            }
            [krate, comments, expects, global] if !krate.is_empty() => match (number(*comments), number(*expects), number(*global)) {
                (Some(comments), Some(expects), Some(global)) => Some((*krate, [comments, expects, global])),
                _ => None,
            },
            _ => None,
        };
        let Some((krate, n)) = row else {
            return Err(format!("{LIST}:{}: {line:?} is not `<crate><TAB><comments><TAB><expects><TAB><global>`", at + 1));
        };
        if rows.insert(krate.to_owned(), n).is_some() {
            return Err(format!("{LIST}:{}: `{krate}` has two rows", at + 1));
        }
    }
    Ok(rows)
}

/// Whether `text` is a list of the previous layout: its first row is `<crate><TAB><count>`.
fn previous_layout(text: &str) -> bool {
    let first = text.split('\n').find(|line| !line.is_empty() && !line.starts_with('#'));
    first.is_some_and(|line| line.split('\t').count() == 2)
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
        // A list of the previous layout counted one number the columns split: nothing to hold
        // a column to, as before the ratchet began.
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
                let instead = match kind {
                    Kind::Comment => "no comment keeps an allocation: borrow or write into a buffer, or name the allocator where it is used",
                    _ => "borrow or write into a buffer, or give each value its allocator, from a parameter or the task's frame",
                };
                out.push(format!("`{krate}` has {now} {what} outside tests, over {shown} in {LIST} ({new}): {instead} ({RULE})"));
            } else if now < held {
                let lower = match now_row {
                    [0, 0, 0] => "remove the row".to_owned(),
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
            let was = base.get(krate).copied().unwrap_or([0; 3]);
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
fn total(counts: &BTreeMap<String, Vec<Site>>) -> Row {
    let mut row = [0; 3];
    for sites in counts.values() {
        for (sum, n) in row.iter_mut().zip(tally(sites)) {
            *sum += n;
        }
    }
    row
}

/// `counts`' totals, for a line of output.
fn summary(counts: &BTreeMap<String, Vec<Site>>) -> String {
    let [comments, expects, global] = total(counts);
    format!(
        "{comments} `// alloc:` lines left to convert, {expects} `allow`/`expect`s of the lint and {global} lines naming `Global`, outside tests, in {} crates",
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
            "{LIST} is not as command `write` writes it (its header, then a sorted `<crate><TAB><comments><TAB><expects><TAB><global>` row per crate with a site): run it"
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
    let global = global_lines(text);
    expect(global == [18, 24, 29, 33], &format!("the lines that name `Global`: {global:?}"));
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
        found.mods == [("t".to_owned(), true), ("live".to_owned(), false)],
        &format!("the modules declared out of line: {:?}", found.mods),
    );
    let inner = scan_file(p, "#![cfg(test)]\nmod deep;\nfn x() { Vec::new_in(Global) } // alloc: a test file's\n");
    expect(inner.test && inner.sites.is_empty() && inner.mods == [("deep".to_owned(), true)], &format!("{inner:?}"));
    // A field's attribute: the item ends with its struct, so the attribute's line alone is test.
    let field = scan_file(p, "struct S {\n    #[cfg(test)]\n    a: Vec<u8, Global>, // alloc: counted\n}\n");
    expect(
        field.sites == [(3, Kind::Comment), (3, Kind::Global)],
        &format!("a field under cfg(test) is counted: {:?}", field.sites),
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
    expect(total(&counts) == [5, 1, 3], &format!("the totals: {:?}", total(&counts)));

    // The ratchet. As recorded, nothing is wrong, and the list reads back as it was written.
    let rows = parse_rows(&render(&counts)).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    expect(
        rows == BTreeMap::from([("a".to_owned(), [4, 1, 3]), ("b/c".to_owned(), [1, 0, 0])]),
        &format!("the rows read back: {rows:?}"),
    );
    expect(problems(&counts, &rows, Some(&rows), None).is_empty(), "the tree as recorded is right");
    // Planted: one `// alloc:` line added. Named by its crate, its column, its place (given the
    // base's diff, which also added the `Global` on that line: not named, it is another column)
    // and the rule.
    let mut fewer = rows.clone();
    fewer.insert("a".to_owned(), [3, 1, 3]);
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
    fewer_expects.insert("a".to_owned(), [4, 0, 3]);
    let said = problems(&counts, &fewer_expects, None, None);
    expect(said.len() == 1 && said[0].contains("`a` has 1 `allow`/`expect`s"), &format!("one expect added is caught: {said:?}"));
    // Planted: one site removed. Its column must come down.
    let mut more = rows.clone();
    more.insert("b/c".to_owned(), [2, 0, 0]);
    let said = problems(&counts, &more, None, None);
    expect(
        said.len() == 1 && said[0].contains("`b/c`") && said[0].contains("lower its `comments` to 1"),
        &format!("one site removed is caught: {said:?}"),
    );
    // Planted: `global` off its row, either way: the list is written again.
    for off in [2, 4] {
        let mut stale = rows.clone();
        stale.insert("a".to_owned(), [4, 1, off]);
        let said = problems(&counts, &stale, None, None);
        expect(
            said.len() == 1 && said[0].contains("`a` has 3 lines that name `Global`") && said[0].contains("run command `write`"),
            &format!("`global` off its row by {off} is caught: {said:?}"),
        );
    }
    // Planted: a crate with no row is held to 0; a row with no site goes; a row raised above
    // the base's is caught even where the tree matches it.
    let said = problems(&counts, &BTreeMap::from([("a".to_owned(), [4, 1, 3])]), None, None);
    expect(said.len() == 1 && said[0].contains("`b/c` has 1") && said[0].contains("held"), &format!("a new crate starts at 0: {said:?}"));
    let mut gone = rows.clone();
    gone.insert("old".to_owned(), [5, 0, 2]);
    let said = problems(&counts, &gone, None, None);
    expect(
        !said.is_empty() && said.iter().all(|it| it.contains("`old`")) && said.iter().any(|it| it.contains("remove the row")),
        &format!("a crate gone: {said:?}"),
    );
    let said = problems(&counts, &rows, Some(&fewer), None);
    expect(said.len() == 1 && said[0].contains("the row `a`'s `comments` is 4 and was 3 at the base"), &format!("a row raised: {said:?}"));
    // `global` rises while the base has `// alloc:` lines left to convert, and not after.
    let mut converting = rows.clone();
    converting.insert("a".to_owned(), [5, 1, 2]);
    expect(problems(&counts, &rows, Some(&converting), None).is_empty(), "`global` rises while converting");
    let converted = BTreeMap::from([("a".to_owned(), [0, 1, 2])]);
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
    let lines = added_lines(diff).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    expect(
        lines == BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([3, 10, 11]))]),
        &format!("the lines added: {lines:?}"),
    );
    expect(
        parse_rows("a\t1\t0\t0\nb 2\n").is_err()
            && parse_rows("a\t1\t0\t0\na\t2\t0\t0\n").is_err()
            && parse_rows("a\t1\t0\n").is_err()
            && parse_rows("\t1\t0\t0\n").is_err(),
        "a bad row is refused",
    );
    let old = parse_rows("# header\na\t1\n");
    expect(
        old.as_ref().is_err_and(|e| e.contains("previous layout") && e.contains("`write`")),
        &format!("a row of the previous layout says to write the list: {old:?}"),
    );
    expect(previous_layout("# h\n\na\t3\n") && !previous_layout("# h\na\t3\t0\t1\n") && !previous_layout("# h\n"), "the previous layout");
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
}
