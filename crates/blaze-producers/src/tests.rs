//! The walk over an in-memory workspace. The causal claim (rule 23): a change to one file moves
//! exactly the rows whose crates hold it, and a file outside every closure moves none; with the
//! cause removed (no edit) nothing moves, and a listing in another order gives the same bytes.

use blaze_ids::producer::Row;
use blaze_own::{Alloc, Vec};

use crate::{Digest, Digests, Entry, ROWS, Refused, Step, Walk};

const LOCK: &str = r#"version = 4

[[package]]
name = "inner"
version = "0.1.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aa"

[[package]]
name = "outside"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "bb"
dependencies = [
 "inner",
 "patched",
]

[[package]]
name = "unused-outside"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "cc"

[[package]]
name = "patched"
version = "0.3.0"
dependencies = [
 "inner",
]
"#;

const ROOT: &str = r#"[workspace]
members = ["crates/*", "tools/lone"]
exclude = ["crates/parked"]

[workspace.package]
edition = "2024"

[workspace.dependencies]
blaze-own = { path = "crates/blaze-own" }
blaze-typeck = { path = "crates/blaze-typeck" }
outside = "1.0"
unused-outside = "2.0"

[patch.crates-io]
patched = { path = "vendor/patched" }
unreached = { path = "vendor/unreached" }
"#;

/// The workspace: blaze-lower depends on blaze-expand and blaze-typeck; blaze-ladder on
/// blaze-typeck (under a target table); blaze-scope on an outside crate, and on blaze-typeck
/// only for its tests; blaze-cli (an entry that names no row), blaze-other and tools/lone are in
/// no closure; crates/parked is excluded and has no manifest. vendor/patched
/// is a `[patch]` path crate (its lock row has no source) that `outside` depends on, so it is in
/// the names row by its files (its optional `absent`, in no lock, is not built: no refusal);
/// vendor/unreached is patched and reached by none.
const TREE: &[(&str, &str)] = &[
    ("Cargo.toml", ROOT),
    ("Cargo.lock", LOCK),
    ("pins/toolchain", "nightly-2026-11-01\n"),
    ("pins/answers/rustc.json", "{}\n"),
    ("docs/README.md", "words\n"),
    ("crates/parked/src/lib.rs", "// parked\n"),
    ("crates/blaze-own/Cargo.toml", "[package]\nname = \"blaze-own\"\n"),
    ("crates/blaze-own/src/lib.rs", "pub struct Vec;\n"),
    (
        "crates/blaze-expand/Cargo.toml",
        "[package]\nname = \"blaze-expand\"\n\n[package.metadata.blaze]\nproducer = [\"expander\"]\n\n[dependencies]\nblaze-own.workspace = true\n",
    ),
    ("crates/blaze-expand/src/lib.rs", "pub fn expand() {}\n"),
    ("crates/blaze-expand/tests/expand.rs", "#[test] fn t() {}\n"),
    ("crates/blaze-expand/.DS_Store", "finder\n"),
    (
        "crates/blaze-typeck/Cargo.toml",
        "[package]\nname = \"blaze-typeck\"\n\n[dependencies]\nblaze-own.workspace = true\n",
    ),
    ("crates/blaze-typeck/src/lib.rs", "pub fn check() {}\n"),
    ("crates/blaze-typeck/src/tests/mod.rs", "// unit tests in src count\n"),
    (
        "crates/blaze-lower/Cargo.toml",
        "[package]\nname = \"blaze-lower\"\n\n[package.metadata.blaze]\nproducer = [\"lowering\"]\n\n[dependencies]\nblaze-expand = { path = \"../blaze-expand\" }\nblaze-typeck.workspace = true\n",
    ),
    ("crates/blaze-lower/src/lib.rs", "pub fn lower() {}\n"),
    (
        "crates/blaze-scope/Cargo.toml",
        "[package]\nname = \"blaze-scope\"\n\n[package.metadata.blaze]\nproducer = [\"names\"]\n\n[dependencies]\noutside.workspace = true\n\n[dev-dependencies]\nblaze-typeck.workspace = true\n",
    ),
    ("crates/blaze-scope/src/lib.rs", "pub fn names() {}\n"),
    (
        "crates/blaze-ladder/Cargo.toml",
        "[package]\nname = \"blaze-ladder\"\n\n[package.metadata.blaze]\nproducer = [\"body-checker\", \"code-builder\"]\n\n[target.'cfg(unix)'.dependencies]\nblaze-typeck = { path = \"../blaze-typeck\" }\n",
    ),
    ("crates/blaze-ladder/src/lib.rs", "pub fn front() {}\n"),
    (
        "crates/blaze-cli/Cargo.toml",
        "[package]\nname = \"blaze-cli\"\n\n[dependencies]\nblaze-ladder = { path = \"../blaze-ladder\" }\nblaze-lower = { path = \"../blaze-lower\" }\nblaze-scope = { path = \"../blaze-scope\" }\n",
    ),
    ("crates/blaze-cli/src/main.rs", "fn main() {}\n"),
    ("crates/blaze-cli/build.rs", "fn main() {}\n"),
    ("crates/blaze-cli/build/producers.rs", "// driver\n"),
    (
        "crates/blaze-other/Cargo.toml",
        "[package]\nname = \"blaze-other\"\n\n[dependencies]\nunused-outside.workspace = true\n",
    ),
    ("crates/blaze-other/src/lib.rs", "pub fn other() {}\n"),
    ("tools/lone/Cargo.toml", "[package]\nname = \"lone\"\n"),
    ("tools/lone/src/main.rs", "fn main() {}\n"),
    (
        "vendor/patched/Cargo.toml",
        "[package]\nname = \"patched\"\nversion = \"0.3.0\"\n\n[dependencies.inner]\nversion = \"0.1\"\n\n[dependencies.absent]\nversion = \"1\"\noptional = true\n",
    ),
    ("vendor/patched/src/lib.rs", "pub fn patched() {}\n"),
    ("vendor/patched/tests/t.rs", "#[test] fn t() {}\n"),
    ("vendor/unreached/Cargo.toml", "[package]\nname = \"unreached\"\nversion = \"1.0.0\"\n"),
    ("vendor/unreached/src/lib.rs", "pub fn unreached() {}\n"),
];

/// The tree with `edits` applied: each replaces the file at its path (or adds it).
fn tree<'t, 'a>(edits: &[(&'t str, &'t str)], alloc: Alloc<'a>) -> Vec<(&'t str, &'t str), Alloc<'a>> {
    let mut out = Vec::new_in(alloc);
    for &(path, text) in TREE {
        if !edits.iter().any(|(p, _)| *p == path) {
            out.push((path, text));
        }
    }
    for &edit in edits {
        out.push(edit);
    }
    out
}

/// Drives a walk over `files`; `reversed` lists every directory backwards.
fn run<'a>(files: &[(&str, &str)], reversed: bool, alloc: Alloc<'a>) -> Result<Digests<'a>, Refused<'a>> {
    let mut walk = Walk::new(alloc);
    loop {
        match walk.next()? {
            Step::Done => break,
            Step::Read(path) => {
                let found = files.iter().find(|(p, _)| *p == path).map(|(_, text)| text.as_bytes());
                walk.read(found)?;
            }
            Step::List(dir) => {
                let mut entries: Vec<Entry<'_>, Alloc<'_>> = Vec::new_in(alloc);
                let mut owned: Vec<(usize, usize, usize, bool), Alloc<'_>> = Vec::new_in(alloc);
                for (at, (path, _)) in files.iter().enumerate() {
                    let Some(rest) = path.strip_prefix(dir).and_then(|r| r.strip_prefix('/')) else { continue };
                    let start = path.len() - rest.len();
                    let (len, is_dir) = match rest.find('/') {
                        Some(end) => (end, true),
                        None => (rest.len(), false),
                    };
                    if !owned.iter().any(|&(a, s, l, _)| files[a].0[s..s + l] == path[start..start + len]) {
                        owned.push((at, start, len, is_dir));
                    }
                }
                let listed = owned.iter().map(|&(at, start, len, is_dir)| Entry { name: &files[at].0[start..start + len], is_dir });
                for entry in listed {
                    entries.push(entry);
                }
                if reversed {
                    entries.reverse();
                }
                walk.listed(if entries.is_empty() { None } else { Some(&entries[..]) })?;
            }
        }
    }
    walk.finish()
}

/// The rows that differ between two walks.
fn moved<'a>(a: &Digests<'_>, b: &Digests<'_>, alloc: Alloc<'a>) -> Vec<&'static str, Alloc<'a>> {
    let mut out = Vec::new_in(alloc);
    for at in 0..ROWS.len() {
        assert_eq!(a.rows[at].0, b.rows[at].0);
        let (one, two) = (a.rows[at].1.source(), b.rows[at].1.source());
        assert!(one.is_some() && two.is_some(), "row {} refused: {:?} {:?}", a.rows[at].0.name(), a.rows[at].1, b.rows[at].1);
        if one != two {
            out.push(a.rows[at].0.name());
        }
    }
    out
}

#[test]
fn one_producers_source_moves_its_rows_alone() {
    let alloc = blaze_own::testing::heap().alloc();
    let base = run(&tree(&[], alloc), false, alloc).expect("the workspace walks");
    // The control: the cause removed (no edit), in either listing order: nothing moves.
    let again = run(&tree(&[], alloc), true, alloc).expect("the workspace walks backwards");
    assert!(moved(&base, &again, alloc).is_empty(), "the same tree in another listing order");
    assert_eq!(base.rows.each_ref().map(|(row, _)| *row), Row::FROM_SOURCE);
    let inner_moved = LOCK.replace("\"aa\"", "\"ab\"");
    let unused_moved = LOCK.replace("\"cc\"", "\"cd\"");
    let entry_moved = ROOT.replace(
        "blaze-typeck = { path = \"crates/blaze-typeck\" }",
        "blaze-typeck = { path = \"crates/blaze-typeck\", default-features = false }",
    );
    let edition_moved = ROOT.replace("edition = \"2024\"", "edition = \"2021\"");
    let all: &[&str] = &["expander", "lowering", "body-checker", "names", "code-builder"];
    let cases: &[(&str, &str, &[&str])] = &[
        ("crates/blaze-expand/src/lib.rs", "pub fn expand() { 1; }\n", &["expander", "lowering"]),
        ("crates/blaze-typeck/src/tests/mod.rs", "// changed\n", &["lowering", "body-checker", "code-builder"]),
        ("crates/blaze-own/src/lib.rs", "pub struct Vec(u8);\n", &["expander", "lowering", "body-checker", "code-builder"]),
        ("crates/blaze-cli/src/main.rs", "fn main() { 2; }\n", &[]),
        ("crates/blaze-scope/src/lib.rs", "pub fn names() { 3; }\n", &["names"]),
        ("Cargo.lock", inner_moved.as_str(), &["names"]),
        ("Cargo.lock", unused_moved.as_str(), &[]),
        ("Cargo.toml", entry_moved.as_str(), &["lowering"]),
        ("Cargo.toml", edition_moved.as_str(), all),
        ("pins/toolchain", "nightly-2026-11-02\n", all),
        // Every file under pins/ is an input of every row, at any depth, new ones too: blaze-pins
        // includes them from outside its own directory.
        ("pins/answers/rustc.json", "{\"version\": 2}\n", all),
        ("pins/llvm.json", "{}\n", all),
        ("pins/.DS_Store", "finder moved\n", &[]),
        // Prose about the pins is not a pin.
        ("pins/README.md", "other words about the pins\n", &[]),
        ("crates/blaze-other/src/lib.rs", "pub fn other() { 4; }\n", &[]),
        ("crates/blaze-expand/tests/expand.rs", "#[test] fn u() {}\n", &[]),
        ("crates/blaze-expand/.DS_Store", "finder moved\n", &[]),
        ("tools/lone/src/main.rs", "fn main() { 5; }\n", &[]),
        ("docs/README.md", "other words\n", &[]),
        ("crates/blaze-expand/src/new.rs", "// a new file\n", &["expander", "lowering"]),
        // A `[patch]` path crate the lock reaches through `outside`: its files are the names
        // row's (with rows of a source alone, this moved nothing: the stale-answer hole).
        ("vendor/patched/src/lib.rs", "pub fn patched() { 6; }\n", &["names"]),
        ("vendor/patched/tests/t.rs", "#[test] fn u() {}\n", &[]),
        ("vendor/unreached/src/lib.rs", "pub fn unreached() { 7; }\n", &[]),
    ];
    for &(path, text, wanted) in cases {
        let edited = run(&tree(&[(path, text)], alloc), false, alloc).expect("the edited workspace walks");
        let got = moved(&base, &edited, alloc);
        assert_eq!(&got[..], wanted, "an edit of {path}");
    }
    std::println!("producers: {} edits each moved exactly their producers' rows", cases.len());
}

#[test]
fn watch_names_the_digested_entries_and_every_manifest() {
    let alloc = blaze_own::testing::heap().alloc();
    let made = run(&tree(&[], alloc), false, alloc).expect("the workspace walks");
    let has = |path: &str| made.watch.iter().any(|w| w.as_str() == path);
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "pins",
        "crates/blaze-expand/src",
        "crates/blaze-expand/Cargo.toml",
        "crates/blaze-cli/Cargo.toml",
        "crates/blaze-other/Cargo.toml",
        "tools/lone/Cargo.toml",
        "vendor/patched/src",
        "vendor/patched/Cargo.toml",
        "vendor/unreached/Cargo.toml",
    ] {
        assert!(has(path), "watch holds {path}");
    }
    for path in [
        "crates/blaze-expand/tests",
        "crates/blaze-expand/.DS_Store",
        "crates/blaze-other/src",
        "crates/blaze-cli/src",
        "crates/blaze-cli/build.rs",
        "docs",
        "vendor/patched/tests",
        "vendor/unreached/src",
    ] {
        assert!(!has(path), "watch leaves out {path}");
    }
    assert!(made.watch.windows(2).all(|w| w[0].as_str() < w[1].as_str()), "watch sorted, each once");
    std::println!("producers: watch holds {} paths, sorted", made.watch.len());
}

#[test]
fn refusals_name_the_thing() {
    let alloc = blaze_own::testing::heap().alloc();
    let refused = |edits: &[(&str, &str)]| match run(&tree(edits, alloc), false, alloc) {
        Ok(_) => panic!("refused: {edits:?}"),
        Err(why) => why,
    };
    let scope = "crates/blaze-scope/Cargo.toml";
    assert!(matches!(
        refused(&[(scope, "[package]\nname = \"blaze-scope\"\n")]),
        Refused::NoRoot { row: Row::Names }
    ));
    assert!(matches!(
        refused(&[(scope, "[package]\nname = \"blaze-scope\"\n[package.metadata.blaze]\nproducer = [\"checker\"]\n")]),
        Refused::UnknownRow { .. }
    ));
    assert!(matches!(
        refused(&[(scope, "[package]\nname = \"blaze-scope\"\n[package.metadata.blaze]\nproducer = [\"whole\"]\n")]),
        Refused::UnknownRow { .. }
    ));
    assert!(matches!(
        refused(&[(
            scope,
            "[package]\nname = \"blaze-scope\"\n[package.metadata.blaze]\nproducer = [\"names\"]\n[dependencies]\nx = { path = \"../../../x\" }\n"
        )]),
        Refused::Escapes { .. }
    ));
    assert!(matches!(
        refused(&[(
            scope,
            "[package]\nname = \"blaze-scope\"\n[package.metadata.blaze]\nproducer = [\"names\"]\n[dependencies]\nnope.workspace = true\n"
        )]),
        Refused::NoWorkspaceDep { .. }
    ));
    assert!(matches!(refused(&[("Cargo.toml", &*ROOT.replace("exclude = [\"crates/parked\"]\n", ""))]), Refused::Missing { .. }));
    assert!(matches!(refused(&[("Cargo.toml", "[package]\nname = \"x\"\n")]), Refused::Shape { .. }));
    let own = "[package]\nname = \"blaze-cli\"\n[package.metadata.blaze]\nproducer-own = [\"body-checker\"]\n";
    match refused(&[("crates/blaze-cli/Cargo.toml", own)]) {
        why @ Refused::OwnFilesOnly { .. } => {
            assert_eq!(why.path(), Some("crates/blaze-cli/Cargo.toml"));
            let shown = std::format!("{why}");
            assert!(shown.contains("producer-own") && shown.contains("rule 21"), "{shown}");
        }
        other => panic!("`producer-own` read as {other:?}"),
    }
    let text = std::format!("{}", refused(&[(scope, "[package]\nname = \"blaze-scope\"\n")]));
    assert!(text.contains("names") && text.contains("producer-id.md"), "{text}");
    std::println!("producers: eight refusals each named");
}

/// The class this crate closes (rule 21; 2026-11-17, two builds of blaze wrote two values under
/// one `CrateScope` key): a row covers every line its producing function can run. The function
/// is in `cli`, which names the row and calls `writer`, which calls `format`; neither is named
/// anywhere. An edit to either moves the row; an edit outside the closure moves nothing, and no
/// edit moves nothing (the control). The form that left them out, `producer-own` (the crate's
/// own files alone), is refused by name: under it an edit to `writer` moved no row.
#[test]
fn a_row_covers_every_crate_its_producer_calls() {
    let alloc = blaze_own::testing::heap().alloc();
    const SMALL: &[(&str, &str)] = &[
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nedition = \"2024\"\n"),
        ("Cargo.lock", "version = 4\n"),
        ("pins/toolchain", "nightly-2026-11-01\n"),
        (
            "crates/cli/Cargo.toml",
            "[package]\nname = \"cli\"\n\n[package.metadata.blaze]\nproducer = [\"names\"]\n\n[dependencies]\nwriter = { path = \"../writer\" }\n",
        ),
        ("crates/cli/src/lib.rs", "pub fn scope() { writer::put(); }\n"),
        ("crates/writer/Cargo.toml", "[package]\nname = \"writer\"\n\n[dependencies]\nformat = { path = \"../format\" }\n"),
        ("crates/writer/src/lib.rs", "pub fn put() { format::bytes(); }\n"),
        ("crates/format/Cargo.toml", "[package]\nname = \"format\"\n"),
        ("crates/format/src/lib.rs", "pub fn bytes() {}\n"),
        (
            "crates/rest/Cargo.toml",
            "[package]\nname = \"rest\"\n\n[package.metadata.blaze]\nproducer = [\"expander\", \"lowering\", \"body-checker\", \"code-builder\"]\n",
        ),
        ("crates/rest/src/lib.rs", "pub fn rest() {}\n"),
        ("crates/elsewhere/Cargo.toml", "[package]\nname = \"elsewhere\"\n"),
        ("crates/elsewhere/src/lib.rs", "pub fn elsewhere() {}\n"),
    ];
    let edited = |path: &str, text: &'static str| {
        let mut out: Vec<(&str, &str), Alloc<'_>> = Vec::new_in(alloc);
        for &(at, was) in SMALL {
            out.push((at, if at == path { text } else { was }));
        }
        out
    };
    let base = run(SMALL, false, alloc).expect("the small workspace walks");
    assert!(moved(&base, &run(SMALL, true, alloc).expect("walks backwards"), alloc).is_empty(), "the control: no edit");
    let cases: &[(&str, &'static str, &[&str])] = &[
        ("crates/cli/src/lib.rs", "pub fn scope() { writer::put(); writer::put(); }\n", &["names"]),
        ("crates/writer/src/lib.rs", "pub fn put() { format::bytes(); 1; }\n", &["names"]),
        ("crates/format/src/lib.rs", "pub fn bytes() { 2; }\n", &["names"]),
        ("crates/elsewhere/src/lib.rs", "pub fn elsewhere() { 3; }\n", &[]),
    ];
    for &(path, text, wanted) in cases {
        let made = run(&edited(path, text), false, alloc).expect("the edited workspace walks");
        assert_eq!(&moved(&base, &made, alloc)[..], wanted, "an edit of {path}");
    }
    let own = "[package]\nname = \"cli\"\n\n[package.metadata.blaze]\nproducer-own = [\"names\"]\n\n[dependencies]\nwriter = { path = \"../writer\" }\n";
    match run(&edited("crates/cli/Cargo.toml", own), false, alloc) {
        Err(Refused::OwnFilesOnly { path }) => assert_eq!(path.as_str(), "crates/cli/Cargo.toml"),
        other => panic!("`producer-own` walked: {:?}", other.map(|made| made.rows.map(|(row, _)| row))),
    }
    std::println!("producers: an edit to each crate the producer calls moved its row; outside, none; `producer-own` refused");
}

/// A name a closure reaches that the lock lacks (a lock of another moment than the sources, as a
/// snapshot mix builds) and a source-less lock row no crate read is named for each refuse the
/// row that reaches them, by name, and that row alone: every other row has the digest it has with
/// the whole lock. The controls (rule 23): the whole lock refuses nothing, and the optional
/// `absent` the lock lacks refuses only once it is made required.
#[test]
fn a_missing_lock_row_refuses_its_row_alone() {
    let alloc = blaze_own::testing::heap().alloc();
    let base = run(&tree(&[], alloc), false, alloc).expect("the workspace walks");
    assert!(base.rows.iter().all(|(_, digest)| digest.source().is_some()), "the whole lock refuses nothing");
    let no_outside = LOCK.replace("name = \"outside\"", "name = \"elsewhere\"");
    let unpatched = ROOT.replace("patched = { path = \"vendor/patched\" }\n", "");
    // The optional dependency's control: made required, the lock's lack of it refuses the row.
    let required = "[package]\nname = \"patched\"\nversion = \"0.3.0\"\n\n[dependencies.absent]\nversion = \"1\"\n";
    let cases = [
        ("Cargo.lock", no_outside.as_str(), "outside"),
        ("Cargo.toml", unpatched.as_str(), "patched"),
        ("vendor/patched/Cargo.toml", required, "absent"),
    ];
    for (path, text, name) in cases {
        let made = run(&tree(&[(path, text)], alloc), false, alloc).expect("a missing row stops no walk");
        for (at, (row, digest)) in made.rows.iter().enumerate() {
            match digest {
                Digest::Refused(why @ (Refused::NotLocked { name: named } | Refused::NoPathCrate { name: named }))
                    if *row == Row::Names =>
                {
                    assert_eq!(named.as_str(), name, "an edit of {path}");
                    let shown = std::format!("{why}");
                    assert!(shown.contains(name) && shown.contains("producer-id.md"), "{shown}");
                }
                Digest::Source(digest) if *row != Row::Names => {
                    assert_eq!(Some(digest), base.rows[at].1.source(), "an edit of {path}: row {}", row.name());
                }
                _ => panic!("an edit of {path}: row {} is {digest:?}", row.name()),
            }
        }
    }
    std::println!("producers: a missing lock row, an unnamed path crate and a required unlocked dependency each refused the names row alone");
}
