//! The `owns` ratchet: each `#[blaze_memo::tracked(.., owns)]`, `#[blaze_memo::interned(..,
//! owns)]` and call of `blaze_memo::keep_owned(..)` is debt against the end state (results as
//! flat records read in place): a value in an arena that owns memory, on the arena's drop list.
//! Counted per crate from its library's source (`src/`, or the crate's directory where the
//! library is at its top) and held to `owns.txt` (a crate with no row is held to 0), which may only go down: the count is how the debt is paid. Lines of
//! `owns.txt`: `<crate dir>\t<count>`, the attribute sites and the `keep_owned` calls together.
//!
//! The invariant: no crate has more `owns` sites than its row says.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// The `owns` sites in `text`: its `owns` attributes and its `keep_owned` calls.
fn sites(text: &str) -> usize {
    attribute_sites(text) + keep_owned_sites(text)
}

/// The calls of `keep_owned` in `text`: the identifier `keep_owned` as a whole word, however
/// its path is spelled (`blaze_memo::keep_owned`, a bare one after a `use`), followed by `(`
/// or a turbofish `::<`, outside a `//` comment line (a doc naming it is no call). A `use` of
/// it is no call. Not seen: a call through a renamed import (`use .. keep_owned as k`).
fn keep_owned_sites(text: &str) -> usize {
    const NAME: &str = "keep_owned";
    let mut count = 0;
    for line in text.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find(NAME) {
            let before = rest[..at].chars().next_back();
            let after = rest[at + NAME.len()..].trim_start();
            rest = &rest[at + NAME.len()..];
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            if after.starts_with('(') || after.starts_with("::<") {
                count += 1;
            }
        }
    }
    count
}

/// The `owns` attribute sites in `text`: every attribute `#[<path>(..)]` whose path's last
/// segment is `tracked` or `interned`, however spelled (`blaze_memo::tracked`, a bare `tracked`
/// after a `use`, a renamed import's path ending the same), whose options (up to its closing
/// `)]`) name `owns` as a word. Spelling-independent by the attribute's last segment, because a
/// marker the macro emitted lives in the expansion, which a test of this crate cannot see
/// without linking every crate that uses the macro.
fn attribute_sites(text: &str) -> usize {
    let mut count = 0;
    let mut rest = text;
    while let Some(at) = rest.find("#[") {
        let after = &rest[at + 2..];
        rest = after;
        let Some(open) = after.find('(') else { break };
        let path = after[..open].trim();
        if path.is_empty() || !path.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ':') {
            continue;
        }
        let last = path.rsplit("::").next().unwrap_or(path);
        if last != "tracked" && last != "interned" {
            continue;
        }
        let options = &after[open + 1..];
        let end = options.find(")]").unwrap_or(options.len());
        if options[..end].split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|word| word == "owns") {
            count += 1;
        }
    }
    count
}

/// The directory of `krate`'s library source: that of its `[lib] path` where its `Cargo.toml`
/// names one (a `[lib] path = "mod.rs"` puts it at the crate's top, which a walk of `src/` alone
/// would never count), else `src/`; `None` for a directory with no manifest or no library (a
/// workspace of its own, as `crates/harness`).
fn lib_root(krate: &Path) -> Option<PathBuf> {
    let manifest = fs::read_to_string(krate.join("Cargo.toml")).ok()?;
    let mut in_lib = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_lib = line == "[lib]";
            continue;
        }
        if !in_lib {
            continue;
        }
        let Some(value) = line.strip_prefix("path").map(str::trim_start).and_then(|it| it.strip_prefix('=')) else {
            continue;
        };
        let file = krate.join(value.trim().trim_matches('"'));
        return Some(file.parent().expect("a library file is in a directory").to_path_buf());
    }
    let src = krate.join("src");
    src.is_dir().then_some(src)
}

/// The directories of a crate's top that are not its library's source, when the library is at
/// the top (no `src/`).
const NOT_SOURCE: &[&str] = &["tests", "benches", "examples", "target"];

/// Adds the sites of every `.rs` file under `dir` to `total`, but none of a directory that is
/// a crate of its own (has a `Cargo.toml`), nor, where `top` is the crate's own directory, of
/// its [`NOT_SOURCE`] directories.
fn walk(dir: &Path, top: bool, total: &mut usize) {
    let entries = fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let skipped = path.join("Cargo.toml").exists()
                || (top && NOT_SOURCE.iter().any(|it| name == *it));
            if !skipped {
                walk(&path, false, total);
            }
        } else if path.extension().is_some_and(|it| it == "rs") {
            let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            *total += sites(&text);
        }
    }
}

fn counts(crates: &Path) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(crates).expect("the workspace's crates/").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "blaze-memo" || name == "blaze-memo-macros" || !entry.path().is_dir() {
            continue; // the attribute's own docs and tests
        }
        let mut total = 0;
        if let Some(root) = lib_root(&entry.path()) {
            let top = root == entry.path();
            walk(&root, top, &mut total);
        }
        if total > 0 {
            out.insert(name, total);
        }
    }
    out
}

fn baseline(text: &str) -> BTreeMap<String, usize> {
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let (name, count) = line.split_once('\t').unwrap_or_else(|| panic!("owns.txt: `{line}` is not `crate\\tcount`"));
            (name.to_owned(), count.parse().unwrap_or_else(|_| panic!("owns.txt: `{count}` is not a count")))
        })
        .collect()
}

fn over(counts: &BTreeMap<String, usize>, held: &BTreeMap<String, usize>) -> Vec<String> {
    counts
        .iter()
        .filter(|(name, n)| **n > held.get(*name).copied().unwrap_or(0))
        .map(|(name, n)| format!("{name}: {n} `owns` sites, owns.txt holds it to {}", held.get(name).copied().unwrap_or(0)))
        .collect()
}

#[test]
fn owns_only_goes_down() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let held = baseline(&fs::read_to_string(here.join("owns.txt")).expect("crates/blaze-memo/owns.txt"));
    let found = counts(here.parent().expect("crates/"));
    let over = over(&found, &held);
    assert!(over.is_empty(), "more `owns` than the ratchet allows (lower it, never raise it):\n{}", over.join("\n"));
}

/// The control: a crate one site over its row is refused, and the attribute forms are counted.
#[test]
fn the_ratchet_catches_one_more() {
    assert_eq!(sites("#[blaze_memo::tracked(returns(ref), owns)]\nfn f() {}"), 1);
    assert_eq!(sites("#[blaze_memo::interned(owns, debug)]\nstruct S;"), 1);
    assert_eq!(sites("#[blaze_memo::tracked(returns(ref))]\nfn owns() {}"), 0);
    // Spelling-independent: a bare `tracked` after `use blaze_memo::tracked`, and another path.
    assert_eq!(sites("use blaze_memo::tracked;\n#[tracked(owns)]\nfn f() {}"), 1);
    assert_eq!(sites("#[memo::interned(debug, owns)]\nstruct S;"), 1);
    assert_eq!(sites("#[derive(Clone)]\n#[cfg(owns)]\nfn f() {}"), 0);
    // `keep_owned` calls, however the path is spelled; a `use`, a doc line and a longer name
    // are no call.
    assert_eq!(sites("let a = blaze_memo::keep_owned(db.memo(), tree);"), 1);
    assert_eq!(sites("use blaze_memo::keep_owned;\nlet a = keep_owned (memo, x);"), 1);
    assert_eq!(sites("let a = blaze_memo::keep_owned::<Tree>(memo, x);"), 1);
    assert_eq!(sites("/// kept with `blaze_memo::keep_owned(..)`\nfn f() {}"), 0);
    assert_eq!(sites("let a = my_keep_owned(memo, x); let b = keep_owned_all(memo, x);"), 0);
    assert_eq!(sites("#[blaze_memo::tracked(owns)]\nfn f() { keep_owned(m, x); keep_owned(m, y) }"), 3);
    let held = baseline("a\t1\n");
    let mut found = BTreeMap::new();
    found.insert("a".to_owned(), 2);
    assert_eq!(over(&found, &held).len(), 1, "a planted extra `owns` must be refused");
    found.insert("a".to_owned(), 1);
    assert!(over(&found, &held).is_empty());
}
