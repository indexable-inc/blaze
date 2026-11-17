//! The tests' count (decision D72, docs/v2/plan/decisions.json: test code is held to bstd too).
//! It counts exactly the code ambient.rs's own count skips, by the same walk (`walk`) and the
//! same definition of test code (`scan_file`, `test_path`, the modules test code declares):
//! one definition, one home, read the other way round. `TESTS_DOC` below says what it counts.
//!
//!     blaze run -q -p scripts --bin ambient -- '{"command": "tests-check", "base": null}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "tests-write"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "tests-report"}'

use super::*;

/// What ambient.rs's `DOC` says of the tests' count, appended to it in the usage error.
pub(super) const TESTS_DOC: &str = r#"
The tests' count (decision D72): the same walk over exactly the code the count above skips, its
own list tools/ratchets/std-alloc-tests.txt, rows `<crate><TAB>` and ten columns:
  std, alloc, vec_box  as above, read in test code
  is_std               1 for each test target's root (a `.rs` file directly in the crate's
                       `tests` or `benches` directory, or `<that>/<dir>/main.rs`) with no
                       `#![no_std]`: an integration test is its own crate
  fs        the real file system: `std::fs`, `env::temp_dir`, a path led by `tempfile` or
            `tempdir`, `File::open`, `File::create`, `File::create_new`, `File::options`
  clock     the real clock: `Instant::now`, `SystemTime::now`, `thread::sleep`
  threads   operating-system threads: `thread::spawn`, `thread::scope`, `thread::Builder`, a
            path led by `rayon`
  env       the process's environment: `env::var`, `var_os`, `vars`, `vars_os`, `set_var`,
            `remove_var`, `args`, `args_os`, `current_dir`, `set_current_dir`
  random    randomness not drawn from the test's tape: a path led by `rand`, `getrandom` or
            `fastrand`, and `RandomState`
  std_hash  `HashMap` or `HashSet` where `vec_box` reads it as `std`'s (it counts in `vec_box`
            too): its iteration order is seeded per process, so a test that walks one is not
            deterministic
A line counts once per column. An effect is read from the path as written, so `use std::{fs};`,
a renamed import and `.par_iter()` are missed, and a crate's own `thread::spawn` or `env::var`
over-counts. Test code here is what a test target compiles: the test code inside counted files,
every file the count above skips by `#![cfg(test)]`, its name or a declaration, each test
target's root and the modules it declares (beside it); not a file under `tests` or `benches`
no target declares (a fixture, data a test reads), nor a package under such a directory.

    command   tests-check   each crate's tests count is at or under its row; one over is
                            refused, naming its first three sites (the new ones given `base`);
                            an effect column of a crate in the EXCEPTIONS section is not
                            refused, only said; under a row is said, to be lowered by hand
              tests-write   writes the list from the tree: the FIRST snapshot only; it refuses
                            when the list exists (after it, rows only go down, by hand)
              tests-report  every crate by its total, with whether it is on the rebuilt path:
                            reached from crates/blaze-cli's manifest by `[dependencies]` and
                            `[build-dependencies]` (plain and per target, not dev), a dependency
                            being a workspace package by its name (or its `package = ".."`)

The list ends with an EXCEPTIONS section (`# EXCEPTIONS` and `<crate>: <reason>` lines): crates
whose tests' subject IS the machine (D72: running built machine code natively, the operating
system's sandbox's hostile tests, the real-project censuses). An excepted crate is counted and
printed like any other; only its effect columns may go over its row.
"#;

/// The list, from the root.
pub(super) const TESTS_LIST: &str = "tools/ratchets/std-alloc-tests.txt";
/// The list's columns: [`Kind`]'s four, then [`Effect`]'s six.
const WIDTH: usize = 10;
const COLUMNS: [&str; WIDTH] = ["std", "is_std", "alloc", "vec_box", "fs", "clock", "threads", "env", "random", "std_hash"];
/// The first effect column: the ones before are [`Kind`]'s.
const FIRST_EFFECT: usize = KINDS.len();
type TestRow = [usize; WIDTH];
/// The line that starts the list's EXCEPTIONS section.
const EXCEPTIONS: &str = "# EXCEPTIONS";
/// The crate every rebuilt-path crate is reached from.
const REBUILT_FROM: &str = "blaze-cli";
/// What every message about a crate's tests over their row ends with.
const TESTS_RULE: &str = "D72 (docs/v2/plan/decisions.json): a test's world is passed in like any other code's: \
                          `blaze_core::sim::Sim` for files, the clock and crashes, `blaze_sim::check`'s tape for \
                          every choice, blaze-own's and blaze-hash's collections with a fixed seed";
/// The exceptions the first snapshot writes, from D72's three kinds.
const FIRST_EXCEPTIONS: [(&str, &str); 5] = [
    ("blaze-build", "its native tests run the machine code it built, on this machine (D72: built machine code run natively)"),
    ("blaze-emit", "its native tests run the machine code it emitted, on this machine (D72: built machine code run natively)"),
    (
        "blaze-proc-macro",
        "native_sandbox's hostile tests probe the operating system's sandbox itself (D72); the exception is the crate's, so \
         its other tests are held by review until it is split by file",
    ),
    ("blaze-ladder", "its censuses build the real projects from their checkouts and the toolchain on disk (D72)"),
    ("blaze-real", "its censuses read the real projects from their checkouts and the toolchain on disk (D72)"),
];
/// The list's first lines.
const TESTS_HEADER: &str = "# What each crate's TEST code takes from Rust's `std` and `alloc` and from the machine, by\n\
                            # crate (its directory under crates/): the columns of std-alloc.txt read in test code (is_std:\n\
                            # test targets' roots without `#![no_std]`), then lines naming the real file system, the real\n\
                            # clock, OS threads, the environment, randomness not from the test's tape, and a `std` hash\n\
                            # map. Only shrinks; a crate with no row is held to 0 (decision D72, docs/v2/plan/decisions.json).\n\
                            # Written once by scripts/src/bin/ambient.rs (`{\"command\": \"tests-write\"}`), then lowered by\n\
                            # hand; checked by `{\"command\": \"tests-check\"}`.\n\
                            # crate\tstd\tis_std\talloc\tvec_box\tfs\tclock\tthreads\tenv\trandom\tstd_hash\n";

/// An ambient effect that makes a test's result depend on the machine or the moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Effect {
    Fs,
    Clock,
    Threads,
    Env,
    Random,
    /// A `std` `HashMap` or `HashSet`: seeded per process.
    StdHash,
}

/// The collections whose `std` form is [`Effect::StdHash`].
pub(super) const HASHES: [&str; 2] = ["HashMap", "HashSet"];

impl Effect {
    const fn column(self) -> usize {
        FIRST_EFFECT + self as usize
    }
}

/// The effect the word at bytes `at..end` of `text` names, read from the path around it.
pub(super) fn effect_of(text: &str, at: usize, end: usize) -> Option<Effect> {
    let word = &text[at..end];
    let prev = text[..at].trim_end().strip_suffix("::").map(trailing_word).filter(|it| !it.is_empty());
    let next = text[end..].trim_start().strip_prefix("::").map(|rest| {
        let rest = rest.trim_start();
        &rest[..ident_len(rest.as_bytes())]
    });
    // The word leads a path: `rand::thread_rng`, `use tempfile::TempDir`.
    let leads = path_head(text, at).is_none() && next.is_some();
    Some(match word {
        "fs" if prev == Some("std") => Effect::Fs,
        "temp_dir" if prev == Some("env") => Effect::Fs,
        "tempfile" | "tempdir" if leads => Effect::Fs,
        "File" if matches!(next, Some("open" | "create" | "create_new" | "options")) => Effect::Fs,
        "Instant" | "SystemTime" if next == Some("now") => Effect::Clock,
        "sleep" if prev == Some("thread") => Effect::Clock,
        "spawn" | "scope" | "Builder" if prev == Some("thread") => Effect::Threads,
        "rayon" if leads => Effect::Threads,
        "var" | "var_os" | "vars" | "vars_os" | "set_var" | "remove_var" | "args" | "args_os" | "current_dir"
        | "set_current_dir"
            if prev == Some("env") =>
        {
            Effect::Env
        }
        "rand" | "getrandom" | "fastrand" if leads => Effect::Random,
        "RandomState" => Effect::Random,
        _ => return None,
    })
}

/// One line of test code in one column.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct TestSite {
    /// Relative to the root.
    path: String,
    line: usize,
    column: usize,
}

/// A package under a `tests` or `benches` directory: a test's fixture, not a crate.
fn fixture(krate: &str) -> bool {
    krate.split('/').any(|it| TEST_DIRS.contains(&it))
}

/// Each crate's sites in test code, in file, line and column order; a crate with none is absent.
pub(super) fn count_tests(root: &Path) -> Result<BTreeMap<String, Vec<TestSite>>, TreeError> {
    let walked = walk(root)?;
    let index: BTreeMap<String, usize> =
        walked.scanned.iter().enumerate().map(|(at, (parts, _))| (parts.join("/"), at)).collect();
    let mut manifests: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    let krates: Vec<String> = walked.scanned.iter().map(|(parts, _)| crate_of(root, parts, &mut manifests)).collect();
    // The skipped files a test target compiles, whole; the test targets' roots, whose modules
    // are beside them.
    let mut counted: BTreeSet<usize> = BTreeSet::new();
    let mut beside: BTreeSet<usize> = BTreeSet::new();
    let mut todo: Vec<usize> = Vec::new();
    for (at, (parts, _)) in walked.scanned.iter().enumerate() {
        if !walked.skipped.contains(&parts.join("/")) || fixture(&krates[at]) {
            continue;
        }
        // The directories between the crate's and the file.
        let depth = 1 + krates[at].split('/').count();
        let inner: &[String] = parts.get(depth..parts.len().saturating_sub(1)).unwrap_or(&[][..]);
        let target = match inner {
            [dir] => TEST_DIRS.contains(&dir.as_str()),
            [dir, _] => TEST_DIRS.contains(&dir.as_str()) && parts.last().is_some_and(|it| it == "main.rs"),
            _ => false,
        };
        if target {
            beside.insert(at);
        }
        if (target || !inner.iter().any(|it| TEST_DIRS.contains(&it.as_str()))) && counted.insert(at) {
            todo.push(at);
        }
    }
    // Bounded: a file enters `counted` once, and only a file that entered is taken again.
    while let Some(at) = todo.pop() {
        let (parts, found) = &walked.scanned[at];
        let next: Vec<String> = if beside.contains(&at) {
            let dir = parts[..parts.len() - 1].join("/");
            found
                .mods
                .iter()
                .flat_map(|decl| match &decl.path {
                    Some(named) => vec![super::path_file(parts, named)],
                    None => vec![format!("{dir}/{}.rs", decl.name), format!("{dir}/{}/mod.rs", decl.name)],
                })
                .collect()
        } else {
            walked.children.get(&parts.join("/")).cloned().unwrap_or_default()
        };
        for file in next {
            let Some(&child) = index.get(&file) else { continue };
            if !fixture(&krates[child]) && counted.insert(child) {
                todo.push(child);
            }
        }
    }
    let mut out: BTreeMap<String, Vec<TestSite>> = BTreeMap::new();
    for (at, (parts, found)) in walked.scanned.iter().enumerate() {
        let path = parts.join("/");
        let whole = counted.contains(&at);
        // A fixture, or a file under `tests` that no target declares.
        if walked.skipped.contains(&path) && !whole {
            continue;
        }
        let site = |line: usize, column: usize| TestSite { path: path.clone(), line, column };
        let mut sites: Vec<TestSite> = Vec::new();
        if beside.contains(&at) && !found.no_std {
            sites.push(site(1, Kind::IsStd.column()));
        }
        let code: &[(usize, Kind)] = if whole { &found.sites } else { &[] };
        sites.extend(found.test_sites.iter().chain(code).map(|(line, kind)| site(*line, kind.column())));
        let effects: &[(usize, Effect)] = if whole { &found.effects } else { &[] };
        sites.extend(found.test_effects.iter().chain(effects).map(|(line, effect)| site(*line, effect.column())));
        if sites.is_empty() {
            continue;
        }
        sites.sort();
        out.entry(krates[at].clone()).or_default().extend(sites);
    }
    Ok(out)
}

/// The counts of `sites`, by column.
fn tally_tests(sites: &[TestSite]) -> TestRow {
    let mut row = [0; WIDTH];
    for it in sites {
        row[it.column] += 1;
    }
    row
}

/// The tree's totals, as a line of output.
fn totals_line(counts: &BTreeMap<String, Vec<TestSite>>) -> String {
    let mut row = [0; WIDTH];
    for sites in counts.values() {
        for (sum, n) in row.iter_mut().zip(tally_tests(sites)) {
            *sum += n;
        }
    }
    let shown: Vec<String> = COLUMNS.iter().zip(row).map(|(name, n)| format!("{name} {n}")).collect();
    format!("in test code: {} (lines; is_std: test targets' roots), in {} crates", shown.join(", "), counts.len())
}

/// The tests' list: each crate's row, and each excepted crate with its reason.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TestsList {
    rows: BTreeMap<String, TestRow>,
    exceptions: BTreeMap<String, String>,
}

/// The tests' list read wrong: where, and what is wrong there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TestsListError {
    /// A line (from 1) before the EXCEPTIONS section that is not a row, a comment or blank.
    NotARow { line: usize, text: String },
    /// A crate's second row.
    TwoRows { line: usize, krate: String },
    /// A line in the EXCEPTIONS section that is not `<crate>: <reason>`.
    NotAnException { line: usize, text: String },
    /// A crate excepted twice.
    TwoExceptions { line: usize, krate: String },
}

impl fmt::Display for TestsListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TestsListError::NotARow { line, text } => write!(
                f,
                "{TESTS_LIST}:{line}: `{text}` is not a row (`<crate>` and {WIDTH} counts, tab-separated), a comment or blank"
            ),
            TestsListError::TwoRows { line, krate } => write!(f, "{TESTS_LIST}:{line}: a second row for `{krate}`"),
            TestsListError::NotAnException { line, text } => {
                write!(f, "{TESTS_LIST}:{line}: `{text}` in the EXCEPTIONS section is not `<crate>: <reason>`")
            }
            TestsListError::TwoExceptions { line, krate } => {
                write!(f, "{TESTS_LIST}:{line}: `{krate}` is excepted a second time")
            }
        }
    }
}

impl std::error::Error for TestsListError {}

fn parse_tests_list(text: &str) -> Result<TestsList, TestsListError> {
    let mut list = TestsList::default();
    let mut excepting = false;
    for (at, line) in text.split('\n').enumerate() {
        if line.starts_with(EXCEPTIONS) {
            excepting = true;
            continue;
        }
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if excepting {
            let parsed = line
                .split_once(':')
                .map(|(krate, reason)| (krate.trim(), reason.trim()))
                .filter(|(krate, reason)| !krate.is_empty() && !reason.is_empty() && !krate.contains(char::is_whitespace));
            let Some((krate, reason)) = parsed else {
                return Err(TestsListError::NotAnException { line: at + 1, text: line.to_owned() });
            };
            if list.exceptions.insert(krate.to_owned(), reason.to_owned()).is_some() {
                return Err(TestsListError::TwoExceptions { line: at + 1, krate: krate.to_owned() });
            }
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let row = match fields.as_slice() {
            [krate, rest @ ..] if !krate.is_empty() && rest.len() == WIDTH => {
                let mut n: TestRow = [0; WIDTH];
                let read = rest.iter().zip(&mut n).all(|(field, slot)| field.parse::<usize>().map(|it| *slot = it).is_ok());
                read.then_some((*krate, n))
            }
            _ => None,
        };
        let Some((krate, n)) = row else {
            return Err(TestsListError::NotARow { line: at + 1, text: line.to_owned() });
        };
        if list.rows.insert(krate.to_owned(), n).is_some() {
            return Err(TestsListError::TwoRows { line: at + 1, krate: krate.to_owned() });
        }
    }
    Ok(list)
}

/// The list as `tests-write` writes it: the header, a sorted row per crate with a site, then
/// the EXCEPTIONS section.
fn render_tests(counts: &BTreeMap<String, Vec<TestSite>>, exceptions: &BTreeMap<String, String>) -> String {
    let mut out = TESTS_HEADER.to_owned();
    for (krate, sites) in counts.iter().filter(|(_, sites)| !sites.is_empty()) {
        let row = tally_tests(sites).map(|n| n.to_string());
        out.push_str(&format!("{krate}\t{}\n", row.join("\t")));
    }
    out.push_str(&format!(
        "\n{EXCEPTIONS}: crates whose tests' subject IS the machine (D72); each is counted and printed, and only its\n\
         # effect columns (fs, clock, threads, env, random, std_hash) may go over its row. `<crate>: <reason>`.\n"
    ));
    for (krate, reason) in exceptions {
        out.push_str(&format!("{krate}: {reason}\n"));
    }
    out
}

/// What `tests-check` finds: the lines that refuse the tree, and the lines it only says.
#[derive(Debug, Default)]
struct Verdict {
    wrong: Vec<String>,
    notes: Vec<String>,
}

/// `counts` against `list`, `base_rows` the base's list (when it had one), `added` the lines
/// added since the base (when one was given).
fn tests_problems(
    counts: &BTreeMap<String, Vec<TestSite>>,
    list: &TestsList,
    base_rows: Option<&BTreeMap<String, TestRow>>,
    added: Option<&BTreeMap<String, BTreeSet<usize>>>,
) -> Verdict {
    let mut out = Verdict::default();
    let crates: BTreeSet<&String> = counts.keys().chain(list.rows.keys()).collect();
    for krate in crates {
        let sites = counts.get(krate).map_or(&[][..], Vec::as_slice);
        let now = tally_tests(sites);
        let held = list.rows.get(krate).copied().unwrap_or([0; WIDTH]);
        let reason = list.exceptions.get(krate);
        let (mut over, mut excused, mut under) = (Vec::new(), Vec::new(), Vec::new());
        for column in 0..WIDTH {
            let (n, h) = (now[column], held[column]);
            if n > h && reason.is_some() && column >= FIRST_EFFECT {
                excused.push(format!("`{}` {n} over {h}", COLUMNS[column]));
            } else if n > h {
                over.push(column);
            } else if n < h {
                under.push(format!("`{}` {n} under {h}", COLUMNS[column]));
            }
        }
        if !over.is_empty() {
            let of_over: Vec<&TestSite> = sites.iter().filter(|it| over.contains(&it.column)).collect();
            let fresh: Vec<&TestSite> = match added {
                Some(added) => of_over
                    .iter()
                    .copied()
                    .filter(|it| added.get(&it.path).is_some_and(|lines| lines.contains(&it.line)))
                    .collect(),
                None => Vec::new(),
            };
            let (which, from) = if fresh.is_empty() { ("sites", &of_over) } else { ("new sites", &fresh) };
            let mut at: Vec<String> = from.iter().map(|it| format!("{}:{}", it.path, it.line)).collect();
            at.dedup();
            at.truncate(3);
            let columns: Vec<String> = over.iter().map(|c| format!("`{}` {} over {}", COLUMNS[*c], now[*c], held[*c])).collect();
            let shown = if list.rows.contains_key(krate) { "its row" } else { "0, as a crate with no row is held" };
            out.wrong.push(format!(
                "`{krate}`'s tests are over {shown} in {TESTS_LIST}: {} (first {which}: {}): {TESTS_RULE}",
                columns.join(", "),
                at.join(", ")
            ));
        }
        if let Some(reason) = reason.filter(|_| !excused.is_empty()) {
            out.notes.push(format!("`{krate}` is excepted ({reason}): {}", excused.join(", ")));
        }
        if !under.is_empty() {
            out.notes.push(format!(
                "`{krate}`'s tests are under their row in {TESTS_LIST}: {}: lower the row by hand (the list only shrinks)",
                under.join(", ")
            ));
        }
    }
    if let Some(base) = base_rows {
        for (krate, n) in &list.rows {
            let was = base.get(krate).copied().unwrap_or([0; WIDTH]);
            for column in 0..WIDTH {
                if n[column] > was[column] {
                    out.wrong.push(format!(
                        "{TESTS_LIST}: the row `{krate}`'s `{}` is {} and was {} at the base: the list only shrinks ({TESTS_RULE})",
                        COLUMNS[column], n[column], was[column]
                    ));
                }
            }
        }
    }
    out
}

/// The base's tests' rows, or `None` when it has no tests' list.
fn base_tests_rows(root: &Path, base: &str) -> Option<BTreeMap<String, TestRow>> {
    let dir = root.to_string_lossy();
    let listed = git(root, &["-C", dir.as_ref(), "ls-tree", "--name-only", base, "--", TESTS_LIST])
        .unwrap_or_else(|why| fail(SCRIPT, &format!("{base}: git could not list its tree: {why}")));
    if listed.trim().is_empty() {
        return None;
    }
    let spec = format!("{base}:{TESTS_LIST}");
    let text = git(root, &["-C", dir.as_ref(), "show", spec.as_str()])
        .unwrap_or_else(|why| fail(SCRIPT, &format!("{base}: git could not show {TESTS_LIST}: {why}")));
    let list = parse_tests_list(&text).unwrap_or_else(|e| fail(SCRIPT, &format!("{base}'s {e}")));
    Some(list.rows)
}

/// What a manifest's text says of its package: its name, and the names of the dependencies it
/// builds with (`[dependencies]` and `[build-dependencies]`, plain and per target, inline or as
/// a table, a `package = ".."` rename read); `None` when it has no `[package]` name.
fn manifest_deps(text: &str) -> Option<(String, Vec<String>)> {
    let mut name = None;
    let mut deps: Vec<String> = Vec::new();
    let mut section = String::new();
    // The section is a dependency's own table (`[dependencies.x]`): its index in `deps`.
    let mut table: Option<usize> = None;
    for line in text.split('\n') {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.trim_start_matches('[').split(']').next().unwrap_or("").trim().to_owned();
            table = None;
            if let Some(Some(dep)) = dependency_section(&section) {
                deps.push(dep.to_owned());
                table = Some(deps.len() - 1);
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if section == "package" && key == "name" {
            name = Some(value.trim().trim_matches('"').to_owned());
            continue;
        }
        if let Some(at) = table {
            if key == "package" {
                deps[at] = value.trim().trim_matches('"').to_owned();
            }
            continue;
        }
        if dependency_section(&section) == Some(None) {
            let dep = renamed(value).unwrap_or_else(|| key.split('.').next().unwrap_or(key).trim().trim_matches('"'));
            deps.push(dep.to_owned());
        }
    }
    name.map(|name| (name, deps))
}

/// Whether the section `section` holds build dependencies: `Some(None)` for the list
/// (`dependencies`, `build-dependencies`, either under `target.<t>.`), `Some(Some(name))` for
/// one dependency's table, `None` for any other section (`dev-dependencies` among them).
fn dependency_section(section: &str) -> Option<Option<&str>> {
    let rest = match section.strip_prefix("target.") {
        Some(rest) => {
            let after = match rest.chars().next() {
                Some(quote @ ('\'' | '"')) => rest[1..].find(quote).map(|end| &rest[end + 2..])?,
                _ => rest.find('.').map(|end| &rest[end..])?,
            };
            after.strip_prefix('.')?
        }
        None => section,
    };
    let table = rest.strip_prefix("dependencies").or_else(|| rest.strip_prefix("build-dependencies"))?;
    match table {
        "" => Some(None),
        _ => table.strip_prefix('.').map(|name| Some(name.trim().trim_matches('"'))),
    }
}

/// The package an inline dependency's `package = ".."` names, if it renames one.
fn renamed(value: &str) -> Option<&str> {
    let at = value.find("package")?;
    let rest = value[at + "package".len()..].trim_start().strip_prefix('=')?.trim_start();
    rest.strip_prefix('"')?.split('"').next()
}

/// The crates (by their path under crates/) on the rebuilt path: [`REBUILT_FROM`] and every
/// workspace package it builds with, directly or through others.
fn rebuilt_path(root: &Path) -> Result<BTreeSet<String>, TreeError> {
    let (_, manifests) = tree_files(root)?;
    let mut by_name: BTreeMap<String, String> = BTreeMap::new();
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for parts in manifests.iter().filter(|it| !test_path(it)) {
        let manifest = parts.join("/");
        let text = std::fs::read(root.join(&manifest))
            .map_err(|e| TreeError::Unreadable { path: manifest.clone(), why: e.to_string() })?;
        let Some((name, names)) = manifest_deps(&universal_newlines(String::from_utf8_lossy(&text).into_owned())) else {
            continue;
        };
        let krate = parts[1..parts.len() - 1].join("/");
        by_name.insert(name, krate.clone());
        deps.insert(krate, names);
    }
    if !deps.contains_key(REBUILT_FROM) {
        return Err(TreeError::Unreadable {
            path: format!("crates/{REBUILT_FROM}/Cargo.toml"),
            why: "no package is there, and the rebuilt path is read from it".to_owned(),
        });
    }
    let mut reached = BTreeSet::from([REBUILT_FROM.to_owned()]);
    let mut todo = vec![REBUILT_FROM.to_owned()];
    // Bounded: a crate enters `reached` once, and only a crate that entered is taken again.
    while let Some(krate) = todo.pop() {
        for name in deps.get(&krate).into_iter().flatten() {
            let Some(dep) = by_name.get(name) else { continue };
            if reached.insert(dep.clone()) {
                todo.push(dep.clone());
            }
        }
    }
    Ok(reached)
}

/// Runs `tests-check`, `tests-write` or `tests-report` on the tree at `root`.
pub(super) fn main_with(command: &str, base: Option<&str>, root: &Path) {
    let counts = count_tests(root).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let path = root.join(TESTS_LIST);
    match command {
        "tests-write" => {
            if path.exists() {
                fail(
                    SCRIPT,
                    &format!(
                        "{TESTS_LIST} exists: `tests-write` makes the FIRST snapshot only (D72); after it a row is lowered by \
                         hand as its crate's tests move to bstd, and `tests-check` holds it (delete the file to take the first \
                         snapshot again)"
                    ),
                );
            }
            let exceptions: BTreeMap<String, String> =
                FIRST_EXCEPTIONS.iter().map(|(krate, reason)| ((*krate).to_owned(), (*reason).to_owned())).collect();
            if let Err(e) = std::fs::write(&path, render_tests(&counts, &exceptions)) {
                fail(SCRIPT, &format!("{TESTS_LIST} is not written: {e}"));
            }
            println!("{SCRIPT}: wrote {TESTS_LIST}: {}", totals_line(&counts));
        }
        "tests-report" => {
            let rebuilt = rebuilt_path(root).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
            let exceptions = match read_text(&path) {
                Some(text) => parse_tests_list(&text).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())).exceptions,
                None => BTreeMap::new(),
            };
            let mut rows: Vec<(&String, TestRow, usize)> = counts
                .iter()
                .map(|(krate, sites)| {
                    let row = tally_tests(sites);
                    (krate, row, row.iter().sum())
                })
                .collect();
            rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
            let on_path: BTreeMap<String, Vec<TestSite>> =
                counts.iter().filter(|(krate, _)| rebuilt.contains(krate.as_str())).map(|(k, s)| (k.clone(), s.clone())).collect();
            println!("{SCRIPT} tests-report: totals {}", totals_line(&counts));
            println!(
                "{SCRIPT} tests-report: the rebuilt path ({} crates reached from crates/{REBUILT_FROM}'s manifest): {}",
                rebuilt.len(),
                totals_line(&on_path)
            );
            println!("crate\ttotal\t{}\trebuilt\texcepted", COLUMNS.join("\t"));
            for (krate, row, sum) in rows {
                let yes = |it: bool| if it { "yes" } else { "no" };
                println!(
                    "{krate}\t{sum}\t{}\t{}\t{}",
                    row.map(|n| n.to_string()).join("\t"),
                    yes(rebuilt.contains(krate.as_str())),
                    yes(exceptions.contains_key(krate.as_str()))
                );
            }
        }
        _ => {
            let Some(text) = read_text(&path) else {
                fail(SCRIPT, &format!("{TESTS_LIST} could not be read: `{{\"command\": \"tests-write\"}}` makes the first snapshot"))
            };
            let list = parse_tests_list(&text).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
            let (base_rows, added) = match base {
                Some(base) => {
                    let (_, added) = at_base(root, base).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
                    (base_tests_rows(root, base), Some(added))
                }
                None => (None, None),
            };
            let found = tests_problems(&counts, &list, base_rows.as_ref(), added.as_ref());
            for line in &found.notes {
                println!("{SCRIPT}: {line}");
            }
            println!("{SCRIPT}: {}", totals_line(&counts));
            if !found.wrong.is_empty() {
                let lines: Vec<String> = found.wrong.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
                eprintln!("{}", lines.join("\n"));
                std::process::exit(1);
            }
            println!("{SCRIPT}: each crate's tests at or under its row in {TESTS_LIST}");
        }
    }
}

/// The tests' count on a small tree, crate `a`:
/// - lib.rs:1, a library fn naming `std::fs`: not test code, so in no tests column (it is the
///   other count's `std`);
/// - lib.rs:4-9, a `#[test]` fn: line 6 `std::fs::read` (`std`, `fs`), line 7 `Instant::now()`
///   (`clock`), line 8 a `std::collections::HashMap` (`std`, `vec_box`, `std_hash`);
/// - t.rs, declared by `#[cfg(test)] mod t;`: line 1 `std::thread::spawn` (`std`, `threads`);
/// - tests/ui/data.rs, under `tests` and declared by no target: a fixture, not counted (its
///   `std::env::var` would be `env` 1).
///
/// Then the ratchet: an `fs` over its row is refused, naming lib.rs:6, unless the crate is
/// excepted. Prints `ambient tests selftest: ok`.
pub(super) fn selftest() {
    let tmp = TempDir::new("ambient-tests-selftest-").unwrap_or_else(|e| failed(&e));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    // The root workspace: what the counts read (tree_files).
    put("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    put(
        "crates/a/src/lib.rs",
        "pub fn lib() -> usize { std::fs::read(\"x\").map_or(0, |it| it.len()) }\n\
         #[cfg(test)]\n\
         mod t;\n\
         #[test]\n\
         fn reads() {\n\
         \x20   let _ = std::fs::read(\"x\");\n\
         \x20   let _ = Instant::now();\n\
         \x20   let _: std::collections::HashMap<u8, u8> = std::collections::HashMap::new();\n\
         }\n",
    );
    put("crates/a/src/t.rs", "fn go() { std::thread::spawn(|| ()); }\n");
    put("crates/a/tests/ui/data.rs", "fn main() { let _ = std::env::var(\"X\"); }\n");
    let counts = count_tests(tmp.path()).unwrap_or_else(|e| failed(&e));
    let row = counts.get("a").map(|sites| tally_tests(sites));
    // std, is_std, alloc, vec_box, fs, clock, threads, env, random, std_hash
    expect(
        counts.len() == 1 && row == Some([3, 0, 0, 1, 1, 1, 1, 0, 0, 1]),
        &format!("the tests' count of crate `a`: {row:?} in {:?}", counts.keys().collect::<Vec<_>>()),
    );
    // The other count is unchanged: lib.rs's root on `std`, and its line 1.
    let other = count(tmp.path()).unwrap_or_else(|e| failed(&e));
    expect(
        other.get("a").map(|sites| tally(sites)) == Some([1, 1, 0, 0]),
        &format!("the count outside tests: {}", shown(&other)),
    );
    // The ratchet: `fs` held at 0 is refused at lib.rs:6; excepted, it is only said.
    let mut list = TestsList::default();
    list.rows.insert("a".to_owned(), [3, 0, 0, 1, 0, 1, 1, 0, 0, 1]);
    let said = tests_problems(&counts, &list, None, None);
    expect(
        said.wrong.len() == 1 && said.wrong[0].contains("`fs` 1 over 0") && said.wrong[0].contains("crates/a/src/lib.rs:6"),
        &format!("an `fs` over its row: {:?}", said.wrong),
    );
    list.exceptions.insert("a".to_owned(), "its subject is the machine".to_owned());
    let said = tests_problems(&counts, &list, None, None);
    expect(said.wrong.is_empty() && said.notes.len() == 1, &format!("an excepted crate: {said:?}"));
    let back = parse_tests_list(&render_tests(&counts, &list.exceptions)).unwrap_or_else(|e| failed(&e));
    expect(
        back.rows.get("a") == row.as_ref() && back.exceptions == list.exceptions,
        &format!("the list reads back as written: {back:?}"),
    );
    expect(
        dependency_section("target.'cfg(unix)'.dependencies") == Some(None)
            && dependency_section("dependencies.x") == Some(Some("x"))
            && dependency_section("dev-dependencies").is_none()
            && manifest_deps("[package]\nname = \"p\"\n[dependencies]\nq.workspace = true\nr = { path = \"../r\", package = \"s\" }\n[dev-dependencies]\nt = \"1\"\n")
                == Some(("p".to_owned(), vec!["q".to_owned(), "s".to_owned()])),
        "a manifest's build dependencies",
    );
    println!("{SCRIPT} tests selftest: ok");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D72's count of test code: prints `ambient tests selftest: ok` (see [`selftest`]).
    #[test]
    fn the_tests_count_holds() {
        selftest();
    }
}
