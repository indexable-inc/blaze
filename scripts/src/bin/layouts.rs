//! layouts: a persisted layout's version, schema or magic constant does not change without a
//! test that a file of the previous layout is handled (rule persisted-state-round-trips). `DOC`
//! below is what its usage error prints.
//!
//!     blaze run -q -p scripts --bin layouts -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin layouts -- selftest
//!
//! The list, scripts/layouts.txt, names every constant of crates/ whose name holds SCHEMA,
//! LAYOUT, FORMAT, MAGIC or VERSION, each with its verdict: the crate's test of the previous
//! layout, `none` (persisted, no such test yet), or `not persisted` and why. A constant with no
//! row is refused, so the list stays whole; a persisted constant whose value differs from the
//! base's is refused unless its row names a test that the crate defines and whose name says
//! previous, old or earlier and layout, format, version, schema or magic. No compiler: it reads
//! the text, a gate step of the `scripts` job (.github/workflows/blaze.yaml), beside `sizes`.
//!
//! The instance: the layouts of the store's records changed (hir-ty's check_cache pages, the
//! frozen name tables) with a test of the previous layout each time; others (blaze-kernel's
//! tables, blaze-vfs's journal) changed their magic with none, and nothing said so.

/// What the usage error prints.
const DOC: &str = r#"layouts: a persisted layout's constant does not change without a test of its previous layout (rule persisted-state-round-trips).

    blaze run -q -p scripts --bin layouts -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin layouts -- selftest

    command   check   every SCHEMA/LAYOUT/FORMAT/MAGIC/VERSION constant of a tracked .rs file under
                      crates/ (not a test, bench or build script; no directory named `target`,
                      `vendor`, `tests` or `benches`) has a row of scripts/layouts.txt, and every
                      row's constant exists; a row's `test <fn>` is a function of the constant's
                      crate whose name says the previous layout; with `base`, a constant whose
                      value changed against the base and whose row is `none` is refused, and so
                      is one whose row turned `not persisted` since the base (CI)
    root      the checkout (".")
    base      the commit the change is against, or null (no value is compared)

The row is `<path><TAB><CONST><TAB><verdict>`, the verdict `test <fn>`, `none` or
`not persisted: <why>`. A value is the constant's text from `=` to its `;`, whitespace folded.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_script::{
    fail, git,
    input::{self, Fields, Input, InputError},
    read_text,
};
use regex::Regex;

const SCRIPT: &str = "layouts";
/// The list, from the root.
const LIST: &str = "scripts/layouts.txt";
/// The directories whose files are not scanned for constants (`target`, `vendor`: not this
/// tree's; `tests`, `benches`: no persisted layout is defined there).
const SKIPPED: [&str; 4] = ["target", "vendor", "tests", "benches"];
/// The words of a test's name that say it reads an earlier layout, and what of.
const EARLIER: [&str; 4] = ["previous", "old", "older", "earlier"];
const WHAT: [&str; 9] = ["layout", "layouts", "format", "formats", "version", "versions", "schema", "schemas", "magic"];
/// What every message about a changed layout ends with.
const RULE: &str = "rule persisted-state-round-trips: anything persisted has a test that a file of its previous layout is handled";

struct Args {
    root: String,
    base: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "base"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        f.one_of("command", &["check"], None)?;
        Ok(Args { root: f.str("root", Some("."))?, base: f.str_or_null("base")? })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Verdict {
    /// The crate's test of the previous layout.
    Test(String),
    /// Persisted, with no such test yet.
    Untested,
    /// Not persisted, and why.
    NotPersisted(String),
}

impl Verdict {
    fn persisted(&self) -> bool {
        !matches!(self, Verdict::NotPersisted(_))
    }
}

/// A constant: its file and its name.
type Key = (String, String);

struct Patterns {
    konst: Regex,
    func: Regex,
}

impl Patterns {
    fn new() -> Patterns {
        let re = |pattern: &str| Regex::new(pattern).unwrap_or_else(|e| fail(SCRIPT, &format!("{pattern}: {e}")));
        Patterns {
            konst: re(
                r"(?m)^[ \t]*(?:pub(?:\([a-z: ]+\))?[ \t]+)?const[ \t]+([A-Z0-9_]*(?:SCHEMA|LAYOUT|FORMAT|MAGIC|VERSION)[A-Z0-9_]*)[ \t]*:",
            ),
            func: re(r"\bfn[ \t]+([a-z0-9_]+)[ \t]*[<(]"),
        }
    }
}

/// Whether a test's name says it is of an earlier layout: a word of [`EARLIER`] and one of
/// [`WHAT`], words being its `_`-separated parts.
fn says_earlier(name: &str) -> bool {
    let words: Vec<&str> = name.split('_').collect();
    words.iter().any(|w| EARLIER.contains(w)) && words.iter().any(|w| WHAT.contains(w))
}

/// Whether the constants of `path` (from the root) are read: a `.rs` file under crates/ that is
/// no test, bench or build script.
fn scanned(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let file = parts.last().copied().unwrap_or("");
    path.starts_with("crates/")
        && file.ends_with(".rs")
        && !parts[..parts.len() - 1].iter().any(|p| SKIPPED.contains(p))
        && file != "tests.rs"
        && file != "build.rs"
        && !file.ends_with("_tests.rs")
}

/// The end of the value that starts at `from`: the first `;` outside brackets, strings and
/// character literals.
fn value_end(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => return Some(at),
            b'"' => {
                // A raw string's hashes come before its quote: `r#"`.
                let hashes = text[from..at].bytes().rev().take_while(|b| *b == b'#').count();
                let raw = at >= hashes + 1 && bytes[at - hashes - 1] == b'r';
                at += 1;
                if raw {
                    let close = format!("\"{}", "#".repeat(hashes));
                    at += text[at..].find(&close)? + close.len() - 1;
                } else {
                    while at < bytes.len() && bytes[at] != b'"' {
                        at += if bytes[at] == b'\\' { 2 } else { 1 };
                    }
                }
            }
            // A character literal (`';'`, `'\''`); a lifetime has no closing quote near.
            b'\'' if bytes.get(at + 1) == Some(&b'\\') => {
                at += 3 + text.get(at + 3..)?.find('\'')?;
            }
            b'\'' if bytes.get(at + 2) == Some(&b'\'') => at += 2,
            _ => {}
        }
        at += 1;
    }
    None
}

/// The layout constants a file defines, by name, each with its value: its text from `=` to its
/// `;`, whitespace folded (empty for a trait's constant, which has none). The first definition
/// of a name is the one kept; one that starts a line, as rustfmt writes every item.
fn consts(p: &Patterns, text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for found in p.konst.captures_iter(text) {
        let (Some(whole), Some(name)) = (found.get(0), found.get(1)) else { continue };
        // The type's own `;` (`[u8; 8]`) is inside brackets, so the first one outside ends it.
        let Some(end) = value_end(text, whole.end()) else { continue };
        let item = &text[whole.end()..end];
        let value = item.find('=').map_or("", |eq| &item[eq + 1..]);
        out.entry(name.as_str().to_owned()).or_insert_with(|| value.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    out
}

/// The list's rows; `Err` names the line that is not a row, a comment or blank.
fn parse_rows(text: &str) -> Result<BTreeMap<Key, Verdict>, String> {
    let mut rows = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let row = match line.split('\t').collect::<Vec<_>>().as_slice() {
            [path, name, verdict] if !path.is_empty() && !name.is_empty() => {
                let verdict = if *verdict == "none" {
                    Some(Verdict::Untested)
                } else if let Some(test) = verdict.strip_prefix("test ").filter(|t| !t.is_empty()) {
                    Some(Verdict::Test(test.to_owned()))
                } else {
                    verdict.strip_prefix("not persisted").and_then(|why| why.strip_prefix(": ")).filter(|why| !why.trim().is_empty()).map(|why| Verdict::NotPersisted(why.to_owned()))
                };
                verdict.map(|v| ((*path).to_owned(), (*name).to_owned(), v))
            }
            _ => None,
        };
        let Some((path, name, verdict)) = row else {
            return Err(format!(
                "{LIST}:{}: {line:?} is not `<path><TAB><CONST><TAB>test <fn>|none|not persisted: <why>`",
                at + 1
            ));
        };
        if rows.insert((path.clone(), name.clone()), verdict).is_some() {
            return Err(format!("{LIST}:{}: `{path}` `{name}` has two rows", at + 1));
        }
    }
    Ok(rows)
}

/// The crate a file is of: the longest directory holding a Cargo.toml that is a prefix of it.
fn crate_of<'m>(path: &str, manifests: &'m BTreeSet<String>) -> Option<&'m String> {
    manifests.iter().filter(|dir| path.starts_with(&format!("{dir}/"))).max_by_key(|dir| dir.len())
}

/// What the tree says at head.
struct Head {
    /// Every scanned file's layout constants, with their values.
    consts: BTreeMap<Key, String>,
    /// Each crate's (by its directory) functions whose names say an earlier layout.
    tests: BTreeMap<String, BTreeSet<String>>,
    /// The directories that hold a Cargo.toml.
    manifests: BTreeSet<String>,
}

/// What the base says: its constants' values, and its list's rows when it had one.
struct Base {
    name: String,
    consts: BTreeMap<Key, String>,
    rows: Option<BTreeMap<Key, Verdict>>,
}

/// What is wrong with `rows` against `head`, and with each change since `base`.
fn problems(head: &Head, rows: &BTreeMap<Key, Verdict>, base: Option<&Base>) -> Vec<String> {
    let mut out = Vec::new();
    for (path, name) in head.consts.keys() {
        if !rows.contains_key(&(path.clone(), name.clone())) {
            out.push(format!(
                "{path} defines {name}, a layout constant with no row in {LIST}: add `{path}\t{name}\tnone` \
                 (or `test <the crate's previous-layout test>`, or `not persisted: <why>`)"
            ));
        }
    }
    for ((path, name), verdict) in rows {
        if !head.consts.contains_key(&(path.clone(), name.clone())) {
            out.push(format!("{LIST} has a row for {name} of {path}, which that file does not define: remove it"));
            continue;
        }
        let Verdict::Test(test) = verdict else { continue };
        if !says_earlier(test) {
            out.push(format!(
                "{LIST}: the row of {name} of {path} names {test}, whose name says no earlier layout (a word of {EARLIER:?} and one of {WHAT:?})"
            ));
            continue;
        }
        let krate = crate_of(path, &head.manifests);
        let defined = krate.and_then(|k| head.tests.get(k)).is_some_and(|tests| tests.contains(test));
        if !defined {
            let krate = krate.map_or("its crate", String::as_str);
            out.push(format!("{LIST}: the row of {name} of {path} names {test}, which no file of {krate} defines"));
        }
    }
    let Some(base) = base else { return out };
    for (key, verdict) in rows {
        let (Some(was), Some(now)) = (base.consts.get(key), head.consts.get(key)) else { continue };
        if was == now {
            continue;
        }
        let (path, name) = key;
        let was_persisted = base.rows.as_ref().and_then(|rows| rows.get(key)).is_some_and(Verdict::persisted);
        match verdict {
            Verdict::Untested => out.push(format!(
                "{path}: {name} changed from `{was}` to `{now}` against {}, and its row in {LIST} names no previous-layout test: \
                 write one in its crate that a file of the previous layout is handled and name it in the row ({RULE})",
                base.name
            )),
            Verdict::NotPersisted(_) if was_persisted => out.push(format!(
                "{path}: {name} changed from `{was}` to `{now}` against {}, and its row turned `not persisted` since then: \
                 a layout that was persisted at the base still is ({RULE})",
                base.name
            )),
            Verdict::NotPersisted(_) | Verdict::Test(_) => {}
        }
    }
    out
}

fn split_z(text: &str) -> Vec<String> {
    text.split('\0').filter(|it| !it.is_empty()).map(str::to_owned).collect()
}

fn read_head(p: &Patterns, root: &Path, dir: &str) -> Head {
    let tracked = git(root, &["-C", dir, "ls-files", "-z", "--", "crates"]).unwrap_or_else(|e| fail(SCRIPT, &e));
    let mut head = Head { consts: BTreeMap::new(), tests: BTreeMap::new(), manifests: BTreeSet::new() };
    let files: Vec<String> = split_z(&tracked)
        .into_iter()
        .filter(|path| !path.split('/').any(|part| part == "target" || part == "vendor"))
        .collect();
    for path in &files {
        if let Some(dir) = path.strip_suffix("/Cargo.toml") {
            head.manifests.insert(dir.to_owned());
        }
    }
    for path in files.iter().filter(|path| path.ends_with(".rs")) {
        // A tracked file deleted in the working tree is gone: not read.
        let Some(text) = read_text(&root.join(path)) else { continue };
        if let Some(krate) = crate_of(path, &head.manifests).cloned() {
            let named = p.func.captures_iter(&text).filter_map(|it| it.get(1)).map(|it| it.as_str()).filter(|it| says_earlier(it));
            head.tests.entry(krate).or_default().extend(named.map(str::to_owned));
        }
        if scanned(path) && text.contains("const") {
            for (name, value) in consts(p, &text) {
                head.consts.insert((path.clone(), name), value);
            }
        }
    }
    head
}

fn read_base(p: &Patterns, root: &Path, dir: &str, base: &str, paths: &BTreeSet<&String>) -> Base {
    let commit = format!("{base}^{{commit}}");
    git(root, &["-C", dir, "rev-parse", "--verify", "--quiet", commit.as_str()])
        .unwrap_or_else(|e| fail(SCRIPT, &format!("the base {base} is no commit here (fetch it): {e}")));
    let there: BTreeSet<String> =
        split_z(&git(root, &["-C", dir, "ls-tree", "-r", "-z", "--name-only", base, "--", "crates", LIST]).unwrap_or_else(|e| fail(SCRIPT, &e)))
            .into_iter()
            .collect();
    let show = |path: &str| {
        let spec = format!("{base}:{path}");
        git(root, &["-C", dir, "show", spec.as_str()]).unwrap_or_else(|e| fail(SCRIPT, &e))
    };
    let mut consts = BTreeMap::new();
    for path in paths.iter().filter(|path| there.contains(path.as_str())) {
        for (name, value) in self::consts(p, &show(path)) {
            consts.insert(((*path).clone(), name), value);
        }
    }
    let rows = there.contains(LIST).then(|| parse_rows(&show(LIST)).unwrap_or_else(|e| fail(SCRIPT, &format!("at {base}: {e}"))));
    Base { name: base.to_owned(), consts, rows }
}

fn main_with(it: &Args) {
    let p = Patterns::new();
    let root: PathBuf = std::fs::canonicalize(&it.root).unwrap_or_else(|e| fail(SCRIPT, &format!("the root {} is no directory: {e}", it.root)));
    let dir = root.to_string_lossy().into_owned();
    let head = read_head(&p, &root, &dir);
    if head.consts.is_empty() {
        fail(SCRIPT, &format!("{dir}: no layout constant was found under crates/: nothing was checked"));
    }
    let Some(listed) = read_text(&root.join(LIST)) else { fail(SCRIPT, &format!("{LIST} could not be read")) };
    let rows = parse_rows(&listed).unwrap_or_else(|e| fail(SCRIPT, &e));
    let base = it.base.as_ref().map(|base| {
        let paths: BTreeSet<&String> = rows.keys().map(|(path, _)| path).collect();
        read_base(&p, &root, &dir, base, &paths)
    });
    let wrong = problems(&head, &rows, base.as_ref());
    if !wrong.is_empty() {
        let lines: Vec<String> = wrong.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
    let tested = rows.values().filter(|v| matches!(v, Verdict::Test(_))).count();
    let untested = rows.values().filter(|v| **v == Verdict::Untested).count();
    println!(
        "{SCRIPT}: {} layout constants, each listed: {tested} with a previous-layout test, {untested} persisted with none yet, {} not persisted",
        rows.len(),
        rows.len() - tested - untested
    );
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn selftest() {
    let p = Patterns::new();
    expect(
        says_earlier("a_previous_layout_is_refused") && says_earlier("a_store_of_an_older_format_is_read")
            && !says_earlier("a_hold_of_the_layout") && !says_earlier("the_previous_value"),
        "a test says an earlier layout by its words, not by a substring (`hold` is no `old`)",
    );
    expect(
        scanned("crates/a/src/lib.rs") && !scanned("crates/a/src/tests.rs") && !scanned("crates/a/src/x_tests.rs")
            && !scanned("crates/a/tests/t.rs") && !scanned("crates/a/build.rs") && !scanned("crates/a/vendor/b/lib.rs")
            && !scanned("scripts/src/bin/x.rs"),
        "constants are read from crates/' non-test sources",
    );
    let text = "pub const MAGIC: [u8; 8] = *b\"BLZ;X\\\"1\";\n\
                pub(crate) const LAYOUT: u32 =\n    2;\n\
                const SCHEMA: SchemaHash = schema_of(&[\n    Part::Str(\"a; b\"),\n    Part::U32(LAYOUT),\n]);\n\
                const RAW_FORMAT: &str = r#\"x\"; y\"#;\n\
                const C_VERSION: u8 = b';' as u8;\n\
                const OTHER: u32 = 1;\n\
                trait T {\n    const T_LAYOUT: u32;\n}\n\
                fn f() { const INNER_MAGIC: u8 = 3; }\n\
                    const NESTED_MAGIC: u8 = 4;\n";
    let found = consts(&p, text);
    let want: BTreeMap<String, String> = [
        ("MAGIC", "*b\"BLZ;X\\\"1\""),
        ("LAYOUT", "2"),
        ("SCHEMA", "schema_of(&[ Part::Str(\"a; b\"), Part::U32(LAYOUT), ])"),
        ("RAW_FORMAT", "r#\"x\"; y\"#"),
        ("C_VERSION", "b';' as u8"),
        ("NESTED_MAGIC", "4"),
        ("T_LAYOUT", ""),
    ]
    .iter()
    .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
    .collect();
    expect(
        found == want,
        &format!("a constant's value runs to its own `;`, past strings, brackets and characters; one that does not start its line is not read: {found:?}"),
    );

    let key = |path: &str, name: &str| (path.to_owned(), name.to_owned());
    let manifests: BTreeSet<String> = ["crates/a".to_owned(), "crates/a/sub".to_owned()].into_iter().collect();
    expect(
        crate_of("crates/a/sub/src/x.rs", &manifests).map(String::as_str) == Some("crates/a/sub")
            && crate_of("crates/a/src/x.rs", &manifests).map(String::as_str) == Some("crates/a")
            && crate_of("crates/b/src/x.rs", &manifests).is_none(),
        "a file's crate is the nearest directory with a manifest",
    );
    let head = Head {
        consts: [
            (key("crates/a/src/disk.rs", "MAGIC"), "*b\"BLZD2\"".to_owned()),
            (key("crates/a/src/disk.rs", "LAYOUT"), "2".to_owned()),
            (key("crates/a/src/wire.rs", "SCHEMA"), "4".to_owned()),
        ]
        .into_iter()
        .collect(),
        tests: [("crates/a".to_owned(), ["a_previous_layout_is_refused".to_owned()].into_iter().collect())].into_iter().collect(),
        manifests,
    };
    let rows = parse_rows(
        "# head\ncrates/a/src/disk.rs\tMAGIC\tnone\ncrates/a/src/disk.rs\tLAYOUT\ttest a_previous_layout_is_refused\n\
         crates/a/src/wire.rs\tSCHEMA\tnot persisted: the socket's messages\n",
    )
    .unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    expect(
        rows.get(&key("crates/a/src/wire.rs", "SCHEMA")) == Some(&Verdict::NotPersisted("the socket's messages".to_owned())),
        "a row's verdict is read",
    );
    let same = Base { name: "base".to_owned(), consts: head.consts.clone(), rows: Some(rows.clone()) };
    expect(problems(&head, &rows, Some(&same)).is_empty(), "the tree as listed, unchanged since the base, holds");

    // Planted: a persisted constant with no test changed.
    let mut was = same.consts.clone();
    was.insert(key("crates/a/src/disk.rs", "MAGIC"), "*b\"BLZD1\"".to_owned());
    let changed = Base { name: "base".to_owned(), consts: was, rows: Some(rows.clone()) };
    let said = problems(&head, &rows, Some(&changed));
    expect(
        said.len() == 1 && said[0].contains("MAGIC changed from `*b\"BLZD1\"` to `*b\"BLZD2\"`") && said[0].contains("persisted-state-round-trips"),
        &format!("a changed magic with no previous-layout test is refused, naming both values: {said:?}"),
    );
    // A changed constant whose row names its test holds; one not persisted holds.
    let mut was = same.consts.clone();
    was.insert(key("crates/a/src/disk.rs", "LAYOUT"), "1".to_owned());
    was.insert(key("crates/a/src/wire.rs", "SCHEMA"), "3".to_owned());
    let tested = Base { name: "base".to_owned(), consts: was.clone(), rows: Some(rows.clone()) };
    expect(problems(&head, &rows, Some(&tested)).is_empty(), "a change with a previous-layout test, or of no persisted layout, holds");
    // Planted: the row turned `not persisted` in the same change.
    let mut dodged = rows.clone();
    dodged.insert(key("crates/a/src/disk.rs", "LAYOUT"), Verdict::NotPersisted("said so".to_owned()));
    let said = problems(&head, &dodged, Some(&tested));
    expect(said.len() == 1 && said[0].contains("turned `not persisted`"), &format!("a row turned not persisted to pass: {said:?}"));
    // Planted: a constant with no row, a row with no constant, a test the crate lacks, a test
    // whose name says nothing of a layout.
    let mut missing = rows.clone();
    missing.remove(&key("crates/a/src/disk.rs", "MAGIC"));
    missing.insert(key("crates/a/src/gone.rs", "LAYOUT"), Verdict::Untested);
    missing.insert(key("crates/a/src/wire.rs", "SCHEMA"), Verdict::Test("an_old_schema_is_read".to_owned()));
    let said = problems(&head, &missing, None);
    expect(
        said.len() == 3
            && said.iter().any(|it| it.contains("defines MAGIC, a layout constant with no row"))
            && said.iter().any(|it| it.contains("which that file does not define"))
            && said.iter().any(|it| it.contains("an_old_schema_is_read, which no file of crates/a defines")),
        &format!("an unlisted constant, a stale row and a missing test are refused: {said:?}"),
    );
    let mut vague = rows.clone();
    vague.insert(key("crates/a/src/disk.rs", "LAYOUT"), Verdict::Test("it_round_trips".to_owned()));
    let said = problems(&head, &vague, None);
    expect(said.len() == 1 && said[0].contains("says no earlier layout"), &format!("a test that is not of a previous layout: {said:?}"));
    expect(
        parse_rows("a\tB\n").is_err() && parse_rows("a\tB\tmaybe\n").is_err() && parse_rows("a\tB\tnot persisted\n").is_err()
            && parse_rows("a\tB\ttest \n").is_err() && parse_rows("a\tB\tnone\na\tB\tnone\n").is_err(),
        "a bad row is refused",
    );
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    /// The script's own cases run under `blaze test` too.
    #[test]
    fn the_selftest_holds() {
        super::selftest();
    }
}
