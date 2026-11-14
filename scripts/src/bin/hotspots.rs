//! hotspots: where refactoring pays off. Files ranked by how often they change times how
//! complex they are (Tornhill, "Your Code as a Crime Scene", 2015: defects and effort gather
//! where the two meet), and pairs of files that change together but are not in one module
//! (logical coupling, Gall et al., ICSM 1998). It is for deciding where to spend refactoring,
//! not a gate: nothing it prints fails a build, and its numbers are estimates. `DOC` below is
//! what its usage error prints.
//!
//!     blaze run -q -p scripts --bin hotspots -- '{"since": "2026-08-13", "top": 20}'
//!     blaze run -q -p scripts --bin hotspots -- selftest
//!
//! Change frequency is the commits that touched a file since `since`, from the one program it
//! runs, `git log --numstat --format=@%H` (with a timeout). Complexity is the file's functions'
//! `function_weight` values held in crates/blaze-lints/baselines/items.tsv (docs/lints.md,
//! "Complexity"): the file holds only items over their bound, so a file of simple functions
//! weighs 0 here and is no hotspot. An item is placed in a file by its module path, read from
//! the file's path (`crates/<dir>/src/a/b.rs` is `<dir with _>::a::b`): a crate whose name is
//! not its directory's, or a module declared with `#[path]`, is not placed.

/// What the usage error prints.
const DOC: &str = r#"hotspots: files ranked by change frequency times complexity, and files that change together
across modules. For deciding where to refactor; not a gate. Every number is an estimate.

    blaze run -q -p scripts --bin hotspots -- '{"since": "2026-08-13", "top": 20}'
    blaze run -q -p scripts --bin hotspots -- selftest

    since    the start of the history read, as `git log --since` takes it ("6 months ago")
    top      how many files and pairs to print (20)
    paths    only these paths (`git log -- <paths>`), all when empty ([])
    root     the checkout (".")
    output   "table" (default) or "json"

score = commits since `since` x the summed `function_weight` of the file's functions held in
crates/blaze-lints/baselines/items.tsv. A pair is coupled when it shares at least 5 commits and
at least half of the commits of the one that changed less; commits of more than 50 files
(moves, sweeping renames) are not counted for pairs.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use blaze_script::{
    expect, fail,
    input::{self, Fields, Input, InputError},
    json::{self, Value},
    read_text, run_within,
};

const SCRIPT: &str = "hotspots";
/// The held values, from the root.
const ITEMS: &str = "crates/blaze-lints/baselines/items.tsv";
/// How long `git log` may run.
const GIT_LIMIT: Duration = Duration::from_secs(120);
/// A commit of more files than this is a sweep, not a change of related files: not counted for
/// pairs.
const MOST_FILES_PER_COMMIT: usize = 50;
/// The fewest commits two files share to be a pair.
const LEAST_SHARED: u32 = 5;
/// The least share, in percent, of the commits of the file that changed less.
const LEAST_DEGREE: u32 = 50;

struct Args {
    since: String,
    top: usize,
    paths: Vec<String>,
    root: String,
    output: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["since", "top", "paths", "root", "output"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let since = f.str("since", Some("6 months ago"))?;
        let top = f.int("top", Some(20))?;
        let top = usize::try_from(top).map_err(|_| InputError(format!("{} is a count, not {top}", f.at("top"))))?;
        Ok(Args {
            since,
            top,
            paths: f.strs("paths")?,
            root: f.str("root", Some("."))?,
            output: f.one_of("output", &["table", "json"], Some("table"))?,
        })
    }
}

/// What the history says of one file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Changes {
    commits: u32,
    /// Lines added and deleted, a binary file's not counted.
    churn: u64,
}

/// The history: each file's changes, and each commit's files (for pairs).
#[derive(Debug, Default)]
struct History {
    files: BTreeMap<String, Changes>,
    commits: Vec<Vec<String>>,
}

/// `git log --numstat --format=@%H`'s output read: a line `@<hash>` starts a commit, a line
/// `<added>\t<deleted>\t<path>` is a file it changed (`-` for a binary file's counts).
fn history(log: &str) -> History {
    let mut out = History::default();
    let mut current: Option<Vec<String>> = None;
    for line in log.lines() {
        if line.starts_with('@') {
            out.commits.extend(current.take());
            current = Some(Vec::new());
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let lines = added.parse::<u64>().unwrap_or(0) + deleted.parse::<u64>().unwrap_or(0);
        let it = out.files.entry(path.to_owned()).or_default();
        it.commits += 1;
        it.churn += lines;
        if let Some(files) = current.as_mut() {
            files.push(path.to_owned());
        }
    }
    out.commits.extend(current);
    out
}

/// The module path of a Rust source file, from its path: `crates/blaze-lints/src/blaze/x.rs` is
/// `blaze_lints::blaze::x`, `.../src/lib.rs` the crate, `.../src/bin/hotspots.rs` `hotspots`.
fn module_of(path: &str) -> Option<String> {
    let path = path.strip_suffix(".rs")?;
    let parts: Vec<&str> = path.split('/').collect();
    let src = parts.iter().rposition(|it| *it == "src")?;
    let mut rest = &parts[src + 1..];
    let krate = match rest {
        ["bin", name, ..] => {
            let name = *name;
            rest = &rest[2..];
            name
        }
        _ => *parts.get(src.checked_sub(1)?)?,
    };
    let mut module = vec![krate.replace('-', "_")];
    module.extend(rest.iter().map(|it| (*it).to_owned()));
    if matches!(module.last().map(String::as_str), Some("lib" | "main" | "mod")) && module.len() > 1 {
        module.pop();
    }
    Some(module.join("::"))
}

/// Each file's summed `function_weight` from the held values `items` (a `items.tsv`), each item
/// placed in the file whose module is the longest prefix of its path; `files` are the files to
/// place items in.
fn weights<'a>(items: &str, files: impl Iterator<Item = &'a str>) -> BTreeMap<String, u64> {
    let mut by_module: BTreeMap<String, String> = BTreeMap::new();
    for file in files {
        if let Some(module) = module_of(file) {
            by_module.entry(module).or_insert_with(|| file.to_owned());
        }
    }
    let mut out = BTreeMap::new();
    for line in items.lines().filter(|it| !it.starts_with('#') && !it.is_empty()) {
        let fields: Vec<&str> = line.split('\t').collect();
        let [measure, path, _, value] = fields.as_slice() else { continue };
        if *measure != "function_weight" {
            continue;
        }
        let Ok(value) = value.parse::<u64>() else { continue };
        let mut module = *path;
        while let Some((parent, _)) = module.rsplit_once("::") {
            module = parent;
            if let Some(file) = by_module.get(module) {
                *out.entry(file.clone()).or_insert(0) += value;
                break;
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hotspot {
    file: String,
    commits: u32,
    churn: u64,
    weight: u64,
    score: u64,
}

/// The files with a weight, by score (highest first), then by path.
fn ranked(history: &History, weights: &BTreeMap<String, u64>) -> Vec<Hotspot> {
    let mut out: Vec<Hotspot> = weights
        .iter()
        .filter_map(|(file, &weight)| {
            let changes = history.files.get(file)?;
            let score = u64::from(changes.commits) * weight;
            Some(Hotspot { file: file.clone(), commits: changes.commits, churn: changes.churn, weight, score })
        })
        .filter(|it| it.score > 0)
        .collect();
    out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.file.cmp(&b.file)));
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pair {
    a: String,
    b: String,
    shared: u32,
    /// Shared commits as a percent of the commits of the one that changed less.
    degree: u32,
}

/// The directory a file is in: two files in one directory are in one module or its children.
fn directory(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// The pairs of Rust files in different directories that change together, by shared commits
/// (most first), then by degree, then by path.
fn coupled(history: &History) -> Vec<Pair> {
    let mut shared: BTreeMap<(String, String), u32> = BTreeMap::new();
    for files in &history.commits {
        if files.len() > MOST_FILES_PER_COMMIT {
            continue;
        }
        let rust: BTreeSet<&str> = files.iter().map(String::as_str).filter(|it| it.ends_with(".rs")).collect();
        let rust: Vec<&str> = rust.into_iter().collect();
        for (at, a) in rust.iter().enumerate() {
            for b in &rust[at + 1..] {
                if directory(a) != directory(b) {
                    *shared.entry(((*a).to_owned(), (*b).to_owned())).or_insert(0) += 1;
                }
            }
        }
    }
    let commits = |file: &str| history.files.get(file).map_or(0, |it| it.commits);
    let mut out: Vec<Pair> = shared
        .into_iter()
        .filter(|&(_, count)| count >= LEAST_SHARED)
        .filter_map(|((a, b), count)| {
            let fewer = commits(&a).min(commits(&b)).max(1);
            let degree = count * 100 / fewer;
            (degree >= LEAST_DEGREE).then_some(Pair { a, b, shared: count, degree })
        })
        .collect();
    out.sort_by(|x, y| y.shared.cmp(&x.shared).then(y.degree.cmp(&x.degree)).then_with(|| (&x.a, &x.b).cmp(&(&y.a, &y.b))));
    out
}

fn table(since: &str, spots: &[Hotspot], pairs: &[Pair]) -> String {
    let mut out = format!("Hotspots since {since} (commits x summed function_weight over the bound; estimates, not a gate)\n");
    out.push_str(&format!("{:>4}  {:>9}  {:>7}  {:>6}  file\n", "rank", "score", "commits", "weight"));
    for (at, it) in spots.iter().enumerate() {
        out.push_str(&format!("{:>4}  {:>9}  {:>7}  {:>6}  {}\n", at + 1, it.score, it.commits, it.weight, it.file));
    }
    out.push_str(&format!(
        "\nFiles that change together across modules (at least {LEAST_SHARED} shared commits, {LEAST_DEGREE}% of the rarer one's)\n"
    ));
    out.push_str(&format!("{:>6}  {:>6}  files\n", "shared", "degree"));
    for it in pairs {
        out.push_str(&format!("{:>6}  {:>5}%  {} <-> {}\n", it.shared, it.degree, it.a, it.b));
    }
    out
}

fn as_json(since: &str, spots: &[Hotspot], pairs: &[Pair]) -> Value {
    let spots: Vec<Value> = spots
        .iter()
        .map(|it| json::json!({"file": it.file, "commits": it.commits, "churn": it.churn, "weight": it.weight, "score": it.score}))
        .collect();
    let pairs: Vec<Value> = pairs
        .iter()
        .map(|it| json::json!({"a": it.a, "b": it.b, "shared": it.shared, "degree_percent": it.degree}))
        .collect();
    json::json!({"since": since, "estimate": true, "hotspots": spots, "coupled": pairs})
}

fn main_with(it: &Args) {
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|e| fail(SCRIPT, &format!("the root {} is no directory: {e}", it.root)));
    let since = format!("--since={}", it.since);
    let mut argv = vec!["git", "log", "--numstat", "--format=@%H", "--no-renames", since.as_str(), "--"];
    argv.extend(it.paths.iter().map(String::as_str));
    let ran = match run_within(&argv, &root, GIT_LIMIT) {
        Ok(Some(ran)) => ran,
        Ok(None) => fail(SCRIPT, &format!("git log did not end within {} s in {}", GIT_LIMIT.as_secs(), root.display())),
        Err(e) => fail(SCRIPT, &e),
    };
    if ran.status != 0 {
        fail(SCRIPT, &format!("git log failed in {} (status {}): {}", root.display(), ran.status, ran.err().trim()));
    }
    let history = history(&ran.out());
    let items_path = root.join(ITEMS);
    let Some(items) = read_text(&items_path) else {
        fail(SCRIPT, &format!("{ITEMS} could not be read: the complexity values come from it (a bless of the complexity lints writes it)"))
    };
    let weights = weights(&items, history.files.keys().map(String::as_str));
    let mut spots = ranked(&history, &weights);
    spots.truncate(it.top);
    let mut pairs = coupled(&history);
    pairs.truncate(it.top);
    match it.output.as_str() {
        "json" => println!("{}", json::to_string_pretty(&as_json(&it.since, &spots, &pairs)).unwrap_or_default()),
        _ => print!("{}", table(&it.since, &spots, &pairs)),
    }
}

fn selftest() {
    let check = |holds: bool, what: &str| expect(SCRIPT, holds, what);
    check(module_of("crates/blaze-lints/src/blaze/complexity.rs").as_deref() == Some("blaze_lints::blaze::complexity"), "a module file");
    check(module_of("crates/blaze-lints/src/lib.rs").as_deref() == Some("blaze_lints"), "a crate root");
    check(module_of("crates/a/src/x/mod.rs").as_deref() == Some("a::x"), "a mod.rs");
    check(module_of("scripts/src/bin/hotspots.rs").as_deref() == Some("hotspots"), "a binary");
    check(module_of("docs/lints.md").is_none(), "not Rust");
    // Six commits: `a/src/x.rs` in all, `b/src/y.rs` in five of them (coupled across modules),
    // `a/src/z.rs` beside `x.rs` in one (one directory, never a pair).
    let mut log = String::new();
    for at in 0..6 {
        log.push_str(&format!("@{at:040}\n\n3\t1\tcrates/a/src/x.rs\n"));
        if at > 0 {
            log.push_str("1\t1\tcrates/b/src/y.rs\n");
        }
        if at == 0 {
            log.push_str("-\t-\tcrates/a/src/z.rs\n");
        }
    }
    let history = history(&log);
    check(history.commits.len() == 6, "six commits");
    check(history.files["crates/a/src/x.rs"] == Changes { commits: 6, churn: 24 }, "a file's commits and churn");
    check(history.files["crates/a/src/z.rs"].churn == 0, "a binary file's counts are not churn");
    let items = "# header\n\
                 function_weight\ta::x::f\t0123456789abcdef\t40\n\
                 function_weight\ta::x::S::g\t0123456789abcdef\t20\n\
                 function_weight\tb::y::h\t0123456789abcdef\t30\n\
                 file_lines\ta::x\t0123456789abcdef\t1200\n\
                 function_weight\tnowhere::k\t0123456789abcdef\t99\n";
    let weights = weights(items, history.files.keys().map(String::as_str));
    check(weights.get("crates/a/src/x.rs") == Some(&60), &format!("a method is placed past its type: {weights:?}"));
    check(weights.get("crates/b/src/y.rs") == Some(&30), "another file");
    check(weights.len() == 2, "an item of no file read is not placed, and other measures are not weight");
    let spots = ranked(&history, &weights);
    check(
        spots.iter().map(|it| (it.file.as_str(), it.score)).collect::<Vec<_>>() == [("crates/a/src/x.rs", 360), ("crates/b/src/y.rs", 150)],
        &format!("ranked by commits x weight: {spots:?}"),
    );
    let pairs = coupled(&history);
    check(
        pairs == [Pair { a: "crates/a/src/x.rs".into(), b: "crates/b/src/y.rs".into(), shared: 5, degree: 100 }],
        &format!("one pair across modules, none within one: {pairs:?}"),
    );
    // Planted: under the shared commits, no pair.
    let few = history_of_shared(LEAST_SHARED - 1);
    check(coupled(&few).is_empty(), "too few shared commits are no pair");
    let text = table("then", &spots, &pairs);
    check(text.contains("   1        360        6      60  crates/a/src/x.rs"), &format!("the table: {text}"));
    let value = as_json("then", &spots, &pairs);
    check(value["hotspots"][0]["score"] == 360 && value["estimate"] == true, "the JSON");
    println!("{SCRIPT} selftest: ok");
}

/// A history of two files in different modules changed together `count` times.
fn history_of_shared(count: u32) -> History {
    let mut log = String::new();
    for at in 0..count {
        log.push_str(&format!("@{at}\n1\t0\tcrates/a/src/x.rs\n1\t0\tcrates/b/src/y.rs\n"));
    }
    history(&log)
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
