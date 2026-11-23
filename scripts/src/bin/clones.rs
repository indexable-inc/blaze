//! clones: the `.clone()` calls only go down, per crate (AGENTS.md bump-allocate, rule 11;
//! correct-by-construction, rule 76; type-then-lint-then-rule, rule 73). `DOC` below is what its
//! usage error prints; it says what a call is, how each is classified, what the text scan misses,
//! which code is a test's, and the rule.
//!
//!     blaze run -q -p scripts --bin clones -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin clones -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin clones -- '{"command": "classify", "sites": false}'
//!     blaze run -q -p scripts --bin clones -- selftest
//!
//! Why (2026-11-15): `.clone()` is either an O(1) duplicate of a handle that allocates nothing
//! (an `Arc`, an `Rc`, a reference) or a deep copy that allocates, and the reader cannot tell
//! which; a third of the tree's calls carry a `// clone: <why>` comment to say so, a convention
//! that should be a type. blaze-own names each: `Share` (`.share()`) for the handle,
//! `CloneIn` (`.clone_in(alloc)`) for the deep copy, its allocator named. This holds the count
//! of what is left from the text, with no compiler, as `casts` holds the integer casts, until a
//! blaze lint with a fix (rule 73) replaces it; `classify` says how much of a rewrite is
//! mechanical.
//!
//! Test code, the scanner, the file/crate walk helpers and the list/base parsing are
//! `blaze_script::ratchet`'s.

/// What the usage error prints.
const DOC: &str = r#"clones: the `.clone()` calls only go down, per crate (AGENTS.md bump-allocate, rule 11; correct-by-construction, rule 76).

    blaze run -q -p scripts --bin clones -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin clones -- '{"command": "write"}'
    blaze run -q -p scripts --bin clones -- '{"command": "classify", "sites": false}'
    blaze run -q -p scripts --bin clones -- selftest

    command   check     each crate's count equals its row's in the list: one over its row is
                        refused (its new calls named given `base`), one under it must be lowered
                        with `write`; a crate with no row is asked for one (`write`): a new crate
                        starts at its count; with `base`, no row is above the base's (CI)
              write     write the list from the tree as it is
              classify  print, as JSON, each crate's calls by kind (below), and with `sites`
                        each call's file, line, kind, what gave it and its comment
              findings  print every call, with its kind, as a `blaze_script::finding`
                        document (for `advise`): `handle` is the mechanical one (`.share()`)
    root      the checkout (".")
    base      the commit the change is against, or null: its list (`git show`) holds every
              row it has down, and the lines added since it (`git diff --unified=0`) name a
              crate's new calls
    sites     classify only: list every call too (false)

The list's row is `<crate><TAB><calls>`, of `.rs` files under crates/ (a directory named
`target` is not read).

A call is the word `clone` after a `.` and followed by `()` (spaces and line breaks between
allowed: `x.clone()`, `x . clone ( )`; counted at the line of the word), or after a path ending in the word `Clone` and `::`, and
followed by `(` (`Clone::clone(&x)`, `std::clone::Clone::clone(&x)`).
Comments, strings (raw and byte strings too) and character literals are skipped. Not counted:
  - `..clone()`: a range's end;
  - `Clone::clone` not called (`.map(Clone::clone)`), `.cloned()`, `.clone_from(..)`;
  - test code (below).
The count is read from the text, with no types, so it differs from a lint's:
  missed  `T::clone(&x)`, `<T as Clone>::clone(&x)`, `Arc::clone(&x)` (already explicit), and
          a call a macro makes from tokens
  over    a type's own inherent `clone()` that is not `Clone`'s

Each call's kind, for `classify`: first from its `// clone: <why>` comment, on the call's line
or above it (over comment lines and the lines of a statement not yet ended, to the first line
that ends in `;`, `{` or `}` or holds code with its own comment, eight lines at most), its
words read lowercased:
  handle      `refcount`, `count`, `handle`, `arc`, `rc`, `interned`, `cursor`, or the phrases
              `allocates nothing`, `no allocation`: an O(1) duplicate, `.share()`
  error-path  `error`, `errors`, `diagnostic`, `diagnostics`, or `error path`: a copy where
              speed does not matter
  deep        `vec`, `vector`, `string`, `map`, `set`, `list`, `box`, `copy`, `copies`, `deep`,
              `allocates`, `allocation` (each with its plural): `.clone_in(alloc)`
in that order; then, with no comment or one that says none of those, from the receiver:
  handle      a receiver `x` declared in the same function (from its last `fn` before the call)
              as `x: Arc<..>`/`x: Rc<..>` (`&`, `mut` and a path before it allowed) or given
              `x = Arc::..`/`Rc::..` (`new`, `clone`, `from`, any); a receiver `a.b.x` or
              `f().x` whose field `x` is declared `x: Arc<..>`/`x: Rc<..>` in the same file
  unknown     anything else
Each call says what gave its kind (`source`): `comment`, `local`, `field`, or `none`.

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
by its path under crates/ (`blaze-cargo`, `harness/harness-native`).
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_script::{
    Map, TempDir, Value, fail,
    finding::{self, Finding},
    input::{self, Fields, Input, InputError},
    json,
    ratchet::{self, BaseError, CrateScope, FileTests, Hit, ListError, Tests, comment_end, ident_len, is_ident, quote_end, string_end, word_end},
    read_text,
};

const SCRIPT: &str = "clones";
/// The list, from the root.
const LIST: &str = "tools/ratchets/clones.txt";
/// The comment that says why a call clones.
const MARK: &str = "// clone:";
/// How many lines above a call its comment is looked for, at most.
const LOOK_UP: usize = 8;
/// The handle types a receiver's declaration names.
const HANDLES: [&str; 2] = ["Arc", "Rc"];
/// A comment's words and phrases that say each kind, in the order they are tried.
const HANDLE_WORDS: [&str; 11] =
    ["refcount", "refcounts", "count", "counts", "handle", "handles", "arc", "rc", "interned", "cursor", "cursors"];
const HANDLE_PHRASES: [&str; 2] = ["allocates nothing", "no allocation"];
const ERROR_WORDS: [&str; 4] = ["error", "errors", "diagnostic", "diagnostics"];
const ERROR_PHRASES: [&str; 1] = ["error path"];
const DEEP_WORDS: [&str; 19] = [
    "vec", "vecs", "vector", "vectors", "string", "strings", "map", "maps", "set", "sets", "list", "lists", "box", "copy",
    "copies", "deep", "allocates", "allocation", "allocations",
];
/// What every message about a crate over its row ends with.
const RULE: &str = "AGENTS.md bump-allocate (rule 11) and correct-by-construction (rule 76): a copy that allocates \
                    names its allocator, and a duplicate that does not says so; `.clone()` says neither";
/// What to write instead, for a message.
const INSTEAD: &str = "say what the duplicate is, from blaze-own: `x.share()` (`Share`) where it is an O(1) duplicate \
                       of a handle that allocates nothing (an `Arc`, an `Rc`, a `&T`, a struct of such), \
                       `x.clone_in(alloc)` (`CloneIn`) for a deep copy that allocates, its allocator named";
/// The list's first lines.
const HEADER: &str = "# The `.clone()` and `Clone::clone(..)` calls outside test code, by crate (its directory under\n\
                      # crates/), read from the text. Only shrinks; a crate with no row is asked for one, so a new\n\
                      # crate starts at its count (AGENTS.md bump-allocate, correct-by-construction; `Share` and\n\
                      # `CloneIn` are crates/blaze-own's).\n\
                      # Written by scripts/src/bin/clones.rs (`{\"command\": \"write\"}`), checked in CI.\n\
                      # crate\tcalls\n";

struct Args {
    command: String,
    root: String,
    base: Option<String>,
    sites: bool,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "base", "sites"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["check", "write", "classify", "findings"], None)?,
            root: f.str("root", Some("."))?,
            base: f.str_or_null("base")?,
            sites: f.bool("sites", Some(false))?,
        })
    }
}

/// What a call's duplicate is, as far as the text says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// An O(1) duplicate of a handle that allocates nothing: `.share()`.
    Handle,
    /// A copy on an error path, where its cost does not matter.
    ErrorPath,
    /// A deep copy that allocates: `.clone_in(alloc)`.
    Deep,
    /// The text does not say.
    Unknown,
}

impl Kind {
    const ALL: [Kind; 4] = [Kind::Handle, Kind::ErrorPath, Kind::Deep, Kind::Unknown];

    fn name(self) -> &'static str {
        match self {
            Kind::Handle => "handle",
            Kind::ErrorPath => "error-path",
            Kind::Deep => "deep",
            Kind::Unknown => "unknown",
        }
    }

    /// The kind as a finding: a handle's duplicate is the mechanical rewrite, `.share()`.
    const fn finding(self) -> finding::Kind {
        const RULES: &str = "11 bump-allocate, 76 correct-by-construction";
        match self {
            Kind::Handle => finding::Kind {
                name: "handle",
                label: "handle clones",
                rule: RULES,
                instead: "`x.share()` (blaze-own's `Share`): an O(1) duplicate that allocates nothing",
                mechanical: true,
            },
            Kind::ErrorPath => finding::Kind {
                name: "error-path",
                label: "error-path clones",
                rule: RULES,
                instead: "`x.clone_in(alloc)` with the error's allocator, or move the value into the error",
                mechanical: false,
            },
            Kind::Deep => finding::Kind {
                name: "deep",
                label: "deep clones",
                rule: RULES,
                instead: "`x.clone_in(alloc)` (blaze-own's `CloneIn`), the allocator named, or a borrow",
                mechanical: false,
            },
            Kind::Unknown => finding::Kind {
                name: "unknown",
                label: "unexplained clones",
                rule: RULES,
                instead: "`x.share()` for a handle, `x.clone_in(alloc)` for a deep copy, a borrow where one does",
                mechanical: false,
            },
        }
    }
}

/// What gave a call its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    /// Its `// clone:` comment (also when the comment says none of the kinds and nothing else does).
    Comment,
    /// Its receiver's declaration in the same function.
    Local,
    /// Its receiver field's declaration in the same file.
    Field,
    /// Nothing: no comment, no declaration found.
    None,
}

impl Source {
    const ALL: [Source; 4] = [Source::Comment, Source::Local, Source::Field, Source::None];

    fn name(self) -> &'static str {
        match self {
            Source::Comment => "comment",
            Source::Local => "local",
            Source::Field => "field",
            Source::None => "none",
        }
    }
}

/// What [`classify`] says of one call: its kind, what gave it, and its comment's text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Classified {
    kind: Kind,
    source: Source,
    why: Option<String>,
}

/// One call: its file, its line, and what [`Classified`] says of it.
type Site = Hit<Classified>;

/// Emits each call of `text` outside its test code, classified, in order.
fn scan_sites(text: &str, found: &FileTests, emit: &mut dyn FnMut(usize, Classified)) {
    let lines: Vec<&str> = text.split('\n').collect();
    let (calls, fns) = clone_calls(text);
    // A field's name, and whether this file declares it a handle: read once per name.
    let mut fields: BTreeMap<String, bool> = BTreeMap::new();
    for call in &calls {
        if !found.test[call.line - 1] {
            emit(call.line, classify(text, &lines, &fns, &mut fields, call));
        }
    }
}

/// The calls of one file's text outside test code, classified (for selftest).
fn scan_file(tests: &Tests, text: &str) -> Vec<(usize, Classified)> {
    let found = tests.of(text);
    let mut sites = Vec::new();
    if !found.whole {
        scan_sites(text, &found, &mut |line, what| sites.push((line, what)));
    }
    sites
}

/// A call's receiver, for a method call: the last name of the expression before its `.`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Receiver {
    name: String,
    /// The name is a field (`a.b.x`, `f().x`), not a local.
    field: bool,
}

/// One call, test code included: its line (from 1), its byte, and its receiver where it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    line: usize,
    at: usize,
    receiver: Option<Receiver>,
}

/// Each call in `text`, in text order, and the byte of each word `fn`: comments, strings and
/// characters skipped as [`ratchet::item_end`] skips them.
fn clone_calls(text: &str) -> (Vec<Call>, Vec<usize>) {
    let bytes = text.as_bytes();
    let mut calls = Vec::new();
    let mut fns = Vec::new();
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
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let end = word_end(text, at);
                match &text[at..end] {
                    "fn" => fns.push(at),
                    "clone" => {
                        if let Some(receiver) = call_at(text, at, end) {
                            line += text[counted..at].matches('\n').count();
                            counted = at;
                            calls.push(Call { line, at, receiver });
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
    (calls, fns)
}

/// Whether the word `clone` from byte `at` to `end` is a call: `Some` with the receiver of a
/// method call (`x.clone()`; `None` inside when no name is before the `.`), `Some(None)` for a
/// call through `Clone` (`Clone::clone(&x)`), `None` for no call.
fn call_at(text: &str, at: usize, end: usize) -> Option<Option<Receiver>> {
    let after = text[end..].trim_start();
    let before = text[..at].trim_end();
    if let Some(dot) = before.strip_suffix('.') {
        // `..clone()`: a range's end, not a method.
        if dot.ends_with('.') {
            return None;
        }
        let args = after.strip_prefix('(')?.trim_start();
        return args.starts_with(')').then(|| receiver_of(dot));
    }
    let head = before.strip_suffix("::")?.trim_end().strip_suffix("Clone")?;
    if head.as_bytes().last().is_some_and(|b| is_ident(*b)) {
        return None;
    }
    after.starts_with('(').then_some(None)
}

/// The receiver whose text ends `before` (the `.` taken off): the last name of a run of names
/// and dots, a field when a dot or a closing bracket is before it.
fn receiver_of(before: &str) -> Option<Receiver> {
    let s = before.trim_end();
    let len = s.bytes().rev().take_while(|b| is_ident(*b) || *b == b'.').count();
    let run = &s[s.len() - len..];
    let name = run.rsplit('.').next().unwrap_or("");
    if name.is_empty() || name.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    let field = run.contains('.') || s[..s.len() - len].trim_end().ends_with([')', ']']);
    Some(Receiver { name: name.to_owned(), field })
}

/// A call's kind: by its comment, then by its receiver's declaration.
fn classify(text: &str, lines: &[&str], fns: &[usize], fields: &mut BTreeMap<String, bool>, call: &Call) -> Classified {
    let why = clone_comment(lines, call.line);
    if let Some(kind) = why.as_deref().and_then(kind_of) {
        return Classified { kind, source: Source::Comment, why };
    }
    let declared = match &call.receiver {
        Some(r) if r.field => {
            let handle = *fields.entry(r.name.clone()).or_insert_with(|| declared_handle(text, &r.name, false));
            handle.then_some(Source::Field)
        }
        Some(r) => {
            let from = fns[..fns.partition_point(|it| *it < call.at)].last().copied().unwrap_or(0);
            declared_handle(&text[from..call.at], &r.name, true).then_some(Source::Local)
        }
        None => None,
    };
    match declared {
        Some(source) => Classified { kind: Kind::Handle, source, why },
        None => {
            let source = if why.is_some() { Source::Comment } else { Source::None };
            Classified { kind: Kind::Unknown, source, why }
        }
    }
}

/// The text after `// clone:` of the comment of the call at `line` (from 1): on its line, or
/// above it over comment lines and a statement's unended lines, [`LOOK_UP`] lines at most.
fn clone_comment(lines: &[&str], line: usize) -> Option<String> {
    let why = |l: &str| l.find(MARK).map(|n| l[n + MARK.len()..].trim().to_owned());
    if let Some(it) = why(lines[line - 1]) {
        return Some(it);
    }
    let mut at = line - 1;
    for _ in 0..LOOK_UP {
        at = at.checked_sub(1)?;
        let l = lines[at].trim();
        if l.starts_with("//") {
            if let Some(it) = why(l) {
                return Some(it);
            }
            continue;
        }
        let code = l.split("//").next().unwrap_or("").trim_end();
        // A line of code with a comment of its own, or a line that ends a statement or opens or
        // closes a block: the comment above is not this call's.
        if code.len() < l.len() || code.is_empty() || code.ends_with([';', '{', '}']) {
            return None;
        }
    }
    None
}

/// The kind a comment's text says, if it says one: its words lowercased, tried in the order
/// handle, error path, deep.
fn kind_of(why: &str) -> Option<Kind> {
    let lower = why.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let says = |list: &[&str], phrases: &[&str]| {
        words.iter().any(|w| list.contains(w)) || phrases.iter().any(|it| lower.contains(it))
    };
    if says(&HANDLE_WORDS, &HANDLE_PHRASES) {
        Some(Kind::Handle)
    } else if says(&ERROR_WORDS, &ERROR_PHRASES) {
        Some(Kind::ErrorPath)
    } else if says(&DEEP_WORDS, &[]) {
        Some(Kind::Deep)
    } else {
        None
    }
}

/// Whether `scope` declares `name` a handle: `name: Arc<..>` or `name: Rc<..>`, and with
/// `assigned`, `name = Arc::..` or `name = Rc::..`.
fn declared_handle(scope: &str, name: &str, assigned: bool) -> bool {
    let bytes = scope.as_bytes();
    let mut from = 0;
    // Bounded: every turn moves `from` past one match of `name`.
    while let Some(n) = scope[from..].find(name) {
        let at = from + n;
        let end = at + name.len();
        from = end;
        let whole = at.checked_sub(1).is_none_or(|i| !is_ident(bytes[i])) && bytes.get(end).is_none_or(|b| !is_ident(*b));
        if !whole {
            continue;
        }
        let rest = scope[end..].trim_start();
        if let Some(ty) = rest.strip_prefix(':') {
            if !ty.starts_with(':') && handle_type(ty) {
                return true;
            }
        } else if let Some(value) = rest.strip_prefix('=') {
            if assigned && !value.starts_with('=') && handle_value(value) {
                return true;
            }
        }
    }
    false
}

/// The run of names and `::` that `s` starts with.
fn path_run(s: &str) -> &str {
    let n = s.bytes().take_while(|b| is_ident(*b) || *b == b':').count();
    &s[..n]
}

/// Whether the type `ty` starts with is `Arc<..>` or `Rc<..>`, behind `&`, `mut` or a path.
fn handle_type(ty: &str) -> bool {
    let ty = ty.trim_start().trim_start_matches('&').trim_start();
    let ty = ty.strip_prefix("mut ").unwrap_or(ty).trim_start();
    let path = path_run(ty);
    let last = path.rsplit("::").next().unwrap_or("");
    HANDLES.contains(&last) && ty[path.len()..].trim_start().starts_with('<')
}

/// Whether the value `value` starts with is a call of an associated function of `Arc` or `Rc`.
fn handle_value(value: &str) -> bool {
    let segments: Vec<&str> = path_run(value.trim_start()).split("::").collect();
    segments.len() >= 2 && HANDLES.contains(&segments[segments.len() - 2])
}

/// Each crate's calls outside test code, in file and line order; a crate with none is absent.
fn count(tests: &Tests, root: &Path) -> BTreeMap<String, Vec<Site>> {
    ratchet::walk_with(SCRIPT, root, CrateScope::AllDirs, tests, |_| false, |file, emit| {
        scan_sites(file.text, file.tests, emit);
    })
}

/// What is wrong: `counts` the tree's calls by crate, `rows` the list's, `base_rows` the base's
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
        let now = sites.len();
        let Some(held) = rows.get(krate).copied() else {
            out.push(format!(
                "`{krate}` has {now} `.clone()` calls outside tests and no row in {LIST}: a new crate starts at its count, \
                 so add its row (command `write`); it only falls from there"
            ));
            continue;
        };
        if now > held {
            let new = match added {
                Some(added) => {
                    let new: Vec<String> = sites
                        .iter()
                        .filter(|it| added.get(&it.path).is_some_and(|lines| lines.contains(&it.line)))
                        .map(|it| format!("{}:{}", it.path, it.line))
                        .collect();
                    if new.is_empty() {
                        "none is on a line added since the base: a call moved in from another crate, or a file git does not track".to_owned()
                    } else {
                        format!("new: {}", new.join(", "))
                    }
                }
                None => "give `base`, the commit the change is against, to have the new ones named".to_owned(),
            };
            out.push(format!("`{krate}` has {now} `.clone()` calls outside tests, over its row's {held} in {LIST} ({new}): {INSTEAD} ({RULE})"));
        } else if now < held {
            let lower = if now == 0 { "remove the row".to_owned() } else { format!("lower it to {now}") };
            out.push(format!(
                "`{krate}` has {now} `.clone()` calls outside tests, under its row's {held} in {LIST}: {lower} (command `write`); the list only shrinks"
            ));
        }
    }
    if let Some(base) = base_rows {
        for (krate, now) in rows {
            // A crate the base had no row for starts at its count.
            let Some(then) = base.get(krate).copied() else { continue };
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
    format!("{total} `.clone()` calls outside tests, in {} crates", counts.len())
}

/// The calls of `sites` by kind, and with `sources`, by what gave the kind, as a JSON object,
/// `calls` first.
fn tally(sites: &[Site], sources: bool) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("calls".to_owned(), Value::from(sites.len()));
    for kind in Kind::ALL {
        out.insert(kind.name().to_owned(), Value::from(sites.iter().filter(|it| it.what.kind == kind).count()));
    }
    if sources {
        let mut by = Map::new();
        for source in Source::ALL {
            by.insert(source.name().to_owned(), Value::from(sites.iter().filter(|it| it.what.source == source).count()));
        }
        out.insert("sources".to_owned(), Value::Object(by));
    }
    out
}

/// What `classify` prints: the totals by kind and by source, each crate's by kind, and with
/// `with_sites` every call.
fn report(counts: &BTreeMap<String, Vec<Site>>, with_sites: bool) -> Value {
    let all: Vec<Site> = counts.values().flatten().cloned().collect();
    let mut out = tally(&all, true);
    let mut crates = Map::new();
    for (krate, sites) in counts {
        crates.insert(krate.clone(), Value::Object(tally(sites, false)));
    }
    out.insert("crates".to_owned(), Value::Object(crates));
    if with_sites {
        let sites = all
            .iter()
            .map(|it| {
                let mut site = Map::new();
                site.insert("path".to_owned(), Value::from(it.path.clone()));
                site.insert("line".to_owned(), Value::from(it.line));
                site.insert("kind".to_owned(), Value::from(it.what.kind.name()));
                site.insert("source".to_owned(), Value::from(it.what.source.name()));
                site.insert("why".to_owned(), it.what.why.clone().map_or(Value::Null, Value::from));
                Value::Object(site)
            })
            .collect();
        out.insert("sites".to_owned(), Value::Array(sites));
    }
    Value::Object(out)
}

fn main_with(tests: &Tests, it: &Args) {
    // `Path(root).resolve()`.
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    if !root.join("crates").is_dir() {
        fail(SCRIPT, &format!("{} has no crates/ directory", root.display()));
    }
    let counts = count(tests, &root);
    if it.command == "findings" {
        let found = counts.iter().flat_map(|(krate, sites)| {
            sites.iter().map(|it| Finding {
                krate: krate.clone(),
                file: it.path.clone(),
                line: it.line,
                kind: it.what.kind.finding().name,
                detail: it.what.why.clone(),
            })
        });
        finding::print(SCRIPT, &Kind::ALL.map(Kind::finding), found.collect());
        return;
    }
    if it.command == "classify" {
        let shown = json::to_string_pretty(&report(&counts, it.sites)).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
        println!("{shown}");
        return;
    }
    let text = ratchet::render(HEADER, &counts, &[]);
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
    let rows = ratchet::parse_rows(LIST, &listed).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let (base_rows, added) = match &it.base {
        Some(base) => {
            let (rows, added) = ratchet::at_base(&root, base, LIST).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
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
            "{LIST} is not as command `write` writes it (its header, then a sorted `<crate><TAB><calls>` row per crate with a call): run it"
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

/// The calls of the counted crates, as `crate: path:line, ...`, for a selftest's message.
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

/// One file's calls, as (line, kind) pairs, for a selftest's expectation.
fn lines_of(sites: &[Site], path: &str) -> Vec<usize> {
    sites.iter().filter(|it| it.path == path).map(|it| it.line).collect()
}

/// The selftest's file of crate `a`. Line by line, what each call is:
/// - 1, `x.clone()`: counted, unknown;
/// - 2, `Clone::clone(&v)`: counted, unknown;
/// - 3, `x . clone ( )`: counted, unknown;
/// - 4, a call in a string and in a comment: not counted;
/// - 5, one in a block comment, `clone_from`, `cloned`: not counted;
/// - 6, `Clone::clone` not called: not counted;
/// - 7, `S::clone(x)`: a miss DOC names, not counted;
/// - 8, `a: Arc<u8>` then `a.clone()`: handle, by the local;
/// - 9, `let r = std::rc::Rc::new(1)` then `r.clone()`: handle, by the local;
/// - 10-11, a `// clone:` comment saying a refcount, above the call: handle, by the comment;
/// - 12, a comment on the call's line saying the error path: error-path;
/// - 13-16, a comment saying a deep copy of a vector, above the call in a block: deep;
/// - 17, a call after a block's end, no comment, `v: &W`: unknown (the comment at 14 is not its);
/// - 19, in `#[cfg(test)] mod tests`: test code;
/// - 21, after `#[test]`: test code;
/// - 22-23, `self.inner.clone()` with `inner: Arc<u8>` in the file: handle, by the field;
/// - 24, `x: &S`: unknown.
const FIXTURE: &str = r#"fn a(x: &String) -> String { x.clone() }
fn b(v: Vec<u8>) -> Vec<u8> { Clone::clone(&v) }
fn c(x: &Foo) -> Foo { x . clone ( ) }
const S: &str = "x.clone()"; // y.clone()
/* z.clone() */ fn d(x: &Foo) { x.clone_from(x); let _ = x.cloned(); }
fn e(xs: &[u8]) -> Vec<u8> { xs.iter().map(Clone::clone).collect() }
fn p(x: &S) -> S { S::clone(x) }
fn g(a: Arc<u8>) -> Arc<u8> { a.clone() }
fn h() { let r = std::rc::Rc::new(1); let _ = r.clone(); }
    // clone: a refcount of the table
fn i(t: &Table) -> Table { t.clone() }
fn j(e: &E) -> E { e.clone() } // clone: on the error path
fn k(v: &Vec<u8>) -> Vec<u8> {
    // clone: a deep copy of the vector
    v.clone()
}
fn l(v: &W) -> W { v.clone() }
#[cfg(test)]
mod tests { fn t(x: &S) -> S { x.clone() } }
#[test]
fn a_test() { let _ = 1.clone(); }
struct H { inner: Arc<u8> }
impl H { fn m(&self) -> Arc<u8> { self.inner.clone() } }
fn n(x: &S) -> S { x.clone() }
"#;

/// The lines of [`FIXTURE`]'s calls outside its test code, each with its kind and what gave it.
const FIXTURE_SITES: [(usize, Kind, Source); 11] = [
    (1, Kind::Unknown, Source::None),
    (2, Kind::Unknown, Source::None),
    (3, Kind::Unknown, Source::None),
    (8, Kind::Handle, Source::Local),
    (9, Kind::Handle, Source::Local),
    (11, Kind::Handle, Source::Comment),
    (12, Kind::ErrorPath, Source::Comment),
    (15, Kind::Deep, Source::Comment),
    (17, Kind::Unknown, Source::None),
    (23, Kind::Handle, Source::Field),
    (24, Kind::Unknown, Source::None),
];

fn selftest(tests: &Tests) {
    // What a call is, and its receiver.
    let calls = |text: &str| clone_calls(text).0.iter().map(|it| it.line).collect::<Vec<usize>>();
    // Counted at the line of its word `clone`.
    expect(calls("a.clone()\nb\n.clone\n(\n)\n") == [1, 3], "a call over spaces and line breaks");
    expect(calls("0..clone()\nx.clone(y)\nr#clone()\nstd::clone::Clone::clone(&x)\nMyClone::clone(&x)") == [4], "a range, an argument, a raw name and another type's path are no call");
    let receiver = |text: &str| clone_calls(text).0.first().and_then(|it| it.receiver.clone());
    expect(receiver("x.clone()") == Some(Receiver { name: "x".to_owned(), field: false }), "a local receiver");
    expect(receiver("a.b.x.clone()") == Some(Receiver { name: "x".to_owned(), field: true }), "a field of a path");
    expect(receiver("f().x.clone()") == Some(Receiver { name: "x".to_owned(), field: true }), "a field of a call");
    expect(receiver("f().clone()").is_none() && receiver("1.clone()").is_none(), "no name before the `.`");

    // A comment's kind, in order.
    expect(kind_of("a refcount") == Some(Kind::Handle) && kind_of("an `Arc` count, the error's span") == Some(Kind::Handle), "a handle first");
    expect(kind_of("the error's span") == Some(Kind::ErrorPath) && kind_of("on the error path, a Vec") == Some(Kind::ErrorPath), "an error path before a copy");
    expect(kind_of("a deep copy") == Some(Kind::Deep) && kind_of("a copy, no allocation") == Some(Kind::Handle), "a copy, unless it allocates nothing");
    expect(kind_of("the source's line").is_none() && kind_of("arcane").is_none(), "words, not parts of words");
    // A declaration's kind.
    expect(declared_handle("fn f(x: &mut std::sync::Arc<T>) {", "x", true), "a parameter behind `&mut` and a path");
    expect(declared_handle("let mut x = Rc::from(v);", "x", true) && !declared_handle("let mut x = Rc::from(v);", "x", false), "an assignment, in a function only");
    expect(!declared_handle("xs: Arc<T>, x: Box<T>, y == Arc::new(1)", "x", true), "another name, another type, a comparison");

    // One file: every case of [`FIXTURE`], test code in, then out.
    let all = calls(FIXTURE);
    let mut want: Vec<usize> = FIXTURE_SITES.iter().map(|it| it.0).collect();
    want.extend([19, 21]);
    want.sort();
    expect(all == want, &format!("the fixture's calls, test code in: found {all:?}, wanted {want:?}"));
    let found = scan_file(tests, FIXTURE);
    let got: Vec<(usize, Kind, Source)> = found.iter().map(|(line, it)| (*line, it.kind, it.source)).collect();
    expect(got == FIXTURE_SITES, &format!("the fixture's calls outside test code, classified: {got:?}"));
    // Planted: each cause removed, its effect goes.
    let planted = scan_file(tests, &FIXTURE.replace("#[cfg(test)]\n", "\n"));
    expect(planted.iter().any(|(line, _)| *line == 19), "`t` outside test code is a site");
    let planted = scan_file(tests, &FIXTURE.replace("    // clone: a deep copy of the vector", "    // a deep copy of the vector"));
    expect(planted.iter().any(|(line, it)| *line == 15 && it.kind == Kind::Unknown), "line 15 without its comment is unknown");
    let planted = scan_file(tests, &FIXTURE.replace("fn g(a: Arc<u8>)", "fn g(a: Box<u8>)"));
    expect(planted.iter().any(|(line, it)| *line == 8 && it.kind == Kind::Unknown), "line 8 without its `Arc` is unknown");
    let planted = scan_file(tests, &FIXTURE.replace("struct H { inner: Arc<u8> }", "struct H { inner: Box<u8> }"));
    expect(planted.iter().any(|(line, it)| *line == 23 && it.kind == Kind::Unknown), "line 23 without its field's `Arc` is unknown");

    // A tree: crates, test files by path and by declaration, a nested crate.
    let tmp = TempDir::new("clones-selftest-").unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    let one = "pub fn f(x: &S) -> S { x.clone() }\n";
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
    let counts = count(tests, tmp.path());
    let a = counts.get("a").map_or(&[][..], Vec::as_slice);
    let lib: Vec<usize> = FIXTURE_SITES.iter().map(|it| it.0).collect();
    expect(
        counts.len() == 2
            && lines_of(a, "crates/a/build.rs") == [1]
            && lines_of(a, "crates/a/src/lib.rs") == lib
            && lines_of(a, "crates/a/src/live.rs") == [1]
            && a.len() == 13
            && counts.get("b/c").is_some_and(|it| lines_of(it, "crates/b/c/src/lib.rs") == [1] && it.len() == 1),
        &format!("the tree's calls: {}", shown(&counts)),
    );
    // The report: totals by kind and source, each crate's by kind.
    let said = report(&counts, false);
    let at = |path: &[&str]| path.iter().try_fold(&said, |it, key| it.get(*key)).and_then(Value::as_u64);
    expect(
        at(&["calls"]) == Some(14)
            && at(&["handle"]) == Some(4)
            && at(&["error-path"]) == Some(1)
            && at(&["deep"]) == Some(1)
            && at(&["unknown"]) == Some(8)
            && at(&["sources", "comment"]) == Some(3)
            && at(&["sources", "local"]) == Some(2)
            && at(&["sources", "field"]) == Some(1)
            && at(&["sources", "none"]) == Some(8)
            && at(&["crates", "a", "calls"]) == Some(13)
            && at(&["crates", "b/c", "unknown"]) == Some(1)
            && said.get("sites").is_none(),
        &format!("the report: {said}"),
    );
    let listed = report(&counts, true);
    expect(listed.get("sites").and_then(Value::as_array).is_some_and(|it| it.len() == 14), "the report with its sites");

    // The ratchet. As recorded, nothing is wrong, and the list reads back as it was written.
    let rows = ratchet::parse_rows(LIST, &ratchet::render(HEADER, &counts, &[])).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    expect(rows == BTreeMap::from([("a".to_owned(), 13), ("b/c".to_owned(), 1)]), &format!("the rows read back: {rows:?}"));
    expect(problems(&counts, &rows, Some(&rows), None).is_empty(), "the tree as recorded is right");
    // Planted: one call added. Named by its crate, its place (given the base's diff), what to
    // write instead and the rule.
    let mut fewer = rows.clone();
    fewer.insert("a".to_owned(), 12);
    let added = BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([24]))]);
    let said = problems(&counts, &fewer, None, Some(&added));
    expect(
        said.len() == 1
            && said[0].contains("`a` has 13 `.clone()` calls outside tests, over its row's 12")
            && said[0].contains("new: crates/a/src/lib.rs:24)")
            && said[0].contains("share()")
            && said[0].contains("bump-allocate"),
        &format!("one call added is caught and named: {said:?}"),
    );
    // Planted: one call removed. The row must come down; a crate with none left goes.
    let mut more = rows.clone();
    more.insert("b/c".to_owned(), 2);
    let said = problems(&counts, &more, None, None);
    expect(said.len() == 1 && said[0].contains("`b/c`") && said[0].contains("lower it to 1"), &format!("one call removed is caught: {said:?}"));
    let mut gone = rows.clone();
    gone.insert("old".to_owned(), 3);
    let said = problems(&counts, &gone, None, None);
    expect(said.len() == 1 && said[0].contains("`old`") && said[0].contains("remove the row"), &format!("a crate gone: {said:?}"));
    // Planted: a crate with no row is asked for one at its count; a row raised above the base's
    // is caught even where the tree matches it; a row the base did not have is not.
    let only_a = BTreeMap::from([("a".to_owned(), 13)]);
    let said = problems(&counts, &only_a, None, None);
    expect(said.len() == 1 && said[0].contains("`b/c` has 1") && said[0].contains("no row"), &format!("a new crate is asked for its row: {said:?}"));
    let said = problems(&counts, &rows, Some(&fewer), None);
    expect(said.len() == 1 && said[0].contains("the row `a` is 13 and was 12 at the base"), &format!("a row raised: {said:?}"));
    expect(problems(&counts, &rows, Some(&only_a), None).is_empty(), "a crate new since the base starts at its count");

    // The base's diff: hunks of added lines, a deleted file, a hunk that only removes.
    // The first hunk adds a line whose text starts `++ `: it is no file's header.
    let diff = "diff --git a/crates/a/src/lib.rs b/crates/a/src/lib.rs\n--- a/crates/a/src/lib.rs\n+++ b/crates/a/src/lib.rs\n\
        @@ -2,0 +3 @@ fn f() {\n+++ b/not/a/file\n@@ -9 +10,2 @@\n-old\n+y\n+z\n@@ -20 +21,0 @@\n-w\n\
        diff --git a/crates/a/src/gone.rs b/crates/a/src/gone.rs\n--- a/crates/a/src/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-v\n";
    let lines = ratchet::added_lines(diff).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    expect(
        lines == BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([3, 10, 11]))]),
        &format!("the lines added: {lines:?}"),
    );
    expect(ratchet::added_lines("@@ nonsense\n").is_err_and(|e| matches!(e, BaseError::Hunk { .. })), "a hunk that is none is refused");
    expect(
        ratchet::parse_rows(LIST, "a\t1\nb 2\n").is_err_and(|e| e == ListError::NotARow { list: LIST.to_owned(), line: 2, text: "b 2".to_owned() })
            && ratchet::parse_rows(LIST, "a\t1\na\t2\n").is_err_and(|e| matches!(e, ListError::TwoRows { line: 2, .. }))
            && ratchet::parse_rows(LIST, "a\t1\t0\n").is_err()
            && ratchet::parse_rows(LIST, "a\tx\n").is_err()
            && ratchet::parse_rows(LIST, "\t1\n").is_err(),
        "a bad row is refused, naming its line",
    );
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tests = Tests::new(SCRIPT);
    match args.as_slice() {
        [one] if one == "selftest" => selftest(&tests),
        [one] => main_with(&tests, &input::parse::<Args>(one, SCRIPT)),
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
        let _ = Tests::new(SCRIPT);
    }

    /// The script's own cases run under `blaze test` and `cargo test` too: one that fails ends
    /// the test process with status 1, which fails the run.
    #[test]
    fn the_selftest_holds() {
        selftest(&Tests::new(SCRIPT));
    }
}
